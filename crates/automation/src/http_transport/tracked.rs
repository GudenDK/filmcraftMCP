//! rmcp's session store ([`LocalSessionManager`]), with every session it creates or closes and
//! every call its sessions' handlers run reported to [`Sessions`], where they happen: an HTTP
//! request future can be dropped at any await (its client went away), so bookkeeping done there,
//! after rmcp returns, could miss a session rmcp already created, which nothing would then ever
//! close or count. Calls are tracked on the session's transport, between rmcp's worker and the
//! handler, so a call counts as running until the handler answers it, whether or not its client is
//! still connected.

use std::sync::{Arc, Mutex, PoisonError};

use futures_util::Stream;
use rmcp::RoleServer;
use rmcp::model::{ClientJsonRpcMessage, ClientNotification, JsonRpcMessage, ServerJsonRpcMessage};
use rmcp::transport::Transport;
use rmcp::transport::WorkerTransport;
use rmcp::transport::streamable_http_server::SessionManager;
use rmcp::transport::streamable_http_server::session::local::{LocalSessionManager, LocalSessionManagerError, LocalSessionWorker};
use rmcp::transport::streamable_http_server::session::{ServerSseMessage, SessionId};

use super::sessions::{Flight, Reservation, Sessions};

tokio::task_local! {
    /// The `initialize` the gate is handing to rmcp on this task: the place it reserved, which
    /// [`TrackedSessions::create_session`] turns into the new session's first flight for the gate
    /// to hold while the result streams. Dropped with the request (the client went away), the
    /// flight ends and the session is merely idle: still counted, and closed in time.
    pub(super) static INITIALIZING: Mutex<Initializing>;
}

/// See [`INITIALIZING`].
pub(super) enum Initializing {
    Reserved(Reservation),
    Created(Flight),
    Done,
}

impl Initializing {
    /// The new session's first flight, once rmcp created it.
    pub(super) fn take_flight(&mut self) -> Option<Flight> {
        match std::mem::replace(self, Initializing::Done) {
            Initializing::Created(f) => Some(f),
            other => {
                *self = other;
                None
            }
        }
    }
}

/// [`LocalSessionManager`], reporting to [`Sessions`].
pub(super) struct TrackedSessions {
    pub(super) inner: LocalSessionManager,
    pub(super) sessions: Arc<Sessions>,
}

impl SessionManager for TrackedSessions {
    type Error = LocalSessionManagerError;
    type Transport = TrackedTransport;

    async fn create_session(&self) -> Result<(SessionId, Self::Transport), Self::Error> {
        let (id, transport) = self.inner.create_session().await?;
        // in the same poll as rmcp's insert: no await in between
        let made = INITIALIZING.try_with(|slot| {
            let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
            let reserved = match std::mem::replace(&mut *slot, Initializing::Done) {
                Initializing::Reserved(r) => Some(r),
                other => {
                    *slot = other;
                    None
                }
            };
            *slot = Initializing::Created(self.sessions.created(reserved, &id));
        });
        if made.is_err() {
            // not an `initialize` the gate handed over (none is expected): tracked all the same
            drop(self.sessions.created(None, &id));
        }
        let transport = TrackedTransport { inner: transport, sessions: self.sessions.clone(), id: id.to_string() };
        Ok((id, transport))
    }

    async fn initialize_session(&self, id: &SessionId, message: ClientJsonRpcMessage) -> Result<ServerJsonRpcMessage, Self::Error> {
        self.inner.initialize_session(id, message).await
    }

    async fn has_session(&self, id: &SessionId) -> Result<bool, Self::Error> {
        self.inner.has_session(id).await
    }

    async fn close_session(&self, id: &SessionId) -> Result<(), Self::Error> {
        self.sessions.ended(id);
        self.inner.close_session(id).await
    }

    async fn create_stream(
        &self,
        id: &SessionId,
        message: ClientJsonRpcMessage,
    ) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.create_stream(id, message).await
    }

    async fn accept_message(&self, id: &SessionId, message: ClientJsonRpcMessage) -> Result<(), Self::Error> {
        self.inner.accept_message(id, message).await
    }

    async fn create_standalone_stream(&self, id: &SessionId) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.create_standalone_stream(id).await
    }

    async fn resume(&self, id: &SessionId, last_event_id: String) -> Result<impl Stream<Item = ServerSseMessage> + Send + Sync + 'static, Self::Error> {
        self.inner.resume(id, last_event_id).await
    }

    // restore_session: the default (not supported); no session store is configured
}

/// A session's transport between rmcp's worker and the handler: a request the handler receives is
/// a call running until the handler sends its answer, or the client cancels it (rmcp sends no
/// answer then).
pub(super) struct TrackedTransport {
    inner: WorkerTransport<LocalSessionWorker>,
    sessions: Arc<Sessions>,
    id: String,
}

impl Transport<RoleServer> for TrackedTransport {
    type Error = <WorkerTransport<LocalSessionWorker> as Transport<RoleServer>>::Error;

    fn send(&mut self, item: ServerJsonRpcMessage) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let answered = match &item {
            JsonRpcMessage::Response(r) => Some(&r.id),
            JsonRpcMessage::Error(e) => e.id.as_ref(),
            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
        };
        if let Some(request) = answered {
            self.sessions.call_ended(&self.id, request);
        }
        self.inner.send(item)
    }

    async fn receive(&mut self) -> Option<ClientJsonRpcMessage> {
        let m = self.inner.receive().await;
        match &m {
            Some(JsonRpcMessage::Request(r)) => self.sessions.call_began(&self.id, &r.id),
            Some(JsonRpcMessage::Notification(n)) => {
                if let ClientNotification::CancelledNotification(c) = &n.notification
                    && let Some(request) = &c.params.request_id
                {
                    self.sessions.call_ended(&self.id, request);
                }
            }
            _ => {}
        }
        m
    }

    async fn close(&mut self) -> Result<(), Self::Error> {
        self.inner.close().await
    }
}
