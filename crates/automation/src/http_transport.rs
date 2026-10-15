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
/// How long shutdown waits for open connections and cancelled exports to finish.
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

        // The token ended every SSE stream and cancelled the stateless calls, but session workers
        // outlive it: close them, so their calls see `context.ct` cancelled (after rmcp's response
        // drain) and an export stops and deletes its partial output before the process exits.
        let ids: Vec<_> = sessions.sessions.read().await.keys().cloned().collect();
        for id in ids {
            let _ = sessions.close_session(&id).await;
        }
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        let _ = tokio::time::timeout_at(deadline, permits.acquire_many(MAX_CONNECTIONS)).await;
        while crate::long_job::calls_in_flight() > 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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
mod tests {
    use super::*;
    use filmcraft_engine::Session;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    fn demo() -> Session {
        let mut s = Session::default();
        s.execute("file.openDemoProject", json!({})).unwrap();
        s
    }

    /// A server on 127.0.0.1:<free port>; dropping `stop` (or sending on it) shuts it down.
    struct Server {
        addr: SocketAddr,
        stop: Option<tokio::sync::oneshot::Sender<()>>,
        done: tokio::task::JoinHandle<Result<(), AutomationError>>,
    }

    impl Server {
        async fn start(s: Session, opts: HttpOptions) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (stop, rx) = tokio::sync::oneshot::channel::<()>();
            let done = tokio::spawn(FilmcraftMcp::headless(s).serve_http_listener(listener, opts, async move {
                let _ = rx.await;
            }));
            Self { addr, stop: Some(stop), done }
        }
        fn host(&self) -> String {
            format!("127.0.0.1:{}", self.addr.port())
        }
    }

    /// One HTTP/1.1 exchange on its own connection (`Connection: close`), read incrementally so
    /// SSE events can be taken one at a time.
    struct Reply {
        status: u16,
        headers: Vec<(String, String)>,
        rd: BufReader<TcpStream>,
        chunked: bool,
        left: Option<usize>,
        buf: Vec<u8>,
        eof: bool,
    }

    async fn send(addr: SocketAddr, method: &str, headers: &[(&str, &str)], body: &str) -> Reply {
        let mut s = TcpStream::connect(addr).await.unwrap();
        let mut req = format!("{method} {MCP_PATH} HTTP/1.1\r\n");
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            req += &format!("Host: 127.0.0.1:{}\r\n", addr.port());
        }
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        req += &format!("Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        s.write_all(req.as_bytes()).await.unwrap();
        let mut rd = BufReader::new(s);
        let mut line = String::new();
        rd.read_line(&mut line).await.unwrap();
        let status = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
        let mut headers = Vec::new();
        loop {
            line.clear();
            rd.read_line(&mut line).await.unwrap();
            let l = line.trim_end();
            if l.is_empty() {
                break;
            }
            if let Some((k, v)) = l.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let get = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let chunked = get("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked"));
        let left = get("content-length").and_then(|v| v.parse().ok());
        Reply { status, headers, rd, chunked, left, buf: Vec::new(), eof: false }
    }

    impl Reply {
        fn header(&self, k: &str) -> Option<&str> {
            self.headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
        }
        /// More body bytes into `buf`; false at the end of the body.
        async fn fill(&mut self) -> bool {
            if self.eof {
                return false;
            }
            if self.chunked {
                let mut size = String::new();
                if self.rd.read_line(&mut size).await.unwrap_or(0) == 0 {
                    self.eof = true;
                    return false;
                }
                let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
                if n == 0 {
                    self.eof = true;
                    return false;
                }
                let mut chunk = vec![0; n + 2];
                self.rd.read_exact(&mut chunk).await.unwrap();
                chunk.truncate(n);
                self.buf.extend(chunk);
                return true;
            }
            let want = match self.left {
                Some(0) => {
                    self.eof = true;
                    return false;
                }
                Some(l) => l.min(8192),
                None => 8192,
            };
            let mut chunk = vec![0; want];
            let n = self.rd.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                self.eof = true;
                return false;
            }
            if let Some(l) = &mut self.left {
                *l -= n;
            }
            chunk.truncate(n);
            self.buf.extend(chunk);
            true
        }
        async fn text(mut self) -> String {
            while self.fill().await {}
            String::from_utf8_lossy(&self.buf).into_owned()
        }
        /// The next JSON-RPC message: the body of a JSON reply, or the next SSE event with data.
        async fn next(&mut self) -> Option<Value> {
            if !self.header("content-type").is_some_and(|c| c.starts_with("text/event-stream")) {
                while self.fill().await {}
                let v = serde_json::from_slice(&self.buf).ok();
                self.buf.clear();
                return v;
            }
            loop {
                let text = String::from_utf8_lossy(&self.buf).replace("\r\n", "\n");
                if let Some(end) = text.find("\n\n") {
                    let event = text[..end].to_string();
                    self.buf = text.as_bytes()[end + 2..].to_vec();
                    let data: Vec<&str> = event.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim).collect();
                    if let Ok(v) = serde_json::from_str(&data.join("\n")) {
                        return Some(v);
                    }
                    continue; // priming event or keep-alive
                }
                if !self.fill().await {
                    return None;
                }
            }
        }
        /// The next message that is not a notification.
        async fn reply(&mut self) -> Value {
            loop {
                let m = self.next().await.expect("a reply");
                if m.get("id").is_some() {
                    return m;
                }
            }
        }
    }

    const ACCEPT: (&str, &str) = ("Accept", "application/json, text/event-stream");
    const JSON: (&str, &str) = ("Content-Type", "application/json");

    async fn post(addr: SocketAddr, session: Option<&str>, extra: &[(&str, &str)], body: &Value) -> Reply {
        let mut h = vec![ACCEPT, JSON];
        if let Some(s) = session {
            h.push(("Mcp-Session-Id", s));
        }
        h.extend_from_slice(extra);
        send(addr, "POST", &h, &body.to_string()).await
    }

    /// `initialize` + `notifications/initialized`; returns the session id and the init result.
    async fn init(addr: SocketAddr, extra: &[(&str, &str)]) -> (String, Value) {
        let mut r = post(
            addr,
            None,
            extra,
            &json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}),
        )
        .await;
        assert_eq!(r.status, 200);
        let sid = r.header("mcp-session-id").expect("Mcp-Session-Id").to_string();
        let v = r.reply().await;
        let mut h = vec![("MCP-Protocol-Version", "2025-06-18")];
        h.extend_from_slice(extra);
        let n = post(addr, Some(&sid), &h, &json!({"jsonrpc":"2.0","method":"notifications/initialized"})).await;
        assert_eq!(n.status, 202);
        (sid, v)
    }

    async fn call(addr: SocketAddr, sid: &str, id: u64, name: &str, arguments: Value) -> Value {
        let msg = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}});
        let mut r = post(addr, Some(sid), &[("MCP-Protocol-Version", "2025-06-18")], &msg).await;
        assert_eq!(r.status, 200);
        r.reply().await
    }

    fn tool_json(r: &Value) -> Value {
        serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    /// Sessions over HTTP: tools, strict arguments, images and resources work as on stdio, and two
    /// clients share one engine session.
    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_share_one_engine_session() {
        let srv = Server::start(demo(), HttpOptions::default()).await;
        let (a, init_a) = init(srv.addr, &[]).await;
        assert_eq!(init_a["result"]["serverInfo"]["name"], "filmcraft", "{init_a}");
        let mut r = post(srv.addr, Some(&a), &[], &json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).await;
        let tools = r.reply().await;
        let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().filter_map(|t| t["name"].as_str()).collect();
        assert!(names.contains(&"command_run") && names.contains(&"render_frame"), "{names:?}");
        let r = call(srv.addr, &a, 2, "command_list", json!({"filter": "razor"})).await;
        assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("timeline.razor"), "{r}");
        // strict arguments: an in-band invalid-params error
        let r = call(srv.addr, &a, 3, "command_list", json!({"filtr": "x"})).await;
        assert_eq!(r["error"]["code"], -32602, "{r}");
        let r = call(srv.addr, &a, 4, "render_frame", json!({"max_side": 64})).await;
        assert_eq!(r["result"]["content"][0]["type"], "image", "{r}");
        let mut r = post(srv.addr, Some(&a), &[], &json!({"jsonrpc":"2.0","id":5,"method":"resources/read","params":{"uri":"filmcraft://document"}})).await;
        let doc = r.reply().await;
        assert!(doc["result"]["contents"][0]["text"].as_str().unwrap().contains("\"sequence\""), "{doc}");

        // an edit in one session is seen by another
        let (b, _) = init(srv.addr, &[]).await;
        assert_ne!(a, b);
        let before = tool_json(&call(srv.addr, &b, 6, "project_inspect", json!({})).await).to_string();
        assert!(!before.contains("FromSessionA"), "{before}");
        let r = call(srv.addr, &a, 7, "command_run", json!({"id": "file.newBin", "params": {"name": "FromSessionA"}})).await;
        assert_eq!(r["result"]["isError"], false, "{r}");
        let after = tool_json(&call(srv.addr, &b, 8, "project_inspect", json!({})).await).to_string();
        assert!(after.contains("FromSessionA"), "{after}");

        // DELETE ends a session; it is then unknown (404)
        let d = send(srv.addr, "DELETE", &[("Mcp-Session-Id", &a)], "").await;
        assert_eq!(d.status, 202);
        let r = post(srv.addr, Some(&a), &[], &json!({"jsonrpc":"2.0","id":9,"method":"ping"})).await;
        assert_eq!(r.status, 404);
    }

    /// MCP 2026-07-28 clients (current Claude Code) send self-contained requests: no session, the
    /// protocol fields in `_meta` and SEP-2243 headers. Unknown tool names are answered in-band.
    #[tokio::test(flavor = "multi_thread")]
    async fn stateless_2026_07_28_requests() {
        let srv = Server::start(demo(), HttpOptions::default()).await;
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let h = |method: &'static str| vec![("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", method)];
        let mut r = post(srv.addr, None, &h("server/discover"), &json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}})).await;
        assert_eq!(r.status, 200);
        assert!(r.header("mcp-session-id").is_none());
        let v = r.reply().await;
        assert!(v["result"]["supportedVersions"].as_array().unwrap().contains(&json!("2026-07-28")), "{v}");
        let mut r = post(srv.addr, None, &h("tools/list"), &json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":meta}})).await;
        let v = r.reply().await;
        assert_eq!(v["result"]["resultType"], "complete", "{v}");
        assert!(v["result"]["ttlMs"].is_u64(), "{v}");
        let mut hh = h("tools/call");
        hh.push(("Mcp-Name", "command_list"));
        let msg = json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"_meta":meta,"name":"command_list","arguments":{"filter":"razor"}}});
        let mut r = post(srv.addr, None, &hh, &msg).await;
        let v = r.reply().await;
        assert!(v["result"]["content"][0]["text"].as_str().unwrap().contains("timeline.razor"), "{v}");
        // an unknown tool: answered here, in-band, without growing rmcp's schema cache
        let mut hh = h("tools/call");
        hh.push(("Mcp-Name", "no_such_tool"));
        let msg = json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"_meta":meta,"name":"no_such_tool","arguments":{}}});
        let mut r = post(srv.addr, None, &hh, &msg).await;
        assert_eq!(r.status, 200);
        let v = r.reply().await;
        assert_eq!((v["id"].as_u64(), v["error"]["code"].as_i64()), (Some(4), Some(-32602)), "{v}");
    }

    /// Hostile requests get an HTTP error and change nothing; the server keeps serving.
    #[tokio::test(flavor = "multi_thread")]
    async fn rejects_hostile_requests() {
        let srv = Server::start(demo(), HttpOptions { allowed_origins: vec!["http://localhost:6274".into()], ..HttpOptions::default() }).await;
        let initialize = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
        // DNS rebinding: a Host that isn't this server, or the right host on another port
        for host in ["evil.example", "evil.example:80", "127.0.0.1:1", "0.0.0.0"] {
            let r = post(srv.addr, None, &[("Host", host)], &initialize).await;
            assert_eq!(r.status, 403, "Host {host}");
        }
        for host in [srv.host(), format!("localhost:{}", srv.addr.port()), format!("[::1]:{}", srv.addr.port())] {
            let r = post(srv.addr, None, &[("Host", host.as_str())], &initialize).await;
            assert_eq!(r.status, 200, "Host {host}");
        }
        // browsers: only listed origins
        for origin in ["http://evil.example", "null", "http://localhost:3000", "http://127.0.0.1"] {
            let r = post(srv.addr, None, &[("Origin", origin)], &initialize).await;
            assert_eq!(r.status, 403, "Origin {origin}");
        }
        assert_eq!(post(srv.addr, None, &[("Origin", "http://localhost:6274")], &initialize).await.status, 200);
        // browser-shaped requests: preflight and "simple" requests
        assert_eq!(send(srv.addr, "OPTIONS", &[("Origin", "http://evil.example")], "").await.status, 403);
        assert_eq!(send(srv.addr, "OPTIONS", &[], "").await.status, 405);
        let r = send(srv.addr, "POST", &[("Content-Type", "text/plain"), ("Accept", "*/*")], &initialize.to_string()).await;
        assert!(matches!(r.status, 406 | 415), "{}", r.status);
        let r = send(srv.addr, "POST", &[ACCEPT, ("Content-Type", "application/x-www-form-urlencoded")], &initialize.to_string()).await;
        assert_eq!(r.status, 415);
        // other paths
        let mut s = TcpStream::connect(srv.addr).await.unwrap();
        s.write_all(format!("GET / HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", srv.host()).as_bytes()).await.unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).await.unwrap();
        assert!(out.starts_with("HTTP/1.1 404"), "{out}");
        // not JSON: a JSON-RPC parse error
        let mut r = send(srv.addr, "POST", &[ACCEPT, JSON], "{\"jsonrpc\": \"2.0\", \"id\": 5, BROKEN").await;
        assert_eq!(r.status, 400);
        let v = r.next().await.unwrap();
        assert_eq!((v["error"]["code"].as_i64(), v["id"].is_null()), (Some(-32700), true), "{v}");
        // too large
        let big = format!("{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"pad\":\"{}\"}}", "x".repeat(MAX_BODY + 1));
        assert_eq!(send(srv.addr, "POST", &[ACCEPT, JSON], &big).await.status, 413);
        // unknown session; GET without a session
        assert_eq!(post(srv.addr, Some("nope"), &[], &json!({"jsonrpc":"2.0","id":1,"method":"ping"})).await.status, 404);
        assert_eq!(send(srv.addr, "GET", &[("Accept", "text/event-stream")], "").await.status, 400);
        // still serving
        let (sid, _) = init(srv.addr, &[]).await;
        let r = call(srv.addr, &sid, 1, "command_list", json!({"filter": "razor"})).await;
        assert_eq!(r["result"]["isError"], false, "{r}");
    }

    /// With a token, every request needs `Authorization: Bearer <token>`, checked first.
    #[tokio::test(flavor = "multi_thread")]
    async fn bearer_token_is_required() {
        let token = "s3cret-token-0123456789";
        let srv = Server::start(demo(), HttpOptions { token: Some(token.into()), ..HttpOptions::default() }).await;
        let initialize = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
        for auth in [None, Some("Bearer wrong"), Some("Bearer s3cret-token-012345678"), Some("Basic czNjcmV0"), Some(token), Some("Bearer")] {
            let h: Vec<(&str, &str)> = auth.map(|a| ("Authorization", a)).into_iter().collect();
            let mut r = post(srv.addr, None, &h, &initialize).await;
            assert_eq!(r.status, 401, "{auth:?}");
            assert_eq!(r.header("www-authenticate"), Some("Bearer realm=\"filmcraft\""));
            assert!(r.header("mcp-session-id").is_none());
            assert!(r.next().await.is_none());
        }
        // every method, before Host/Origin
        assert_eq!(send(srv.addr, "OPTIONS", &[("Origin", "http://evil.example")], "").await.status, 401);
        assert_eq!(send(srv.addr, "DELETE", &[("Mcp-Session-Id", "x")], "").await.status, 401);
        let good = format!("Bearer {token}");
        let lower = format!("bearer {token}");
        for auth in [good.as_str(), lower.as_str()] {
            let (sid, _) = init(srv.addr, &[("Authorization", auth)]).await;
            let msg = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"command_list","arguments":{"filter":"razor"}}});
            let mut r = post(srv.addr, Some(&sid), &[("Authorization", auth)], &msg).await;
            assert_eq!(r.status, 200);
            assert_eq!(r.reply().await["result"]["isError"], false);
            // the session id alone is not a credential
            assert_eq!(post(srv.addr, Some(&sid), &[], &msg).await.status, 401);
        }
    }

    /// Sessions are capped: an `initialize` past the cap gets 503 until one ends.
    #[tokio::test(flavor = "multi_thread")]
    async fn sessions_are_capped() {
        let srv = Server::start(Session::default(), HttpOptions::default()).await;
        let initialize = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
        let mut first = None;
        for _ in 0..MAX_SESSIONS {
            let mut r = post(srv.addr, None, &[], &initialize).await;
            assert_eq!(r.status, 200);
            first.get_or_insert_with(|| r.header("mcp-session-id").unwrap().to_string());
            r.reply().await;
        }
        let mut r = post(srv.addr, None, &[], &initialize).await;
        assert_eq!(r.status, 503);
        assert_eq!(r.next().await.unwrap()["error"]["code"], -32000);
        let first = first.unwrap();
        assert_eq!(send(srv.addr, "DELETE", &[("Mcp-Session-Id", &first)], "").await.status, 202);
        let mut ok = false;
        for _ in 0..50 {
            // the session is removed once its worker has stopped
            if post(srv.addr, None, &[], &initialize).await.status == 200 {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ok, "a session frees a slot when it ends");
    }

    #[test]
    fn listen_addresses_and_bind_rules() {
        let a = |s: &str| parse_listen_addr(s).map(|a| a.to_string()).map_err(|e| e.to_string());
        assert_eq!(a("9000").unwrap(), "127.0.0.1:9000");
        assert_eq!(a("localhost:9000").unwrap(), "127.0.0.1:9000");
        assert_eq!(a(" 127.0.0.1:0 ").unwrap(), "127.0.0.1:0");
        assert_eq!(a("[::1]:9000").unwrap(), "[::1]:9000");
        assert_eq!(a("0.0.0.0:9000").unwrap(), "0.0.0.0:9000");
        for bad in ["", "70000", "localhost", "localhost:x", "example.com:9000", "127.0.0.1", ":9000", "127.0.0.1:-1", "[::1]"] {
            assert!(a(bad).is_err(), "{bad}");
        }
        let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
        assert!(check_bind(addr("127.0.0.1:9000"), None).is_ok());
        assert!(check_bind(addr("[::1]:9000"), None).is_ok());
        for remote in ["0.0.0.0:9000", "[::]:9000", "192.168.1.5:9000"] {
            assert!(check_bind(addr(remote), None).is_err(), "{remote}");
            assert!(check_bind(addr(remote), Some("")).is_err(), "{remote}");
            assert!(check_bind(addr(remote), Some("tok")).is_ok(), "{remote}");
        }
        assert!(check_token("abc-DEF_123.~+/=").is_ok());
        for bad in ["", "has space", "tab\t", "ünïcode", "new\nline"] {
            assert!(check_token(bad).is_err(), "{bad:?}");
        }
        assert!(!format!("{:?}", HttpOptions { token: Some("hunter2".into()), ..HttpOptions::default() }).contains("hunter2"));
    }

    #[test]
    fn bearer_header_matching() {
        let h = |s: &str| HeaderValue::from_str(s).unwrap();
        assert!(bearer_matches(Some(&h("Bearer abc")), b"abc"));
        assert!(bearer_matches(Some(&h("BEARER  abc ")), b"abc"));
        for bad in ["Bearer abd", "Bearer ab", "Bearer abcd", "Bearerabc", "Basic abc", "abc", "Bearer", "Bearer "] {
            assert!(!bearer_matches(Some(&h(bad)), b"abc"), "{bad}");
        }
        assert!(!bearer_matches(None, b"abc"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_non_loopback_bind_needs_a_token() {
        let m = FilmcraftMcp::headless(Session::default());
        let r = m.serve_http("0.0.0.0:0".parse().unwrap(), HttpOptions::default(), async {}).await;
        assert!(matches!(r, Err(AutomationError::BadRequest(_))), "{r:?}");
        let m = FilmcraftMcp::headless(Session::default());
        let r = m.serve_http("127.0.0.1:0".parse().unwrap(), HttpOptions { token: Some("bad token".into()), ..HttpOptions::default() }, async {}).await;
        assert!(matches!(r, Err(AutomationError::BadRequest(_))), "{r:?}");
    }

    /// Shutdown ends the accept loop and returns; the port is closed.
    #[tokio::test(flavor = "multi_thread")]
    async fn shutdown_returns() {
        let mut srv = Server::start(demo(), HttpOptions::default()).await;
        let (_sid, _) = init(srv.addr, &[]).await;
        let _ = srv.stop.take().unwrap().send(());
        let r = tokio::time::timeout(Duration::from_secs(20), &mut srv.done).await.expect("shut down in time").unwrap();
        assert!(r.is_ok(), "{r:?}");
        assert!(TcpStream::connect(srv.addr).await.is_err());
    }

    fn export_call(id: u64, path: &str, seconds: f64, token: Option<Value>, meta: Option<&Value>) -> Value {
        let params = json!({"path": path, "format": "h264", "width": 640, "height": 360, "audio": false, "range": "custom", "startSeconds": 0, "endSeconds": seconds, "wait": true});
        let mut call =
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"command_run","arguments":{"id":"file.exportMedia","params":params}}});
        let mut m = meta.cloned().unwrap_or_else(|| json!({}));
        if let Some(t) = token {
            m["progressToken"] = t;
        }
        call["params"]["_meta"] = m;
        call
    }

    async fn wait_gone(path: &std::path::Path) {
        let t0 = std::time::Instant::now();
        while path.exists() || t0.elapsed() < Duration::from_millis(500) {
            assert!(t0.elapsed().as_secs() < 30, "partial file still there: {}", path.display());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// docs/agents.md § Long exports over HTTP: progress arrives on the call's SSE stream; a session
    /// client cancels with `notifications/cancelled`, a 2026-07-28 client by closing the stream;
    /// either way the export stops and its partial file is deleted.
    #[tokio::test(flavor = "multi_thread")]
    async fn export_progress_and_cancel_over_http() {
        let dir = std::env::temp_dir().join(format!("filmcraft-mcp-http-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let srv = Server::start(demo(), HttpOptions::default()).await;
        let (sid, _) = init(srv.addr, &[]).await;
        let path = |n: &str| dir.join(n).to_string_lossy().to_string();

        // progress, then the result, on one stream
        let mut done = None;
        let mut notes = 0;
        for (i, seconds) in [2.0, 8.0, 20.0].into_iter().enumerate() {
            let name = format!("a{i}.mp4");
            let mut r = post(srv.addr, Some(&sid), &[], &export_call(10 + i as u64, &path(&name), seconds, Some(json!("tok")), None)).await;
            notes = 0;
            let m = loop {
                let m = r.next().await.expect("progress or result");
                if m["method"] == "notifications/progress" {
                    assert_eq!(m["params"]["progressToken"], "tok", "{m}");
                    notes += 1;
                } else {
                    break m;
                }
            };
            assert_eq!(m["result"]["isError"], false, "{m}");
            assert!(dir.join(&name).exists());
            done = Some(m);
            if notes >= 2 {
                break;
            }
        }
        assert!(notes >= 2 && done.is_some(), "{notes} progress notifications");

        // session client: cancel by notification
        let b = path("b.mp4");
        let mut r = post(srv.addr, Some(&sid), &[], &export_call(20, &b, 30.0, Some(json!(7)), None)).await;
        loop {
            let m = r.next().await.expect("progress");
            assert_ne!(m["id"], 20, "finished before the cancel: {m}");
            if m["method"] == "notifications/progress" {
                break;
            }
        }
        let c = post(srv.addr, Some(&sid), &[], &json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":20}})).await;
        assert_eq!(c.status, 202);
        wait_gone(std::path::Path::new(&b)).await;
        let jobs = tool_json(&call(srv.addr, &sid, 21, "command_run", json!({"id": "jobs.list"})).await);
        assert_eq!(jobs.as_array().and_then(|a| a.last()).map(|j| j["result"]["error"].clone()), Some(json!("cancelled")), "{jobs}");
        // no response for a cancelled request
        while let Some(m) = r.next().await {
            assert_ne!(m["id"], 20, "{m}");
        }

        // 2026-07-28 client: cancel by closing the stream
        let meta = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        let c = path("c.mp4");
        let h = [("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/call"), ("Mcp-Name", "command_run")];
        let mut r = post(srv.addr, None, &h, &export_call(30, &c, 30.0, Some(json!("t2")), Some(&meta))).await;
        assert_eq!(r.status, 200);
        loop {
            let m = r.next().await.expect("progress");
            assert_ne!(m["id"], 30, "finished before the disconnect: {m}");
            if m["method"] == "notifications/progress" {
                break;
            }
        }
        drop(r);
        wait_gone(std::path::Path::new(&c)).await;
        let jobs = tool_json(&call(srv.addr, &sid, 31, "command_run", json!({"id": "jobs.list"})).await);
        assert_eq!(jobs.as_array().and_then(|a| a.last()).map(|j| j["result"]["error"].clone()), Some(json!("cancelled")), "{jobs}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Methods the endpoint doesn't serve get 405 with a reason.
    #[tokio::test(flavor = "multi_thread")]
    async fn other_methods_get_405() {
        let srv = Server::start(demo(), HttpOptions::default()).await;
        let r = send(srv.addr, "PUT", &[], "").await;
        assert_eq!(r.status, 405);
        assert!(r.text().await.contains("Method Not Allowed"));
    }
}
