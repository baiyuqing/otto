//! The PKCE sign-in flow and its loopback callback listener. Port of
//! `internal/auth/login.go`.
//!
//! Divergence from Go: Go serves the callback with `net/http`. This port reads
//! the request line off the accepted socket and writes a fixed response,
//! because the crate has no HTTP server dependency and the listener answers
//! exactly one path with one of four fixed bodies.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::Credentials;
use super::claims::account_id_from_id_token;
use super::oauth::{self, Endpoint};

/// The failure modes of the sign-in flow. Fieldless, so no callback query
/// value, token endpoint body, or OS error text can reach a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LoginError {
    #[error("no loopback port available")]
    NoLoopbackPort,
    #[error("open authorization URL")]
    OpenFailed,
    #[error("callback server")]
    CallbackServer,
    #[error("authorization error")]
    Authorization,
    #[error("state mismatch on callback")]
    StateMismatch,
    #[error("callback missing authorization code")]
    MissingCode,
    /// Port of `errAuthorizationCodeExchangeFailed`.
    #[error("chatgpt authorization code exchange failed")]
    ExchangeFailed,
    #[error("token response missing id_token")]
    MissingIdToken,
    #[error("id token has no chatgpt_account_id claim")]
    MissingAccountId,
    #[error("context canceled")]
    Cancelled,
}

/// What the flow calls to put the authorization URL in front of the user. Go
/// passes `func(url string) error`.
pub type Opener<'a> = dyn Fn(&str) -> Result<(), String> + Send + Sync + 'a;

/// The largest request head the callback listener will buffer. A browser's
/// callback request is a few hundred bytes; anything larger is not the
/// redirect this listener exists for.
const MAX_REQUEST_HEAD_BYTES: usize = 8 * 1024;

/// Runs the "Sign in with ChatGPT" OAuth PKCE flow against the production
/// endpoint. Port of `auth.Login`. The caller persists the result.
pub async fn login(
    cancel: &CancellationToken,
    open: &Opener<'_>,
) -> Result<Credentials, LoginError> {
    login_with(
        &Endpoint::production(),
        &oauth::LOOPBACK_PORTS,
        cancel,
        open,
    )
    .await
}

/// Port of the unexported `login`, with the endpoint and the candidate ports
/// injected the way Go's tests replace `loopbackPorts`.
pub(crate) async fn login_with(
    endpoint: &Endpoint,
    ports: &[u16],
    cancel: &CancellationToken,
    open: &Opener<'_>,
) -> Result<Credentials, LoginError> {
    let (listener, port) = oauth::listen_loopback(ports)
        .await
        .map_err(|_| LoginError::NoLoopbackPort)?;
    let redirect_uri = format!("http://localhost:{port}/auth/callback");

    let verifier = oauth::generate_verifier().map_err(|_| LoginError::CallbackServer)?;
    let state = oauth::random_state().map_err(|_| LoginError::CallbackServer)?;
    let authorize = oauth::authorize_url(
        endpoint,
        &redirect_uri,
        &state,
        &oauth::s256_challenge(&verifier),
    )
    .map_err(|_| LoginError::CallbackServer)?;

    open(&authorize).map_err(|_| LoginError::OpenFailed)?;

    let code = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(LoginError::Cancelled),
        result = serve_callback(&listener, &state) => result?,
    };
    drop(listener);

    if cancel.is_cancelled() {
        return Err(LoginError::Cancelled);
    }
    let client = oauth::http_client().map_err(|_| LoginError::ExchangeFailed)?;
    let token = tokio::select! {
        biased;
        () = cancel.cancelled() => return Err(LoginError::Cancelled),
        result = oauth::exchange_code(&client, endpoint, &redirect_uri, &code, &verifier) => {
            result.map_err(|()| LoginError::ExchangeFailed)?
        }
    };
    if token.id_token.is_empty() {
        return Err(LoginError::MissingIdToken);
    }
    let account_id =
        account_id_from_id_token(&token.id_token).map_err(|_| LoginError::MissingAccountId)?;
    Ok(Credentials {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        id_token: token.id_token,
        account_id,
        expiry: token.expiry,
    })
}

/// Accepts loopback connections until one requests `/auth/callback`, then
/// applies Go's `callbackHandler` checks in the same order: `error` parameter,
/// then `state` equality, then a non-empty `code`.
async fn serve_callback(
    listener: &tokio::net::TcpListener,
    want_state: &str,
) -> Result<String, LoginError> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|_| LoginError::CallbackServer)?;
        let Some(target) = read_request_target(&mut stream).await else {
            let _ =
                write_browser_message(&mut stream, 400, "Sign-in failed. You can close this tab.")
                    .await;
            continue;
        };
        let Ok(url) = reqwest::Url::parse(&format!("http://localhost{target}")) else {
            let _ =
                write_browser_message(&mut stream, 400, "Sign-in failed. You can close this tab.")
                    .await;
            continue;
        };
        if url.path() != "/auth/callback" {
            // Go's ServeMux answers any other path with 404 and the flow keeps
            // waiting; a browser's favicon request must not end sign-in.
            let _ = write_browser_message(&mut stream, 404, "Not found.").await;
            continue;
        }
        let parameter = |name: &str| -> String {
            url.query_pairs()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.into_owned())
                .unwrap_or_default()
        };
        let (message, outcome) = if !parameter("error").is_empty() {
            (
                "Sign-in failed. You can close this tab.",
                Err(LoginError::Authorization),
            )
        } else if parameter("state") != want_state {
            (
                "Sign-in failed (state mismatch). You can close this tab.",
                Err(LoginError::StateMismatch),
            )
        } else if parameter("code").is_empty() {
            (
                "Sign-in failed (missing code). You can close this tab.",
                Err(LoginError::MissingCode),
            )
        } else {
            (
                "Signed in to ChatGPT. You can close this tab and return to Otto.",
                Ok(parameter("code")),
            )
        };
        let _ = write_browser_message(&mut stream, 200, message).await;
        return outcome;
    }
}

/// Reads the request line and returns its target, discarding the rest of the
/// head. Returns `None` when the head is malformed or exceeds
/// [`MAX_REQUEST_HEAD_BYTES`].
async fn read_request_target(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 512];
    loop {
        if let Some(position) = buffer.windows(2).position(|window| window == b"\r\n") {
            let line = String::from_utf8(buffer[..position].to_vec()).ok()?;
            let mut fields = line.split(' ');
            let method = fields.next()?;
            let target = fields.next()?;
            if method != "GET" {
                return None;
            }
            return Some(target.to_owned());
        }
        if buffer.len() > MAX_REQUEST_HEAD_BYTES {
            return None;
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
}

/// Port of `writeBrowserMessage`. The four messages are compile-time constants,
/// so nothing user-controlled reaches the HTML.
async fn write_browser_message(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    message: &str,
) -> std::io::Result<()> {
    let body = format!("<!doctype html><html><body><p>{message}</p></body></html>");
    let response = format!(
        "HTTP/1.1 {status} \r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::claims::fake_id_token;
    use crate::auth::testserver::{self, json_response, redirect_response};
    use serde_json::json;

    /// Drives the callback the way a browser would, from the authorize URL the
    /// flow hands to `open`. `state` is taken from that URL unless overridden.
    fn browser(code: &'static str, state_override: Option<&'static str>) -> Box<Opener<'static>> {
        Box::new(move |authorize: &str| {
            let url = reqwest::Url::parse(authorize).map_err(|_| "parse".to_owned())?;
            let parameter = |name: &str| {
                url.query_pairs()
                    .find(|(key, _)| key == name)
                    .map(|(_, value)| value.into_owned())
                    .unwrap_or_default()
            };
            assert_eq!(parameter("code_challenge_method"), "S256");
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

    /// Port of `TestLoginExchangesCodeAndExtractsAccountID`.
    #[tokio::test]
    async fn login_exchanges_the_code_and_extracts_the_account_id() {
        let id_token = fake_id_token(json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-xyz"}
        }));
        let body = json!({
            "access_token": "acc",
            "refresh_token": "ref",
            "token_type": "bearer",
            "expires_in": 3600,
            "id_token": id_token,
        })
        .to_string();
        let server = testserver::spawn(move |_| json_response(&body)).await;
        let endpoint = Endpoint {
            authorize_url: "https://auth.example/authorize".to_owned(),
            token_url: format!("{}/oauth/token", server.url),
        };

        let credentials = login_with(
            &endpoint,
            &[0],
            &CancellationToken::new(),
            browser("the-code", None).as_ref(),
        )
        .await
        .unwrap();

        assert_eq!(credentials.account_id, "acct-xyz");
        assert_eq!(credentials.access_token, "acc");
        assert_eq!(credentials.refresh_token, "ref");
        assert_eq!(credentials.id_token, id_token);
        assert!(credentials.expiry > chrono::Utc::now().fixed_offset());

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        let exchange = &requests[0];
        assert_eq!(exchange.target, "/oauth/token");
        assert_eq!(exchange.form("grant_type"), "authorization_code");
        assert_eq!(exchange.form("code"), "the-code");
        assert_eq!(exchange.form("client_id"), oauth::CLIENT_ID);
        assert!(!exchange.form("code_verifier").is_empty());
        assert!(
            exchange
                .form("redirect_uri")
                .starts_with("http://localhost:")
                && exchange.form("redirect_uri").ends_with("/auth/callback"),
            "{}",
            exchange.form("redirect_uri")
        );
    }

    /// Port of `TestLoginRejectsStateMismatch`: the token endpoint is never
    /// reached.
    #[tokio::test]
    async fn login_rejects_a_state_mismatch_without_calling_the_token_endpoint() {
        let server = testserver::spawn(|_| json_response("{}")).await;
        let endpoint = Endpoint {
            authorize_url: "https://auth.example/authorize".to_owned(),
            token_url: server.url.clone(),
        };
        let error = login_with(
            &endpoint,
            &[0],
            &CancellationToken::new(),
            browser("x", Some("WRONG")).as_ref(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, LoginError::StateMismatch);
        assert_eq!(server.count(), 0);
    }

    /// Port of `TestLoginExchangeBlocksAllRedirects`.
    #[tokio::test]
    async fn login_exchange_blocks_every_redirect_status() {
        for status in [301u16, 302, 303, 307, 308] {
            let target = testserver::spawn(|_| json_response(r#"{"access_token":"leaked"}"#)).await;
            let location = target.url.clone();
            let source = testserver::spawn(move |_| redirect_response(status, &location)).await;
            let endpoint = Endpoint {
                authorize_url: "https://auth.example/authorize".to_owned(),
                token_url: source.url.clone(),
            };

            let error = login_with(
                &endpoint,
                &[0],
                &CancellationToken::new(),
                browser("the-code", None).as_ref(),
            )
            .await
            .unwrap_err();

            assert_eq!(error, LoginError::ExchangeFailed, "status {status}");
            assert_eq!(source.count(), 1, "status {status}");
            assert_eq!(target.count(), 0, "status {status}");
        }
    }

    /// An `error` parameter on the callback fails before any exchange, and a
    /// missing `code` is reported separately.
    #[tokio::test]
    async fn callback_error_and_missing_code_are_distinct_failures() {
        for (query, want) in [
            ("error=access_denied", LoginError::Authorization),
            ("code=", LoginError::MissingCode),
        ] {
            let server = testserver::spawn(|_| json_response("{}")).await;
            let endpoint = Endpoint {
                authorize_url: "https://auth.example/authorize".to_owned(),
                token_url: server.url.clone(),
            };
            let open: Box<Opener<'static>> = Box::new(move |authorize: &str| {
                let url = reqwest::Url::parse(authorize).unwrap();
                let redirect = url
                    .query_pairs()
                    .find(|(key, _)| key == "redirect_uri")
                    .map(|(_, value)| value.into_owned())
                    .unwrap();
                let state = url
                    .query_pairs()
                    .find(|(key, _)| key == "state")
                    .map(|(_, value)| value.into_owned())
                    .unwrap();
                let callback = format!("{redirect}?{query}&state={state}");
                tokio::spawn(async move {
                    let _ = reqwest::get(&callback).await;
                });
                Ok(())
            });
            let error = login_with(&endpoint, &[0], &CancellationToken::new(), open.as_ref())
                .await
                .unwrap_err();
            assert_eq!(error, want, "query {query}");
            assert_eq!(server.count(), 0, "query {query}");
        }
    }

    /// A request to any other path is answered with 404 and sign-in continues,
    /// which is what Go's `ServeMux` does for e.g. `/favicon.ico`.
    #[tokio::test]
    async fn an_unrelated_path_does_not_end_the_flow() {
        let id_token = fake_id_token(json!({"chatgpt_account_id": "acct-1"}));
        let body = json!({"access_token": "acc", "id_token": id_token}).to_string();
        let server = testserver::spawn(move |_| json_response(&body)).await;
        let endpoint = Endpoint {
            authorize_url: "https://auth.example/authorize".to_owned(),
            token_url: server.url.clone(),
        };
        let open: Box<Opener<'static>> = Box::new(|authorize: &str| {
            let url = reqwest::Url::parse(authorize).unwrap();
            let redirect = url
                .query_pairs()
                .find(|(key, _)| key == "redirect_uri")
                .map(|(_, value)| value.into_owned())
                .unwrap();
            let state = url
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
                .unwrap();
            tokio::spawn(async move {
                let base = redirect.trim_end_matches("/auth/callback").to_owned();
                let noise = reqwest::get(format!("{base}/favicon.ico")).await.unwrap();
                assert_eq!(noise.status(), 404);
                let _ = reqwest::get(format!("{redirect}?code=c&state={state}")).await;
            });
            Ok(())
        });
        let credentials = login_with(&endpoint, &[0], &CancellationToken::new(), open.as_ref())
            .await
            .unwrap();
        assert_eq!(credentials.account_id, "acct-1");
        // No `expires_in` in the response, so the expiry stays Go's zero time.
        assert_eq!(credentials.expiry, super::super::go_time::zero());
    }

    /// A token response without an `id_token` cannot yield an account id, so
    /// the flow fails rather than saving unusable credentials.
    #[tokio::test]
    async fn a_response_without_an_id_token_fails() {
        let server = testserver::spawn(|_| json_response(r#"{"access_token":"acc"}"#)).await;
        let endpoint = Endpoint {
            authorize_url: "https://auth.example/authorize".to_owned(),
            token_url: server.url.clone(),
        };
        let error = login_with(
            &endpoint,
            &[0],
            &CancellationToken::new(),
            browser("c", None).as_ref(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, LoginError::MissingIdToken);
    }

    /// A cancelled token ends the flow while it waits for the callback.
    #[tokio::test]
    async fn cancellation_ends_the_wait_for_the_callback() {
        let endpoint = Endpoint {
            authorize_url: "https://auth.example/authorize".to_owned(),
            token_url: "http://127.0.0.1:1/token".to_owned(),
        };
        let cancel = CancellationToken::new();
        let opener: Box<Opener<'static>> = {
            let cancel = cancel.clone();
            Box::new(move |_: &str| {
                cancel.cancel();
                Ok(())
            })
        };
        let error = login_with(&endpoint, &[0], &cancel, opener.as_ref())
            .await
            .unwrap_err();
        assert_eq!(error, LoginError::Cancelled);
    }

    /// A failing opener stops before the listener is ever contacted.
    #[tokio::test]
    async fn an_opener_failure_stops_the_flow() {
        let endpoint = Endpoint {
            authorize_url: "https://auth.example/authorize".to_owned(),
            token_url: "http://127.0.0.1:1/token".to_owned(),
        };
        let opener: Box<Opener<'static>> = Box::new(|_: &str| Err("no browser".to_owned()));
        let error = login_with(&endpoint, &[0], &CancellationToken::new(), opener.as_ref())
            .await
            .unwrap_err();
        assert_eq!(error, LoginError::OpenFailed);
    }

    /// Every error message is fixed text: no query value, endpoint body, or
    /// path can appear in one.
    #[test]
    fn login_errors_render_fixed_text() {
        assert_eq!(
            LoginError::ExchangeFailed.to_string(),
            "chatgpt authorization code exchange failed"
        );
        assert_eq!(
            LoginError::StateMismatch.to_string(),
            "state mismatch on callback"
        );
    }
}
