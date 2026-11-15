//! A scripted OpenAI-compatible origin server for tests that drive a whole
//! agent turn offline: the built binary, [`super::runtime_session`] or a controller.
//!
//! It runs on [`crate::faults::http`], a plain HTTP/1.1 listener on 127.0.0.1,
//! so the tests need no credentials and reach no external network.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::faults::http;

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

/// Two tool calls in one reply, matching how the provider streams a turn
/// that starts a sub-agent and runs a second tool in the same turn.
pub fn two_tool_call_reply(
    id1: &str,
    name1: &str,
    arguments1: &str,
    id2: &str,
    name2: &str,
    arguments2: &str,
) -> String {
    let arguments1 = serde_json::to_string(arguments1).expect("encode arguments1");
    let arguments2 = serde_json::to_string(arguments2).expect("encode arguments2");
    format!(
        "data: \
            {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{id1}\",\
            \"type\":\"function\",\"function\":{{\"name\":\"{name1}\",\"arguments\":{arguments1}}}}\
            },{{\"index\":1,\"id\":\"{id2}\",\"type\":\"function\",\
            \"function\":{{\"name\":\"{name2}\",\"arguments\":{arguments2}}}}}]}}}}]}}\n\n\
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

/// Serves `script` on an ephemeral loopback port and returns its base URL
/// and the raw request bodies received so far, in arrival order.
///
/// The server runs on its own thread and runtime, which end with the process;
/// a test binary that fails early therefore never blocks on it, and callers
/// need no async runtime.
pub fn serve(script: Script) -> (String, Arc<Mutex<Vec<String>>>) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let (url_sender, url_receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build the server runtime");
        runtime.block_on(async move {
            let server = http::spawn(move |request| {
                captured.lock().unwrap().push(request.body.clone());
                http::sse_response(&next_reply(&script, &request.body))
            })
            .await;
            url_sender.send(server.url.clone()).expect("send the URL");
            std::future::pending::<()>().await;
        });
    });
    let base_url = url_receiver.recv().expect("the server binds");
    (base_url, requests)
}

/// The next scripted reply for the request `body`.
fn next_reply(script: &Script, body: &str) -> String {
    let index = script.served.fetch_add(1, Ordering::SeqCst);
    let mut reply = script
        .replies
        .get(index)
        .cloned()
        .unwrap_or_else(|| text_reply("out of script"));
    // Tests that revoke approvals must use the exact service-generated
    // ID advertised to the model, never assume a sequential ID format.
    if reply.contains("$EXACT_APPROVAL_ID") {
        let request: serde_json::Value = serde_json::from_str(body).expect("provider request");
        let system = request["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .find(|message| message["role"] == "system")
            .expect("system message")["content"]
            .as_str()
            .expect("system prompt")
            .to_string();
        let id = system
            .split("Only withdraw request ")
            .nth(1)
            .expect("bound approval")
            .split('.')
            .next()
            .expect("approval ID");
        reply = reply.replace("$EXACT_APPROVAL_ID", id);
    }
    reply
}

/// A compaction summary that passes the structure check, with `note` under
/// every heading.
pub fn summary_reply(note: &str) -> String {
    let body = [
        "## Goal",
        "## Constraints & Preferences",
        "## Observations",
        "## Progress",
        "### Done",
        "### In Progress",
        "### Blocked",
        "## Key Decisions",
        "## Next Steps",
        "## Critical Context",
    ]
    .iter()
    .map(|heading| format!("{heading}\n{note}\n"))
    .collect::<Vec<_>>()
    .join("\n");
    text_reply(&body)
}
