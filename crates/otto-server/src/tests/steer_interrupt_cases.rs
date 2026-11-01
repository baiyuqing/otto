//! `POST /v1/sessions/{id}/tasks/steer/interrupt`.

use super::*;

/// Fails without the route: the router answers 404 or 405 and the body has no
/// `no_queued_guidance` code.
#[tokio::test]
async fn an_idle_session_refuses_a_step_interrupt_with_409() {
    let harness = Harness::new();
    let id = harness.create().await;
    let reply = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/tasks/steer/interrupt"),
            None,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "no_queued_guidance");
}

#[tokio::test]
async fn an_unknown_session_answers_404_to_a_step_interrupt() {
    let harness = Harness::new();
    let reply = harness
        .send("POST", "/v1/sessions/missing/tasks/steer/interrupt", None)
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND, "{}", reply.body);
}
