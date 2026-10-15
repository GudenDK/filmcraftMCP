//! Sessions of clients before MCP 2026-07-28 (`Mcp-Session-Id`), as the gate sees them: when each
//! was last used, how many of its responses are still streaming, and which request ids are in
//! flight. rmcp's own idle timeout counts session events only, so a long call that sends nothing
//! (an export without a `progressToken`) would be cut off by it, its output deleted; it is turned
//! off, and the reaper here closes only sessions with nothing in flight. At the cap, the least
//! recently used idle session makes room for a new one; only when every session has something in
//! flight is an `initialize` refused.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::hash::{BuildHasher, RandomState};
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::Response;
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Frame, SizeHint};
use serde_json::Value;

use super::HttpBody;

/// The sessions this server created, and their traffic.
pub(super) struct Sessions {
    max: usize,
    /// Request ids are kept as hashes: a client picks them, and any length.
    ids: RandomState,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    live: HashMap<String, Session>,
    /// `initialize` requests that may create a session, not answered yet.
    pending: usize,
}

struct Session {
    last_used: Instant,
    /// Responses still streaming (calls in flight, the GET stream).
    open: usize,
    /// Hashes of the JSON-RPC ids of the requests in flight.
    requests: HashSet<u64>,
}

/// A request id that is already in flight in its session.
#[derive(Debug)]
pub(super) struct Duplicate;

impl Sessions {
    pub(super) fn new(max: usize) -> Arc<Self> {
        Arc::new(Self { max: max.max(1), ids: RandomState::new(), inner: Mutex::new(Inner::default()) })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Room for one more session: a reservation, plus the least recently used idle session to
    /// close first when the cap is reached (it is forgotten here at once). `None` when every
    /// session has something in flight.
    pub(super) fn reserve(self: &Arc<Self>) -> Option<(Reservation, Option<String>)> {
        let mut inner = self.lock();
        let evict = if inner.live.len().saturating_add(inner.pending) >= self.max {
            let victim = inner.live.iter().filter(|(_, s)| s.open == 0).min_by_key(|(_, s)| s.last_used).map(|(id, _)| id.clone())?;
            inner.live.remove(&victim);
            Some(victim)
        } else {
            None
        };
        inner.pending = inner.pending.saturating_add(1);
        Some((Reservation { sessions: self.clone() }, evict))
    }

    /// The reserved session exists now, as `id`; its first response (the `initialize` result)
    /// is in flight until the returned guard drops.
    pub(super) fn created(self: &Arc<Self>, reservation: Reservation, id: &str) -> Flight {
        drop(reservation);
        let session = Session { last_used: Instant::now(), open: 1, requests: HashSet::new() };
        self.lock().live.insert(id.to_string(), session);
        Flight { sessions: self.clone(), id: id.to_string(), request: None }
    }

    /// A request on session `id` begins: it counts as in flight until the returned guard drops
    /// (when its response ends). `request` is its JSON-RPC id, for a request; one already in
    /// flight in the session is a [`Duplicate`]. `None` for a session this server does not know
    /// (rmcp answers 404).
    pub(super) fn begin(self: &Arc<Self>, id: &str, request: Option<&Value>) -> Result<Option<Flight>, Duplicate> {
        let key = request.map(|r| self.ids.hash_one(r.to_string()));
        let mut inner = self.lock();
        let Some(s) = inner.live.get_mut(id) else { return Ok(None) };
        s.last_used = Instant::now();
        if let Some(k) = key
            && !s.requests.insert(k)
        {
            return Err(Duplicate);
        }
        s.open = s.open.saturating_add(1);
        Ok(Some(Flight { sessions: self.clone(), id: id.to_string(), request: key }))
    }

    /// Session `id` ended (DELETE).
    pub(super) fn ended(&self, id: &str) {
        self.lock().live.remove(id);
    }

    /// Forget and return the sessions with nothing in flight that were not used for `idle`.
    pub(super) fn take_idle(&self, idle: Duration) -> Vec<String> {
        let now = Instant::now();
        let mut inner = self.lock();
        let ids: Vec<String> = inner.live.iter().filter(|(_, s)| s.open == 0 && now.duration_since(s.last_used) >= idle).map(|(id, _)| id.clone()).collect();
        for id in &ids {
            inner.live.remove(id);
        }
        ids
    }

    /// Every session's id.
    pub(super) fn ids(&self) -> Vec<String> {
        self.lock().live.keys().cloned().collect()
    }
}

/// A place for a session being created; dropping it unused gives it back.
pub(super) struct Reservation {
    sessions: Arc<Sessions>,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut inner = self.sessions.lock();
        inner.pending = inner.pending.saturating_sub(1);
    }
}

/// One response of a session in flight; dropping it ends it, and counts as the session's last use.
pub(super) struct Flight {
    sessions: Arc<Sessions>,
    id: String,
    request: Option<u64>,
}

impl Drop for Flight {
    fn drop(&mut self) {
        let mut inner = self.sessions.lock();
        if let Some(s) = inner.live.get_mut(&self.id) {
            s.open = s.open.saturating_sub(1);
            if let Some(k) = self.request {
                s.requests.remove(&k);
            }
            s.last_used = Instant::now();
        }
    }
}

/// `response`, with `flight` held until its body has been sent or dropped (the client went away).
pub(super) fn hold(response: Response<HttpBody>, flight: Flight) -> Response<HttpBody> {
    response.map(|body| BoxBody::new(Held { body, _flight: flight }))
}

struct Held {
    body: HttpBody,
    _flight: Flight,
}

impl Body for Held {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        Pin::new(&mut self.get_mut().body).poll_frame(cx)
    }
    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }
    fn size_hint(&self) -> SizeHint {
        self.body.size_hint()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sessions_in_use_are_kept_and_idle_ones_make_room() {
        let s = Sessions::new(2);
        let (r, evict) = s.reserve().unwrap();
        assert!(evict.is_none());
        drop(s.created(r, "a"));
        let (r, _) = s.reserve().unwrap();
        let b = s.created(r, "b"); // b's first response still streams
        std::thread::sleep(Duration::from_millis(5));
        // full: the idle one makes room, the busy one stays
        let (r, evict) = s.reserve().unwrap();
        assert_eq!(evict.as_deref(), Some("a"));
        drop(s.created(r, "c"));
        let c = s.begin("c", None).unwrap().unwrap();
        assert!(s.reserve().is_none(), "every session has something in flight");
        drop(c);
        let (r, evict) = s.reserve().unwrap();
        assert_eq!(evict.as_deref(), Some("c"));
        drop(r);
        assert_eq!(s.take_idle(Duration::ZERO), Vec::<String>::new(), "b is busy, c is gone");
        drop(b);
        assert_eq!(s.take_idle(Duration::from_secs(60)), Vec::<String>::new(), "b was just used");
        assert_eq!(s.take_idle(Duration::ZERO), ["b"]);
        assert!(s.ids().is_empty());
    }

    #[test]
    fn a_request_id_is_in_flight_once_per_session() {
        let s = Sessions::new(4);
        for id in ["a", "b"] {
            let (r, _) = s.reserve().unwrap();
            drop(s.created(r, id));
        }
        let first = s.begin("a", Some(&json!(7))).unwrap().unwrap();
        assert!(s.begin("a", Some(&json!(7))).is_err());
        assert!(s.begin("a", Some(&json!("7"))).unwrap().is_some(), "a string id is another id");
        assert!(s.begin("b", Some(&json!(7))).unwrap().is_some(), "another session");
        assert!(s.begin("a", None).unwrap().is_some(), "notifications have no id");
        assert!(s.begin("unknown", Some(&json!(7))).unwrap().is_none());
        drop(first);
        assert!(s.begin("a", Some(&json!(7))).unwrap().is_some(), "free again once answered");
    }
}
