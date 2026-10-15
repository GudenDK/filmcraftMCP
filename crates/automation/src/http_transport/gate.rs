//! The checks in front of rmcp, cheapest first: the path, the bearer token, `Host` and `Origin`
//! (DNS rebinding, browsers), and for a POST its `Accept` and `Content-Type`; only then is the body
//! read (bounded), and parsed once to keep rmcp's caches and sessions bounded. rmcp repeats the
//! header checks with the same lists; running them here first means a refused request costs no body
//! read and gets the same answer whatever its body holds (no oracle for a DNS-rebinding page).

use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http::header::{self, HeaderName};
use http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use rmcp::ServerHandler as _;
use rmcp::model::ProtocolVersion;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use serde_json::{Value, json};

use super::limits::Log;
use super::{HttpBody, MAX_BODY, MAX_SESSIONS, MCP_PATH, Mcp};
use crate::FilmcraftMcp;

/// Request headers a listed browser origin may send (answer to its CORS preflight).
const CORS_ALLOW_HEADERS: &str = "content-type, accept, authorization, mcp-session-id, mcp-protocol-version, mcp-method, mcp-name, last-event-id";

/// What every request is checked against before rmcp sees it.
pub(super) struct Gate {
    pub(super) token: Option<Box<[u8]>>,
    /// `Host` values this server answers to, as (host, port; `None` = any port).
    pub(super) hosts: Vec<(String, Option<u16>)>,
    /// Browser origins allowed to call, normalized (`scheme://host:port`, or `null`).
    pub(super) origins: Vec<String>,
    /// For `get_tool` (the tool router), to answer unknown `tools/call` names.
    pub(super) tools: FilmcraftMcp,
    pub(super) mcp: Mcp,
    pub(super) sessions: Arc<LocalSessionManager>,
    pub(super) legacy_session_mode: bool,
    pub(super) log: Log,
}

/// A request refused here: answered with `status` and `body` (which also names the reason in the
/// log), plus a detail for the log only.
struct Refusal {
    status: StatusCode,
    body: &'static str,
    detail: Option<String>,
}

fn refusal(status: StatusCode, body: &'static str) -> Refusal {
    Refusal { status, body, detail: None }
}

impl Refusal {
    /// What the client sent, quoted and cut short, for the log.
    fn showing(mut self, what: &str, value: &HeaderValue) -> Self {
        let v = String::from_utf8_lossy(value.as_bytes());
        let shown: String = v.chars().take(80).collect();
        self.detail = Some(format!("{what} {shown:?}"));
        self
    }
}

impl Gate {
    /// Check `req`, then hand it to rmcp. Never fails: errors are responses.
    pub(super) async fn route(&self, peer: SocketAddr, req: Request<Incoming>) -> Response<HttpBody> {
        if req.uri().path() != MCP_PATH {
            return text(StatusCode::NOT_FOUND, "Not Found: the MCP endpoint is /mcp");
        }
        // CORS headers go to a listed origin only (a page allowed with `--allow-origin`).
        let cors = req.headers().get(header::ORIGIN).filter(|o| self.origin_listed(o)).cloned();
        let method = req.method().clone();
        let mut r = match self.checked(req, cors.is_some()).await {
            Ok(r) => r,
            Err(no) => self.refuse(peer, &method, no),
        };
        if let Some(origin) = cors {
            allow_origin(r.headers_mut(), origin);
        }
        r
    }

    async fn checked(&self, req: Request<Incoming>, listed_origin: bool) -> Result<Response<HttpBody>, Refusal> {
        // A browser sends a CORS preflight without credentials: it is answered before the token
        // check, and only for a listed origin.
        if listed_origin && req.method() == Method::OPTIONS && req.headers().contains_key(header::ACCESS_CONTROL_REQUEST_METHOD) {
            self.check_host(&req)?;
            return Ok(preflight(req.headers()));
        }
        self.check_token(req.headers())?;
        self.check_host(&req)?;
        self.check_origin(req.headers())?;
        if req.method() != Method::POST {
            return Ok(self.mcp.handle(req).await); // GET = SSE stream, DELETE = end the session, others 405
        }
        check_accept_and_type(req.headers())?;
        self.post(req).await
    }

    /// A POST that passed the header checks: read and parse its body, then hand it to rmcp.
    async fn post(&self, req: Request<Incoming>) -> Result<Response<HttpBody>, Refusal> {
        let (parts, body) = req.into_parts();
        let bytes = match Limited::new(body, MAX_BODY).collect().await {
            Ok(c) => c.to_bytes(),
            Err(e) if e.downcast_ref::<http_body_util::LengthLimitError>().is_some() => {
                return Err(refusal(StatusCode::PAYLOAD_TOO_LARGE, "Payload Too Large: the request body is over 4 MiB"));
            }
            Err(_) => return Err(refusal(StatusCode::BAD_REQUEST, "Bad Request: could not read the request body")),
        };
        let msg: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => return Ok(jsonrpc_error(StatusCode::BAD_REQUEST, Value::Null, -32700, format!("parse error: {e}"))),
        };
        let method = msg.get("method").and_then(Value::as_str);
        if self.legacy_session_mode
            && method == Some("initialize")
            && !parts.headers.contains_key("mcp-session-id")
            && self.sessions.sessions.read().await.len() >= MAX_SESSIONS
        {
            let id = msg.get("id").cloned().unwrap_or(Value::Null);
            return Ok(jsonrpc_error(
                StatusCode::SERVICE_UNAVAILABLE,
                id,
                -32000,
                format!("too many MCP sessions ({MAX_SESSIONS}); end unused ones with DELETE"),
            ));
        }
        // rmcp caches one schema lookup per distinct tools/call name, without bound, for messages
        // declaring MCP-Protocol-Version >= 2026-07-28 (SEP-2243 header checks; the same lexical
        // comparison and header as rmcp): only known tool names may reach it. A request gets
        // rmcp's own unknown-tool error; a notification is accepted and dropped, as it gets no reply.
        let checks_headers =
            parts.headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok()).is_some_and(|v| v >= ProtocolVersion::STANDARD_HEADERS.as_str());
        if checks_headers && method == Some("tools/call") {
            let name = msg.pointer("/params/name").and_then(Value::as_str).unwrap_or_default();
            if self.tools.get_tool(name).is_none() {
                return Ok(match msg.get("id").filter(|id| !id.is_null()) {
                    Some(id) => jsonrpc_error(StatusCode::OK, id.clone(), -32602, "tool not found".into()),
                    None => accepted(),
                });
            }
        }
        Ok(self.mcp.handle(Request::from_parts(parts, Full::new(bytes))).await)
    }

    /// The answer to a refused request; logged (at most one line per peer and reason every few
    /// seconds), never with a credential.
    fn refuse(&self, peer: SocketAddr, method: &Method, no: Refusal) -> Response<HttpBody> {
        let detail = no.detail.map(|d| format!(" ({d})")).unwrap_or_default();
        self.log.event(peer.ip(), no.body, format_args!("refused {method} from {peer}: {} {}{detail}", no.status.as_u16(), no.body));
        let mut r = text(no.status, no.body);
        let h = r.headers_mut();
        if no.status == StatusCode::UNAUTHORIZED {
            h.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"filmcraft\""));
        }
        // The body (if any) was not read, so the connection cannot carry another request; and a
        // client refused once gets no idle connection to hold.
        h.insert(header::CONNECTION, HeaderValue::from_static("close"));
        r
    }

    fn check_token(&self, headers: &HeaderMap) -> Result<(), Refusal> {
        match self.token.as_deref() {
            Some(expected) if !bearer_matches(headers.get(header::AUTHORIZATION), expected) => {
                Err(refusal(StatusCode::UNAUTHORIZED, "Unauthorized: missing or wrong bearer token"))
            }
            _ => Ok(()),
        }
    }

    /// rmcp's DNS-rebinding check: `Host` (or, without one, the request's authority) must name this
    /// server.
    fn check_host<B>(&self, req: &Request<B>) -> Result<(), Refusal> {
        let (host, port) = match req.headers().get(header::HOST) {
            Some(v) => {
                let s = v.to_str().map_err(|_| refusal(StatusCode::BAD_REQUEST, "Bad Request: Invalid Host header encoding"))?;
                let a =
                    http::uri::Authority::try_from(s).map_err(|_| refusal(StatusCode::BAD_REQUEST, "Bad Request: Invalid Host header").showing("Host", v))?;
                (normalize_host(a.host()), a.port_u16())
            }
            None => {
                let a = req.uri().authority().ok_or_else(|| refusal(StatusCode::BAD_REQUEST, "Bad Request: missing Host header"))?;
                (normalize_host(a.host()), a.port_u16())
            }
        };
        if self.hosts.iter().any(|(h, p)| *h == host && p.is_none_or(|p| port == Some(p))) {
            return Ok(());
        }
        let no = refusal(StatusCode::FORBIDDEN, "Forbidden: Host header is not allowed");
        Err(match req.headers().get(header::HOST) {
            Some(v) => no.showing("Host", v),
            None => no,
        })
    }

    /// A request with an `Origin` (a browser) must come from a listed origin.
    fn check_origin(&self, headers: &HeaderMap) -> Result<(), Refusal> {
        let Some(v) = headers.get(header::ORIGIN) else { return Ok(()) };
        let s = v.to_str().map_err(|_| refusal(StatusCode::FORBIDDEN, "Forbidden: Invalid Origin header encoding"))?;
        let origin = normalize_origin(s).ok_or_else(|| refusal(StatusCode::FORBIDDEN, "Forbidden: Invalid Origin header").showing("Origin", v))?;
        if self.origins.contains(&origin) {
            return Ok(());
        }
        Err(refusal(StatusCode::FORBIDDEN, "Forbidden: Origin header is not allowed").showing("Origin", v))
    }

    fn origin_listed(&self, v: &HeaderValue) -> bool {
        v.to_str().ok().and_then(normalize_origin).is_some_and(|o| self.origins.contains(&o))
    }
}

/// rmcp's POST checks on `Accept` and `Content-Type`, with its wording.
fn check_accept_and_type(headers: &HeaderMap) -> Result<(), Refusal> {
    let has = |name: HeaderName, ok: fn(&str) -> bool| headers.get(name).and_then(|v| v.to_str().ok()).is_some_and(ok);
    if !has(header::ACCEPT, |a| a.contains("application/json") && a.contains("text/event-stream")) {
        return Err(refusal(StatusCode::NOT_ACCEPTABLE, "Not Acceptable: Client must accept both application/json and text/event-stream"));
    }
    if !has(header::CONTENT_TYPE, |c| c.starts_with("application/json")) {
        return Err(refusal(StatusCode::UNSUPPORTED_MEDIA_TYPE, "Unsupported Media Type: Content-Type must be application/json"));
    }
    Ok(())
}

/// A host name as rmcp compares them: no IPv6 brackets, lower case.
fn normalize_host(host: &str) -> String {
    host.trim_matches('[').trim_matches(']').to_ascii_lowercase()
}

/// An allowed `Host` entry as rmcp reads it: `host` (any port) or `host:port`.
pub(super) fn parse_allowed_host(entry: &str) -> Option<(String, Option<u16>)> {
    let e = entry.trim();
    if e.is_empty() {
        return None;
    }
    Some(match http::uri::Authority::try_from(e) {
        Ok(a) => (normalize_host(a.host()), a.port_u16()),
        Err(_) => (normalize_host(e), None),
    })
}

/// An origin as the allow-list holds it: `scheme://host:port` in lower case with the scheme's
/// default port filled in, or `null`; `None` when `value` is not an origin.
pub(super) fn normalize_origin(value: &str) -> Option<String> {
    let v = value.trim();
    if v.eq_ignore_ascii_case("null") {
        return Some("null".into());
    }
    let uri: http::Uri = v.parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let authority = uri.authority()?;
    let default_port = match scheme.as_str() {
        "http" => Some(80),
        "https" => Some(443),
        _ => None,
    };
    let port = authority.port_u16().or(default_port)?;
    Some(format!("{scheme}://{}:{port}", authority.host().to_ascii_lowercase()))
}

/// The answer to a listed origin's CORS preflight. SEP-2243 `Mcp-Param-*` headers the page asks
/// to send are allowed too.
fn preflight(headers: &HeaderMap) -> Response<HttpBody> {
    let mut allow = CORS_ALLOW_HEADERS.to_string();
    let asked = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS).and_then(|v| v.to_str().ok()).unwrap_or_default();
    for h in asked.split(',').map(str::trim).take(64) {
        let is_param = h.get(..10).is_some_and(|p| p.eq_ignore_ascii_case("mcp-param-")) && h.len() > 10;
        if is_param && HeaderName::from_bytes(h.as_bytes()).is_ok() {
            allow.push_str(", ");
            allow.push_str(&h.to_ascii_lowercase());
        }
    }
    let mut r = Response::new(Full::new(Bytes::new()).boxed());
    *r.status_mut() = StatusCode::NO_CONTENT;
    let h = r.headers_mut();
    h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, HeaderValue::from_static("GET, POST, DELETE"));
    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, HeaderValue::from_str(&allow).unwrap_or(HeaderValue::from_static(CORS_ALLOW_HEADERS)));
    h.insert(header::ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    r
}

/// CORS headers on every answer to a listed origin (and only to one).
fn allow_origin(h: &mut HeaderMap, origin: HeaderValue) {
    h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    h.append(header::VARY, HeaderValue::from_static("Origin"));
    h.insert(header::ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("Mcp-Session-Id, Mcp-Protocol-Version, WWW-Authenticate"));
}

/// `Authorization: Bearer <token>` (scheme in any case), compared without an early exit.
pub(super) fn bearer_matches(value: Option<&HeaderValue>, expected: &[u8]) -> bool {
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

/// `202 Accepted` with no body, as rmcp answers a notification.
fn accepted() -> Response<HttpBody> {
    let mut r = Response::new(Full::new(Bytes::new()).boxed());
    *r.status_mut() = StatusCode::ACCEPTED;
    r
}

pub(super) fn text(status: StatusCode, msg: &'static str) -> Response<HttpBody> {
    let mut r = Response::new(Full::new(Bytes::from_static(msg.as_bytes())).boxed());
    *r.status_mut() = status;
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}
