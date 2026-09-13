//! OAuth endpoints, PKCE material and the token calls. Port of
//! `internal/auth/oauth.go` plus the `golang.org/x/oauth2` behaviour Go relies
//! on for the code exchange and the refresh.
//!
//! Divergence from Go: `golang.org/x/oauth2` probes the token endpoint with
//! HTTP Basic auth first and only retries with the client id in the form body.
//! This port always sends `client_id` in the body (`AuthStyleInParams`), which
//! is what the endpoint accepts and what the Codex CLI sends, so there is no
//! probe request.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, FixedOffset, Utc};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::random_bytes;

/// OAuth "Sign in with ChatGPT" constants. These mirror the OpenAI Codex CLI
/// public client. Verify against openai/codex (codex-rs/login) if the flow
/// changes.
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const SCOPES: &str = "openid profile email offline_access";

/// The redirect ports tried in order; matches the Codex CLI default and its
/// fallback. Port of `loopbackPorts`.
pub const LOOPBACK_PORTS: [u16; 2] = [1455, 1457];

/// Port of `oauth2.Endpoint` for the two URLs this flow uses. Tests point it
/// at a local server; [`Endpoint::production`] is the only other constructor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub authorize_url: String,
    pub token_url: String,
}

impl Endpoint {
    /// Port of `productionEndpoint`.
    pub fn production() -> Self {
        Self {
            authorize_url: AUTHORIZE_URL.to_owned(),
            token_url: TOKEN_URL.to_owned(),
        }
    }
}

/// Port of `randomState`: 32 random bytes, base64url without padding.
pub fn random_state() -> Result<String, String> {
    let bytes = random_bytes::<32>().map_err(|_| "generate state".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Port of `oauth2.GenerateVerifier`: 32 random bytes, base64url without
/// padding, which lands inside RFC 7636's 43..128 character range.
pub fn generate_verifier() -> Result<String, String> {
    let bytes = random_bytes::<32>().map_err(|_| "generate code verifier".to_owned())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Port of `oauth2.S256ChallengeFromVerifier`.
pub fn s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Port of `oauth2.Config.AuthCodeURL` with `AccessTypeOffline` and
/// `S256ChallengeOption`. Go encodes the parameters through
/// `url.Values.Encode`, which sorts keys, so they are appended sorted here and
/// the two binaries produce the same URL for the same inputs.
pub fn authorize_url(
    endpoint: &Endpoint,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> Result<String, String> {
    let mut url = reqwest::Url::parse(&endpoint.authorize_url)
        .map_err(|_| "parse authorize URL".to_owned())?;
    url.query_pairs_mut()
        .append_pair("access_type", "offline")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code_challenge", challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", SCOPES)
        .append_pair("state", state);
    Ok(url.into())
}

/// The token endpoint client. Port of `oauthHTTPClient`: every redirect is
/// refused so a 3xx from the token endpoint cannot move the client credentials
/// to another origin.
pub fn http_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "build oauth http client".to_owned())
}

/// The subset of the OAuth token response this flow reads.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    expires_in: Option<serde_json::Number>,
}

/// A token endpoint result. `expiry` is already absolute; Go's oauth2 does the
/// same conversion from `expires_in` at the moment the response is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub access_token: String,
    pub refresh_token: String,
    pub id_token: String,
    pub expiry: DateTime<FixedOffset>,
}

/// Posts `form` to the token endpoint and parses the response.
///
/// The error is a fixed string in every case. The token endpoint's body can
/// echo the credential that was sent (Go's own test asserts exactly that), so
/// it is never read into an error.
async fn post_token(
    client: &reqwest::Client,
    token_url: &str,
    form: &[(&str, &str)],
) -> Result<Token, ()> {
    let response = client
        .post(token_url)
        .form(form)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|_| ())?;
    if !response.status().is_success() {
        return Err(());
    }
    // Bounded: the body is small and a hostile endpoint must not be able to
    // stream an unbounded one into memory.
    let body = response.bytes().await.map_err(|_| ())?;
    if body.len() > super::MAX_CREDENTIAL_FILE_BYTES {
        return Err(());
    }
    let parsed: TokenResponse = serde_json::from_slice(&body).map_err(|_| ())?;
    if parsed.access_token.is_empty() {
        return Err(());
    }
    let expiry = match parsed
        .expires_in
        .as_ref()
        .and_then(serde_json::Number::as_i64)
    {
        Some(seconds) if seconds > 0 => {
            Utc::now().fixed_offset() + chrono::TimeDelta::seconds(seconds)
        }
        // Go leaves `Token.Expiry` zero when the response omits `expires_in`,
        // and a zero expiry means "never expires".
        _ => super::go_time::zero(),
    };
    Ok(Token {
        access_token: parsed.access_token,
        refresh_token: parsed.refresh_token,
        id_token: parsed.id_token,
        expiry,
    })
}

/// Port of `oauth2.Config.Exchange` with `VerifierOption`.
#[allow(clippy::result_unit_err)] // The unit error is the point: no
// endpoint body, status or transport detail may reach a caller.
pub async fn exchange_code(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<Token, ()> {
    post_token(
        client,
        &endpoint.token_url,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("client_id", CLIENT_ID),
        ],
    )
    .await
}

/// Port of the refresh half of `oauth2.Config.TokenSource`.
#[allow(clippy::result_unit_err)] // The unit error is the point: no
// endpoint body, status or transport detail may reach a caller.
pub async fn refresh_token(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    refresh: &str,
) -> Result<Token, ()> {
    post_token(
        client,
        &endpoint.token_url,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", CLIENT_ID),
        ],
    )
    .await
}

/// Binds the first available port from `ports` on loopback. Port of
/// `listenLoopback`; port 0 asks the kernel for an ephemeral port, which is
/// what the tests use so they never depend on 1455 being free.
pub async fn listen_loopback(ports: &[u16]) -> Result<(tokio::net::TcpListener, u16), String> {
    for port in ports {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", *port)).await {
            let bound = listener
                .local_addr()
                .map_err(|_| "read loopback address".to_owned())?
                .port();
            return Ok((listener, bound));
        }
    }
    Err(format!("no loopback port available (tried {ports:?})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B's worked example pins the challenge derivation.
    #[test]
    fn s256_challenge_matches_rfc_7636_appendix_b() {
        assert_eq!(
            s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    /// `oauth2.GenerateVerifier` and `randomState` both produce 43 characters
    /// from 32 bytes, and both must differ between calls.
    #[test]
    fn verifier_and_state_are_43_unpadded_base64url_characters() {
        for value in [generate_verifier().unwrap(), random_state().unwrap()] {
            assert_eq!(value.len(), 43, "{value}");
            assert!(
                value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'),
                "{value}"
            );
        }
        assert_ne!(generate_verifier().unwrap(), generate_verifier().unwrap());
        assert_ne!(random_state().unwrap(), random_state().unwrap());
    }

    /// The authorize URL carries the parameters `oauth2.Config.AuthCodeURL`
    /// writes, in the sorted order `url.Values.Encode` produces.
    #[test]
    fn authorize_url_matches_the_oauth2_encoding() {
        let url = authorize_url(
            &Endpoint::production(),
            "http://localhost:1455/auth/callback",
            "the-state",
            "the-challenge",
        )
        .unwrap();
        assert_eq!(
            url,
            "https://auth.openai.com/oauth/authorize\
             ?access_type=offline\
             &client_id=app_EMoamEEZ73f0CkXaXp7hrann\
             &code_challenge=the-challenge\
             &code_challenge_method=S256\
             &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
             &response_type=code\
             &scope=openid+profile+email+offline_access\
             &state=the-state"
        );
    }

    /// The issuer constants stay consistent with the two derived URLs.
    #[test]
    fn endpoint_urls_are_built_from_the_issuer() {
        assert_eq!(AUTHORIZE_URL, format!("{ISSUER}/oauth/authorize"));
        assert_eq!(TOKEN_URL, format!("{ISSUER}/oauth/token"));
    }

    /// Port 0 binds an ephemeral port; an already-bound port falls through to
    /// the next candidate, which is what `listenLoopback`'s loop does.
    #[tokio::test]
    async fn listen_loopback_falls_through_to_the_next_port() {
        let (occupied, port) = listen_loopback(&[0]).await.unwrap();
        let (_second, other) = listen_loopback(&[port, 0]).await.unwrap();
        assert_ne!(other, port);
        drop(occupied);
        assert!(listen_loopback(&[]).await.is_err());
    }
}
