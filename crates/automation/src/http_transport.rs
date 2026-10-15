//! MCP over Streamable HTTP: rmcp's `StreamableHttpService` (protocol, sessions, SSE) on a hyper 1
//! HTTP/1.1 listener, since rmcp ships no listener. One endpoint, `http://<addr>/mcp`, serves any
//! number of clients, and they all drive the same backend (one engine session, or one app).
//!
//! Secure by default (MCP spec, transports § Security Warning):
//! * loopback addresses only, unless a bearer token is set;
//! * with a token, every request needs `Authorization: Bearer <token>` (constant-time compare),
//!   checked before anything else but a listed origin's CORS preflight, else 401;
//! * `Host` must name this server (`127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`, the
//!   bound address, plus any `allowed_hosts`): a DNS-rebinding page gets 403;
//! * a request with an `Origin` header (a browser) gets 403 unless that origin is listed in
//!   `allowed_origins` (which then also get CORS headers); MCP clients send none.
//!
//! [`gate`] runs these checks before reading a body, maps a non-JSON body to a JSON-RPC parse error
//! (`-32700`, as `serve_io` does on stdio), caps sessions, and keeps rmcp's per-tool-name schema
//! cache bounded by answering unknown tool names itself. Refused requests are logged (stderr, or
//! [`HttpOptions::log`]), rate limited per peer.

mod gate;
mod limits;

use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
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

pub use limits::LogSink;

use crate::{AutomationError, FilmcraftMcp};

/// The MCP endpoint path.
pub const MCP_PATH: &str = "/mcp";
/// Environment variable the CLI reads the bearer token from.
pub const TOKEN_ENV: &str = "FILMCRAFT_MCP_TOKEN";
/// Largest request body read (the control channel's line limit; rmcp's default too).
const MAX_BODY: usize = 4 << 20;
/// Live sessions (pre-2026-07-28 clients); a further `initialize` gets 503.
const MAX_SESSIONS: usize = 256;
/// At most one log line per peer and reason this often.
const LOG_EVERY: Duration = Duration::from_secs(10);
/// Connections refused with a 503 at once; past this, further ones are closed unanswered.
const MAX_REFUSING: usize = 32;

/// The server's limits and timeouts (tests shorten them).
#[derive(Clone, Debug)]
pub(crate) struct Limits {
    /// Connections served at once; one more gets a 503 and is closed.
    pub(crate) connections: u32,
    /// Connections from one peer (an IPv4 address or an IPv6 /64) at once, so that one address
    /// cannot hold them all, credentials or not. Local clients all come from 127.0.0.1.
    pub(crate) connections_per_peer: usize,
    /// How long a client may take to send a request's head; also how long a keep-alive
    /// connection may sit idle between requests.
    pub(crate) header_timeout: Duration,
    /// How long a client may take to send a request's body (408 after).
    pub(crate) body_timeout: Duration,
    /// How long shutdown waits for cancelled exports to delete their partial files and for
    /// connections to close.
    pub(crate) grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connections: 256,
            connections_per_peer: 32,
            header_timeout: Duration::from_secs(10),
            body_timeout: Duration::from_secs(30),
            grace: Duration::from_secs(15),
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
    /// A session with no traffic for this long is dropped. An export without a `progressToken`
    /// sends nothing while it runs, so keep this longer than the longest one.
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
            session_idle: Some(Duration::from_secs(3600)),
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
/// reached from the network, and every tool writes files or drives the app.
pub fn check_bind(addr: SocketAddr, token: Option<&str>) -> Result<(), AutomationError> {
    if addr.ip().is_loopback() || token.is_some_and(|t| !t.is_empty()) {
        return Ok(());
    }
    Err(AutomationError::BadRequest(format!("serving MCP on non-loopback {addr} needs a bearer token (set {TOKEN_ENV}); use 127.0.0.1 for local clients")))
}

/// A usable bearer token: visible ASCII without spaces, as `Authorization: Bearer` carries it.
pub fn check_token(token: &str) -> Result<(), AutomationError> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AutomationError::BadRequest("the MCP token must be non-empty visible ASCII without spaces".into()));
    }
    Ok(())
}

/// An `allowed_origins` entry: `scheme://host:port` with an `http` or `https` scheme and the port
/// spelled out (`http://localhost:6274`), or `null`. Returns it normalized. Wildcards (`*`, `:*`),
/// a missing port (which rmcp would match against every port) and anything else are refused.
/// `null` is accepted, but every web page can send it (from a sandboxed frame or a `file:` page),
/// so the caller should warn about it.
pub fn check_allowed_origin(origin: &str) -> Result<String, AutomationError> {
    let o = origin.trim();
    let bad = |why: String| Err(AutomationError::BadRequest(format!("allowed origin `{o}`: {why}")));
    if o.eq_ignore_ascii_case("null") {
        return Ok("null".into());
    }
    if o == "*" || o.ends_with(":*") {
        return bad("wildcards would let any web page (or any port) call; list each origin as scheme://host:port".into());
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
    if authority.port_u16().is_none() {
        let port = if scheme == "https" { 443 } else { 80 };
        return bad(format!("add the port (every other port would match too), e.g. {scheme}://{}:{port}", authority.host()));
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
type Mcp = StreamableHttpService<FilmcraftMcp, LocalSessionManager>;

/// What a server counted (tests read it).
#[derive(Default)]
pub(crate) struct Stats {
    /// Handlers rmcp asked for: one per session or stateless request, and one per tool name it
    /// looks up a schema for.
    pub(crate) services: AtomicUsize,
}

impl FilmcraftMcp {
    /// Serve MCP over Streamable HTTP at `http://<addr>/mcp` until `shutdown` completes.
    pub async fn serve_http(self, addr: SocketAddr, opts: HttpOptions, shutdown: impl Future<Output = ()> + Send + 'static) -> Result<(), AutomationError> {
        check_bind(addr, opts.token.as_deref())?;
        let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| AutomationError::Other(format!("cannot listen on {addr}: {e}")))?;
        self.serve_http_listener(listener, opts, shutdown).await
    }

    /// [`Self::serve_http`] on a bound listener (the caller can print the bound port first, and
    /// tests bind `127.0.0.1:0`).
    pub async fn serve_http_listener(
        self,
        listener: tokio::net::TcpListener,
        opts: HttpOptions,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), AutomationError> {
        self.serve_with(listener, opts, Limits::default(), Arc::default(), shutdown).await
    }

    /// [`Self::serve_http_listener`] with these limits, counting into `stats`.
    pub(crate) async fn serve_with(
        self,
        listener: tokio::net::TcpListener,
        opts: HttpOptions,
        limits: Limits,
        stats: Arc<Stats>,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), AutomationError> {
        let bound = listener.local_addr().map_err(|e| AutomationError::Other(format!("listener: {e}")))?;
        let token = opts.token.filter(|t| !t.is_empty());
        if let Some(t) = &token {
            check_token(t)?;
        }
        check_bind(bound, token.as_deref())?;
        let token = token.map(|t| t.into_bytes().into_boxed_slice());
        let origins = opts.allowed_origins.iter().map(|o| check_allowed_origin(o)).collect::<Result<Vec<_>, _>>()?;
        let extra_hosts = opts.allowed_hosts.iter().map(|h| check_allowed_host(h)).collect::<Result<Vec<_>, _>>()?;
        let hosts = accepted_hosts(bound, &extra_hosts);

        let ct = CancellationToken::new();
        let stop = ct.clone();
        tokio::spawn(async move {
            shutdown.await;
            stop.cancel();
        });

        let mut sessions = LocalSessionManager::default();
        sessions.session_config.keep_alive = opts.session_idle;
        let sessions = Arc::new(sessions);
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
        // Called once per session (pre-2026-07-28 clients) or per request (2026-07-28): every
        // clone shares the one backend (`Arc<Backend>`), so all clients see the same project.
        let mcp: Mcp = StreamableHttpService::new(
            move || {
                stats.services.fetch_add(1, Ordering::Relaxed);
                Ok(self.clone())
            },
            sessions.clone(),
            config,
        );
        let gate = Arc::new(gate::Gate {
            token,
            hosts: hosts.iter().filter_map(|h| gate::parse_allowed_host(h)).collect(),
            origins,
            tools,
            mcp,
            sessions: sessions.clone(),
            legacy_session_mode: opts.legacy_session_mode,
            body_timeout: limits.body_timeout,
            log: limits::Log::new(opts.log, LOG_EVERY),
        });
        let conns = limits::Conns::new(limits.connections, limits.connections_per_peer);
        let refusing = Arc::new(tokio::sync::Semaphore::new(MAX_REFUSING));

        loop {
            let (stream, peer) = tokio::select! {
                _ = ct.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok(accepted) => accepted,
                    Err(_) => {
                        // out of file descriptors and the like: back off instead of spinning
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            let slot = match conns.open(peer.ip()) {
                Ok(slot) => slot,
                Err(why) => {
                    gate.log.event(peer.ip(), why, format_args!("refused a connection from {peer} with 503: {why}"));
                    if let Ok(permit) = refusing.clone().try_acquire_owned() {
                        tokio::spawn(async move {
                            let _permit = permit;
                            limits::refuse_connection(stream).await;
                        });
                    }
                    continue;
                }
            };
            let (gate, ct, header_timeout) = (gate.clone(), ct.clone(), limits.header_timeout);
            // A panic in a request is contained by its task: the server keeps serving.
            tokio::spawn(async move {
                let _slot = slot;
                let svc = service_fn(move |req: Request<Incoming>| {
                    let gate = gate.clone();
                    async move { Ok::<_, Infallible>(gate.route(peer, req).await) }
                });
                // hyper panics on a timeout configured without a timer (hyper common/time.rs). Its
                // header timer also runs while a keep-alive connection waits for its next request.
                let conn = http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(header_timeout).serve_connection(TokioIo::new(stream), svc);
                tokio::pin!(conn);
                tokio::select! {
                    _ = conn.as_mut() => {}
                    _ = ct.cancelled() => {
                        conn.as_mut().graceful_shutdown();
                        let _ = tokio::time::timeout(Duration::from_secs(5), conn).await;
                    }
                }
            });
        }

        // Refuse new connections now rather than leave them hanging until the process exits.
        drop(listener);
        let deadline = deadline_after(limits.grace);
        // The token ended every SSE stream and cancelled the stateless calls, but session workers
        // outlive it: close them, so their calls see `context.ct` cancelled (after rmcp's response
        // drain) and an export stops and deletes its partial output before the process exits.
        let ids: Vec<_> = sessions.sessions.read().await.keys().cloned().collect();
        for id in ids {
            let _ = sessions.close_session(&id).await;
        }
        // Exports first: whatever still runs at the deadline has its partial output deleted.
        long_calls.drain(deadline).await;
        let _ = tokio::time::timeout_at(deadline, conns.all_closed()).await;
        Ok(())
    }
}

/// `d` from now (far in the future if that overflows).
fn deadline_after(d: Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now.checked_add(d).or_else(|| now.checked_add(Duration::from_secs(86_400 * 365))).unwrap_or(now)
}

#[cfg(test)]
mod tests;
