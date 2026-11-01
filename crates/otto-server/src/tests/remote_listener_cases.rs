//! Token gating and peer logging for a non-loopback TCP listener.

use super::*;

const TEST_TOKEN: &str = "secret";

fn harness_with(remote_listener: bool) -> Harness {
    Harness::with(HarnessOptions {
        token: TEST_TOKEN.to_string(),
        remote_listener,
        ..HarnessOptions::default()
    })
}

#[tokio::test]
async fn a_remote_listener_gates_metrics_and_unmatched_paths_but_not_healthz() {
    let harness = harness_with(true);
    for path in ["/metrics", "/no-such-path", "/v1/info"] {
        assert_eq!(
            harness.send("GET", path, None).await.status,
            StatusCode::UNAUTHORIZED,
            "{path} must need a token"
        );
    }
    assert_eq!(
        harness.send("GET", "/healthz", None).await.status,
        StatusCode::OK
    );
    for path in ["/metrics", "/v1/info"] {
        let reply = harness
            .send_with("GET", path, None, &[("authorization", b"Bearer secret")])
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{path} with the token");
    }
}

#[tokio::test]
async fn a_loopback_listener_still_serves_metrics_without_a_token() {
    let harness = harness_with(false);
    assert_eq!(
        harness.send("GET", "/metrics", None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_socket_router_skips_the_gate_on_a_remote_listener() {
    let harness = harness_with(true);
    assert_eq!(
        harness.send_on_socket("/metrics").await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_peer_address_is_logged_when_present() {
    let harness = harness_with(false);
    let peer: std::net::SocketAddr = "203.0.113.9:4455".parse().expect("address");
    let request = Request::builder()
        .method("GET")
        .uri("/healthz")
        .extension(axum::extract::ConnectInfo(crate::listen::PeerAddress(peer)))
        .body(Body::empty())
        .expect("request");
    let response = harness
        .router
        .clone()
        .oneshot(request)
        .await
        .expect("infallible");
    assert_eq!(response.status(), StatusCode::OK);
    let logged = harness.logged();
    let line = logged
        .lines()
        .find(|line| line.contains("msg=http_request"))
        .expect("an http_request line");
    assert!(line.contains("peer=203.0.113.9:4455"), "{line}");
}
