//! A minimal loopback HTTP origin for the auth and ChatGPT provider tests.
//!
//! Go's tests use `net/http/httptest`. There is no equivalent in this crate's
//! dependency set, and adding an HTTP server for tests alone is not worth it,
//! so the handful of bytes these tests need are written directly onto the
//! socket. The server never leaves loopback and serves canned responses only.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request the server received.
#[derive(Debug, Clone, Default)]
pub(crate) struct Request {
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

pub(crate) struct TestServer {
    pub url: String,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl TestServer {
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

/// Starts a loopback server that answers every request with `respond(request)`,
/// which returns the complete raw HTTP response.
pub(crate) async fn spawn<F>(respond: F) -> TestServer
where
    F: Fn(&Request) -> String + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&requests);
    let respond = Arc::new(respond);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            // Read the head, then exactly Content-Length body bytes.
            let head_end = loop {
                match stream.read(&mut chunk).await {
                    Ok(0) => break buffer.len(),
                    Ok(read) => {
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(position) = find(&buffer, b"\r\n\r\n") {
                            break position + 4;
                        }
                    }
                    Err(_) => break buffer.len(),
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
                    line.strip_prefix("content-length: ")
                        .or_else(|| line.strip_prefix("Content-Length: "))
                })
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            while buffer.len() < head_end + length {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let body = String::from_utf8_lossy(&buffer[head_end..]).to_string();
            let request = Request {
                target,
                head: head.clone(),
                body,
            };
            recorded.lock().unwrap().push(request.clone());
            let response = respond(&request);
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });
    TestServer {
        url: format!("http://127.0.0.1:{port}"),
        requests,
    }
}

/// A complete 200 response carrying `body` as a `text/event-stream`.
pub(crate) fn sse_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A 200 response that promises `declared` more bytes than it sends, so the
/// client sees the body cut short mid-stream.
pub(crate) fn truncated_sse_response(body: &str, declared: usize) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {declared}\r\nConnection: close\r\n\r\n{body}"
    )
}

/// A complete 200 response carrying `body` as JSON.
pub(crate) fn json_response(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A complete response with `status` and `body`.
pub(crate) fn status_response(status: u16, body: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A redirect to `location` with the given 3xx status.
pub(crate) fn redirect_response(status: u16, location: &str) -> String {
    format!(
        "HTTP/1.1 {status} X\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

/// A 401 response carrying `header` as its `WWW-Authenticate` value. Used by
/// `mcp::oauth`'s discovery tests to simulate the initial unauthorized probe.
pub(crate) fn unauthorized_response(header: &str) -> String {
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
pub(crate) async fn spawn_hanging() -> String {
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
