//! Tests for the model catalog and its cache.

use super::*;

#[tokio::test]
async fn catalog_uses_subscription_auth_and_official_models_shape() {
    let server = testserver::spawn(|_| testserver::json_response( r#"{"models":[{"slug":"fixture-text","visibility":"list","input_modalities":["text"],"supported_in_api":false,"description":"catalog-access-secret"}]}"#)).await;
    let client = Client::with_base_url(
        &server.url,
        static_tokens("catalog-access-secret"),
        "catalog-account-secret",
    );
    let catalog = client.catalog(&CancellationToken::new()).await.unwrap();
    assert_eq!(catalog.models[0].id, "fixture-text");
    assert_eq!(catalog.models[0].metadata["description"], "[REDACTED]");
    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target, "/models?client_version=0.163.0");
    assert_eq!(
        requests[0].header("authorization"),
        "Bearer catalog-access-secret"
    );
    assert_eq!(
        requests[0].header("chatgpt-account-id"),
        "catalog-account-secret"
    );
}

#[tokio::test]
async fn catalog_cache_survives_client_restart_and_rejects_stale_or_other_accounts() {
    let server =
        testserver::spawn(|_| testserver::json_response(r#"{"models":[{"slug":"fixture-text"}]}"#))
            .await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache/models.json");
    let client = |account: &str| {
        Client::with_base_url(&server.url, static_tokens("access"), account)
            .with_catalog_cache(path.clone())
    };
    assert_eq!(
        client("account-a")
            .catalog(&CancellationToken::new())
            .await
            .unwrap()
            .models[0]
            .id,
        "fixture-text"
    );
    assert_eq!(server.count(), 1);
    client("account-a")
        .catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(server.count(), 1, "fresh JSON cache should skip HTTP");

    let mut entry: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    entry["fetched_at"] = 0.into();
    std::fs::write(&path, serde_json::to_vec(&entry).unwrap()).unwrap();
    client("account-a")
        .catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(server.count(), 2, "expired JSON should refresh");

    client("account-b")
        .catalog(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(server.count(), 3, "another account must refresh");
}

#[tokio::test]
async fn empty_catalog_is_not_persisted_and_rejection_invalidates_cache() {
    let empty = testserver::spawn(|_| testserver::json_response(r#"{"models":[]}"#)).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache/models.json");
    let client = Client::with_base_url(&empty.url, static_tokens("access"), "account")
        .with_catalog_cache(path.clone());
    client.catalog(&CancellationToken::new()).await.unwrap();
    client.catalog(&CancellationToken::new()).await.unwrap();
    assert_eq!(empty.count(), 2);
    assert!(!path.exists());

    let server = testserver::spawn(|request| {
        if request.target.starts_with("/models") {
            testserver::json_response(r#"{"models":[{"slug":"fixture-text"}]}"#)
        } else {
            testserver::status_response(404, "unavailable")
        }
    })
    .await;
    let client = Client::with_base_url(&server.url, static_tokens("access"), "account")
        .with_catalog_cache(path.clone());
    client.catalog(&CancellationToken::new()).await.unwrap();
    assert!(path.exists());
    client
        .complete(&model_request(), &mut |_| {}, &CancellationToken::new())
        .await;
    assert!(!path.exists(), "rejected model should invalidate cache");
}

#[tokio::test]
async fn catalog_missing_auth_and_pre_cancel_are_safe() {
    let client = Client::with_base_url("http://127.0.0.1:1", static_tokens(""), "acct");
    assert!(
        client
            .catalog(&CancellationToken::new())
            .await
            .unwrap_err()
            .to_string()
            .contains("otto login")
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        client.catalog(&cancel).await,
        Err(ProviderError::Cancelled)
    ));
}
