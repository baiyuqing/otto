//! OAuth 2.1 authorization for HTTP MCP servers: discovery, dynamic client
//! registration, the PKCE authorization code flow, and the on-disk token
//! store. Design: `docs/specs/2026-09-19-mcp-design.md`, "OAuth 2.1
//! authorization" and "Security".
//!
//! Reuses `crate::auth`'s PKCE primitives, loopback callback listener, and
//! atomic secret-file writer rather than duplicating them; the pieces that
//! differ from the ChatGPT sign-in flow are discovery (RFC 9728 protected
//! resource metadata, RFC 8414 authorization server metadata, RFC 7591
//! dynamic client registration) and a token endpoint that is not fixed to
//! one provider, so the authorize-URL and token-exchange helpers here are
//! generic over `client_id` and the discovered endpoints.
//!
//! Ownership: one [`TokenStore`] per server, held behind `Arc` by the
//! server's [`super::http::HttpTransport`]. `bearer` and `refresh` take
//! `&self`; the cached token is behind a `std::sync::Mutex` held only for the
//! duration of a read or write, never across an `.await`. Errors: every
//! failure collapses to a fieldless [`McpLoginError`] (for `login`/`logout`)
//! or [`super::CallError`] (for [`BearerSource`]); no token, code, or
//! endpoint response body is ever formatted into one.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::auth;

use super::{BearerSource, CallError};

/// Every discovery, registration, and token request is bounded by this
/// timeout, independent of the transport's own `connect_timeout_secs`.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Failure modes of `otto mcp login`/`logout` and the discovery chain.
/// Fieldless, following `auth::LoginError`: no query value, endpoint body, or
/// token can escape through a rendered message.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpLoginError {
    #[error("no loopback port available")]
    NoLoopbackPort,
    #[error("open authorization URL")]
    OpenFailed,
    #[error("context canceled")]
    Cancelled,
    #[error("oauth discovery failed")]
    Discovery,
    #[error("resource mismatch")]
    ResourceMismatch,
    #[error("insecure oauth endpoint")]
    InsecureEndpoint,
    #[error("dynamic client registration failed")]
    Registration,
    #[error("server requires a client_id (set oauth_client_id)")]
    MissingClientId,
    #[error("callback server")]
    CallbackServer,
    #[error("state mismatch on callback")]
    StateMismatch,
    #[error("authorization code exchange failed")]
    Exchange,
    #[error("token could not be saved")]
    Persistence,
}

/// The persisted state of one server's OAuth grant. Written 0600 in a 0700
/// directory by [`auth::write_secret_file`]; loaded at runner start and on
/// every [`TokenStore::bearer`] call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenFile {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expiry: Option<DateTime<FixedOffset>>,
    pub client_id: String,
    pub token_endpoint: String,
    pub resource: String,
    pub scope: Option<String>,
}

/// `<home>/.otto/auth/mcp/<server>.json`, next to the ChatGPT credential file
/// (`auth::path_for_home`) but one directory per server rather than one file.
pub fn token_path(home: &Path, server: &str) -> PathBuf {
    home.join(".otto")
        .join("auth")
        .join("mcp")
        .join(format!("{server}.json"))
}

/// Loads the token file at `path`. `Ok(None)` when it does not exist; every
/// other failure (oversized, malformed) is [`McpLoginError::Persistence`].
pub fn load_token(path: &Path) -> Result<Option<TokenFile>, McpLoginError> {
    let data = match std::fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(McpLoginError::Persistence),
    };
    if data.len() > auth::MAX_CREDENTIAL_FILE_BYTES {
        return Err(McpLoginError::Persistence);
    }
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|_| McpLoginError::Persistence)
}

fn save_token(path: &Path, file: &TokenFile) -> Result<(), McpLoginError> {
    let data = serde_json::to_vec_pretty(file).map_err(|_| McpLoginError::Persistence)?;
    auth::write_secret_file(path, &data).map_err(|_| McpLoginError::Persistence)
}

/// Removes the token file at `path`. `Ok(false)` when it was already absent.
pub fn logout(path: &Path) -> Result<bool, McpLoginError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(McpLoginError::Persistence),
    }
}

/// One `otto mcp login <server>` invocation's inputs.
pub struct LoginRequest<'a> {
    pub server: &'a str,
    /// The server's configured `url`; also the OAuth `resource`.
    pub url: &'a str,
    /// The configured `oauth_client_id`, used only when the authorization
    /// server offers no dynamic registration.
    pub client_id: Option<&'a str>,
    /// The configured `oauth_scopes`, tried before the server's advertised
    /// scopes.
    pub scopes: &'a [String],
    /// Candidate loopback ports for the callback listener.
    pub ports: &'a [u16],
    pub token_path: &'a Path,
}

/// Runs discovery, dynamic registration (if offered), the PKCE authorization
/// code flow, and persists the resulting token. Port of the "OAuth 2.1
/// authorization" section of the design doc.
pub async fn login(
    request: LoginRequest<'_>,
    cancel: &CancellationToken,
    open: &auth::login::Opener<'_>,
) -> Result<(), McpLoginError> {
    let _ = request.server;
    let client = auth::oauth::http_client().map_err(|_| McpLoginError::Discovery)?;

    let (resource_metadata, header_scope) =
        discover_protected_resource(&client, request.url).await?;
    let resource = check_resource(&resource_metadata.resource, request.url)?;
    let server_metadata = discover_authorization_server(&client, &resource_metadata).await?;
    let client_id = resolve_client_id(&client, &server_metadata, request.client_id).await?;

    let (listener, port) = auth::oauth::listen_loopback(request.ports)
        .await
        .map_err(|_| McpLoginError::NoLoopbackPort)?;
    let redirect_uri = format!("http://localhost:{port}/auth/callback");
    let verifier = auth::oauth::generate_verifier().map_err(|_| McpLoginError::CallbackServer)?;
    let state = auth::oauth::random_state().map_err(|_| McpLoginError::CallbackServer)?;
    let challenge = auth::oauth::s256_challenge(&verifier);
    let scope = select_scope(
        request.scopes,
        &resource_metadata.scopes_supported,
        header_scope.as_deref(),
    );
    let authorize = authorize_url(
        &server_metadata.authorization_endpoint,
        &client_id,
        &redirect_uri,
        &state,
        &challenge,
        scope.as_deref(),
        &resource,
    )?;

    open(&authorize).map_err(|_| McpLoginError::OpenFailed)?;

    let code = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(McpLoginError::Cancelled),
        result = auth::login::serve_callback(&listener, &state) => map_callback_error(result)?,
    };
    drop(listener);
    if cancel.is_cancelled() {
        return Err(McpLoginError::Cancelled);
    }

    let token = exchange_code(
        &client,
        &server_metadata.token_endpoint,
        &client_id,
        &redirect_uri,
        &code,
        &verifier,
        &resource,
    )
    .await
    .map_err(|_| McpLoginError::Exchange)?;

    save_token(
        request.token_path,
        &TokenFile {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expiry: token.expiry,
            client_id,
            token_endpoint: server_metadata.token_endpoint,
            resource,
            scope,
        },
    )
}

fn map_callback_error(
    result: Result<String, auth::login::LoginError>,
) -> Result<String, McpLoginError> {
    result.map_err(|error| match error {
        auth::login::LoginError::StateMismatch => McpLoginError::StateMismatch,
        auth::login::LoginError::Cancelled => McpLoginError::Cancelled,
        _ => McpLoginError::CallbackServer,
    })
}

/// Reuses a discovered grant to fetch bearer tokens for one server. Loads
/// its token file lazily: `new` does no I/O, and the first `bearer`,
/// `refresh`, or `secrets` call reads the file into the cache.
pub struct TokenStore {
    server: String,
    path: PathBuf,
    http: Result<reqwest::Client, String>,
    cached: Mutex<Option<TokenFile>>,
    /// Serializes refresh attempts so two concurrent callers near expiry
    /// cannot both spend the same rotating refresh token. Held across the
    /// whole refresh (load, POST, save); a waiter re-checks the cached
    /// token after acquiring it in case another task already refreshed.
    refresh_lock: tokio::sync::Mutex<()>,
}

impl TokenStore {
    pub fn new(server: String, path: PathBuf) -> Self {
        Self {
            server,
            path,
            http: auth::oauth::http_client(),
            cached: Mutex::new(None),
            refresh_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// The configured server name this store was built for.
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Reads the cached token, loading the file on first access.
    fn loaded(&self) -> Result<Option<TokenFile>, CallError> {
        let mut guard = self.cached.lock().expect("mcp token cache lock");
        if guard.is_none() {
            *guard = load_token(&self.path).map_err(|_| CallError::NeedsLogin)?;
        }
        Ok(guard.clone())
    }
}

#[async_trait::async_trait]
impl BearerSource for TokenStore {
    async fn bearer(&self, cancel: &CancellationToken) -> Result<String, CallError> {
        let token = self.loaded()?.ok_or(CallError::NeedsLogin)?;
        let expiring = token.expiry.is_some_and(|expiry| {
            expiry <= Utc::now().fixed_offset() + chrono::TimeDelta::seconds(60)
        });
        if expiring {
            return self.refresh(&token.access_token, cancel).await;
        }
        Ok(token.access_token)
    }

    async fn refresh(
        &self,
        rejected: &str,
        cancel: &CancellationToken,
    ) -> Result<String, CallError> {
        let _guard = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CallError::Cancelled),
            guard = self.refresh_lock.lock() => guard,
        };

        // Another task may have refreshed this token while this call waited
        // for the lock. Reuse its result instead of spending the refresh
        // token (which a rotating authorization server accepts only once)
        // a second time.
        if let Some(token) = self.loaded()?
            && token.access_token != rejected
        {
            return Ok(token.access_token);
        }

        let token = self.loaded()?.ok_or(CallError::NeedsLogin)?;
        let refresh_token = token.refresh_token.clone().ok_or(CallError::NeedsLogin)?;
        let client = self.http.as_ref().map_err(|_| CallError::NeedsLogin)?;
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", token.client_id.as_str()),
            ("resource", token.resource.as_str()),
        ];
        let refreshed = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(CallError::Cancelled),
            result = post_token(client, &token.token_endpoint, &form) => {
                result.map_err(|_| CallError::NeedsLogin)?
            }
        };
        let updated = TokenFile {
            access_token: refreshed.access_token,
            refresh_token: refreshed.refresh_token.or(Some(refresh_token)),
            expiry: refreshed.expiry,
            client_id: token.client_id,
            token_endpoint: token.token_endpoint,
            resource: token.resource,
            scope: token.scope,
        };
        save_token(&self.path, &updated).map_err(|_| CallError::NeedsLogin)?;
        *self.cached.lock().expect("mcp token cache lock") = Some(updated.clone());
        Ok(updated.access_token)
    }

    fn secrets(&self) -> Vec<String> {
        match self.loaded().ok().flatten() {
            Some(token) => {
                let mut secrets = vec![token.access_token];
                secrets.extend(token.refresh_token);
                secrets
            }
            None => Vec::new(),
        }
    }
}

// ---- discovery -------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProtectedResourceMetadata {
    resource: String,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AuthorizationServerMetadata {
    #[serde(default)]
    registration_endpoint: Option<String>,
    authorization_endpoint: String,
    token_endpoint: String,
}

#[derive(Debug, Serialize)]
struct RegistrationRequest<'a> {
    client_name: &'a str,
    redirect_uris: [&'a str; 2],
    grant_types: [&'a str; 2],
    token_endpoint_auth_method: &'a str,
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
}

/// The `WWW-Authenticate: Bearer ...` challenge from the initial
/// unauthorized probe.
#[derive(Debug, Default)]
struct Challenge {
    resource_metadata: Option<String>,
    scope: Option<String>,
}

/// Step 1: probe `url` without a token and read the `WWW-Authenticate`
/// challenge, then step 2: fetch the protected resource metadata, trying the
/// header's `resource_metadata` URL first and the two well-known fallback
/// paths otherwise.
async fn discover_protected_resource(
    client: &reqwest::Client,
    url: &str,
) -> Result<(ProtectedResourceMetadata, Option<String>), McpLoginError> {
    let challenge = probe_unauthorized(client, url).await?;
    let base = reqwest::Url::parse(url).map_err(|_| McpLoginError::Discovery)?;
    // The `resource_metadata` URL comes from the server's own 401 response,
    // so a hostile HTTPS server could point it at an arbitrary origin (e.g.
    // a loopback port `otto mcp login` will then trust). Only follow it when
    // it names the same origin as the configured server; otherwise fall
    // back to the well-known path under that server's own origin, exactly
    // as when the header omits `resource_metadata` entirely.
    let same_origin = challenge
        .resource_metadata
        .as_deref()
        .and_then(|candidate| reqwest::Url::parse(candidate).ok())
        .is_some_and(|candidate_url| candidate_url.origin() == base.origin());
    let candidates = match &challenge.resource_metadata {
        Some(explicit) if same_origin => vec![explicit.clone()],
        _ => well_known_candidates(&base, "oauth-protected-resource"),
    };
    for candidate in &candidates {
        check_secure(candidate)?;
        if let Ok(metadata) = fetch_json::<ProtectedResourceMetadata>(client, candidate).await {
            return Ok((metadata, challenge.scope));
        }
    }
    Err(McpLoginError::Discovery)
}

async fn probe_unauthorized(
    client: &reqwest::Client,
    url: &str,
) -> Result<Challenge, McpLoginError> {
    let body = serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/list",
        "params": {},
    }))
    .expect("static json-rpc probe body serializes");
    let response = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream")
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| McpLoginError::Discovery)?
    .map_err(|_| McpLoginError::Discovery)?;
    if response.status() != reqwest::StatusCode::UNAUTHORIZED {
        return Err(McpLoginError::Discovery);
    }
    let header = response
        .headers()
        .get(reqwest::header::WWW_AUTHENTICATE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    Ok(parse_bearer_challenge(&header))
}

fn parse_bearer_challenge(header: &str) -> Challenge {
    let rest = header
        .trim()
        .strip_prefix("Bearer")
        .unwrap_or(header)
        .trim_start();
    let mut challenge = Challenge::default();
    for part in rest.split(',') {
        let Some((key, value)) = part.split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches('"').to_owned();
        match key.trim() {
            "resource_metadata" => challenge.resource_metadata = Some(value),
            "scope" => challenge.scope = Some(value),
            _ => {}
        }
    }
    challenge
}

/// Step 3: authorization server metadata, trying RFC 8414's well-known path
/// then the OpenID Connect discovery document, each with and without the
/// issuer's path suffix.
async fn discover_authorization_server(
    client: &reqwest::Client,
    resource_metadata: &ProtectedResourceMetadata,
) -> Result<AuthorizationServerMetadata, McpLoginError> {
    let issuer = resource_metadata
        .authorization_servers
        .first()
        .ok_or(McpLoginError::Discovery)?;
    check_secure(issuer)?;
    let issuer_url = reqwest::Url::parse(issuer).map_err(|_| McpLoginError::Discovery)?;
    let mut candidates = well_known_candidates(&issuer_url, "oauth-authorization-server");
    candidates.extend(well_known_candidates(&issuer_url, "openid-configuration"));
    for candidate in &candidates {
        if let Ok(metadata) = fetch_json::<AuthorizationServerMetadata>(client, candidate).await {
            check_secure(&metadata.authorization_endpoint)?;
            check_secure(&metadata.token_endpoint)?;
            return Ok(metadata);
        }
    }
    Err(McpLoginError::Discovery)
}

/// Step 4: dynamic registration when the server offers it, otherwise the
/// configured `oauth_client_id`.
async fn resolve_client_id(
    client: &reqwest::Client,
    server_metadata: &AuthorizationServerMetadata,
    configured: Option<&str>,
) -> Result<String, McpLoginError> {
    let Some(endpoint) = &server_metadata.registration_endpoint else {
        return configured
            .map(str::to_owned)
            .ok_or(McpLoginError::MissingClientId);
    };
    check_secure(endpoint)?;
    let redirect_a = format!(
        "http://localhost:{}/auth/callback",
        auth::oauth::LOOPBACK_PORTS[0]
    );
    let redirect_b = format!(
        "http://localhost:{}/auth/callback",
        auth::oauth::LOOPBACK_PORTS[1]
    );
    let request = RegistrationRequest {
        client_name: "otto",
        redirect_uris: [redirect_a.as_str(), redirect_b.as_str()],
        grant_types: ["authorization_code", "refresh_token"],
        token_endpoint_auth_method: "none",
    };
    let response: RegistrationResponse = post_json(client, endpoint, &request)
        .await
        .map_err(|_| McpLoginError::Registration)?;
    Ok(response.client_id)
}

/// `scope` selection order: configured scopes, then the server's advertised
/// scopes, then the `WWW-Authenticate` challenge's `scope` parameter.
fn select_scope(
    configured: &[String],
    advertised: &[String],
    challenge_scope: Option<&str>,
) -> Option<String> {
    if !configured.is_empty() {
        return Some(configured.join(" "));
    }
    if !advertised.is_empty() {
        return Some(advertised.join(" "));
    }
    challenge_scope.map(str::to_owned)
}

fn authorize_url(
    authorization_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
    scope: Option<&str>,
    resource: &str,
) -> Result<String, McpLoginError> {
    let mut url =
        reqwest::Url::parse(authorization_endpoint).map_err(|_| McpLoginError::Discovery)?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("client_id", client_id);
        pairs.append_pair("redirect_uri", redirect_uri);
        pairs.append_pair("response_type", "code");
        pairs.append_pair("code_challenge", challenge);
        pairs.append_pair("code_challenge_method", "S256");
        pairs.append_pair("state", state);
        if let Some(scope) = scope {
            pairs.append_pair("scope", scope);
        }
        pairs.append_pair("resource", resource);
    }
    Ok(url.into())
}

/// The configured `url` must equal the protected resource metadata's
/// `resource` after normalization: scheme and host lowercased (which
/// `reqwest::Url` parsing already does), no fragment, no trailing slash.
fn check_resource(
    advertised_resource: &str,
    configured_url: &str,
) -> Result<String, McpLoginError> {
    let configured = canonicalize(configured_url)?;
    let advertised = canonicalize(advertised_resource)?;
    if configured != advertised {
        return Err(McpLoginError::ResourceMismatch);
    }
    Ok(configured)
}

fn canonicalize(url_str: &str) -> Result<String, McpLoginError> {
    let mut url = reqwest::Url::parse(url_str).map_err(|_| McpLoginError::Discovery)?;
    url.set_fragment(None);
    let mut rendered = url.to_string();
    if rendered.ends_with('/') {
        rendered.pop();
    }
    Ok(rendered)
}

/// Metadata and token endpoints must be `https`, or `http` on loopback
/// (`localhost`, `127.0.0.1`, `::1`) for local development servers.
fn check_secure(url_str: &str) -> Result<(), McpLoginError> {
    let url = reqwest::Url::parse(url_str).map_err(|_| McpLoginError::InsecureEndpoint)?;
    let loopback_http = url.scheme() == "http"
        && matches!(
            url.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("::1")
        );
    if url.scheme() == "https" || loopback_http {
        Ok(())
    } else {
        Err(McpLoginError::InsecureEndpoint)
    }
}

/// RFC 8414 §3.1's well-known URL construction: the path-suffixed form
/// first (when the base URL has a path), then the bare form.
fn well_known_candidates(base: &reqwest::Url, well_known: &str) -> Vec<String> {
    let mut origin = format!(
        "{}://{}",
        base.scheme(),
        base.host_str().unwrap_or_default()
    );
    if let Some(port) = base.port() {
        origin.push(':');
        origin.push_str(&port.to_string());
    }
    let path = base.path();
    let mut candidates = Vec::new();
    if path != "/" && !path.is_empty() {
        candidates.push(format!("{origin}/.well-known/{well_known}{path}"));
    }
    candidates.push(format!("{origin}/.well-known/{well_known}"));
    candidates
}

// ---- HTTP plumbing -----------------------------------------------------

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<T, ()> {
    let response = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client.get(url).header("Accept", "application/json").send(),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let body = tokio::time::timeout(REQUEST_TIMEOUT, response.bytes())
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    if body.len() > auth::MAX_CREDENTIAL_FILE_BYTES {
        return Err(());
    }
    serde_json::from_slice(&body).map_err(|_| ())
}

async fn post_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    payload: &impl Serialize,
) -> Result<T, ()> {
    let body = serde_json::to_vec(payload).map_err(|_| ())?;
    let response = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client
            .post(url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(body)
            .send(),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let body = tokio::time::timeout(REQUEST_TIMEOUT, response.bytes())
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    if body.len() > auth::MAX_CREDENTIAL_FILE_BYTES {
        return Err(());
    }
    serde_json::from_slice(&body).map_err(|_| ())
}

/// A token endpoint result, generic over the caller's grant type. The body
/// can echo back the credential that was sent, so it is never read into an
/// error (mirrors `auth::oauth::post_token`).
struct ExchangedToken {
    access_token: String,
    refresh_token: Option<String>,
    expiry: Option<DateTime<FixedOffset>>,
}

async fn post_token(
    client: &reqwest::Client,
    token_endpoint: &str,
    form: &[(&str, &str)],
) -> Result<ExchangedToken, ()> {
    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        access_token: String,
        #[serde(default)]
        refresh_token: Option<String>,
        #[serde(default)]
        expires_in: Option<i64>,
    }
    let response = tokio::time::timeout(
        REQUEST_TIMEOUT,
        client
            .post(token_endpoint)
            .form(form)
            .header(reqwest::header::ACCEPT, "application/json")
            .send(),
    )
    .await
    .map_err(|_| ())?
    .map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    let body = tokio::time::timeout(REQUEST_TIMEOUT, response.bytes())
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
    if body.len() > auth::MAX_CREDENTIAL_FILE_BYTES {
        return Err(());
    }
    let parsed: Raw = serde_json::from_slice(&body).map_err(|_| ())?;
    if parsed.access_token.is_empty() {
        return Err(());
    }
    let expiry = parsed
        .expires_in
        .filter(|seconds| *seconds > 0)
        .map(|seconds| Utc::now().fixed_offset() + chrono::TimeDelta::seconds(seconds));
    Ok(ExchangedToken {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        expiry,
    })
}

async fn exchange_code(
    client: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
    resource: &str,
) -> Result<ExchangedToken, ()> {
    post_token(
        client,
        token_endpoint,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("client_id", client_id),
            ("resource", resource),
        ],
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::testserver::{self, json_response, status_response, unauthorized_response};
    use serde_json::json;
    use std::sync::{Arc, Mutex as StdMutex};

    /// Spawns a test server whose `respond` closure can see its own base
    /// URL (needed because the URLs served, e.g. `resource_metadata`, embed
    /// the server's own address, which is not known until after `spawn`
    /// returns).
    async fn spawn_self_aware<F>(respond: F) -> (testserver::TestServer, String)
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
                let callback = reqwest::Url::parse_with_params(
                    &redirect,
                    &[("code", code), ("state", &state)],
                )
                .unwrap();
                let _ = reqwest::get(callback).await;
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
                None => {
                    "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource/mcp\""
                        .to_owned()
                }
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
}
