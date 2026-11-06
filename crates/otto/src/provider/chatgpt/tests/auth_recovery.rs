//! One-time rejected-access-token recovery tests.

use super::*;

async fn rejected_token_source(
    endpoint: &testserver::Server,
    credentials_path: std::path::PathBuf,
) -> TokenSource {
    TokenSource::with_endpoint(
        crate::auth::oauth::Endpoint {
            authorize_url: format!("{}/authorize", endpoint.url),
            token_url: format!("{}/token", endpoint.url),
        },
        Credentials {
            access_token: "access-old-secret".into(),
            refresh_token: "refresh-old-secret".into(),
            account_id: "acct-1".into(),
            ..Credentials::default()
        },
        credentials_path,
        CancellationToken::new(),
    )
}

#[tokio::test]
async fn unauthorized_request_refreshes_and_replays_same_request_once() {
    let request_count = std::sync::atomic::AtomicUsize::new(0);
    let server = testserver::spawn(move |request| {
        if request.target.starts_with("/token") {
            return testserver::json_response(
                r#"{"access_token":"access-new-secret","refresh_token":"refresh-new-secret","expires_in":3600}"#,
            );
        }
        let attempt = request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if attempt == 0 {
            testserver::status_response(401, "ignored unauthorized response")
        } else if request.header("Authorization") == "Bearer access-new-secret" {
            testserver::sse_response(CANNED_STREAM)
        } else {
            testserver::status_response(401, "unexpected token")
        }
    })
    .await;
    let directory = tempfile::tempdir().unwrap();
    let tokens = rejected_token_source(&server, directory.path().join("chatgpt.json")).await;
    let client = trusted_test_client(&server.url, tokens, "acct-1");
    let (settlement, events) = complete(&client, &model_request()).await;
    assert!(settlement.result.is_ok());
    assert_eq!(settlement.attempts, 2);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Completed
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Retry { .. }))
    );
    let requests = server.requests();
    let response_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.target == "/responses")
        .collect();
    assert_eq!(response_requests.len(), 2);
    assert_eq!(response_requests[0].body, response_requests[1].body);
    assert_eq!(
        response_requests[0].header("Authorization"),
        "Bearer access-old-secret"
    );
    assert_eq!(
        response_requests[1].header("Authorization"),
        "Bearer access-new-secret"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/token")
            .count(),
        1
    );
}

#[tokio::test]
async fn unauthorized_refresh_failure_returns_fixed_authorization_error() {
    let server = testserver::spawn(|request| {
        if request.target == "/token" {
            testserver::status_response(400, "secret token endpoint detail")
        } else {
            testserver::status_response(401, "secret provider detail")
        }
    })
    .await;
    let directory = tempfile::tempdir().unwrap();
    let tokens = rejected_token_source(&server, directory.path().join("chatgpt.json")).await;
    let client = trusted_test_client(&server.url, tokens, "acct-1");
    let (settlement, events) = complete(&client, &model_request()).await;
    assert_eq!(settlement.attempts, 1);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
    assert_eq!(
        settlement.result.unwrap_err().to_string(),
        "chatgpt authorization failed; run 'otto login'"
    );
    assert!(events.is_empty());
    assert_eq!(server.count(), 2);
}

#[tokio::test]
async fn unauthorized_recovery_is_attempted_only_once() {
    let server = testserver::spawn(|request| {
        if request.target == "/token" {
            testserver::json_response(
                r#"{"access_token":"access-new-secret","refresh_token":"refresh-new-secret","expires_in":3600}"#,
            )
        } else {
            testserver::status_response(401, "secret provider detail")
        }
    })
    .await;
    let directory = tempfile::tempdir().unwrap();
    let tokens = rejected_token_source(&server, directory.path().join("chatgpt.json")).await;
    let client = trusted_test_client(&server.url, tokens, "acct-1");
    let (settlement, events) = complete(&client, &model_request()).await;
    assert_eq!(settlement.attempts, 2);
    assert_eq!(
        settlement.outcome.effect_certainty,
        EffectCertainty::Unknown
    );
    assert_eq!(
        settlement.result.unwrap_err().to_string(),
        "chatgpt authorization failed; run 'otto login'"
    );
    assert!(events.is_empty());
    let requests = server.requests();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/responses")
            .count(),
        2
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.target == "/token")
            .count(),
        1
    );
}

#[tokio::test]
async fn endpoint_rejection_prevents_refresh_network_and_credential_writes() {
    let server = testserver::spawn(|_| Vec::<u8>::new()).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("credentials.json");
    let tokens = TokenSource::with_endpoint(
        crate::auth::oauth::Endpoint {
            authorize_url: format!("{}/authorize", server.url),
            token_url: format!("{}/token", server.url),
        },
        Credentials {
            expiry: (chrono::Utc::now() - chrono::TimeDelta::minutes(1)).fixed_offset(),
            ..Credentials::default()
        },
        path.clone(),
        CancellationToken::new(),
    );
    let client = Client::with_base_url(&server.url, tokens, "fixture-account");
    let (settlement, events) = complete(&client, &model_request()).await;
    assert!(settlement.result.is_err());
    assert_eq!(settlement.attempts, 0);
    assert!(events.is_empty());
    assert!(client.catalog(&CancellationToken::new()).await.is_err());
    assert_eq!(server.count(), 0);
    assert!(!path.exists());
}
