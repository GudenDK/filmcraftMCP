//! MCP over Streamable HTTP: rmcp's `StreamableHttpService` (protocol, sessions, SSE) on a hyper 1
//! HTTP/1.1 listener, since rmcp ships no listener. One endpoint, `http://<addr>/mcp`, serves any
//! number of clients, and they all drive the same backend (one engine session, or one app).
//!
//! Secure by default (MCP spec, transports § Security Warning):
//! * loopback addresses only, unless a bearer token of 32+ characters is set;
//! * `Host` must name this server (`127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`, the
//!   bound address, plus any `allowed_hosts`): a DNS-rebinding page gets 403;
//! * a request with an `Origin` header (a browser) gets 403 unless that origin is listed in
//!   `allowed_origins` (which then also get CORS headers); MCP clients send none;
//! * with a token, every request needs `Authorization: Bearer <token>` (constant-time compare),
//!   checked next, else 401; a peer that sends 10 wrong tokens in a minute gets 429 for its
//!   wrong ones for a minute (and for the right one too if it is shorter than 32 characters);
//! * an `allowed_hosts` name that is not this machine (a LAN name, a tunnel, a proxy) means clients
//!   from the network, so it needs a token of 32+ characters too ([`check_host_exposure`]).
//!
//! The gate (`gate.rs`) runs these checks before reading a body, maps a non-JSON body to a
//! JSON-RPC parse error (`-32700`, as `serve_io` does on stdio), and keeps rmcp's per-tool-name
//! schema cache bounded by answering unknown tool names itself. Resources are bounded per peer and
//! in all (`limits.rs`: connections, slow or idle clients, bodies). Every session rmcp creates is
//! tracked from its creation, and every call its handler runs until answered (`tracked.rs`), so
//! sessions are counted against a cap and closed when idle, never with a call in flight, even one
//! whose client disconnected (`sessions.rs`). Refused requests are logged (stderr, or
//! [`HttpOptions::log`]), rate limited per peer.

mod gate;
mod limits;
mod sessions;
mod tracked;

use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::FutureExt as _;
use http::Request;
use http_body_util::combinators::BoxBody;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::transport::streamable_http_server::SessionManager as _;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio_util::sync::CancellationToken;
use tracked::TrackedSessions;

pub use limits::LogSink;

use crate::{AutomationError, FilmcraftMcp, Stop};

/// The MCP endpoint path.
pub const MCP_PATH: &str = "/mcp";
/// Environment variable the CLI reads the bearer token from.
pub const TOKEN_ENV: &str = "FILMCRAFT_MCP_TOKEN";
/// The shortest token a server reachable from the network accepts (`openssl rand -hex 32` makes 64).
pub const MIN_REMOTE_TOKEN_LEN: usize = 32;
/// Largest request body read (the control channel's line limit; rmcp's default too).
const MAX_BODY: usize = 4 << 20;
/// At most one log line per peer and reason this often.
const LOG_EVERY: Duration = Duration::from_secs(10);
/// Connections refused with a 503 at once; past this, further ones are closed unanswered.
const MAX_REFUSING: usize = 32;

/// The server's limits and timeouts (tests shorten them).
#[derive(Clone, Debug)]
pub(crate) struct Limits {
    /// Connections served at once. At this cap (or `connections_per_peer`), the oldest connection
    /// that has not made a request past the token, `Host` and `Origin` checks is closed for a new
    /// one; when every one has, the new one gets a 503 and is closed.
    pub(crate) connections: u32,
    /// Connections from one peer (an IPv4 address or an IPv6 /64; every loopback address is one,
    /// this machine) at once, so that one address cannot hold them all.
    pub(crate) connections_per_peer: usize,
    /// How long a client may take to send a request's head; also how long a keep-alive
    /// connection may sit idle between requests.
    pub(crate) header_timeout: Duration,
    /// How long a client may take to send a request's body (408 after).
    pub(crate) body_timeout: Duration,
    /// With a token: this many wrong tokens from one peer within `auth_window` get its wrong
    /// tokens 429 instead of 401 for `auth_block`, and its right one too when that is shorter than
    /// [`MIN_REMOTE_TOKEN_LEN`].
    pub(crate) auth_failures: u32,
    pub(crate) auth_window: Duration,
    pub(crate) auth_block: Duration,
    /// Sessions (clients before 2026-07-28) at once. At the cap the least recently used idle one
    /// is closed for a new one; when every one has something in flight, `initialize` gets 503.
    pub(crate) sessions: usize,
    /// How often idle sessions are looked for.
    pub(crate) reap_every: Duration,
    /// How long shutdown waits for cancelled exports to delete their partial files and for
    /// connections to close, after a graceful and a hurried [`Stop`].
    pub(crate) grace: Duration,
    pub(crate) hurried_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connections: 256,
            connections_per_peer: 32,
            header_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            auth_failures: 10,
            auth_window: Duration::from_secs(60),
            auth_block: Duration::from_secs(60),
            sessions: 256,
            reap_every: Duration::from_secs(30),
            grace: Stop::Graceful.grace(),
            hurried_grace: Stop::Hurried.grace(),
        }
    }
}

/// Options for [`FilmcraftMcp::serve_http`].
#[derive(Clone)]
pub struct HttpOptions {
    /// Require `Authorization: Bearer <token>` on every request. Mandatory for a non-loopback
    /// address. Never logged.
    pub token: Option<String>,
    /// More `Host` values to accept besides this server's own, as `name` (any port) or
    /// `name:port`: a LAN name, `host.docker.internal:<port>`, a tunnel or reverse proxy. See
    /// [`check_allowed_host`].
    pub allowed_hosts: Vec<String>,
    /// Browser origins allowed to call, as `scheme://host:port` (e.g. `http://localhost:6274`), or
    /// `null`; see [`check_allowed_origin`]. They get CORS headers; a request with any other
    /// `Origin` header gets 403.
    pub allowed_origins: Vec<String>,
    /// Serve pre-2026-07-28 clients with `Mcp-Session-Id` sessions. 2026-07-28 requests are
    /// stateless either way.
    pub legacy_session_mode: bool,
    /// Answer a single-message reply as `application/json` instead of an SSE stream.
    pub json_response: bool,
    /// A session (clients before 2026-07-28) with nothing in flight and no requests for this long
    /// is closed; its client must `initialize` again. A call in flight keeps its session open
    /// however long it runs. `None`: only to make room when the session cap is reached.
    pub session_idle: Option<Duration>,
    /// Where refused requests and failures are reported, one line each (default: stderr).
    pub log: Option<LogSink>,
}

impl Default for HttpOptions {
    fn default() -> Self {
        Self {
            token: None,
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            legacy_session_mode: true,
            json_response: false,
            session_idle: Some(Duration::from_secs(600)),
            log: None,
        }
    }
}

/// Debug output never shows the token.
impl std::fmt::Debug for HttpOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpOptions")
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .field("allowed_hosts", &self.allowed_hosts)
            .field("allowed_origins", &self.allowed_origins)
            .field("legacy_session_mode", &self.legacy_session_mode)
            .field("json_response", &self.json_response)
            .field("session_idle", &self.session_idle)
            .field("log", &self.log.as_ref().map_or("stderr", |_| "<sink>"))
            .finish()
    }
}

/// A listen address: `PORT` (127.0.0.1), `localhost:PORT` (127.0.0.1), or `IP:PORT`
/// (`127.0.0.1:9000`, `[::1]:9000`, `0.0.0.0:9000`).
pub fn parse_listen_addr(s: &str) -> Result<SocketAddr, AutomationError> {
    let s = s.trim();
    let bad = || AutomationError::BadRequest(format!("listen address must be PORT, 127.0.0.1:PORT, [::1]:PORT or localhost:PORT, got `{s}`"));
    let localhost = |port: u16| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    if let Ok(port) = s.parse::<u16>() {
        return Ok(localhost(port));
    }
    if let Some(port) = s.strip_prefix("localhost:") {
        return port.parse::<u16>().map(localhost).map_err(|_| bad());
    }
    s.parse::<SocketAddr>().map_err(|_| bad())
}

/// Serving on `addr` without a token is allowed only on a loopback address: anything else can be
/// reached from the network, and every tool writes files or drives the app. There the token must
/// have at least [`MIN_REMOTE_TOKEN_LEN`] characters, too long to guess.
pub fn check_bind(addr: SocketAddr, token: Option<&str>) -> Result<(), AutomationError> {
    if addr.ip().is_loopback() {
        return Ok(());
    }
    match token.map_or(0, str::len) {
        0 => Err(AutomationError::BadRequest(format!(
            "serving MCP on non-loopback {addr} needs a bearer token of {MIN_REMOTE_TOKEN_LEN}+ characters (set {TOKEN_ENV}); use 127.0.0.1 for local clients"
        ))),
        n if n < MIN_REMOTE_TOKEN_LEN => Err(AutomationError::BadRequest(format!(
            "serving MCP on non-loopback {addr} needs a token of at least {MIN_REMOTE_TOKEN_LEN} characters, this one has {n}: make one with `openssl rand -hex 32`"
        ))),
        _ => Ok(()),
    }
}

/// An `allowed_hosts` entry that names something other than this machine's loopback (a LAN name,
/// a tunnel, a reverse proxy) means requests from the network, relayed to a loopback address, so
/// it needs a token of at least [`MIN_REMOTE_TOKEN_LEN`] characters, as a non-loopback address
/// does. `host.docker.internal` is the exception: Docker Desktop forwards it to the loopback, so
/// only containers on this machine reach the server through it (the caller should warn).
pub fn check_host_exposure(allowed_hosts: &[String], token: Option<&str>) -> Result<(), AutomationError> {
    let Some(remote) = allowed_hosts.iter().map(|h| h.trim()).find(|h| !is_local_host(h)) else { return Ok(()) };
    match token.map_or(0, str::len) {
        0 => Err(AutomationError::BadRequest(format!(
            "allowed host `{remote}` is not this machine, so clients from the network reach the server through it (a tunnel or proxy): that needs a bearer token of {MIN_REMOTE_TOKEN_LEN}+ characters (set {TOKEN_ENV})"
        ))),
        n if n < MIN_REMOTE_TOKEN_LEN => Err(AutomationError::BadRequest(format!(
            "allowed host `{remote}` is not this machine, so clients from the network reach the server through it: that needs a token of at least {MIN_REMOTE_TOKEN_LEN} characters, this one has {n}: make one with `openssl rand -hex 32`"
        ))),
        _ => Ok(()),
    }
}

/// Whether an `allowed_hosts` entry names this machine: `localhost` (or a name under it),
/// 127.0.0.0/8, `[::1]`, or `host.docker.internal` (see [`check_host_exposure`]).
pub fn is_local_host(entry: &str) -> bool {
    let Some((host, _)) = gate::parse_allowed_host(entry) else { return true };
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "host.docker.internal"
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// A usable bearer token: visible ASCII without spaces, as `Authorization: Bearer` carries it.
pub fn check_token(token: &str) -> Result<(), AutomationError> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AutomationError::BadRequest("the MCP token must be non-empty visible ASCII without spaces".into()));
    }
    Ok(())
}

/// An `allowed_origins` entry: `scheme://host:port` with an `http` or `https` scheme and the port
/// spelled out (`http://localhost:6274`), or `null`. Returns it normalized. A `*` anywhere, a
/// missing port (which rmcp would match against every port), a port out of range, a fragment, a
/// trailing dot and anything else are refused.
/// `null` is accepted, but every web page can send it (from a sandboxed frame or a `file:` page),
/// so the caller should warn about it.
pub fn check_allowed_origin(origin: &str) -> Result<String, AutomationError> {
    let o = origin.trim();
    let bad = |why: String| Err(AutomationError::BadRequest(format!("allowed origin `{o}`: {why}")));
    if o.eq_ignore_ascii_case("null") {
        return Ok("null".into());
    }
    if o.contains('*') {
        return bad("wildcards would let any web page (or any port) call, and browsers never send one; list each origin as scheme://host:port".into());
    }
    if o.contains('#') {
        return bad("an origin has no fragment (#...)".into());
    }
    let Ok(uri) = o.parse::<http::Uri>() else { return bad("not an origin; write it as scheme://host:port, e.g. http://localhost:6274".into()) };
    let scheme = match uri.scheme_str() {
        Some(s) if s.eq_ignore_ascii_case("http") || s.eq_ignore_ascii_case("https") => s.to_ascii_lowercase(),
        _ => return bad("an origin starts with http:// or https://".into()),
    };
    let Some(authority) = uri.authority() else { return bad("no host; write it as scheme://host:port".into()) };
    if authority.as_str().contains('@') {
        return bad("an origin has no user name".into());
    }
    let host = authority.host();
    if !host.starts_with('[') && host.split('.').any(str::is_empty) {
        return bad("the host has an empty label (a trailing dot?): browsers send the name without one".into());
    }
    // what follows the host: nothing, or `:port`
    let after_host = authority.as_str().get(host.len()..).unwrap_or_default();
    match authority.port_u16() {
        Some(0) => return bad("the port must be 1-65535".into()),
        Some(_) => {}
        None if !after_host.is_empty() => return bad("the port must be 1-65535".into()),
        None => {
            let port = if scheme == "https" { 443 } else { 80 };
            return bad(format!("add the port (every other port would match too), e.g. {scheme}://{host}:{port}"));
        }
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        return bad("an origin has no path or query".into());
    }
    gate::normalize_origin(o).map_or_else(|| bad("not an origin".into()), Ok)
}

/// An `allowed_hosts` entry: `name` (any port) or `name:port`, as a `Host` header carries it: a
/// DNS name or IPv4 address, or an IPv6 address in brackets. Returns it trimmed.
pub fn check_allowed_host(host: &str) -> Result<String, AutomationError> {
    let h = host.trim();
    let bad = || Err(AutomationError::BadRequest(format!("allowed host `{h}`: write it as name or name:port, as clients put it in the URL")));
    let (name_ok, port) = match h.strip_prefix('[') {
        Some(rest) => {
            let Some((ip, after)) = rest.split_once(']') else { return bad() };
            let port = match after {
                "" => None,
                a => match a.strip_prefix(':') {
                    Some(p) => Some(p),
                    None => return bad(),
                },
            };
            (ip.parse::<std::net::Ipv6Addr>().is_ok(), port)
        }
        None => {
            let (name, port) = match h.split_once(':') {
                Some((n, p)) => (n, Some(p)),
                None => (h, None),
            };
            (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')), port)
        }
    };
    let port_ok = port.is_none_or(|p| p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok());
    if name_ok && port_ok { Ok(h.to_string()) } else { bad() }
}

/// Whether one of the `allowed_hosts` entries (`name` for any port, or `name:port`) accepts the
/// `Host` of a client that connects to `addr`.
pub fn allows_host(allowed_hosts: &[String], addr: SocketAddr) -> bool {
    let ip = addr.ip().to_string().to_ascii_lowercase();
    allowed_hosts.iter().filter_map(|h| gate::parse_allowed_host(h)).any(|(host, port)| host == ip && port.is_none_or(|p| p == addr.port()))
}

/// The `Host` values this server answers to on `bound`, plus `extra`: its loopback names on its
/// port, and its own address unless it listens on every address (`0.0.0.0`, `[::]`).
pub fn accepted_hosts(bound: SocketAddr, extra: &[String]) -> Vec<String> {
    let p = bound.port();
    let mut hosts = vec![format!("127.0.0.1:{p}"), format!("localhost:{p}"), format!("[::1]:{p}")];
    if p == 80 {
        // clients leave the default port out of `Host`
        hosts.extend(["127.0.0.1", "localhost", "[::1]"].map(String::from));
    }
    if !bound.ip().is_unspecified() {
        hosts.push(bound.to_string());
    }
    hosts.extend(extra.iter().map(|h| h.trim()).filter(|h| !h.is_empty()).map(String::from));
    let mut seen = std::collections::HashSet::new();
    hosts.retain(|h| seen.insert(h.to_ascii_lowercase()));
    hosts
}

type HttpBody = BoxBody<Bytes, Infallible>;
type Mcp = StreamableHttpService<FilmcraftMcp, TrackedSessions>;

/// What a server counted (tests read it).
#[derive(Default)]
pub(crate) struct Stats {
    /// Handlers rmcp asked for: one per session or stateless request, and one per tool name it
    /// looks up a schema for.
    pub(crate) services: AtomicUsize,
    /// The server's gate, for tests that hand it requests directly.
    #[cfg(test)]
    gate: std::sync::OnceLock<std::sync::Weak<gate::Gate>>,
}

impl FilmcraftMcp {
    /// Serve MCP over Streamable HTTP at `http://<addr>/mcp` until `shutdown` resolves: then the
    /// port closes, calls in flight are cancelled, and exports get [`Stop::grace`] to delete
    /// their partial files.
    pub async fn serve_http<S: Into<Stop> + Send + 'static>(
        self,
        addr: SocketAddr,
        opts: HttpOptions,
        shutdown: impl Future<Output = S> + Send + 'static,
    ) -> Result<(), AutomationError> {
        check_bind(addr, opts.token.as_deref())?;
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| AutomationError::Other(format!("cannot listen on {addr}: {e}")))?;
        self.serve_http_listener(listener, opts, shutdown).await
    }

    /// [`Self::serve_http`] on a bound listener (the caller can print the bound port first, and
    /// tests bind `127.0.0.1:0`).
    pub async fn serve_http_listener<S: Into<Stop> + Send + 'static>(
        self,
        listener: tokio::net::TcpListener,
        opts: HttpOptions,
        shutdown: impl Future<Output = S> + Send + 'static,
    ) -> Result<(), AutomationError> {
        self.serve_http_listeners(vec![listener], opts, shutdown).await
    }

    /// [`Self::serve_http_listener`] on several listeners at once, one server for all: e.g.
    /// `127.0.0.1:<port>` and `[::1]:<port>`, so that a client resolving `localhost` to either
    /// reaches this server. Each one's address is an accepted `Host`.
    pub async fn serve_http_listeners<S: Into<Stop> + Send + 'static>(
        self,
        listeners: Vec<tokio::net::TcpListener>,
        opts: HttpOptions,
        shutdown: impl Future<Output = S> + Send + 'static,
    ) -> Result<(), AutomationError> {
        self.serve_with(listeners, opts, Limits::default(), Arc::default(), shutdown).await
    }

    /// [`Self::serve_http_listeners`] with these limits, counting into `stats`.
    pub(crate) async fn serve_with<S: Into<Stop> + Send + 'static>(
        self,
        listeners: Vec<tokio::net::TcpListener>,
        opts: HttpOptions,
        limits: Limits,
        stats: Arc<Stats>,
        shutdown: impl Future<Output = S> + Send + 'static,
    ) -> Result<(), AutomationError> {
        let bound = listeners.iter().map(|l| l.local_addr()).collect::<Result<Vec<_>, _>>().map_err(|e| AutomationError::Other(format!("listener: {e}")))?;
        if bound.is_empty() {
            return Err(AutomationError::BadRequest("no address to listen on".into()));
        }
        let token = opts.token.filter(|t| !t.is_empty());
        if let Some(t) = &token {
            check_token(t)?;
        }
        for addr in &bound {
            check_bind(*addr, token.as_deref())?;
        }
        let extra_hosts = opts.allowed_hosts.iter().map(|h| check_allowed_host(h)).collect::<Result<Vec<_>, _>>()?;
        check_host_exposure(&extra_hosts, token.as_deref())?;
        let token = token.map(|t| t.into_bytes().into_boxed_slice());
        let origins = opts.allowed_origins.iter().map(|o| check_allowed_origin(o)).collect::<Result<Vec<_>, _>>()?;
        let mut hosts: Vec<String> = bound.iter().flat_map(|addr| accepted_hosts(*addr, &extra_hosts)).collect();
        let mut seen = std::collections::HashSet::new();
        hosts.retain(|h| seen.insert(h.to_ascii_lowercase()));

        let ct = CancellationToken::new();
        let (stop, how) = (ct.clone(), Arc::new(std::sync::Mutex::new(Stop::Graceful)));
        let stopped_how = how.clone();
        tokio::spawn(async move {
            let s: Stop = shutdown.await.into();
            *stopped_how.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = s;
            stop.cancel();
        });

        let tracked = sessions::Sessions::new(limits.sessions);
        let rmcp_sessions = Arc::new(TrackedSessions { inner: Arc::new(session_manager()), sessions: tracked.clone() });
        // rmcp repeats the gate's Host and Origin checks with the same lists
        let config = StreamableHttpServerConfig::default()
            .with_allowed_hosts(hosts.clone())
            .with_allowed_origins(origins.clone())
            // validate Origin even when no origin is allowed: then every browser request is refused
            .enforce_origin_validation()
            .with_legacy_session_mode(opts.legacy_session_mode)
            .with_json_response(opts.json_response)
            .with_sse_keep_alive(Some(Duration::from_secs(15)))
            .with_max_request_body_bytes(MAX_BODY)
            .with_cancellation_token(ct.child_token());
        let tools = self.clone();
        let long_calls = self.long_calls.clone();
        #[cfg(test)]
        let stats_for_tests = stats.clone();
        // Called once per session (pre-2026-07-28 clients) or per request (2026-07-28): every
        // clone shares the one backend (`Arc<Backend>`), so all clients see the same project.
        let mcp: Mcp = StreamableHttpService::new(
            move || {
                stats.services.fetch_add(1, Ordering::Relaxed);
                Ok(self.clone())
            },
            rmcp_sessions.clone(),
            config,
        );
        let gate = Arc::new(gate::Gate {
            token,
            hosts: hosts.iter().filter_map(|h| gate::parse_allowed_host(h)).collect(),
            origins,
            tools,
            mcp,
            rmcp_sessions,
            sessions: tracked,
            legacy_session_mode: opts.legacy_session_mode,
            body_timeout: limits.body_timeout,
            auth: limits::AuthFailures::new(limits.auth_failures, limits.auth_window, limits.auth_block),
            log: limits::Log::new(opts.log, LOG_EVERY),
        });
        #[cfg(test)]
        let _ = stats_for_tests.gate.set(Arc::downgrade(&gate));
        let conns = limits::Conns::new(limits.connections, limits.connections_per_peer);
        if let Some(idle) = opts.session_idle {
            tokio::spawn(reap_idle_sessions(gate.clone(), idle, limits.reap_every, ct.clone()));
        }
        accept(&listeners, &gate, &conns, limits.header_timeout, &ct).await;
        // Refuse new connections now rather than leave them hanging until the process exits.
        drop(listeners);
        let grace = match *how.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
            Stop::Graceful => limits.grace,
            Stop::Hurried => limits.hurried_grace,
        };
        wind_down(&gate, &long_calls, &conns, deadline_after(grace)).await;
        Ok(())
    }
}

/// Accept connections on any of `listeners` until `stop`. One within the limits is served by its
/// own task; one past them gets a 503.
async fn accept(listeners: &[tokio::net::TcpListener], gate: &Arc<gate::Gate>, conns: &Arc<limits::Conns>, header_timeout: Duration, stop: &CancellationToken) {
    let refusing = Arc::new(tokio::sync::Semaphore::new(MAX_REFUSING));
    // the next connection on any listener, each asked first in turn so none waits behind another
    let mut turn = 0usize;
    let mut next = || {
        turn = turn.wrapping_add(1);
        let first = turn % listeners.len().max(1);
        std::future::poll_fn(move |cx| {
            for l in listeners.iter().cycle().skip(first).take(listeners.len()) {
                if let std::task::Poll::Ready(r) = l.poll_accept(cx) {
                    return std::task::Poll::Ready(r);
                }
            }
            std::task::Poll::Pending
        })
    };
    loop {
        let (stream, peer) = tokio::select! {
            _ = stop.cancelled() => return,
            accepted = next() => match accepted {
                Ok(accepted) => accepted,
                // a connection reset or aborted before it was accepted: on to the next one at once
                Err(e) if !accept_error_backs_off(&e) => continue,
                Err(e) => {
                    // out of file descriptors and the like: back off instead of spinning
                    let why = "cannot accept connections";
                    gate.log.event(IpAddr::V4(Ipv4Addr::UNSPECIFIED), why, format_args!("{why}: {e}; trying again in {} ms", ACCEPT_BACKOFF.as_millis()));
                    tokio::select! {
                        _ = stop.cancelled() => return,
                        _ = tokio::time::sleep(ACCEPT_BACKOFF) => continue,
                    }
                }
            },
        };
        match conns.open(peer.ip()) {
            Ok(slot) => {
                tokio::spawn(serve_connection(gate.clone(), stream, peer, slot, header_timeout, stop.clone()));
            }
            Err(why) => {
                gate.log.event(peer.ip(), why, format_args!("refused a connection from {peer} with 503: {why}"));
                if let Ok(permit) = refusing.clone().try_acquire_owned() {
                    tokio::spawn(async move {
                        let _permit = permit;
                        limits::refuse_connection(stream).await;
                    });
                }
            }
        }
    }
}

/// Whether an error from `accept` should pause the accept loop for [`ACCEPT_BACKOFF`]: the system is
/// out of something (file descriptors, buffers, memory) or the listener is broken, and trying again
/// at once would only spin. An error about one connection does not: it was reset or aborted before
/// it was accepted (Windows reports `WSAECONNRESET`, BSD and macOS `ECONNABORTED`), or Linux passes
/// on a network error that is pending on it. The next one is then accepted at once, so that
/// clients resetting connections in a loop cannot slow the server down for everyone.
fn accept_error_backs_off(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset, Interrupted, WouldBlock};
    if matches!(e.kind(), ConnectionReset | ConnectionAborted | ConnectionRefused | Interrupted | WouldBlock) {
        return false;
    }
    // accept(2): network errors pending on the new socket (EPROTO, ENOPROTOOPT, EHOSTDOWN, ENONET,
    // EHOSTUNREACH, ENETUNREACH, ENETDOWN), and firewall rules refusing it (EPERM)
    #[cfg(target_os = "linux")]
    if matches!(e.raw_os_error(), Some(71 | 92 | 112 | 64 | 113 | 101 | 100 | 1)) {
        return false;
    }
    true
}

/// How long the accept loop pauses after an error that [`accept_error_backs_off`].
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Serve one connection's requests through the gate until it closes, or the server stops (then it
/// finishes the request in progress, for at most 5 s).
async fn serve_connection(
    gate: Arc<gate::Gate>,
    stream: tokio::net::TcpStream,
    peer: SocketAddr,
    slot: limits::ConnSlot,
    header_timeout: Duration,
    stop: CancellationToken,
) {
    let reporter = gate.clone();
    let trusted = slot.trusted();
    // a panic in a request is answered with 500 (Gate::serve)
    let svc = service_fn(move |req: Request<Incoming>| {
        let (gate, trusted) = (gate.clone(), trusted.clone());
        async move { Ok::<_, Infallible>(gate.serve(peer, req, &trusted).await) }
    });
    // hyper panics on a timeout configured without a timer (hyper common/time.rs). Its header
    // timer also runs while a keep-alive connection waits for its next request.
    let conn = http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(header_timeout).serve_connection(TokioIo::new(stream), svc);
    let served = AssertUnwindSafe(async {
        tokio::pin!(conn);
        tokio::select! {
            _ = conn.as_mut() => {}
            // closed to make room for another connection: it never made a request that passed
            _ = slot.made_room() => {}
            _ = stop.cancelled() => {
                conn.as_mut().graceful_shutdown();
                let _ = tokio::time::timeout(Duration::from_secs(5), conn).await;
            }
        }
    })
    .catch_unwind()
    .await;
    // a panic while streaming a response: the connection is closed, the server goes on
    if served.is_err() {
        reporter.log.event(peer.ip(), "panic", format_args!("internal error (a panic) while streaming a response to {peer}; closed the connection"));
    }
}

/// After the accept loop, with the stop token cancelled: that ended every SSE stream and cancelled
/// the stateless calls. Exports are cancelled first, also those of sessions (their calls would see
/// `context.ct` cancelled only after rmcp's response drain, up to 5 s after their session closed):
/// each stops and deletes its partial output. Session workers outlive the stop token, so close
/// them. Exports get until `deadline` (what one still running then wrote is deleted), then the
/// connections.
async fn wind_down(gate: &gate::Gate, long_calls: &crate::long_job::LongCalls, conns: &limits::Conns, deadline: tokio::time::Instant) {
    long_calls.stop();
    let ids = gate.sessions.ids();
    let close_all = futures_util::future::join_all(ids.iter().map(|id| async move {
        let _ = gate.rmcp_sessions.close_session(&id.as_str().into()).await;
    }));
    let _ = tokio::time::timeout_at(deadline, close_all).await;
    long_calls.drain(deadline).await;
    let _ = tokio::time::timeout_at(deadline, conns.all_closed()).await;
}

/// rmcp's session store with its idle timeout off: it counts session events only, so it would cut
/// off a long call that sends nothing (an export without a `progressToken`) and delete its output.
/// [`reap_idle_sessions`] closes idle sessions instead, never one with something in flight; it
/// sees every session, as [`TrackedSessions`] reports each one rmcp creates.
fn session_manager() -> LocalSessionManager {
    let mut sessions = LocalSessionManager::default();
    sessions.session_config.keep_alive = None;
    sessions
}

/// Every `every`, close the sessions with nothing in flight that were not used for `idle`
/// (clients that went away without `DELETE`), until `stop`.
async fn reap_idle_sessions(gate: Arc<gate::Gate>, idle: Duration, every: Duration, stop: CancellationToken) {
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(every) => {}
        }
        let ids = gate.sessions.take_idle(idle);
        for id in &ids {
            let _ = gate.rmcp_sessions.close_session(&id.as_str().into()).await;
        }
        if !ids.is_empty() {
            gate.log.line(format_args!("closed {} MCP session(s) unused for {} s", ids.len(), idle.as_secs()));
        }
    }
}

/// `d` from now (far in the future if that overflows).
pub(crate) fn deadline_after(d: Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now.checked_add(d).or_else(|| now.checked_add(Duration::from_secs(86_400 * 365))).unwrap_or(now)
}

#[cfg(test)]
mod tests;
