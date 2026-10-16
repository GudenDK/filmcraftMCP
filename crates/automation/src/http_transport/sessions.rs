//! Sessions of clients before MCP 2026-07-28 (`Mcp-Session-Id`): every one rmcp creates (rmcp's
//! session store reports them, `tracked.rs`), when each was last used, how many of its responses are
//! still streaming, and which of its calls are still running. rmcp's own idle timeout counts session
//! events only, so a long call that sends nothing (an export without a `progressToken`) would be
//! cut off by it, its output deleted; it is turned off, and the reaper here closes only sessions
//! with nothing in flight. A call is in flight from the moment rmcp hands it to the handler until
//! the handler answers it (or it is cancelled), whether or not its client is still connected: per
//! the MCP spec a dropped connection is no cancellation. At the cap, the least recently used idle
//! session makes room for a new one; only when every session has something in flight is an
//! `initialize` refused.

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
use rmcp::model::RequestId;
use serde_json::Value;

use super::HttpBody;

/// The sessions this server has, and their traffic.
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
    /// Responses still streaming (the `initialize` result, call results, the GET stream).
    open: usize,
    /// Hashes of the JSON-RPC ids of the requests whose response is still streaming.
    requests: HashSet<u64>,
    /// Hashes of the ids of the calls the handler is running (rmcp's view: the client may be gone).
    calls: HashSet<u64>,
}

impl Session {
    /// Nothing in flight: no response streaming, no call running.
    fn idle(&self) -> bool {
        self.open == 0 && self.calls.is_empty()
    }
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

    /// A request id as kept: the hash of its JSON text (`7`, `"a"`).
    fn key(&self, id: &Value) -> u64 {
        self.ids.hash_one(id.to_string())
    }

    /// Room for one more session: a reservation, plus the least recently used idle session to
    /// close first when the cap is reached (it is forgotten here at once). `None` when every
    /// session has something in flight.
    pub(super) fn reserve(self: &Arc<Self>) -> Option<(Reservation, Option<String>)> {
        let mut inner = self.lock();
        let evict = if inner.live.len().saturating_add(inner.pending) >= self.max {
            let victim = inner.live.iter().filter(|(_, s)| s.idle()).min_by_key(|(_, s)| s.last_used).map(|(id, _)| id.clone())?;
            inner.live.remove(&victim);
            Some(victim)
        } else {
            None
        };
        inner.pending = inner.pending.saturating_add(1);
        Some((Reservation { sessions: self.clone(), spent: false }, evict))
    }

    /// rmcp created session `id`, in the place `reservation` kept for it (if any); its first
    /// response (the `initialize` result) is in flight until the returned guard drops.
    pub(super) fn created(self: &Arc<Self>, reservation: Option<Reservation>, id: &str) -> Flight {
        let mut inner = self.lock();
        if let Some(mut r) = reservation {
            // the place becomes the session, in one step: never counted twice or not at all
            r.spent = true;
            inner.pending = inner.pending.saturating_sub(1);
        }
        let session = Session { last_used: Instant::now(), open: 1, requests: HashSet::new(), calls: HashSet::new() };
        inner.live.insert(id.to_string(), session);
        Flight { sessions: self.clone(), id: id.to_string(), request: None }
    }

    /// A request on session `id` begins: it counts as in flight until the returned guard drops
    /// (when its response ends). `request` is its JSON-RPC id, for a request; one already in
    /// flight in the session (its response streaming, or its call running) is a [`Duplicate`].
    /// `None` for a session this server does not know (rmcp answers 404).
    pub(super) fn begin(self: &Arc<Self>, id: &str, request: Option<&Value>) -> Result<Option<Flight>, Duplicate> {
        let key = request.map(|r| self.key(r));
        let mut inner = self.lock();
        let Some(s) = inner.live.get_mut(id) else { return Ok(None) };
        s.last_used = Instant::now();
        if let Some(k) = key
            && (s.calls.contains(&k) || !s.requests.insert(k))
        {
            return Err(Duplicate);
        }
        s.open = s.open.saturating_add(1);
        Ok(Some(Flight { sessions: self.clone(), id: id.to_string(), request: key }))
    }

    /// The handler of session `id` took call `request`: it runs until [`Self::call_ended`].
    pub(super) fn call_began(&self, id: &str, request: &RequestId) {
        let key = self.key(&request.clone().into_json_value());
        if let Some(s) = self.lock().live.get_mut(id) {
            s.calls.insert(key);
            s.last_used = Instant::now();
        }
    }

    /// Call `request` of session `id` was answered, or cancelled (then it gets no answer).
    pub(super) fn call_ended(&self, id: &str, request: &RequestId) {
        let key = self.key(&request.clone().into_json_value());
        if let Some(s) = self.lock().live.get_mut(id)
            && s.calls.remove(&key)
        {
            s.last_used = Instant::now();
        }
    }

    /// Session `id` ended (closed by anyone: `DELETE`, the reaper, rmcp).
    pub(super) fn ended(&self, id: &str) {
        self.lock().live.remove(id);
    }

    /// Forget and return the sessions with nothing in flight that were not used for `idle`.
    pub(super) fn take_idle(&self, idle: Duration) -> Vec<String> {
        let now = Instant::now();
        let mut inner = self.lock();
        let ids: Vec<String> = inner.live.iter().filter(|(_, s)| s.idle() && now.duration_since(s.last_used) >= idle).map(|(id, _)| id.clone()).collect();
        for id in &ids {
            inner.live.remove(id);
        }
        ids
    }

    /// Every session's id.
    pub(super) fn ids(&self) -> Vec<String> {
        self.lock().live.keys().cloned().collect()
    }

    /// How many sessions there are.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.lock().live.len()
    }
}

/// A place for a session being created; dropping it unused gives it back.
pub(super) struct Reservation {
    sessions: Arc<Sessions>,
    /// The session was created in it ([`Sessions::created`] took it back already).
    spent: bool,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if !self.spent {
            let mut inner = self.sessions.lock();
            inner.pending = inner.pending.saturating_sub(1);
        }
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
        drop(s.created(Some(r), "a"));
        let (r, _) = s.reserve().unwrap();
        let b = s.created(Some(r), "b"); // b's first response still streams
        std::thread::sleep(Duration::from_millis(5));
        // full: the idle one makes room, the busy one stays
        let (r, evict) = s.reserve().unwrap();
        assert_eq!(evict.as_deref(), Some("a"));
        drop(s.created(Some(r), "c"));
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
            drop(s.created(Some(r), id));
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

    /// A call the handler is still running keeps its session in flight after its client went
    /// away (its response no longer streams): it is not closed for being idle, nor to make room,
    /// and its id is not reused. Once answered, or cancelled (no answer comes then), it no longer
    /// counts.
    #[test]
    fn a_running_call_keeps_its_session_after_the_client_left() {
        let s = Sessions::new(2);
        for id in ["a", "b"] {
            let (r, _) = s.reserve().unwrap();
            drop(s.created(Some(r), id));
        }
        let export = RequestId::Number(9);
        let streaming = s.begin("a", Some(&json!(9))).unwrap().unwrap();
        s.call_began("a", &export);
        drop(streaming); // the client's connection dropped; the call goes on
        assert!(s.begin("a", Some(&json!(9))).is_err(), "its id is still in flight");
        std::thread::sleep(Duration::from_millis(5));
        // full: b (idle) makes room, a (running) does not
        let (r, evict) = s.reserve().unwrap();
        assert_eq!(evict.as_deref(), Some("b"));
        drop(s.created(Some(r), "c"));
        let c = s.begin("c", None).unwrap().unwrap();
        assert!(s.reserve().is_none(), "a's call runs, c's stream is open");
        assert!(s.take_idle(Duration::ZERO).is_empty(), "nothing idle");
        s.call_ended("a", &export);
        assert_eq!(s.take_idle(Duration::ZERO), ["a"]);
        let cancelled = RequestId::String("x".into());
        s.call_began("c", &cancelled);
        drop(c);
        assert!(s.take_idle(Duration::ZERO).is_empty());
        s.call_ended("c", &cancelled);
        assert_eq!(s.take_idle(Duration::ZERO), ["c"]);
        assert_eq!(s.len(), 0);
    }
}
