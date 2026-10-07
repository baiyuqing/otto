//! A minimal loopback HTTP origin for the auth, provider and MCP tests.
//!
//! There is no equivalent in this crate's dependency set, and adding an HTTP
//! server for tests alone is not worth it, so the bytes these tests need are
//! written directly onto the socket. The server never leaves loopback and
//! serves canned responses only.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// One request the server received.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// The request target from the request line, e.g. `/oauth/token`.
    pub target: String,
    /// The request line and every header, verbatim.
    pub head: String,
    /// The decoded request body.
    pub body: String,
}

impl Request {
    /// The value of header `name`, matched case-insensitively, or the empty
    /// string when the request did not carry it.
    pub fn header(&self, name: &str) -> String {
        self.head
            .lines()
            .skip(1)
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case(name)
                    .then(|| value.trim().to_owned())
            })
            .unwrap_or_default()
    }

    /// The value of `name` in an `application/x-www-form-urlencoded` body.
    pub fn form(&self, name: &str) -> String {
        form_urlencoded_pairs(&self.body)
            .into_iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value)
            .unwrap_or_default()
    }
}

pub struct Server {
    pub url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl Server {
    /// Every request received, in arrival order.
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

/// Starts a loopback server that answers every request with `respond(request)`,
/// which returns the complete raw HTTP response. An empty response closes the
/// connection without writing anything.
pub async fn spawn<F, R>(respond: F) -> Server
where
    F: Fn(&Request) -> R + Send + Sync + 'static,
    R: Into<Vec<u8>>,
{
    let listener = bind().await;
    let respond = Arc::new(respond);
    serve(listener, move |mut stream, request| {
        let respond = Arc::clone(&respond);
        async move {
            let response = respond(&request).into();
            if !response.is_empty() {
                let _ = stream.write_all(&response).await;
                let _ = stream.flush().await;
            }
            let _ = stream.shutdown().await;
        }
    })
}

async fn bind() -> TcpListener {
    TcpListener::bind(("127.0.0.1", 0)).await.unwrap()
}

/// Accepts connections on `listener`; each runs in its own task, which reads
/// one request, records it and hands the stream to `handle`. A connection
/// that closes before it sends a complete request is not recorded.
fn serve<H, Fut>(listener: TcpListener, handle: H) -> Server
where
    H: Fn(TcpStream, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let handle = Arc::new(handle);
    tokio::spawn(accept_loop(listener, recorded, handle));
    Server { url, requests }
}

async fn accept_loop<H, Fut>(
    listener: TcpListener,
    recorded: Arc<Mutex<Vec<Request>>>,
    handle: Arc<H>,
) where
    H: Fn(TcpStream, Request) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    while let Ok((mut stream, _)) = listener.accept().await {
        let recorded = Arc::clone(&recorded);
        let handle = Arc::clone(&handle);
        tokio::spawn(async move {
            let Some(request) = read_request(&mut stream).await else {
                return;
            };
            recorded.lock().unwrap().push(request.clone());
            handle(stream, request).await;
        });
    }
}

/// Reads the head, then exactly `Content-Length` body bytes. `None` when the
/// peer closes or fails before the head is complete.
async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let head_end = loop {
        if let Some(position) = find(&buffer, b"\r\n\r\n") {
            break position + 4;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .unwrap_or_default()
        .to_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buffer.len() < head_end + length {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
    let body = String::from_utf8_lossy(&buffer[head_end..]).to_string();
    Some(Request { target, head, body })
}

/// A complete 200 response carrying `body` as a `text/event-stream`.
pub fn sse_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A 200 response that promises `declared` more bytes than it sends, so the
/// client sees the body cut short mid-stream.
pub fn truncated_sse_response(body: &str, declared: usize) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n{body}"
    )
}

/// A complete 200 response carrying `body` as JSON.
pub fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A complete response with `status` and `body`.
pub fn status_response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A redirect to `location` with the given 3xx status.
pub fn redirect_response(status: u16, location: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

/// A 401 response carrying `header` as its `WWW-Authenticate` value. Used by
/// `mcp::oauth`'s discovery tests to simulate the initial unauthorized probe.
pub fn unauthorized_response(header: &str) -> String {
    format!(
        "HTTP/1.1 401 X\r\nWWW-Authenticate: {header}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn form_urlencoded_pairs(body: &str) -> Vec<(String, String)> {
    reqwest::Url::parse(&format!("http://x/?{body}"))
        .map(|url| {
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

/// A loopback address that accepts connections and then never answers, so a
/// caller blocks until it is cancelled. Returns the base URL; the accepted
/// sockets are held by the background task for the life of the test.
pub async fn spawn_hanging() -> String {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// One action on an accepted connection, after its request is read.
#[derive(Debug, Clone)]
pub enum Step {
    /// The status line and headers. The framing header follows from the
    /// script: `Content-Length` when it holds a [`Step::DeclareLength`],
    /// otherwise `Transfer-Encoding: chunked`. `Connection: close` is added.
    Head {
        status: u16,
        headers: Vec<(String, String)>,
    },
    /// The `Content-Length` of the head. Writes nothing itself.
    DeclareLength(u64),
    /// Raw bytes, for `Content-Length` bodies.
    Body(Vec<u8>),
    /// One chunk of a chunked body.
    Chunk(Vec<u8>),
    /// `data: <payload>\n\n` as one chunk.
    Sse(String),
    Flush,
    /// Waits; `None` waits until the client gives up.
    Stall(Option<Duration>),
    /// Ends the response: writes the chunked terminator when the head is
    /// chunked, then sends FIN.
    Close,
    /// Closes with `SO_LINGER(0)`, so the peer sees a TCP reset.
    Reset,
}

/// The steps one connection executes, in order. A script that ends without
/// [`Step::Close`], [`Step::Reset`] or a stall drops the socket: FIN with no
/// chunked terminator.
#[derive(Debug, Clone, Default)]
pub struct Script(Vec<Step>);

impl Script {
    pub fn new(steps: Vec<Step>) -> Self {
        Self(steps)
    }

    pub fn then(mut self, step: Step) -> Self {
        self.0.push(step);
        self
    }

    fn head(status: u16, content_type: &str) -> Step {
        Step::Head {
            status,
            headers: vec![("Content-Type".into(), content_type.into())],
        }
    }

    fn sse_head() -> Step {
        Self::head(200, "text/event-stream")
    }

    /// A chunked `text/event-stream` carrying `events` (JSON payloads) and
    /// then `[DONE]`, ended cleanly.
    pub fn sse_done(events: &[&str]) -> Self {
        let mut steps = vec![Self::sse_head()];
        steps.extend(events.iter().map(|event| Step::Sse((*event).into())));
        steps.push(Step::Sse("[DONE]".into()));
        steps.push(Step::Close);
        Self(steps)
    }

    /// Like [`Script::sse_done`] without the `[DONE]` event.
    pub fn sse_without_done(events: &[&str]) -> Self {
        let mut steps = vec![Self::sse_head()];
        steps.extend(events.iter().map(|event| Step::Sse((*event).into())));
        steps.push(Step::Close);
        Self(steps)
    }

    /// Sends the first `n` of `events`, then resets the connection.
    pub fn sse_cut_after(events: &[&str], n: usize) -> Self {
        let mut steps = vec![Self::sse_head()];
        steps.extend(events.iter().take(n).map(|e| Step::Sse((*e).into())));
        steps.push(Step::Reset);
        Self(steps)
    }

    /// An event whose payload is not JSON, then a clean end.
    pub fn malformed_event() -> Self {
        Self::sse_without_done(&["{not json"])
    }

    /// A complete response with `status` and a JSON `body`.
    pub fn status(code: u16, body: &str) -> Self {
        Self(vec![
            Self::head(code, "application/json"),
            Step::DeclareLength(body.len() as u64),
            Step::Body(body.as_bytes().to_vec()),
            Step::Close,
        ])
    }

    /// Accepts the request and never answers.
    pub fn stall_before_head() -> Self {
        Self(vec![Step::Stall(None)])
    }

    /// Sends the head of an event stream, then nothing.
    pub fn stall_after_head() -> Self {
        Self(vec![Self::sse_head(), Step::Stall(None)])
    }

    /// A 200 that declares `declared` bytes and sends `body`, then closes, so
    /// the client sees the body end early.
    pub fn truncated(body: &str, declared: usize) -> Self {
        Self(vec![
            Self::head(200, "text/event-stream"),
            Step::DeclareLength(declared as u64),
            Step::Body(body.as_bytes().to_vec()),
            Step::Close,
        ])
    }

    /// The script for a connection the test did not plan for.
    fn unplanned() -> Self {
        Self::status(
            500,
            "fault server: the client opened more connections than the test scripted",
        )
    }

    async fn run(self, stream: &mut TcpStream) {
        let declared = self.0.iter().find_map(|step| match step {
            Step::DeclareLength(length) => Some(*length),
            _ => None,
        });
        for step in self.0 {
            let written = match step {
                Step::Head { status, headers } => {
                    let mut head = format!("HTTP/1.1 {status} X\r\n");
                    for (name, value) in headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    match declared {
                        Some(length) => head.push_str(&format!("Content-Length: {length}\r\n")),
                        None => head.push_str("Transfer-Encoding: chunked\r\n"),
                    }
                    head.push_str("Connection: close\r\n\r\n");
                    stream.write_all(head.as_bytes()).await
                }
                Step::DeclareLength(_) => Ok(()),
                Step::Body(bytes) => stream.write_all(&bytes).await,
                Step::Chunk(bytes) => write_chunk(stream, &bytes).await,
                Step::Sse(payload) => {
                    write_chunk(stream, format!("data: {payload}\n\n").as_bytes()).await
                }
                Step::Flush => stream.flush().await,
                Step::Stall(Some(duration)) => {
                    tokio::time::sleep(duration).await;
                    Ok(())
                }
                Step::Stall(None) => std::future::pending().await,
                Step::Close => {
                    if declared.is_none() {
                        let _ = stream.write_all(b"0\r\n\r\n").await;
                    }
                    let _ = stream.shutdown().await;
                    return;
                }
                Step::Reset => {
                    reset(stream);
                    return;
                }
            };
            if written.is_err() {
                return;
            }
        }
    }
}

/// Makes the close of `stream` send RST instead of FIN: `SO_LINGER` on with
/// a zero timeout. tokio deprecates `set_linger`, so the option is set here.
#[allow(unsafe_code)]
fn reset(stream: &TcpStream) {
    use std::os::fd::AsRawFd;
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: the descriptor is open for the borrow of `stream`, and the
    // pointer and length describe `linger`.
    let status = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(status, 0, "setsockopt(SO_LINGER) failed");
}

async fn write_chunk(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    stream
        .write_all(format!("{:x}\r\n", bytes.len()).as_bytes())
        .await?;
    stream.write_all(bytes).await?;
    stream.write_all(b"\r\n").await
}

/// Starts a loopback server that runs one script per accepted connection, in
/// accept order. A connection that closes before it sends a request does not
/// consume a script; a connection beyond the list gets a 500 whose body names
/// the test as the cause.
pub async fn spawn_scripted(scripts: Vec<Script>) -> Server {
    serve(bind().await, script_handler(scripts))
}

fn script_handler(
    scripts: Vec<Script>,
) -> impl Fn(TcpStream, Request) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
    let scripts = Arc::new(Mutex::new(VecDeque::from(scripts)));
    move |mut stream, _| {
        let script = scripts.lock().unwrap().pop_front();
        Box::pin(async move {
            script
                .unwrap_or_else(Script::unplanned)
                .run(&mut stream)
                .await;
        })
    }
}

/// A loopback port nothing listens on, so a connection to it is refused
/// (`reqwest::Error::is_connect`).
pub fn refused_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

/// Like [`spawn_scripted`] on a port that starts listening only after
/// `after`; connections before then are refused. `port` comes from
/// [`refused_port`].
pub fn late_listener(port: u16, after: Duration, scripts: Vec<Script>) -> Server {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let handle = Arc::new(script_handler(scripts));
    tokio::spawn(async move {
        tokio::time::sleep(after).await;
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        accept_loop(listener, recorded, handle).await;
    });
    Server {
        url: format!("http://127.0.0.1:{port}"),
        requests,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(url: &str) -> reqwest::Result<(u16, String)> {
        let response = reqwest::Client::new().post(url).body("ping").send().await?;
        let status = response.status().as_u16();
        Ok((status, response.text().await?))
    }

    #[tokio::test]
    async fn a_chunked_script_streams_events_and_ends_cleanly() {
        let server = spawn_scripted(vec![Script::sse_done(&["{\"a\":1}"])]).await;
        let (status, body) = get(&server.url).await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(body, "data: {\"a\":1}\n\ndata: [DONE]\n\n");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].body, "ping");
    }

    #[tokio::test]
    async fn a_script_without_done_ends_cleanly_before_it() {
        let server = spawn_scripted(vec![Script::sse_without_done(&["{}"])]).await;
        let (_, body) = get(&server.url).await.unwrap();
        assert_eq!(body, "data: {}\n\n");
    }

    #[tokio::test]
    async fn a_content_length_script_sends_the_body_and_status() {
        let server = spawn_scripted(vec![Script::status(429, "slow down")]).await;
        assert_eq!(get(&server.url).await.unwrap(), (429, "slow down".into()));
    }

    #[tokio::test]
    async fn a_truncated_script_fails_the_body_read() {
        let server = spawn_scripted(vec![Script::truncated("data: {}\n\n", 4096)]).await;
        let error = get(&server.url).await.unwrap_err();
        assert!(error.is_body() || error.is_decode(), "{error:?}");
    }

    #[tokio::test]
    async fn a_reset_after_the_head_fails_the_body_read() {
        let server = spawn_scripted(vec![Script::sse_cut_after(&["{}"], 0)]).await;
        let error = get(&server.url).await.unwrap_err();
        assert!(error.is_body() || error.is_decode(), "{error:?}");
    }

    #[tokio::test]
    async fn a_reset_is_not_a_clean_end_of_a_chunked_body() {
        // The chunked terminator is only written by `Close`; after a reset
        // the client must not see a complete body, even with data sent.
        let server = spawn_scripted(vec![Script::sse_cut_after(&["{}"], 1)]).await;
        assert!(get(&server.url).await.is_err());
    }

    #[tokio::test]
    async fn a_stall_before_the_head_leaves_the_request_recorded() {
        let server = spawn_scripted(vec![Script::stall_before_head()]).await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let error = client.post(&server.url).body("x").send().await.unwrap_err();
        assert!(error.is_timeout(), "{error:?}");
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn a_connection_beyond_the_scripts_names_the_test_bug() {
        let server = spawn_scripted(vec![Script::status(200, "{}")]).await;
        get(&server.url).await.unwrap();
        let (status, body) = get(&server.url).await.unwrap();
        assert_eq!(status, 500);
        assert!(body.contains("more connections than the test scripted"));
        assert_eq!(server.count(), 2);
    }

    #[tokio::test]
    async fn a_port_with_no_listener_refuses_until_the_late_listener_binds() {
        let port = refused_port();
        let server = late_listener(
            port,
            Duration::from_millis(300),
            vec![Script::status(200, "up")],
        );
        let error = get(&server.url).await.unwrap_err();
        assert!(error.is_connect(), "{error:?}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(get(&server.url).await.unwrap(), (200, "up".into()));
        assert_eq!(server.count(), 1);
    }
}
