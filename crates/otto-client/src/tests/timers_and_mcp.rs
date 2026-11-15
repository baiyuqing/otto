//! Tests for session timer and MCP status calls.

use super::*;

const TIMER: &str = r#"{"id":"t1","fire_at":"2026-11-08T10:00:00Z","message":"stretch"}"#;

#[tokio::test]
async fn timers_lists_and_cancels_through_the_session_routes() {
    let (client, seen, _dir) = stub(|_, target| {
        if target.ends_with("/timers") {
            json_body(StatusCode::OK, &format!(r#"{{"timers":[{TIMER}]}}"#))
        } else if target.contains("/nope/") {
            json_body(
                StatusCode::NOT_FOUND,
                r#"{"error":{"code":"not_found","message":"timer not found"}}"#,
            )
        } else {
            json_body(StatusCode::OK, TIMER)
        }
    });
    let timers = client.timers("s1").await.unwrap();
    assert_eq!(
        (timers[0].id.as_str(), timers[0].message.as_str()),
        ("t1", "stretch")
    );
    assert_eq!(client.cancel_timer("s1", "t1").await.unwrap().id, "t1");
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/timers");
    assert_eq!(request(&seen, 1).0, "POST /v1/sessions/s1/timers/t1/cancel");
    let error = client.cancel_timer("s1", "nope").await.unwrap_err();
    assert!(error.to_string().contains("timer not found"), "{error}");
}

#[tokio::test]
async fn mcp_servers_decodes_the_wire_rows() {
    let body = r#"{"servers":[{"name":"docs","transport":"http","protocol_version":"2026-07-28",
        "state":"connected","tools":3,"error":null}]}"#;
    let (client, seen, _dir) = stub(move |_, _| json_body(StatusCode::OK, body));
    let servers = client.mcp_servers("s1").await.unwrap();
    assert_eq!(request(&seen, 0).0, "GET /v1/sessions/s1/mcp");
    assert_eq!(
        (
            servers[0].name.as_str(),
            servers[0].state.as_str(),
            servers[0].tools
        ),
        ("docs", "connected", 3)
    );
}
