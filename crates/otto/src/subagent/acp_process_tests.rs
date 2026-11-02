use super::*;
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn runs_agent_protocol_and_reaps_the_adapter() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adapter");
    std::fs::write(&path, r##"#!/bin/sh
read -r initialize
printf '%s\n' '{"id":1,"result":{"protocolVersion":1}}'
read -r create
printf '%s\n' '{"id":2,"result":{"sessionId":"s1"}}'
read -r prompt
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"done"}}}}'
printf '%s\n' '{"id":3,"result":{"stopReason":"end_turn"}}'
while read -r line; do :; done
"##).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut events = Vec::new();
    let result = run(
        &path,
        dir.path(),
        Vec::new(),
        "test",
        &CancellationToken::new(),
        None,
        |event| events.push(event),
    )
    .await
    .unwrap();
    assert_eq!(result["stopReason"], "end_turn");
    assert_eq!(events[0]["update"]["content"]["text"], "done");
}

#[tokio::test]
#[ignore = "requires an installed codex-acp and authenticated live Codex"]
async fn live_codex_prompt() {
    let environment: Vec<String> = std::env::vars_os()
        .filter_map(|(key, value)| {
            Some(format!(
                "{}={}",
                key.into_string().ok()?,
                value.into_string().ok()?
            ))
        })
        .collect();
    let mut output = String::new();
    let cancel = CancellationToken::new();
    let result = tokio::time::timeout(
        Duration::from_secs(90),
        run(
            Path::new("codex-acp"),
            &std::env::current_dir().unwrap(),
            environment,
            "Reply exactly ACP connection works. Do not use tools.",
            &cancel,
            None,
            |event| {
                if let Some(text) = event["update"]["content"]["text"].as_str() {
                    output.push_str(text);
                }
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["stopReason"], "end_turn");
    assert!(output.contains("ACP connection works"));
}

#[tokio::test]
async fn cancellation_reaches_agent_and_waits_for_its_response() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adapter");
    std::fs::write(&path, r##"#!/bin/sh
read -r line
printf '%s\n' '{"id":1,"result":{"protocolVersion":1}}'
read -r line
printf '%s\n' '{"id":2,"result":{"sessionId":"s1"}}'
read -r line
printf '%s\n' '{"method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"started"}}}}'
read -r line
case "$line" in *session/cancel*) printf '%s\n' '{"id":3,"result":{"stopReason":"cancelled"}}';; *) exit 1;; esac
while read -r line; do :; done
"##).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cancel = CancellationToken::new();
    let on_update = cancel.clone();
    let result = run(&path, dir.path(), Vec::new(), "test", &cancel, None, |_| {
        on_update.cancel()
    })
    .await
    .unwrap();
    assert_eq!(result["stopReason"], "cancelled");
}
