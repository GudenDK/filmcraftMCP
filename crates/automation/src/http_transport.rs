//! MCP over Streamable HTTP: rmcp's `StreamableHttpService` (protocol, sessions, SSE) on a hyper 1
//! HTTP/1.1 listener, since rmcp ships no listener. One endpoint, `http://<addr>/mcp`, serves any
//! number of clients, and they all drive the same backend (one engine session, or one app).
//!
//! Secure by default (MCP spec, transports § Security Warning):
//! * loopback addresses only, unless a bearer token is set;
//! * `Host` must name this server (`127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`, plus
//!   any `allowed_hosts`): a DNS-rebinding page gets 403;
//! * a request with an `Origin` header (a browser) gets 403 unless that origin is listed in
//!   `allowed_origins`; MCP clients send none;
//! * with a token, every request needs `Authorization: Bearer <token>` (constant-time compare),
//!   checked before anything else, else 401.
//!
//! In front of rmcp this module also routes the path, maps a non-JSON body to a JSON-RPC parse
//! error (`-32700`, as `serve_io` does on stdio), caps sessions and connections, and keeps rmcp's
//! per-tool-name schema cache bounded by answering unknown tool names itself.

use std::convert::Infallible;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderValue, Method, Request, Response, StatusCode, header};
use http_body_util::{BodyExt, Full, Limited, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use rmcp::ServerHandler as _;
use rmcp::transport::streamable_http_server::SessionManager as _;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{AutomationError, FilmcraftMcp};

/// The MCP endpoint path.
pub const MCP_PATH: &str = "/mcp";
/// Environment variable the CLI reads the bearer token from.
pub const TOKEN_ENV: &str = "FILMCRAFT_MCP_TOKEN";
/// Largest request body read (the control channel's line limit; rmcp's default too).
const MAX_BODY: usize = 4 << 20;
/// Connections served at once; further ones are closed on accept.
const MAX_CONNECTIONS: u32 = 64;
/// Live sessions (pre-2026-07-28 clients); a further `initialize` gets 503.
const MAX_SESSIONS: usize = 256;
/// How long shutdown waits for cancelled exports to delete their partial files and connections to close.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// Options for [`FilmcraftMcp::serve_http`].
#[derive(Clone)]
pub struct HttpOptions {
    /// Require `Authorization: Bearer <token>` on every request. Mandatory for a non-loopback
    /// address. Never logged.
    pub token: Option<String>,
    /// More `Host` values to accept besides this server's loopback forms, as `name` (any port) or
    /// `name:port`: a LAN name, `host.docker.internal:<port>`, a tunnel or reverse proxy.
    pub allowed_hosts: Vec<String>,
    /// Browser origins allowed to call (e.g. `http://localhost:6274` for MCP Inspector); a request
    /// with any other `Origin` header gets 403.
    pub allowed_origins: Vec<String>,
    /// Serve pre-2026-07-28 clients with `Mcp-Session-Id` sessions. 2026-07-28 requests are
    /// stateless either way.
    pub legacy_session_mode: bool,
    /// Answer a single-message reply as `application/json` instead of an SSE stream.
    pub json_response: bool,
    /// A session with no traffic for this long is dropped. An export without a `progressToken`
    /// sends nothing while it runs, so keep this longer than the longest one.
    pub session_idle: Option<Duration>,
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

/// The `Host` values this server answers to on `bound`, plus `extra`.
fn allowed_hosts(bound: SocketAddr, extra: &[String]) -> Vec<String> {
    let p = bound.port();
    let mut hosts = vec![format!("127.0.0.1:{p}"), format!("localhost:{p}"), format!("[::1]:{p}")];
    if p == 80 {
        // clients leave the default port out of `Host`
        hosts.extend(["127.0.0.1", "localhost", "[::1]"].map(String::from));
    }
    let ip = bound.ip();
    if !ip.is_loopback() && !ip.is_unspecified() {
        hosts.push(SocketAddr::new(ip, p).to_string());
    }
    hosts.extend(extra.iter().map(|h| h.trim()).filter(|h| !h.is_empty()).map(String::from));
    hosts
}

type HttpBody = BoxBody<Bytes, Infallible>;
type Mcp = StreamableHttpService<FilmcraftMcp, LocalSessionManager>;

/// What every request is checked against before rmcp sees it.
struct Gate {
    token: Option<Box<[u8]>>,
    /// For `get_tool` (the tool router), to answer unknown `tools/call` names.
    tools: FilmcraftMcp,
    sessions: Arc<LocalSessionManager>,
    legacy_session_mode: bool,
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
        let bound = listener.local_addr().map_err(|e| AutomationError::Other(format!("listener: {e}")))?;
        let token = opts.token.filter(|t| !t.is_empty());
        if let Some(t) = &token {
            check_token(t)?;
        }
        check_bind(bound, token.as_deref())?;
        let token = token.map(|t| t.into_bytes().into_boxed_slice());

        let ct = CancellationToken::new();
        let stop = ct.clone();
        tokio::spawn(async move {
            shutdown.await;
            stop.cancel();
        });

        let mut sessions = LocalSessionManager::default();
        sessions.session_config.keep_alive = opts.session_idle;
        let sessions = Arc::new(sessions);
        let config = StreamableHttpServerConfig::default()
            .with_allowed_hosts(allowed_hosts(bound, &opts.allowed_hosts))
            .with_allowed_origins(opts.allowed_origins)
            // validate Origin even when no origin is allowed: then every browser request is refused
            .enforce_origin_validation()
            .with_legacy_session_mode(opts.legacy_session_mode)
            .with_json_response(opts.json_response)
            .with_sse_keep_alive(Some(Duration::from_secs(15)))
            .with_max_request_body_bytes(MAX_BODY)
            .with_cancellation_token(ct.child_token());
        let gate = Arc::new(Gate { token, tools: self.clone(), sessions: sessions.clone(), legacy_session_mode: opts.legacy_session_mode });
        let long_calls = self.long_calls.clone();
        // Called once per session (pre-2026-07-28 clients) or per request (2026-07-28): every
        // clone shares the one backend (`Arc<Backend>`), so all clients see the same project.
        let mcp: Mcp = StreamableHttpService::new(move || Ok(self.clone()), sessions.clone(), config);
        let permits = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS as usize));

        loop {
            let stream = tokio::select! {
                _ = ct.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        // out of file descriptors and the like: back off instead of spinning
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                },
            };
            let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
            let (mcp, gate, ct) = (mcp.clone(), gate.clone(), ct.clone());
            // A panic in a request is contained by its task: the server keeps serving.
            tokio::spawn(async move {
                let _permit = permit;
                let svc = service_fn(move |req: Request<Incoming>| {
                    let (mcp, gate) = (mcp.clone(), gate.clone());
                    async move { Ok::<_, Infallible>(route(&mcp, &gate, req).await) }
                });
                // hyper panics on a timeout configured without a timer (hyper common/time.rs)
                let conn =
                    http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(30)).serve_connection(TokioIo::new(stream), svc);
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
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        // The token ended every SSE stream and cancelled the stateless calls, but session workers
        // outlive it: close them, so their calls see `context.ct` cancelled (after rmcp's response
        // drain) and an export stops and deletes its partial output before the process exits.
        let ids: Vec<_> = sessions.sessions.read().await.keys().cloned().collect();
        for id in ids {
            let _ = sessions.close_session(&id).await;
        }
        // Exports first: whatever still runs at the deadline has its partial output deleted.
        long_calls.drain(deadline).await;
        let _ = tokio::time::timeout_at(deadline, permits.acquire_many(MAX_CONNECTIONS)).await;
        Ok(())
    }
}

/// Checks in front of rmcp; rmcp does the rest (`handle` never fails: errors are responses).
async fn route(mcp: &Mcp, gate: &Gate, req: Request<Incoming>) -> Response<HttpBody> {
    if req.uri().path() != MCP_PATH {
        return text(StatusCode::NOT_FOUND, "Not Found: the MCP endpoint is /mcp");
    }
    if let Some(expected) = gate.token.as_deref()
        && !bearer_matches(req.headers().get(header::AUTHORIZATION), expected)
    {
        let mut r = text(StatusCode::UNAUTHORIZED, "Unauthorized: missing or wrong bearer token");
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"filmcraft\""));
        return r;
    }
    if req.method() != Method::POST {
        return mcp.handle(req).await; // GET = SSE stream, DELETE = end the session, others 405
    }
    let (parts, body) = req.into_parts();
    let bytes = match Limited::new(body, MAX_BODY).collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => {
            return text(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large: the request body is over 4 MiB");
        }
        Err(_) => return text(StatusCode::BAD_REQUEST, "Bad Request: could not read the request body"),
    };
    let msg: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return jsonrpc_error(StatusCode::BAD_REQUEST, Value::Null, -32700, format!("parse error: {e}")),
    };
    let method = msg.get("method").and_then(Value::as_str);
    if gate.legacy_session_mode
        && method == Some("initialize")
        && !parts.headers.contains_key("mcp-session-id")
        && gate.sessions.sessions.read().await.len() >= MAX_SESSIONS
    {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        return jsonrpc_error(StatusCode::SERVICE_UNAVAILABLE, id, -32000, format!("too many MCP sessions ({MAX_SESSIONS}); end unused ones with DELETE"));
    }
    // rmcp caches one schema lookup per distinct tools/call name on requests declaring
    // MCP-Protocol-Version >= 2026-07-28 (SEP-2243 header checks), without bound: only known tool
    // names may reach it. Same lexical comparison as rmcp.
    let modern = parts.headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok()).is_some_and(|v| v >= "2026-07-28");
    if modern && method == Some("tools/call") {
        let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
        if gate.tools.get_tool(name).is_none() {
            let shown: String = name.chars().take(64).collect();
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            return jsonrpc_error(StatusCode::OK, id, -32602, format!("unknown tool `{shown}`"));
        }
    }
    mcp.handle(Request::from_parts(parts, Full::new(bytes))).await
}

/// `Authorization: Bearer <token>` (scheme in any case), compared without an early exit.
fn bearer_matches(value: Option<&HeaderValue>, expected: &[u8]) -> bool {
    let Some(value) = value else { return false };
    let mut parts = value.as_bytes().splitn(2, |b| *b == b' ');
    let (Some(scheme), Some(given)) = (parts.next(), parts.next()) else { return false };
    scheme.eq_ignore_ascii_case(b"bearer") && constant_time_eq(given.trim_ascii(), expected)
}

/// Equal bytes, taking the same time wherever the first difference is (the length is not secret).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

/// A JSON-RPC error as an `application/json` reply.
fn jsonrpc_error(status: StatusCode, id: Value, code: i64, message: String) -> Response<HttpBody> {
    let body = json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}).to_string();
    let mut r = Response::new(Full::new(Bytes::from(body)).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

fn text(status: StatusCode, msg: &'static str) -> Response<HttpBody> {
    let mut r = Response::new(Full::new(Bytes::from_static(msg.as_bytes())).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

#[cfg(test)]
mod tests;
