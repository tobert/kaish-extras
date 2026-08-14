//! A hand-rolled local HTTP server for `kaish-tools-curl`'s functional tests.
//!
//! **Dependency-light on purpose.** `std::net::TcpListener` and
//! `std::os::unix::net::UnixListener`, no HTTP server framework — the same
//! call `kaish-tools-git`'s `tests/support.rs` makes for its git fixtures, so
//! nothing extra enters the published crate's dependency tree. `tempfile` (a
//! dev-dependency already) supplies a throwaway directory for the unix
//! socket path.
//!
//! **What HTTP/1.1 subset this speaks.** Good enough for the requests our
//! own curl tool sends, no more:
//! - Request line, headers until `\r\n\r\n`, then exactly `Content-Length`
//!   bytes of body. No `Transfer-Encoding: chunked`.
//! - No `Expect: 100-continue` handling, which is safe here rather than
//!   merely unlikely. ureq only awaits a `100 Continue` when the *caller*
//!   set the header (`ureq-proto-0.6.1/src/client/prepare.rs:29` gates it on
//!   `has_expect_100()`), and our tool never sets it on its own. Even if an
//!   agent passes `-H Expect:100-continue`, ureq's `await_100` timeout
//!   (1s by default) treats expiry as *not an error* and proceeds to send
//!   the body (`ureq/src/run.rs:465`). So the worst case against this
//!   harness is a bounded one-second stall, never a hang.
//! - No keep-alive. Every accepted connection serves exactly one
//!   request/response and the harness sends `Connection: close` and drops
//!   the socket. A redirect chain is still multiple requests — curl (via
//!   ureq) opens a fresh connection for each hop, which this harness expects.
//! - Route matching is on the raw request-target string (path *and* query
//!   string, exactly as the client sent it) — register routes accordingly.
//!
//! Included with `#[path = "support.rs"] mod support;` in each test binary,
//! not compiled as its own test target (the crate's `Cargo.toml` sets
//! `autotests = false`, the same layout `kaish-tools-git` uses).

#![allow(dead_code)] // each test binary uses a subset of the harness

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};

// ═══════════════════════════════════════════════════════════════════════════
// Captured requests
// ═══════════════════════════════════════════════════════════════════════════

/// One HTTP request the harness received, captured verbatim.
///
/// This is the half of the harness that catches a tool silently dropping a
/// flag: a test asserts on `headers`/`body` directly rather than trusting
/// the response the tool renders back.
#[derive(Debug, Clone)]
pub struct CapturedRequest {
    /// The request method, exactly as sent (e.g. `"GET"`, `"POST"`).
    pub method: String,
    /// The request-target, exactly as sent — including any query string.
    pub path: String,
    /// The HTTP version token from the request line (e.g. `"HTTP/1.1"`).
    pub version: String,
    /// Headers in the order received, duplicates preserved (curl's `-H` can
    /// repeat a header, so a `HashMap` here would lose evidence).
    pub headers: Vec<(String, String)>,
    /// The body, truncated to `Content-Length` bytes.
    pub body: Vec<u8>,
}

impl CapturedRequest {
    /// The first header matching `name`, case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// Every header matching `name`, case-insensitively, in received order.
    pub fn headers_named(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// The body decoded as UTF-8, lossily.
    pub fn body_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// `path` with any query string stripped.
    pub fn path_no_query(&self) -> &str {
        self.path.split('?').next().unwrap_or(&self.path)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Scripted responses
// ═══════════════════════════════════════════════════════════════════════════

/// A scripted response for one route.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    /// A response with `status` and a conventional reason phrase, no
    /// headers, no body.
    pub fn new(status: u16) -> Self {
        Self {
            status,
            reason: default_reason(status).to_string(),
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Override the reason phrase.
    pub fn reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = reason.into();
        self
    }

    /// Append a response header. Repeatable, so `Set-Cookie`-style
    /// multi-value headers are expressible.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set the response body. A `Content-Length` header is added
    /// automatically at write time unless one was set explicitly.
    pub fn body(mut self, body: impl Into<Vec<u8>>) -> Self {
        self.body = body.into();
        self
    }

    /// `status` with a `text/plain` body.
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self::new(status)
            .header("Content-Type", "text/plain; charset=utf-8")
            .body(body.into().into_bytes())
    }

    /// `status` with an `application/json` body. `body` is passed through
    /// verbatim — the caller supplies valid JSON text.
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self::new(status)
            .header("Content-Type", "application/json")
            .body(body.into().into_bytes())
    }

    /// A redirect: `status` (301/302/303/307/308, or any other 3xx a test
    /// wants to probe) with `Location: location`. A route redirecting to
    /// its own path is the cheap way to build a chain that exceeds
    /// `--max-redirs`.
    pub fn redirect(status: u16, location: impl Into<String>) -> Self {
        Self::new(status).header("Location", location.into())
    }

    fn not_found(req: &CapturedRequest) -> Self {
        Self::text(
            404,
            format!(
                "kaish-curl test harness: no route registered for {} {}\n",
                req.method, req.path
            ),
        )
    }
}

fn default_reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Routing
// ═══════════════════════════════════════════════════════════════════════════

/// A route handler: given the request it matched, produce a response.
///
/// A plain [`Response`] registered via [`ServerBuilder::route`] is just a
/// `Responder` that ignores the request and clones a fixed value; a route
/// that needs to read the request first (echo a header back, count hits)
/// registers a closure via [`ServerBuilder::route_fn`].
type Responder = Box<dyn Fn(&CapturedRequest) -> Response + Send + Sync>;

struct Shared {
    routes: HashMap<String, Responder>,
    requests: Mutex<Vec<CapturedRequest>>,
    /// Connection-level errors (bad request framing, a write that failed
    /// mid-response, an `accept` failure). House rule: never silently
    /// discard an error, so a test can assert this stayed empty.
    errors: Mutex<Vec<String>>,
}

impl Shared {
    fn new(routes: HashMap<String, Responder>) -> Self {
        Self {
            routes,
            requests: Mutex::new(Vec::new()),
            errors: Mutex::new(Vec::new()),
        }
    }
}

/// Builds a scripted [`Server`] one route at a time, then starts it over TCP
/// or (on unix) a unix-domain socket.
#[derive(Default)]
pub struct ServerBuilder {
    routes: HashMap<String, Responder>,
}

impl ServerBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fixed response for `path` (the exact request-target,
    /// including any query string).
    pub fn route(self, path: impl Into<String>, response: Response) -> Self {
        self.route_fn(path, move |_req| response.clone())
    }

    /// Register a response computed from the matched request.
    pub fn route_fn<F>(mut self, path: impl Into<String>, f: F) -> Self
    where
        F: Fn(&CapturedRequest) -> Response + Send + Sync + 'static,
    {
        self.routes.insert(path.into(), Box::new(f));
        self
    }

    /// Start serving on `127.0.0.1:0`. The guard reports the assigned port,
    /// so parallel tests never collide on a fixed one.
    pub fn start_tcp(self) -> TcpGuard {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let port = listener.local_addr().expect("listener local_addr").port();
        let shared = Arc::new(Shared::new(self.routes));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = spawn_server(listener, Arc::clone(&shutdown), Arc::clone(&shared));
        TcpGuard {
            port,
            shared,
            shutdown,
            handle: Some(handle),
        }
    }

    /// Start serving on a fresh unix-domain socket under a throwaway
    /// tempdir, for the `--unix-socket` functional-test suite.
    #[cfg(unix)]
    pub fn start_unix(self) -> UnixGuard {
        let socket_dir = tempfile::Builder::new()
            .prefix("kaish-curl-test-")
            .tempdir()
            .expect("tempdir for unix socket");
        let socket_path = socket_dir.path().join("curl-test.sock");
        let listener = UnixListener::bind(&socket_path)
            .unwrap_or_else(|e| panic!("bind unix socket {}: {e}", socket_path.display()));
        let shared = Arc::new(Shared::new(self.routes));
        let shutdown = Arc::new(AtomicBool::new(false));
        let handle = spawn_server(listener, Arc::clone(&shutdown), Arc::clone(&shared));
        UnixGuard {
            socket_path,
            _socket_dir: socket_dir,
            shared,
            shutdown,
            handle: Some(handle),
        }
    }
}

/// Convenience alias: `Server::builder()` reads a little more like the
/// design doc's "small builder... API" than a bare `ServerBuilder::new()`.
pub struct Server;

impl Server {
    pub fn builder() -> ServerBuilder {
        ServerBuilder::new()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// Accept loop, generic over the stream type
// ═══════════════════════════════════════════════════════════════════════════

/// How often the accept loop wakes to recheck the shutdown flag. Bounds how
/// long a guard's `Drop` blocks joining the server thread.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// The maximum size of the header block (request line + headers) the
/// harness will buffer before giving up. Generous for anything curl sends;
/// exists so a malformed client can't make the harness buffer unboundedly.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// The listener half the accept loop needs, implemented once for
/// `TcpListener` and once for `UnixListener` so the loop itself is written
/// once, not duplicated per transport (house rule: no dual representations).
trait Listener: Send + 'static {
    type Stream: Read + Write + Send + 'static;

    fn accept_once(&self) -> io::Result<Self::Stream>;
    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()>;
}

impl Listener for TcpListener {
    type Stream = TcpStream;

    fn accept_once(&self) -> io::Result<TcpStream> {
        self.accept().map(|(stream, _addr)| stream)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        TcpListener::set_nonblocking(self, nonblocking)
    }
}

#[cfg(unix)]
impl Listener for UnixListener {
    type Stream = UnixStream;

    fn accept_once(&self) -> io::Result<UnixStream> {
        self.accept().map(|(stream, _addr)| stream)
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        UnixListener::set_nonblocking(self, nonblocking)
    }
}

fn spawn_server<L: Listener>(
    listener: L,
    shutdown: Arc<AtomicBool>,
    shared: Arc<Shared>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("kaish-curl-test-server".to_string())
        .spawn(move || accept_loop(listener, shutdown, shared))
        .expect("spawn kaish-curl test server thread")
}

/// Poll-accept in a loop: nonblocking `accept`, so the loop can recheck
/// `shutdown` on a short interval instead of blocking forever in `accept()`
/// with no way to wake it. Simpler than a self-connect wakeup trick and
/// bounded by `POLL_INTERVAL`, which is short enough not to slow the suite.
fn accept_loop<L: Listener>(listener: L, shutdown: Arc<AtomicBool>, shared: Arc<Shared>) {
    if let Err(e) = listener.set_nonblocking(true) {
        record_error(&shared, format!("set_nonblocking failed, server thread exiting: {e}"));
        return;
    }
    while !shutdown.load(Ordering::Relaxed) {
        match listener.accept_once() {
            Ok(stream) => {
                if let Err(e) = handle_connection(stream, &shared) {
                    record_error(&shared, e.to_string());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(e) => {
                record_error(&shared, format!("accept failed: {e}"));
                thread::sleep(POLL_INTERVAL);
            }
        }
    }
}

fn record_error(shared: &Shared, message: String) {
    shared
        .errors
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(message);
}

/// Parse one request off `stream`, capture it, route it, and write back the
/// scripted response. One request per connection (see the module doc);
/// the caller closes `stream` when this returns by dropping it.
fn handle_connection<S: Read + Write>(mut stream: S, shared: &Shared) -> io::Result<()> {
    let req = read_request(&mut stream)?;
    let is_head = req.method.eq_ignore_ascii_case("HEAD");
    shared
        .requests
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .push(req.clone());

    let resp = match shared.routes.get(&req.path) {
        Some(responder) => responder(&req),
        None => Response::not_found(&req),
    };
    write_response(&mut stream, &resp, is_head)
}

fn read_request<S: Read>(stream: &mut S) -> io::Result<CapturedRequest> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];

    let header_end = loop {
        if let Some(pos) = find_double_crlf(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request header block exceeded 64KiB",
            ));
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before the request headers completed",
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut body = buf[header_end + 4..].to_vec();

    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .filter(|line| !line.is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "empty request line"))?;
    let mut parts = request_line.split(' ');
    let method = parts
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "request line missing method"))?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "request line missing target")
        })?
        .to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, value) = line.split_once(':').ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed header line: {line:?}"),
            )
        })?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.trim().parse::<usize>())
        .transpose()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("bad Content-Length: {e}")))?
        .unwrap_or(0);

    while body.len() < content_length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed before the request body completed",
            ));
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_length);

    Ok(CapturedRequest {
        method,
        path,
        version,
        headers,
        body,
    })
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn write_response<S: Write>(stream: &mut S, resp: &Response, omit_body: bool) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {} {}\r\n", resp.status, resp.reason);
    let has_content_length = resp
        .headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("content-length"));
    for (name, value) in &resp.headers {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if !has_content_length {
        head.push_str(&format!("Content-Length: {}\r\n", resp.body.len()));
    }
    // No keep-alive (see module doc): tell the client not to reuse this
    // socket, then the caller drops it.
    head.push_str("Connection: close\r\n\r\n");

    stream.write_all(head.as_bytes())?;
    // RFC 7231 4.3.2: a HEAD response reports the headers a GET would have
    // sent, but never a body, regardless of Content-Length.
    if !omit_body {
        stream.write_all(&resp.body)?;
    }
    stream.flush()
}

// ═══════════════════════════════════════════════════════════════════════════
// Guards
// ═══════════════════════════════════════════════════════════════════════════

/// A running TCP server. Dropping it stops the accept loop and joins the
/// thread, so a failing test can't leak a thread or wedge the suite.
pub struct TcpGuard {
    pub port: u16,
    shared: Arc<Shared>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl TcpGuard {
    /// `http://127.0.0.1:<port><path>` — hand `path` a leading `/`.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Every request received so far, in arrival order.
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Every request received so far, removing them — useful when a test
    /// makes several curl calls against one server and wants each call's
    /// assertions isolated from the last.
    pub fn take_requests(&self) -> Vec<CapturedRequest> {
        std::mem::take(
            &mut *self
                .shared
                .requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }

    /// The most recently received request, if any.
    pub fn last_request(&self) -> Option<CapturedRequest> {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last()
            .cloned()
    }

    /// Connection-level errors recorded so far (see [`Shared::errors`]).
    pub fn errors(&self) -> Vec<String> {
        self.shared
            .errors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

impl Drop for TcpGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // Drop can't propagate a Result, so a server-thread panic is the
            // one error this harness can't hand back to the test that
            // triggered it — the narrowest place left to not swallow it
            // silently is stderr, so a wedged harness doesn't read as a
            // clean run.
            if let Err(panic) = handle.join() {
                eprintln!("kaish-curl test harness: server thread panicked: {panic:?}");
            }
        }
    }
}

/// A running unix-domain-socket server, for the `--unix-socket` functional
/// suite. Same shutdown/join contract as [`TcpGuard`].
#[cfg(unix)]
pub struct UnixGuard {
    socket_path: PathBuf,
    /// Keeps the socket's containing directory alive; removed (with the
    /// socket file inside it) once this guard drops, after the server
    /// thread has already stopped.
    _socket_dir: tempfile::TempDir,
    shared: Arc<Shared>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

#[cfg(unix)]
impl UnixGuard {
    /// The bound socket path, for `--unix-socket <path>`.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// A placeholder-host URL for `path`, the same convention curl (and
    /// this build's `--unix-socket`) uses: the host in the URL is never
    /// actually resolved, only the path/query matter once the connection
    /// goes over the socket instead of TCP.
    pub fn url(&self, path: &str) -> String {
        format!("http://localhost{path}")
    }

    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn take_requests(&self) -> Vec<CapturedRequest> {
        std::mem::take(
            &mut *self
                .shared
                .requests
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        )
    }

    pub fn last_request(&self) -> Option<CapturedRequest> {
        self.shared
            .requests
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .last()
            .cloned()
    }

    pub fn errors(&self) -> Vec<String> {
        self.shared
            .errors
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }
}

#[cfg(unix)]
impl Drop for UnixGuard {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // See TcpGuard::drop: Drop can't propagate this, stderr is the
            // narrowest place left that isn't silent.
            if let Err(panic) = handle.join() {
                eprintln!("kaish-curl test harness: server thread panicked: {panic:?}");
            }
        }
    }
}
