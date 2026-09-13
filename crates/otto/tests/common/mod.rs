//! A scripted OpenAI-compatible origin server shared by the binary tests.
//!
//! It is a plain HTTP/1.1 listener on 127.0.0.1, so the tests that drive the
//! built binary need no credentials and reach no external network.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// One scripted SSE reply per model turn.
pub struct Script {
    pub replies: Vec<String>,
    pub served: Arc<AtomicUsize>,
}

pub fn tool_call_reply(id: &str, name: &str, arguments: &str) -> String {
    let arguments = serde_json::to_string(arguments).expect("encode arguments");
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id}\",\"type\":\"function\",\"function\":{{\"name\":\"{name}\",\"arguments\":{arguments}}}}}]}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

pub fn text_reply(text: &str) -> String {
    let text = serde_json::to_string(text).expect("encode text");
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{text}}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
         data: [DONE]\n\n"
    )
}

/// Serves `script` on an ephemeral loopback port and returns its base URL.
///
/// The listener thread ends when the process does; a test binary that fails
/// early therefore never blocks on it.
pub fn serve(script: Script) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
    let base_url = format!("http://{}", listener.local_addr().expect("local address"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut raw = Vec::new();
            let mut buffer = [0u8; 4096];
            let head_end = loop {
                if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
                    break index + 4;
                }
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break 0,
                    Ok(read) => raw.extend_from_slice(&buffer[..read]),
                }
            };
            if head_end == 0 {
                continue;
            }
            let head = String::from_utf8_lossy(&raw[..head_end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut body = raw[head_end..].to_vec();
            while body.len() < length {
                match stream.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => body.extend_from_slice(&buffer[..read]),
                }
            }
            let index = script.served.fetch_add(1, Ordering::SeqCst);
            let reply = script
                .replies
                .get(index)
                .cloned()
                .unwrap_or_else(|| text_reply("out of script"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                reply.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
        }
    });
    base_url
}
