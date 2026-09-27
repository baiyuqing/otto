//! A minimal HTTP/1.1 client for one call, `POST /v1/workspaces`, made by
//! the app itself (never the remote page) after **File > Open Folder…**.
//! `crates/otto/src/server/workspaces.rs`'s `register` handler is an axum
//! `Json<RegisterRequest>` extractor with `#[serde(deny_unknown_fields)]`
//! and one field, `path`; axum's `Json` extractor requires a JSON content
//! type. A raw `TcpStream` request is smaller than pulling in an HTTP client
//! crate for one localhost call.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};

/// The literal request bytes for `POST /v1/workspaces {"path": path}` against
/// `addr`, authorized with `token`. A pure function so the wire format is
/// unit-testable without a socket.
pub fn build_post_request(addr: SocketAddr, token: &str, path: &str) -> String {
    let body = serde_json::json!({ "path": path }).to_string();
    format!(
        "POST /v1/workspaces HTTP/1.1\r\n\
         Host: {addr}\r\n\
         Authorization: Bearer {token}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    )
}

/// Sends [`build_post_request`]'s request over a fresh `TcpStream` to
/// `addr` and returns the response's status code and body. `otto serve`
/// answers this call directly and closes the connection, so reading to EOF
/// is enough; there is no keep-alive to manage.
pub fn post_workspace(addr: SocketAddr, token: &str, path: &str) -> io::Result<(u16, String)> {
    let request = build_post_request(addr, token, path);
    let mut stream = TcpStream::connect(addr)?;
    stream.write_all(request.as_bytes())?;
    stream.flush()?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| io::Error::other("malformed HTTP status line"))?;
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body)?;
    Ok((status, String::from_utf8_lossy(&body).into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_well_formed_post_request() {
        let addr: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        let request = build_post_request(addr, "tok", "/Users/me/project");
        assert!(request.starts_with("POST /v1/workspaces HTTP/1.1\r\n"));
        assert!(request.contains("Host: 127.0.0.1:8787\r\n"));
        assert!(request.contains("Authorization: Bearer tok\r\n"));
        assert!(request.contains("Content-Type: application/json\r\n"));
        assert!(request.contains("Connection: close\r\n"));
        let body = r#"{"path":"/Users/me/project"}"#;
        assert!(request.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(request.ends_with(&format!("\r\n\r\n{body}")));
    }

    #[test]
    fn escapes_a_quote_in_the_path() {
        let addr: SocketAddr = "127.0.0.1:8787".parse().unwrap();
        let request = build_post_request(addr, "tok", "/Users/me/\"weird\"");
        assert!(request.contains(r#"{"path":"/Users/me/\"weird\""}"#));
    }
}
