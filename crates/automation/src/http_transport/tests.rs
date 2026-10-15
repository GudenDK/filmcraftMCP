//! Tests for the Streamable HTTP transport: real requests on 127.0.0.1:<free port>.

use super::*;
use filmcraft_engine::Session;
use http::HeaderValue;
use serde_json::{Value, json};
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
    /// A clone of the server's handler (shares its backend and long-call count).
    mcp: FilmcraftMcp,
}

impl Server {
    async fn start(s: Session, opts: HttpOptions) -> Self {
        Self::start_with(s, opts, Arc::default()).await
    }
    async fn start_with(s: Session, opts: HttpOptions, stats: Arc<Stats>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::start_on(listener, s, opts, Limits::default(), stats)
    }
    async fn start_limited(s: Session, opts: HttpOptions, limits: Limits) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        Self::start_on(listener, s, opts, limits, Arc::default())
    }
    fn start_on(listener: tokio::net::TcpListener, s: Session, opts: HttpOptions, limits: Limits, stats: Arc<Stats>) -> Self {
        let addr = listener.local_addr().unwrap();
        let (stop, rx) = tokio::sync::oneshot::channel::<()>();
        let mcp = FilmcraftMcp::headless(s);
        let done = tokio::spawn(mcp.clone().serve_with(listener, opts, limits, stats, async move {
            let _ = rx.await;
        }));
        Self { addr, stop: Some(stop), done, mcp }
    }
    fn host(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }
}

/// A log sink that keeps its lines.
fn log_capture() -> (Option<LogSink>, Arc<std::sync::Mutex<Vec<String>>>) {
    let lines = Arc::new(std::sync::Mutex::new(Vec::new()));
    let keep = lines.clone();
    (Some(Arc::new(move |l: &str| keep.lock().unwrap().push(l.to_string()))), lines)
}

/// How long a test waits for any one network read.
const WAIT: Duration = Duration::from_secs(60);

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
    send_declaring(addr, method, headers, body.len(), body).await
}

/// [`send`], declaring a `Content-Length` of `len` but sending only `body`: a client still uploading.
async fn send_declaring(addr: SocketAddr, method: &str, headers: &[(&str, &str)], len: usize, body: &str) -> Reply {
    let s = tokio::time::timeout(WAIT, TcpStream::connect(addr)).await.expect("connect in time").unwrap();
    exchange(s, addr, method, headers, len, body).await
}

/// A connection to `addr` from the loopback address `local` (`127.0.0.2`), where the system
/// allows it.
async fn connect_from(local: &str, addr: SocketAddr) -> Option<TcpStream> {
    let sock = tokio::net::TcpSocket::new_v4().ok()?;
    sock.bind(format!("{local}:0").parse().ok()?).ok()?;
    tokio::time::timeout(WAIT, sock.connect(addr)).await.ok()?.ok()
}

/// Everything the server sends until it closes the connection.
async fn read_all(mut s: TcpStream) -> String {
    let mut out = Vec::new();
    let _ = tokio::time::timeout(WAIT, s.read_to_end(&mut out)).await.expect("closed in time");
    String::from_utf8_lossy(&out).into_owned()
}

/// [`send_declaring`] on an open connection.
async fn exchange(mut s: TcpStream, addr: SocketAddr, method: &str, headers: &[(&str, &str)], len: usize, body: &str) -> Reply {
    let mut req = format!("{method} {MCP_PATH} HTTP/1.1\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req += &format!("Host: 127.0.0.1:{}\r\n", addr.port());
    }
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += &format!("Content-Length: {len}\r\nConnection: close\r\n\r\n{body}");
    // the server may answer and close before reading it all
    let _ = s.write_all(req.as_bytes()).await;
    let mut rd = BufReader::new(s);
    let mut line = String::new();
    tokio::time::timeout(WAIT, rd.read_line(&mut line)).await.expect("an answer in time").unwrap();
    let status = line.split_whitespace().nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
    let mut headers = Vec::new();
    loop {
        line.clear();
        tokio::time::timeout(WAIT, rd.read_line(&mut line)).await.expect("headers in time").unwrap();
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
            if tokio::time::timeout(WAIT, self.rd.read_line(&mut size)).await.expect("a chunk in time").unwrap_or(0) == 0 {
                self.eof = true;
                return false;
            }
            let n = usize::from_str_radix(size.trim().split(';').next().unwrap_or("0"), 16).unwrap_or(0);
            if n == 0 {
                self.eof = true;
                return false;
            }
            let mut chunk = vec![0; n + 2];
            tokio::time::timeout(WAIT, self.rd.read_exact(&mut chunk)).await.expect("a chunk in time").unwrap();
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
        let n = tokio::time::timeout(WAIT, self.rd.read(&mut chunk)).await.expect("the body in time").unwrap_or(0);
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

/// rmcp caches a schema lookup for every tools/call name it sees on a 2026-07-28 message, without
/// bound: unknown names are answered before they reach it, with rmcp's own `tool not found`, and a
/// notification naming one is accepted and dropped (a notification gets no reply).
#[tokio::test(flavor = "multi_thread")]
async fn unknown_tool_names_never_reach_rmcp() {
    let stats = Arc::new(Stats::default());
    let srv = Server::start_with(demo(), HttpOptions::default(), stats.clone()).await;
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": {"name": "t", "version": "1"},
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    let before = stats.services.load(Ordering::Relaxed);
    for i in 0..40u64 {
        let name = format!("no_such_tool_{i}");
        let h = [("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/call"), ("Mcp-Name", name.as_str())];
        let msg = json!({"jsonrpc":"2.0","id":i,"method":"tools/call","params":{"_meta":meta,"name":name,"arguments":{}}});
        let mut r = post(srv.addr, None, &h, &msg).await;
        assert_eq!(r.status, 200);
        let v = r.reply().await;
        assert_eq!((v["id"].as_u64(), v["error"]["code"].as_i64(), v["error"]["message"].as_str()), (Some(i), Some(-32602), Some("tool not found")), "{v}");
        let note = json!({"jsonrpc":"2.0","method":"tools/call","params":{"_meta":meta,"name":name,"arguments":{}}});
        let r = post(srv.addr, None, &h, &note).await;
        assert_eq!(r.status, 202, "a notification is not answered");
        assert_eq!(r.text().await, "");
    }
    assert_eq!(stats.services.load(Ordering::Relaxed), before, "rmcp made no handler (and cached no schema) for an unknown name");
    // a known tool still reaches rmcp
    let h = [("MCP-Protocol-Version", "2026-07-28"), ("Mcp-Method", "tools/call"), ("Mcp-Name", "command_list")];
    let msg = json!({"jsonrpc":"2.0","id":99,"method":"tools/call","params":{"_meta":meta,"name":"command_list","arguments":{"filter":"razor"}}});
    let v = post(srv.addr, None, &h, &msg).await.reply().await;
    assert_eq!(v["result"]["isError"], false, "{v}");
    assert!(stats.services.load(Ordering::Relaxed) > before);
    // a session client (before 2026-07-28) gets the same wording from rmcp's tool router
    let (sid, _) = init(srv.addr, &[]).await;
    let v = call(srv.addr, &sid, 1, "no_such_tool", json!({})).await;
    assert_eq!(v["error"]["message"], "tool not found", "{v}");
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

/// Host, Origin, Accept and Content-Type are checked before the body is read: a refused request
/// is answered at once (not after its upload), the same whatever its body holds (a DNS-rebinding
/// page cannot tell a parse error or a tool name from a refusal), and logged.
#[tokio::test(flavor = "multi_thread")]
async fn refusals_come_before_the_body_is_read() {
    let (log, lines) = log_capture();
    let srv = Server::start(demo(), HttpOptions { allowed_origins: vec!["http://localhost:6274".into()], log, ..HttpOptions::default() }).await;
    let unknown_tool = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"probe_tool","arguments":{}}}).to_string();
    let modern = ("MCP-Protocol-Version", "2026-07-28");
    for body in ["{\"jsonrpc\": \"2.0\", \"id\": 5, BROKEN", unknown_tool.as_str()] {
        let cases: [(&[(&str, &str)], u16); 4] = [
            (&[ACCEPT, JSON, modern, ("Host", "evil.example:80")], 403),
            (&[ACCEPT, JSON, modern, ("Origin", "http://evil.example")], 403),
            (&[("Accept", "application/json"), JSON, modern], 406),
            (&[ACCEPT, ("Content-Type", "text/plain"), modern], 415),
        ];
        for (headers, status) in cases {
            // declares 4 MiB but sends a few bytes: an answer proves the body was not waited for
            let r = send_declaring(srv.addr, "POST", headers, MAX_BODY, body).await;
            assert_eq!(r.status, status, "{headers:?}");
            assert!(r.header("content-type").is_some_and(|c| c.starts_with("text/plain")), "a plain refusal, not JSON-RPC: {headers:?}");
        }
    }
    // the same bodies with acceptable headers: a parse error, and an in-band unknown tool
    assert_eq!(send(srv.addr, "POST", &[ACCEPT, JSON], "{\"jsonrpc\": \"2.0\", \"id\": 5, BROKEN").await.status, 400);
    let mut r = send(srv.addr, "POST", &[ACCEPT, JSON, modern, ("Mcp-Method", "tools/call"), ("Mcp-Name", "probe_tool")], &unknown_tool).await;
    assert_eq!(r.next().await.unwrap()["error"]["message"], "tool not found");
    let l = lines.lock().unwrap().join("\n");
    for want in [
        "refused POST from 127.0.0.1:",
        "403 Forbidden: Host header is not allowed (Host \"evil.example:80\")",
        "403 Forbidden: Origin header is not allowed (Origin \"http://evil.example\")",
        "406 Not Acceptable",
        "415 Unsupported Media Type",
    ] {
        assert!(l.contains(want), "log lacks {want:?}:\n{l}");
    }
    // one line per peer and reason: each case ran twice
    assert_eq!(l.matches("406 Not Acceptable").count(), 1, "{l}");
}

/// The bound address is an accepted `Host` too, so any loopback address works (127.0.0.2), and
/// the list has no duplicates.
#[tokio::test(flavor = "multi_thread")]
async fn the_bound_address_is_an_accepted_host() {
    let h = |a: &str, extra: &[&str]| accepted_hosts(a.parse().unwrap(), &extra.iter().map(|e| e.to_string()).collect::<Vec<_>>());
    assert!(h("127.0.0.2:9000", &[]).contains(&"127.0.0.2:9000".to_string()));
    assert!(h("192.168.1.5:9000", &[]).contains(&"192.168.1.5:9000".to_string()));
    assert!(!h("0.0.0.0:9000", &[]).iter().any(|x| x.starts_with("0.0.0.0")));
    let v6 = h("[::1]:9000", &["LOCALHOST:9000", "lan.example"]);
    assert_eq!(v6, ["127.0.0.1:9000", "localhost:9000", "[::1]:9000", "lan.example"], "{v6:?}");

    let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.2:0").await else {
        eprintln!("skipped: cannot bind 127.0.0.2 here");
        return;
    };
    let srv = Server::start_on(listener, demo(), HttpOptions::default(), Limits::default(), Arc::default());
    let host = format!("127.0.0.2:{}", srv.addr.port());
    let initialize = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
    let r = post(srv.addr, None, &[("Host", host.as_str())], &initialize).await;
    assert_eq!(r.status, 200);
    assert_eq!(post(srv.addr, None, &[("Host", "127.0.0.3:1")], &initialize).await.status, 403);
}

#[test]
fn allowed_origins_and_hosts_are_checked() {
    for (given, want) in [
        ("http://localhost:6274", "http://localhost:6274"),
        (" HTTP://LocalHost:6274/ ", "http://localhost:6274"),
        ("https://app.example.com:443", "https://app.example.com:443"),
        ("http://[::1]:6274", "http://[::1]:6274"),
        ("null", "null"),
    ] {
        assert_eq!(check_allowed_origin(given).map_err(|e| e.to_string()), Ok(want.to_string()), "{given}");
    }
    for bad in [
        "",
        "*",
        "http://localhost:*",
        "http://localhost",
        "https://app.example.com",
        "localhost:6274",
        "ftp://files.example:21",
        "http://user@localhost:6274",
        "http://localhost:6274/app",
        "http://localhost:6274/?x=1",
        "not an origin",
    ] {
        assert!(check_allowed_origin(bad).is_err(), "{bad:?}");
    }
    let e = check_allowed_origin("http://localhost").unwrap_err().to_string();
    assert!(e.contains("http://localhost:80"), "names the port to add: {e}");
    for ok in ["example.com", "example.com:9000", "[::1]:9000", "host.docker.internal:9877", "192.168.1.5:9877"] {
        assert_eq!(check_allowed_host(ok).map_err(|e| e.to_string()), Ok(ok.to_string()));
    }
    for bad in ["", "*", "*.example.com", "a b", "user@host:1", "host:port", "example.com:99999", "http://example.com"] {
        assert!(check_allowed_host(bad).is_err(), "{bad:?}");
    }
}

/// A listed origin (a browser page) gets CORS: its preflight is answered without credentials, and
/// every answer to it carries the CORS headers, errors included. Any other origin gets none.
#[tokio::test(flavor = "multi_thread")]
async fn cors_for_listed_origins_only() {
    let token = "cors-test-token-0123456789-abcdefgh";
    let bearer = format!("Bearer {token}");
    let page = "http://localhost:6274";
    let opts = HttpOptions { token: Some(token.into()), allowed_origins: vec![page.into()], ..HttpOptions::default() };
    let srv = Server::start(demo(), opts).await;
    let no_cors = |r: &Reply| !r.headers.iter().any(|(k, _)| k.starts_with("access-control-"));
    let preflight =
        [("Origin", page), ("Access-Control-Request-Method", "POST"), ("Access-Control-Request-Headers", "content-type, authorization, mcp-param-region")];
    let r = send(srv.addr, "OPTIONS", &preflight, "").await;
    assert_eq!(r.status, 204);
    assert_eq!(r.header("access-control-allow-origin"), Some(page));
    assert_eq!(r.header("access-control-allow-methods"), Some("GET, POST, DELETE"));
    let allowed = r.header("access-control-allow-headers").unwrap_or_default();
    for h in
        ["authorization", "content-type", "accept", "mcp-session-id", "mcp-protocol-version", "mcp-method", "mcp-name", "last-event-id", "mcp-param-region"]
    {
        assert!(allowed.split(", ").any(|a| a == h), "{h} not in {allowed}");
    }
    assert_eq!(r.header("vary"), Some("Origin"));
    // a preflight still needs the right Host
    let mut wrong_host = preflight.to_vec();
    wrong_host.push(("Host", "evil.example"));
    assert_eq!(send(srv.addr, "OPTIONS", &wrong_host, "").await.status, 403);
    // another origin: the token check first, and never a CORS header
    let evil = [("Origin", "http://evil.example"), ("Access-Control-Request-Method", "POST")];
    let r = send(srv.addr, "OPTIONS", &evil, "").await;
    assert!(r.status == 401 && no_cors(&r), "{} {:?}", r.status, r.headers);
    let initialize = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
    let r = post(srv.addr, None, &[("Origin", "http://evil.example"), ("Authorization", &bearer)], &initialize).await;
    assert!(r.status == 403 && no_cors(&r), "{} {:?}", r.status, r.headers);
    let r = post(srv.addr, None, &[("Origin", "http://localhost:6275"), ("Authorization", &bearer)], &initialize).await;
    assert!(r.status == 403 && no_cors(&r), "another port is another origin");
    // the listed page: CORS headers on its answers, the session id readable, errors included
    let r = post(srv.addr, None, &[("Origin", page), ("Authorization", &bearer)], &initialize).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.header("access-control-allow-origin"), Some(page));
    assert!(r.header("access-control-expose-headers").is_some_and(|e| e.contains("Mcp-Session-Id")));
    assert!(r.header("mcp-session-id").is_some());
    let r = post(srv.addr, None, &[("Origin", page)], &initialize).await;
    assert_eq!((r.status, r.header("access-control-allow-origin")), (401, Some(page)));
    // without an Origin it is no preflight: the token, then rmcp's 405
    assert_eq!(send(srv.addr, "OPTIONS", &[], "").await.status, 401);
    assert_eq!(send(srv.addr, "OPTIONS", &[("Authorization", &bearer)], "").await.status, 405);
}

/// Refused requests are logged with the peer's address, at most once per peer and reason every
/// few seconds, and never with the token (right or wrong).
#[tokio::test(flavor = "multi_thread")]
async fn refusals_are_logged_without_credentials() {
    let (log, lines) = log_capture();
    let token = "log-test-token-0123456789-abcdefgh";
    let srv = Server::start(demo(), HttpOptions { token: Some(token.into()), log, ..HttpOptions::default() }).await;
    let initialize = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
    for _ in 0..3 {
        assert_eq!(post(srv.addr, None, &[("Authorization", "Bearer wrong-secret-guess")], &initialize).await.status, 401);
    }
    let bearer = format!("Bearer {token}");
    assert_eq!(post(srv.addr, None, &[("Authorization", &bearer), ("Host", "evil.example")], &initialize).await.status, 403);
    let l = lines.lock().unwrap().join("\n");
    assert_eq!(l.matches("401 Unauthorized").count(), 1, "rate limited: {l}");
    assert!(l.contains("refused POST from 127.0.0.1:") && l.contains("403 Forbidden: Host header is not allowed"), "{l}");
    assert!(!l.contains(token) && !l.contains("wrong-secret-guess"), "credentials leaked: {l}");
}

/// One address cannot take every connection, with or without credentials: past its share (or
/// the total) a connection gets a 503 it can retry, while other addresses still get in. A client
/// that sends nothing, an idle keep-alive connection and a body that never ends are dropped after
/// a timeout.
#[tokio::test(flavor = "multi_thread")]
async fn connections_are_capped_per_peer_and_timed_out() {
    let (log, lines) = log_capture();
    let limits = Limits {
        connections: 5,
        connections_per_peer: 3,
        header_timeout: Duration::from_millis(1500),
        body_timeout: Duration::from_millis(500),
        ..Limits::default()
    };
    let srv = Server::start_limited(demo(), HttpOptions { log, ..HttpOptions::default() }, limits).await;
    let initialize = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}).to_string();
    let t0 = std::time::Instant::now();
    let mut silent = Vec::new();
    for _ in 0..3 {
        silent.push(TcpStream::connect(srv.addr).await.unwrap());
    }
    let refused = read_all(TcpStream::connect(srv.addr).await.unwrap()).await;
    assert!(refused.starts_with("HTTP/1.1 503 ") && refused.contains("Retry-After: 1\r\n") && refused.contains("Connection: close\r\n"), "{refused}");
    assert!(refused.ends_with("too many connections"), "{refused}");
    match connect_from("127.0.0.2", srv.addr).await {
        None => eprintln!("skipped the second-address checks: cannot bind 127.0.0.2 here"),
        Some(s) => {
            // another address is not locked out by the first one's idle connections
            let r = exchange(s, srv.addr, "POST", &[ACCEPT, JSON], initialize.len(), &initialize).await;
            assert_eq!(r.status, 200);
            drop(r);
            tokio::time::sleep(Duration::from_millis(100)).await;
            // two more fill the total of 5: a third address is refused too
            for _ in 0..2 {
                silent.push(connect_from("127.0.0.2", srv.addr).await.unwrap());
            }
            let s = connect_from("127.0.0.3", srv.addr).await.unwrap();
            let refused = read_all(s).await;
            assert!(refused.starts_with("HTTP/1.1 503 "), "{refused}");
            assert!(t0.elapsed() < Duration::from_millis(1400), "the checks ran before the header timeout: {:?}", t0.elapsed());
        }
    }
    // the silent connections are dropped after the header timeout, which frees the share
    for s in &mut silent {
        let mut b = [0u8; 1];
        let n = tokio::time::timeout(WAIT, s.read(&mut b)).await.expect("closed in time").unwrap_or(0);
        assert_eq!(n, 0, "closed without a reply");
    }
    assert!(t0.elapsed() >= Duration::from_millis(1400), "{:?}", t0.elapsed());
    assert_eq!(send(srv.addr, "POST", &[ACCEPT, JSON], &initialize).await.status, 200);
    // a body that never ends: 408
    let r = send_declaring(srv.addr, "POST", &[ACCEPT, JSON], 1000, "{\"jsonrpc\"").await;
    assert_eq!(r.status, 408);
    // an idle keep-alive connection is closed after the header timeout as well
    let mut s = TcpStream::connect(srv.addr).await.unwrap();
    s.write_all(format!("GET / HTTP/1.1\r\nHost: {}\r\n\r\n", srv.host()).as_bytes()).await.unwrap();
    let mut got = Vec::new();
    while !String::from_utf8_lossy(&got).contains("the MCP endpoint is /mcp") {
        let mut b = [0u8; 512];
        let n = tokio::time::timeout(WAIT, s.read(&mut b)).await.expect("a reply in time").unwrap();
        assert!(n > 0, "closed before the reply");
        got.extend_from_slice(b.get(..n).unwrap());
    }
    let idle = std::time::Instant::now();
    let mut b = [0u8; 1];
    assert_eq!(tokio::time::timeout(WAIT, s.read(&mut b)).await.expect("closed in time").unwrap_or(0), 0);
    assert!(idle.elapsed() >= Duration::from_millis(1000), "kept alive for a while: {:?}", idle.elapsed());
    let l = lines.lock().unwrap().join("\n");
    assert!(l.contains("refused a connection from 127.0.0.1:") && l.contains("503: too many connections from this address"), "{l}");
    assert!(l.contains("408 Request Timeout"), "{l}");
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

/// A peer that fails authentication too often is refused for a while (429 with Retry-After),
/// even with the right token, so the token cannot be guessed at speed; it is logged, other peers
/// are not affected, and the block ends.
#[tokio::test(flavor = "multi_thread")]
async fn failed_authentications_get_a_peer_refused_for_a_while() {
    let (log, lines) = log_capture();
    let token = "backoff-test-token-0123456789-abcdef";
    let bearer = format!("Bearer {token}");
    let limits = Limits { auth_failures: 3, auth_block: Duration::from_millis(1500), ..Limits::default() };
    let srv = Server::start_limited(demo(), HttpOptions { token: Some(token.into()), log, ..HttpOptions::default() }, limits).await;
    let initialize = json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}});
    assert_eq!(post(srv.addr, None, &[("Authorization", &bearer)], &initialize).await.status, 200);
    for guess in ["Bearer a", "Bearer b", "Bearer c"] {
        assert_eq!(post(srv.addr, None, &[("Authorization", guess)], &initialize).await.status, 401);
    }
    let r = post(srv.addr, None, &[("Authorization", &bearer)], &initialize).await;
    assert_eq!(r.status, 429, "blocked, right token or not");
    assert!(r.header("retry-after").and_then(|s| s.parse::<u64>().ok()).is_some_and(|s| (1..=2).contains(&s)), "{:?}", r.headers);
    assert!(r.header("mcp-session-id").is_none());
    if let Some(s) = connect_from("127.0.0.2", srv.addr).await {
        let body = initialize.to_string();
        let r = exchange(s, srv.addr, "POST", &[ACCEPT, JSON, ("Authorization", &bearer)], body.len(), &body).await;
        assert_eq!(r.status, 200, "another address is not blocked");
    }
    tokio::time::sleep(Duration::from_millis(1600)).await;
    assert_eq!(post(srv.addr, None, &[("Authorization", &bearer)], &initialize).await.status, 200, "the block ends");
    let l = lines.lock().unwrap().join("\n");
    assert!(l.contains("too many failed authentications from 127.0.0.1: refusing it for a while"), "{l}");
    assert!(l.contains("429 Too Many Requests"), "{l}");
    assert!(!l.contains(token), "{l}");
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
    assert!(check_bind(addr("127.0.0.1:9000"), Some("short")).is_ok(), "a short token is the user's choice on loopback");
    let long = "x".repeat(MIN_REMOTE_TOKEN_LEN);
    for remote in ["0.0.0.0:9000", "[::]:9000", "192.168.1.5:9000"] {
        assert!(check_bind(addr(remote), None).is_err(), "{remote}");
        assert!(check_bind(addr(remote), Some("")).is_err(), "{remote}");
        // reachable from the network: a token too short to resist guessing is refused
        let e = check_bind(addr(remote), Some(&long[1..])).unwrap_err().to_string();
        assert!(e.contains("at least 32 characters") && e.contains("has 31"), "{e}");
        assert!(check_bind(addr(remote), Some("tok")).is_err(), "{remote}");
        assert!(check_bind(addr(remote), Some(&long)).is_ok(), "{remote}");
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
    assert!(gate::bearer_matches(Some(&h("Bearer abc")), b"abc"));
    assert!(gate::bearer_matches(Some(&h("BEARER  abc ")), b"abc"));
    for bad in ["Bearer abd", "Bearer ab", "Bearer abcd", "Bearerabc", "Basic abc", "abc", "Bearer", "Bearer "] {
        assert!(!gate::bearer_matches(Some(&h(bad)), b"abc"), "{bad}");
    }
    assert!(!gate::bearer_matches(None, b"abc"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_non_loopback_bind_needs_a_token() {
    let m = FilmcraftMcp::headless(Session::default());
    let r = m.serve_http("0.0.0.0:0".parse().unwrap(), HttpOptions::default(), async {}).await;
    assert!(matches!(r, Err(AutomationError::BadRequest(_))), "{r:?}");
    let m = FilmcraftMcp::headless(Session::default());
    let r = m.serve_http("0.0.0.0:0".parse().unwrap(), HttpOptions { token: Some("only-20-characters-x".into()), ..HttpOptions::default() }, async {}).await;
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

/// Shutdown cancels a session's export and deletes its partial file, and the port closes as soon
/// as shutdown starts, while the server still waits for open connections.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_deletes_partial_exports_and_closes_the_port_at_once() {
    let dir = std::env::temp_dir().join(format!("filmcraft-mcp-http-shutdown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("partial.mp4");
    let mut srv = Server::start(demo(), HttpOptions::default()).await;
    let (sid, _) = init(srv.addr, &[]).await;
    let mut r = post(srv.addr, Some(&sid), &[], &export_call(1, &out.to_string_lossy(), 30.0, Some(json!("p")), None)).await;
    loop {
        let m = tokio::time::timeout(Duration::from_secs(30), r.next()).await.expect("progress in time").expect("progress");
        assert_ne!(m["id"], 1, "finished before the shutdown: {m}");
        if m["method"] == "notifications/progress" {
            break;
        }
    }
    assert_eq!(srv.mcp.long_calls.count(), 1);
    // a connection that sends nothing keeps the server in its grace period for a while
    let _idle = TcpStream::connect(srv.addr).await.unwrap();
    let _ = srv.stop.take().unwrap().send(());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(TcpStream::connect(srv.addr).await.is_err(), "the port closes when shutdown starts");
    assert!(!srv.done.is_finished(), "still waiting for the open connection");
    let r = tokio::time::timeout(Duration::from_secs(20), &mut srv.done).await.expect("shut down in time").unwrap();
    assert!(r.is_ok(), "{r:?}");
    assert!(!out.exists(), "the partial export is deleted");
    assert_eq!(srv.mcp.long_calls.count(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

fn export_call(id: u64, path: &str, seconds: f64, token: Option<Value>, meta: Option<&Value>) -> Value {
    let params = json!({"path": path, "format": "h264", "width": 640, "height": 360, "audio": false, "range": "custom", "startSeconds": 0, "endSeconds": seconds, "wait": true});
    let mut call = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"command_run","arguments":{"id":"file.exportMedia","params":params}}});
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
