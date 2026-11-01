use super::*;
use crate::faults::http::{
    self as testserver, json_response, status_response, unauthorized_response,
};
use serde_json::json;
use std::sync::{Arc, Mutex as StdMutex};

/// Spawns a test server whose `respond` closure can see its own base
/// URL (needed because the URLs served, e.g. `resource_metadata`, embed
/// the server's own address, which is not known until after `spawn`
/// returns).
async fn spawn_self_aware<F>(respond: F) -> (testserver::Server, String)
where
    F: Fn(&str, &testserver::Request) -> String + Send + Sync + 'static,
{
    let base = Arc::new(StdMutex::new(String::new()));
    let base_for_closure = Arc::clone(&base);
    let server = testserver::spawn(move |request| {
        let base = base_for_closure.lock().unwrap().clone();
        respond(&base, request)
    })
    .await;
    *base.lock().unwrap() = server.url.clone();
    let url = server.url.clone();
    (server, url)
}

/// Performs the callback GET the way a browser would, reading
/// `redirect_uri` and `state` off the authorize URL `open` receives.
/// `on_authorize` can assert on the authorize URL's other parameters
/// before the callback fires.
fn browser(
    code: &'static str,
    state_override: Option<&'static str>,
    on_authorize: impl Fn(&reqwest::Url) + Send + Sync + 'static,
) -> Box<auth::login::Opener<'static>> {
    Box::new(move |authorize: &str| {
        let url = reqwest::Url::parse(authorize).map_err(|_| "parse".to_owned())?;
        on_authorize(&url);
        let parameter = |name: &str| {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default()
        };
        let redirect = parameter("redirect_uri");
        let state = state_override
            .map(str::to_owned)
            .unwrap_or_else(|| parameter("state"));
        tokio::spawn(async move {
            let callback =
                reqwest::Url::parse_with_params(&redirect, &[("code", code), ("state", &state)])
                    .unwrap();
            let _ = crate::tls_provider::http_client()
                .get(callback)
                .send()
                .await;
        });
        Ok(())
    })
}

fn never_opens() -> Box<auth::login::Opener<'static>> {
    Box::new(|_: &str| panic!("authorization URL must not be opened"))
}

#[tokio::test]
async fn happy_path_with_dynamic_client_registration() {
    let (server, _url) = spawn_self_aware(|base, request| match request.target.as_str() {
        "/mcp" => unauthorized_response(&format!(
            r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
        )),
        "/.well-known/oauth-protected-resource/mcp" => json_response(
            &json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": [base],
                "scopes_supported": ["mcp:tools"],
            })
            .to_string(),
        ),
        "/.well-known/oauth-authorization-server" => json_response(
            &json!({
                "registration_endpoint": format!("{base}/register"),
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
            })
            .to_string(),
        ),
        "/register" => json_response(&json!({"client_id": "dyn-client"}).to_string()),
        "/token" => json_response(
            &json!({
                "access_token": "tok-1",
                "refresh_token": "ref-1",
                "expires_in": 3600,
            })
            .to_string(),
        ),
        _ => status_response(404, "{}"),
    })
    .await;

    let url = format!("{}/mcp", server.url);
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("server.json");
    let request = LoginRequest {
        server: "server",
        url: &url,
        client_id: None,
        scopes: &[],
        ports: &[0],
        token_path: &token_path,
    };
    login(
        request,
        &CancellationToken::new(),
        browser("the-code", None, |authorize| {
            assert_eq!(
                authorize
                    .query_pairs()
                    .find(|(k, _)| k == "scope")
                    .map(|(_, v)| v.into_owned()),
                Some("mcp:tools".to_owned())
            );
        })
        .as_ref(),
    )
    .await
    .unwrap();

    let saved = load_token(&token_path).unwrap().unwrap();
    assert_eq!(saved.access_token, "tok-1");
    assert_eq!(saved.refresh_token.as_deref(), Some("ref-1"));
    assert_eq!(saved.client_id, "dyn-client");
    assert_eq!(saved.token_endpoint, format!("{}/token", server.url));
    assert_eq!(saved.resource, url);
    assert_eq!(saved.scope.as_deref(), Some("mcp:tools"));
    assert!(saved.expiry.unwrap() > Utc::now().fixed_offset());
}

#[tokio::test]
async fn happy_path_with_a_configured_client_id_and_no_registration_endpoint() {
    let (server, _url) = spawn_self_aware(|base, request| match request.target.as_str() {
        "/mcp" => unauthorized_response(&format!(
            r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
        )),
        "/.well-known/oauth-protected-resource/mcp" => json_response(
            &json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]})
                .to_string(),
        ),
        "/.well-known/oauth-authorization-server" => json_response(
            &json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
            })
            .to_string(),
        ),
        "/token" => json_response(&json!({"access_token": "tok-1"}).to_string()),
        _ => status_response(404, "{}"),
    })
    .await;

    let url = format!("{}/mcp", server.url);
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("server.json");
    let request = LoginRequest {
        server: "server",
        url: &url,
        client_id: Some("configured-client"),
        scopes: &[],
        ports: &[0],
        token_path: &token_path,
    };
    login(
        request,
        &CancellationToken::new(),
        browser("c", None, |_| {}).as_ref(),
    )
    .await
    .unwrap();

    let saved = load_token(&token_path).unwrap().unwrap();
    assert_eq!(saved.client_id, "configured-client");
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.target != "/register")
    );
}

#[tokio::test]
async fn missing_client_id_fails_before_opening_a_browser() {
    let (server, _url) = spawn_self_aware(|base, request| match request.target.as_str() {
        "/mcp" => unauthorized_response(&format!(
            r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
        )),
        "/.well-known/oauth-protected-resource/mcp" => json_response(
            &json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]})
                .to_string(),
        ),
        "/.well-known/oauth-authorization-server" => json_response(
            &json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
            })
            .to_string(),
        ),
        _ => status_response(404, "{}"),
    })
    .await;

    let url = format!("{}/mcp", server.url);
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("server.json");
    let request = LoginRequest {
        server: "server",
        url: &url,
        client_id: None,
        scopes: &[],
        ports: &[0],
        token_path: &token_path,
    };
    let error = login(request, &CancellationToken::new(), never_opens().as_ref())
        .await
        .unwrap_err();
    assert_eq!(error, McpLoginError::MissingClientId);
}

#[tokio::test]
async fn resource_mismatch_fails_before_any_token_request() {
    let (server, _url) = spawn_self_aware(|base, request| match request.target.as_str() {
        "/mcp" => unauthorized_response(&format!(
            r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
        )),
        "/.well-known/oauth-protected-resource/mcp" => json_response(
            &json!({"resource": "https://other.example/mcp", "authorization_servers": [base]})
                .to_string(),
        ),
        _ => status_response(404, "{}"),
    })
    .await;

    let url = format!("{}/mcp", server.url);
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("server.json");
    let request = LoginRequest {
        server: "server",
        url: &url,
        client_id: Some("c"),
        scopes: &[],
        ports: &[0],
        token_path: &token_path,
    };
    let error = login(request, &CancellationToken::new(), never_opens().as_ref())
        .await
        .unwrap_err();
    assert_eq!(error, McpLoginError::ResourceMismatch);
    assert!(!token_path.exists());
}

#[tokio::test]
async fn a_non_https_non_loopback_authorization_server_fails() {
    let (server, _url) = spawn_self_aware(|base, request| match request.target.as_str() {
        "/mcp" => unauthorized_response(&format!(
            r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
        )),
        "/.well-known/oauth-protected-resource/mcp" => json_response(
            &json!({
                "resource": format!("{base}/mcp"),
                "authorization_servers": ["http://evil.example.com"],
            })
            .to_string(),
        ),
        _ => status_response(404, "{}"),
    })
    .await;

    let url = format!("{}/mcp", server.url);
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("server.json");
    let request = LoginRequest {
        server: "server",
        url: &url,
        client_id: Some("c"),
        scopes: &[],
        ports: &[0],
        token_path: &token_path,
    };
    let error = login(request, &CancellationToken::new(), never_opens().as_ref())
        .await
        .unwrap_err();
    assert_eq!(error, McpLoginError::InsecureEndpoint);
}

#[tokio::test]
async fn a_state_mismatch_on_callback_fails_without_calling_the_token_endpoint() {
    let (server, _url) = spawn_self_aware(|base, request| match request.target.as_str() {
        "/mcp" => unauthorized_response(&format!(
            r#"Bearer resource_metadata="{base}/.well-known/oauth-protected-resource/mcp""#
        )),
        "/.well-known/oauth-protected-resource/mcp" => json_response(
            &json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]})
                .to_string(),
        ),
        "/.well-known/oauth-authorization-server" => json_response(
            &json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
            })
            .to_string(),
        ),
        "/token" => json_response(&json!({"access_token": "leaked"}).to_string()),
        _ => status_response(404, "{}"),
    })
    .await;

    let url = format!("{}/mcp", server.url);
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("server.json");
    let request = LoginRequest {
        server: "server",
        url: &url,
        client_id: Some("c"),
        scopes: &[],
        ports: &[0],
        token_path: &token_path,
    };
    let error = login(
        request,
        &CancellationToken::new(),
        browser("code", Some("WRONG"), |_| {}).as_ref(),
    )
    .await
    .unwrap_err();
    assert_eq!(error, McpLoginError::StateMismatch);
    assert!(
        server
            .requests()
            .iter()
            .all(|request| request.target != "/token")
    );
}

#[tokio::test]
async fn protected_resource_metadata_falls_back_to_well_known_paths() {
    let (server, _url) = spawn_self_aware(|base, request| match request.target.as_str() {
        // No `resource_metadata` in the challenge: the client must try
        // the path-suffixed well-known URL, then the bare one.
        "/mcp" => unauthorized_response("Bearer realm=\"mcp\""),
        "/.well-known/oauth-protected-resource/mcp" => status_response(404, "{}"),
        "/.well-known/oauth-protected-resource" => json_response(
            &json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]})
                .to_string(),
        ),
        "/.well-known/oauth-authorization-server" => json_response(
            &json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
            })
            .to_string(),
        ),
        "/token" => json_response(&json!({"access_token": "tok"}).to_string()),
        _ => status_response(404, "{}"),
    })
    .await;

    let url = format!("{}/mcp", server.url);
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("server.json");
    let request = LoginRequest {
        server: "server",
        url: &url,
        client_id: Some("c"),
        scopes: &[],
        ports: &[0],
        token_path: &token_path,
    };
    login(
        request,
        &CancellationToken::new(),
        browser("code", None, |_| {}).as_ref(),
    )
    .await
    .unwrap();

    let targets: Vec<_> = server
        .requests()
        .iter()
        .map(|request| request.target.clone())
        .collect();
    assert!(targets.contains(&"/.well-known/oauth-protected-resource/mcp".to_owned()));
    assert!(targets.contains(&"/.well-known/oauth-protected-resource".to_owned()));
}

#[tokio::test]
async fn scope_selection_prefers_configured_then_advertised_then_challenge() {
    async fn authorize_scope(
        configured: &[String],
        advertised_scopes: &[&str],
        challenge_scope: Option<&str>,
    ) -> Option<String> {
        let challenge_header = match challenge_scope {
            Some(scope) => format!(
                r#"Bearer resource_metadata="{{base}}/.well-known/oauth-protected-resource/mcp", scope="{scope}""#
            ),
            None => "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""
                .to_owned(),
        };
        let advertised: Vec<String> = advertised_scopes.iter().map(|s| s.to_string()).collect();
        let (server, _url) = spawn_self_aware(move |base, request| {
            let header = challenge_header.replace("{base}", base);
            match request.target.as_str() {
                "/mcp" => unauthorized_response(&header),
                "/.well-known/oauth-protected-resource/mcp" => json_response(
                    &json!({
                        "resource": format!("{base}/mcp"),
                        "authorization_servers": [base],
                        "scopes_supported": advertised,
                    })
                    .to_string(),
                ),
                "/.well-known/oauth-authorization-server" => json_response(
                    &json!({
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                    })
                    .to_string(),
                ),
                "/token" => json_response(&json!({"access_token": "tok"}).to_string()),
                _ => status_response(404, "{}"),
            }
        })
        .await;

        let url = format!("{}/mcp", server.url);
        let directory = tempfile::tempdir().unwrap();
        let token_path = directory.path().join("server.json");
        let request = LoginRequest {
            server: "server",
            url: &url,
            client_id: Some("c"),
            scopes: configured,
            ports: &[0],
            token_path: &token_path,
        };
        login(
            request,
            &CancellationToken::new(),
            browser("code", None, |_| {}).as_ref(),
        )
        .await
        .unwrap();
        load_token(&token_path).unwrap().unwrap().scope
    }

    assert_eq!(
        authorize_scope(
            &["configured:scope".to_owned()],
            &["advertised:scope"],
            Some("challenge:scope")
        )
        .await,
        Some("configured:scope".to_owned())
    );
    assert_eq!(
        authorize_scope(&[], &["advertised:scope"], Some("challenge:scope")).await,
        Some("advertised:scope".to_owned())
    );
    assert_eq!(
        authorize_scope(&[], &[], Some("challenge:scope")).await,
        Some("challenge:scope".to_owned())
    );
    assert_eq!(authorize_scope(&[], &[], None).await, None);
}

#[tokio::test]
async fn token_store_bearer_without_a_file_needs_login() {
    let directory = tempfile::tempdir().unwrap();
    let store = TokenStore::new("server".to_owned(), directory.path().join("absent.json"));
    let error = store.bearer(&CancellationToken::new()).await.unwrap_err();
    assert_eq!(error, CallError::NeedsLogin);
    assert!(store.secrets().is_empty());
}

#[tokio::test]
async fn refresh_on_expiry_persists_the_new_file() {
    let server = testserver::spawn(|_| {
        json_response(
            &json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600,
            })
            .to_string(),
        )
    })
    .await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("server.json");
    let stale = TokenFile {
        access_token: "old-access".to_owned(),
        refresh_token: Some("old-refresh".to_owned()),
        expiry: Some(Utc::now().fixed_offset() - chrono::TimeDelta::seconds(1)),
        client_id: "client".to_owned(),
        token_endpoint: format!("{}/token", server.url),
        resource: format!("{}/mcp", server.url),
        scope: None,
    };
    save_token(&path, &stale).unwrap();

    let store = TokenStore::new("server".to_owned(), path.clone());
    let token = store.bearer(&CancellationToken::new()).await.unwrap();
    assert_eq!(token, "new-access");

    let saved = load_token(&path).unwrap().unwrap();
    assert_eq!(saved.access_token, "new-access");
    assert_eq!(saved.refresh_token.as_deref(), Some("new-refresh"));
    assert!(saved.expiry.unwrap() > Utc::now().fixed_offset());

    let secrets = store.secrets();
    assert!(secrets.contains(&"new-access".to_owned()));
    assert!(secrets.contains(&"new-refresh".to_owned()));
}

#[tokio::test]
async fn a_failed_refresh_needs_login() {
    let server = testserver::spawn(|_| status_response(400, "{}")).await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("server.json");
    let stale = TokenFile {
        access_token: "old-access".to_owned(),
        refresh_token: Some("old-refresh".to_owned()),
        expiry: Some(Utc::now().fixed_offset() - chrono::TimeDelta::seconds(1)),
        client_id: "client".to_owned(),
        token_endpoint: format!("{}/token", server.url),
        resource: format!("{}/mcp", server.url),
        scope: None,
    };
    save_token(&path, &stale).unwrap();

    let store = TokenStore::new("server".to_owned(), path.clone());
    let error = store.bearer(&CancellationToken::new()).await.unwrap_err();
    assert_eq!(error, CallError::NeedsLogin);
    // The stale file on disk is left untouched by a failed refresh.
    assert_eq!(
        load_token(&path).unwrap().unwrap().access_token,
        "old-access"
    );
}

#[tokio::test]
async fn concurrent_refresh_posts_only_once() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls_clone = Arc::clone(&calls);
    let server = testserver::spawn(move |_req| {
        calls_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        json_response(
            &json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600,
            })
            .to_string(),
        )
    })
    .await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("server.json");
    let stale = TokenFile {
        access_token: "old-access".to_owned(),
        refresh_token: Some("old-refresh".to_owned()),
        expiry: Some(Utc::now().fixed_offset() - chrono::TimeDelta::seconds(1)),
        client_id: "client".to_owned(),
        token_endpoint: format!("{}/token", server.url),
        resource: format!("{}/mcp", server.url),
        scope: None,
    };
    save_token(&path, &stale).unwrap();

    let store = TokenStore::new("server".to_owned(), path.clone());
    let cancel = CancellationToken::new();
    let (first, second) = tokio::join!(store.bearer(&cancel), store.bearer(&cancel));
    assert_eq!(first.unwrap(), "new-access");
    assert_eq!(second.unwrap(), "new-access");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn refresh_after_401_posts_even_when_token_is_not_expiring() {
    // A server can reject an access token before its locally recorded
    // expiry (revocation, clock skew). `refresh` must still spend the
    // refresh token when the caller names the token that was rejected,
    // rather than trusting the stored expiry.
    let server = testserver::spawn(|_req| {
        json_response(
            &json!({
                "access_token": "new-access",
                "refresh_token": "new-refresh",
                "expires_in": 3600,
            })
            .to_string(),
        )
    })
    .await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("server.json");
    let current = TokenFile {
        access_token: "old-access".to_owned(),
        refresh_token: Some("old-refresh".to_owned()),
        expiry: Some(Utc::now().fixed_offset() + chrono::TimeDelta::hours(1)),
        client_id: "client".to_owned(),
        token_endpoint: format!("{}/token", server.url),
        resource: format!("{}/mcp", server.url),
        scope: None,
    };
    save_token(&path, &current).unwrap();

    let store = TokenStore::new("server".to_owned(), path.clone());
    let cancel = CancellationToken::new();
    let refreshed = store.refresh("old-access", &cancel).await.unwrap();
    assert_eq!(refreshed, "new-access");
    assert_eq!(server.count(), 1);
}

#[tokio::test]
async fn refresh_skips_post_when_stored_token_already_differs() {
    // If the stored access token no longer matches the one the caller
    // says was rejected, another task already refreshed it; reuse that
    // result instead of spending the refresh token again.
    let server = testserver::spawn(|_req| {
        json_response(
            &json!({
                "access_token": "should-not-be-issued",
                "expires_in": 3600,
            })
            .to_string(),
        )
    })
    .await;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("server.json");
    let current = TokenFile {
        access_token: "current-access".to_owned(),
        refresh_token: Some("current-refresh".to_owned()),
        expiry: Some(Utc::now().fixed_offset() + chrono::TimeDelta::hours(1)),
        client_id: "client".to_owned(),
        token_endpoint: format!("{}/token", server.url),
        resource: format!("{}/mcp", server.url),
        scope: None,
    };
    save_token(&path, &current).unwrap();

    let store = TokenStore::new("server".to_owned(), path.clone());
    let cancel = CancellationToken::new();
    let result = store.refresh("some-other-token", &cancel).await.unwrap();
    assert_eq!(result, "current-access");
    assert_eq!(server.count(), 0);
}

#[tokio::test]
async fn discovery_ignores_a_cross_origin_resource_metadata_url() {
    // A hostile HTTPS server names an attacker-controlled origin in the
    // 401 challenge; discovery must ignore it and fall back to the
    // well-known path under the configured server's own origin instead
    // of ever contacting the attacker's URL.
    let attacker = testserver::spawn(|_req| {
        json_response(
            &json!({
                "resource": "https://stolen.example/mcp",
                "authorization_servers": ["https://stolen.example"],
            })
            .to_string(),
        )
    })
    .await;
    let attacker_url = attacker.url.clone();

    let (server, _url) = spawn_self_aware(move |base, request| match request.target.as_str() {
        "/mcp" => unauthorized_response(&format!(
            r#"Bearer resource_metadata="{attacker_url}/steal""#
        )),
        "/.well-known/oauth-protected-resource/mcp" => json_response(
            &json!({"resource": format!("{base}/mcp"), "authorization_servers": [base]})
                .to_string(),
        ),
        "/.well-known/oauth-authorization-server" => json_response(
            &json!({
                "authorization_endpoint": format!("{base}/authorize"),
                "token_endpoint": format!("{base}/token"),
            })
            .to_string(),
        ),
        "/token" => json_response(&json!({"access_token": "tok"}).to_string()),
        _ => status_response(404, "{}"),
    })
    .await;

    let url = format!("{}/mcp", server.url);
    let directory = tempfile::tempdir().unwrap();
    let token_path = directory.path().join("server.json");
    let request = LoginRequest {
        server: "server",
        url: &url,
        client_id: Some("c"),
        scopes: &[],
        ports: &[0],
        token_path: &token_path,
    };
    login(
        request,
        &CancellationToken::new(),
        browser("code", None, |_| {}).as_ref(),
    )
    .await
    .unwrap();

    assert_eq!(attacker.count(), 0);
    let targets: Vec<_> = server
        .requests()
        .iter()
        .map(|request| request.target.clone())
        .collect();
    assert!(targets.contains(&"/.well-known/oauth-protected-resource/mcp".to_owned()));
}

#[tokio::test]
async fn logout_reports_whether_a_file_was_removed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("server.json");
    assert!(!logout(&path).unwrap());
    save_token(
        &path,
        &TokenFile {
            access_token: "a".to_owned(),
            refresh_token: None,
            expiry: None,
            client_id: "c".to_owned(),
            token_endpoint: "https://auth.example/token".to_owned(),
            resource: "https://mcp.example/mcp".to_owned(),
            scope: None,
        },
    )
    .unwrap();
    assert!(logout(&path).unwrap());
    assert!(!path.exists());
}
