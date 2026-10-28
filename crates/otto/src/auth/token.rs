//! The refreshing, disk-persisting token source.
//!
//! One `tokio::sync::Mutex` serializes the callers of one source: a caller that
//! arrives during a refresh waits and then re-checks validity. An exclusive
//! `flock` on `<credential file>.lock` serializes refreshes across sources and
//! processes. Under that lock the credential file is re-read, and a valid token
//! written by another source is adopted instead of refreshing with a refresh
//! token that may already be rotated. A waiter aborts on cancellation.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use super::oauth::{self, Endpoint};
use super::{AuthError, Credentials};

/// How a due refresh is performed. `Endpoint` is the only variant outside
/// tests; `Stub` is the injectable base token source.
enum Refresher {
    Endpoint(Endpoint),
    #[cfg(test)]
    Stub(Box<dyn Fn() -> Result<oauth::Token, ()> + Send + Sync>),
}

/// Refreshes through the refresh token when the access token has expired and
/// writes the rotated tokens back to `path`, so a refresh survives across
/// processes.
pub struct TokenSource {
    path: PathBuf,
    /// A source created before any credential file may persist its first
    /// refresh. A source that captured an existing login never recreates a
    /// file later removed by logout.
    may_create_credentials: bool,
    /// The process-level cancellation.
    base: CancellationToken,
    refresher: Refresher,
    credentials: tokio::sync::Mutex<Credentials>,
}

impl TokenSource {
    pub fn new(credentials: Credentials, path: PathBuf, base: CancellationToken) -> Self {
        Self::with_endpoint(Endpoint::production(), credentials, path, base)
    }

    pub fn with_endpoint(
        endpoint: Endpoint,
        credentials: Credentials,
        path: PathBuf,
        base: CancellationToken,
    ) -> Self {
        let may_create_credentials = !path.exists();
        Self {
            path,
            may_create_credentials,
            base,
            refresher: Refresher::Endpoint(endpoint),
            credentials: tokio::sync::Mutex::new(credentials),
        }
    }

    /// Returns the stored credentials when the access token is still valid,
    /// otherwise refreshes once and conditionally persists the result.
    pub async fn token(&self, cancel: &CancellationToken) -> Result<Credentials, AuthError> {
        let mut guard = self.lock_memory(cancel).await?;
        if guard.token_valid(chrono::Utc::now().fixed_offset()) {
            return Ok(guard.clone());
        }
        self.refresh_locked(&mut guard, cancel, false).await
    }

    /// Refreshes only when `rejected_access_token` is still current.
    ///
    /// A concurrent caller that already rotated the token wins and its current
    /// credentials are returned without another refresh.
    pub async fn refresh_rejected(
        &self,
        rejected_access_token: &str,
        cancel: &CancellationToken,
    ) -> Result<Credentials, AuthError> {
        let mut guard = self.lock_memory(cancel).await?;
        if guard.access_token != rejected_access_token {
            return Ok(guard.clone());
        }
        self.refresh_locked(&mut guard, cancel, true).await
    }

    async fn lock_memory(
        &self,
        cancel: &CancellationToken,
    ) -> Result<tokio::sync::MutexGuard<'_, Credentials>, AuthError> {
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(AuthError::Cancelled),
            () = self.base.cancelled() => Err(AuthError::Cancelled),
            guard = self.credentials.lock() => Ok(guard),
        }
    }

    async fn refresh_locked(
        &self,
        guard: &mut tokio::sync::MutexGuard<'_, Credentials>,
        cancel: &CancellationToken,
        force: bool,
    ) -> Result<Credentials, AuthError> {
        let _file_lock = super::lock_credentials(&self.path, cancel, Some(&self.base)).await?;
        let stored = super::load(&self.path).ok();
        if let Some(stored) = &stored
            && stored.account_id == guard.account_id
            && stored.access_token != guard.access_token
            && stored.token_valid(chrono::Utc::now().fixed_offset())
        {
            **guard = stored.clone();
            return Ok(stored.clone());
        }
        if !force
            && let Some(stored) = &stored
            && stored.account_id == guard.account_id
            && stored.token_valid(chrono::Utc::now().fixed_offset())
        {
            **guard = stored.clone();
            return Ok(stored.clone());
        }
        let snapshot = stored
            .as_ref()
            .filter(|stored| {
                stored.account_id == guard.account_id
                    && stored.refresh_token == guard.refresh_token
                    && stored.access_token == guard.access_token
            })
            .unwrap_or(&**guard)
            .clone();
        let persist = stored
            .as_ref()
            .map_or(self.may_create_credentials, |stored| {
                stored.account_id == snapshot.account_id
                    && stored.refresh_token == snapshot.refresh_token
                    && stored.access_token == snapshot.access_token
            });
        let refreshed = self.refresh(&snapshot, cancel, persist).await?;
        **guard = refreshed.clone();
        Ok(refreshed)
    }

    /// The rotated credentials are validated against the size bound before
    /// anything is written, and the file is only rewritten when a value
    /// actually changed. `persist` is false when the file belongs to another
    /// account.
    async fn refresh(
        &self,
        snapshot: &Credentials,
        cancel: &CancellationToken,
        persist: bool,
    ) -> Result<Credentials, AuthError> {
        let token = match &self.refresher {
            Refresher::Endpoint(endpoint) => {
                let client =
                    oauth::http_client().map_err(|_| AuthError::AccessTokenRefreshFailed)?;
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(AuthError::Cancelled),
                    () = self.base.cancelled() => return Err(AuthError::Cancelled),
                    result = oauth::refresh_token(&client, endpoint, &snapshot.refresh_token) => result,
                }
            }
            #[cfg(test)]
            Refresher::Stub(stub) => stub(),
        };
        let token = token.map_err(|()| AuthError::AccessTokenRefreshFailed)?;

        let mut candidate = snapshot.clone();
        candidate.access_token = token.access_token;
        candidate.expiry = token.expiry;
        if !token.refresh_token.is_empty() {
            candidate.refresh_token = token.refresh_token;
        }
        if !candidate.within_bounds() {
            return Err(AuthError::AccessTokenRefreshFailed);
        }
        let changed = candidate.access_token != snapshot.access_token
            || candidate.refresh_token != snapshot.refresh_token
            || candidate.expiry != snapshot.expiry;
        if changed && persist {
            candidate.save(&self.path)?;
        }
        Ok(candidate)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;
    use crate::auth::{MAX_CREDENTIAL_FILE_BYTES, expiry, go_time, load};
    use crate::faults::http::{
        self as testserver, json_response, redirect_response, status_response,
    };

    fn stub_source(
        path: PathBuf,
        credentials: Credentials,
        stub: impl Fn() -> Result<oauth::Token, ()> + Send + Sync + 'static,
    ) -> TokenSource {
        let may_create_credentials = !path.exists();
        TokenSource {
            path,
            may_create_credentials,
            base: CancellationToken::new(),
            refresher: Refresher::Stub(Box::new(stub)),
            credentials: tokio::sync::Mutex::new(credentials),
        }
    }

    fn expired(access: &str, refresh: &str, account: &str) -> Credentials {
        Credentials {
            access_token: access.to_owned(),
            refresh_token: refresh.to_owned(),
            account_id: account.to_owned(),
            expiry: (chrono::Utc::now() - chrono::TimeDelta::minutes(1)).fixed_offset(),
            ..Credentials::default()
        }
    }

    #[tokio::test]
    async fn refresh_rotates_the_tokens_and_persists_them_once() {
        let body = r#"{"access_token":"access-new","refresh_token":"refresh-new","token_type":"bearer","expires_in":3600}"#;
        let server = testserver::spawn(move |_| json_response(body)).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let source = TokenSource::with_endpoint(
            Endpoint {
                authorize_url: String::new(),
                token_url: server.url.clone(),
            },
            expired("access-old", "refresh-old", "acct-1"),
            path.clone(),
            CancellationToken::new(),
        );

        let cancel = CancellationToken::new();
        let token = source.token(&cancel).await.unwrap();
        assert_eq!(token.access_token, "access-new");

        let saved = load(&path).unwrap();
        assert_eq!(saved.access_token, "access-new");
        assert_eq!(saved.refresh_token, "refresh-new");
        assert_eq!(saved.account_id, "acct-1");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].form("grant_type"), "refresh_token");
        assert_eq!(requests[0].form("refresh_token"), "refresh-old");
        assert_eq!(requests[0].form("client_id"), oauth::CLIENT_ID);

        // The second call finds a valid token and does not call the endpoint.
        source.token(&cancel).await.unwrap();
        assert_eq!(server.count(), 1);
    }

    #[tokio::test]
    async fn a_valid_token_is_returned_without_contacting_the_endpoint() {
        let server = testserver::spawn(|_| json_response("{}")).await;
        let directory = tempfile::tempdir().unwrap();
        let source = TokenSource::with_endpoint(
            Endpoint {
                authorize_url: String::new(),
                token_url: server.url.clone(),
            },
            Credentials {
                access_token: "access-valid".to_owned(),
                refresh_token: "refresh".to_owned(),
                expiry: (chrono::Utc::now() + chrono::TimeDelta::hours(1)).fixed_offset(),
                ..Credentials::default()
            },
            directory.path().join("chatgpt.json"),
            CancellationToken::new(),
        );
        let token = source.token(&CancellationToken::new()).await.unwrap();
        assert_eq!(token.access_token, "access-valid");
        assert_eq!(server.count(), 0);
    }

    /// The endpoint's body, which echoes the credentials back, never reaches
    /// the error.
    #[tokio::test]
    async fn a_refresh_failure_reports_a_fixed_error_without_the_endpoint_body() {
        const ACCESS: &str = "access-secret";
        const REFRESH: &str = "refresh-secret";
        const ACCOUNT: &str = "acct-secret";
        let body = format!(r#"{{"error":"{ACCESS} {REFRESH} {ACCOUNT}"}}"#);
        let server = testserver::spawn(move |_| status_response(400, &body)).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(ACCOUNT).join("chatgpt.json");
        let source = TokenSource::with_endpoint(
            Endpoint {
                authorize_url: String::new(),
                token_url: server.url.clone(),
            },
            expired(ACCESS, REFRESH, ACCOUNT),
            path.clone(),
            CancellationToken::new(),
        );
        let error = source.token(&CancellationToken::new()).await.unwrap_err();
        assert_eq!(error, AuthError::AccessTokenRefreshFailed);
        let rendered = error.to_string();
        for secret in [ACCESS, REFRESH, ACCOUNT, &path.display().to_string()] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }

    #[tokio::test]
    async fn a_save_failure_reports_the_persistence_error() {
        const ACCOUNT: &str = "acct-secret";
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join(ACCOUNT);
        std::fs::write(&parent, "not a directory").unwrap();
        let path = parent.join("chatgpt.json");
        let source = stub_source(
            path.clone(),
            expired("access-secret", "refresh-secret", ACCOUNT),
            || {
                Ok(oauth::Token {
                    access_token: "access-rotated".to_owned(),
                    refresh_token: "refresh-rotated".to_owned(),
                    id_token: String::new(),
                    expiry: (chrono::Utc::now() + chrono::TimeDelta::hours(1)).fixed_offset(),
                })
            },
        );
        let error = source.token(&CancellationToken::new()).await.unwrap_err();
        assert_eq!(error, AuthError::CredentialsPersistence);
        let rendered = error.to_string();
        for secret in ["access-secret", "refresh-secret", ACCOUNT] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }

    #[tokio::test]
    async fn an_oversized_rotation_is_rejected_without_persisting_or_mutating() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let source = stub_source(
            path.clone(),
            expired("access-old", "refresh-old", "acct-1"),
            || {
                Ok(oauth::Token {
                    access_token: "x".repeat(MAX_CREDENTIAL_FILE_BYTES + 1),
                    refresh_token: "refresh-new".to_owned(),
                    id_token: String::new(),
                    expiry: (chrono::Utc::now() + chrono::TimeDelta::hours(1)).fixed_offset(),
                })
            },
        );
        let error = source.token(&CancellationToken::new()).await.unwrap_err();
        assert_eq!(error, AuthError::AccessTokenRefreshFailed);
        let retained = source.credentials.lock().await.clone();
        assert_eq!(retained.access_token, "access-old");
        assert_eq!(retained.refresh_token, "refresh-old");
        assert!(!path.exists());
    }

    /// A refresh that returns the same values writes nothing.
    #[tokio::test]
    async fn an_unchanged_rotation_does_not_rewrite_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let unchanged_expiry = (chrono::Utc::now() - chrono::TimeDelta::minutes(1)).fixed_offset();
        let source = stub_source(
            path.clone(),
            Credentials {
                access_token: "access-old".to_owned(),
                refresh_token: "refresh-old".to_owned(),
                account_id: "acct-1".to_owned(),
                expiry: unchanged_expiry,
                ..Credentials::default()
            },
            move || {
                Ok(oauth::Token {
                    access_token: "access-old".to_owned(),
                    refresh_token: String::new(),
                    id_token: String::new(),
                    expiry: unchanged_expiry,
                })
            },
        );
        source.token(&CancellationToken::new()).await.unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn a_cancelled_base_token_is_reported_as_cancellation() {
        let directory = tempfile::tempdir().unwrap();
        let base = CancellationToken::new();
        base.cancel();
        let source = TokenSource::with_endpoint(
            Endpoint {
                authorize_url: String::new(),
                token_url: "http://127.0.0.1:1/token".to_owned(),
            },
            expired("access", "refresh", ""),
            directory.path().join("chatgpt.json"),
            base,
        );
        assert_eq!(
            source.token(&CancellationToken::new()).await,
            Err(AuthError::Cancelled)
        );
    }

    #[tokio::test]
    async fn a_refresh_follows_no_redirect() {
        for status in [301u16, 302, 303, 307, 308] {
            let target = testserver::spawn(|_| json_response(r#"{"access_token":"leaked"}"#)).await;
            let location = target.url.clone();
            let server = testserver::spawn(move |_| redirect_response(status, &location)).await;
            let directory = tempfile::tempdir().unwrap();
            let source = TokenSource::with_endpoint(
                Endpoint {
                    authorize_url: String::new(),
                    token_url: server.url.clone(),
                },
                expired("access-old", "refresh-old", "acct-1"),
                directory.path().join("chatgpt.json"),
                CancellationToken::new(),
            );
            assert_eq!(
                source.token(&CancellationToken::new()).await,
                Err(AuthError::AccessTokenRefreshFailed),
                "status {status}"
            );
            assert_eq!(server.count(), 1, "status {status}");
            assert_eq!(target.count(), 0, "status {status}");
        }
    }

    /// The per-call token aborts a refresh that is already in flight.
    #[tokio::test]
    async fn a_refresh_in_flight_aborts_on_the_per_call_token() {
        let url = testserver::spawn_hanging().await;
        let directory = tempfile::tempdir().unwrap();
        let source = TokenSource::with_endpoint(
            Endpoint {
                authorize_url: String::new(),
                token_url: url,
            },
            expired("access-old", "refresh-old", "acct-1"),
            directory.path().join("chatgpt.json"),
            CancellationToken::new(),
        );
        let cancel = CancellationToken::new();
        let cancelling = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancelling.cancel();
        });
        assert_eq!(source.token(&cancel).await, Err(AuthError::Cancelled));
    }

    #[tokio::test]
    async fn a_refresh_in_flight_aborts_on_the_base_token() {
        let url = testserver::spawn_hanging().await;
        let directory = tempfile::tempdir().unwrap();
        let base = CancellationToken::new();
        let source = TokenSource::with_endpoint(
            Endpoint {
                authorize_url: String::new(),
                token_url: url,
            },
            expired("access-old", "refresh-old", "acct-1"),
            directory.path().join("chatgpt.json"),
            base.clone(),
        );
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            base.cancel();
        });
        assert_eq!(
            source.token(&CancellationToken::new()).await,
            Err(AuthError::Cancelled)
        );
    }

    /// A zero expiry never expires, so a credential written by a token
    /// response without `expires_in` is used as is.
    #[tokio::test]
    async fn a_zero_expiry_never_triggers_a_refresh() {
        let server = testserver::spawn(|_| json_response("{}")).await;
        let directory = tempfile::tempdir().unwrap();
        let source = TokenSource::with_endpoint(
            Endpoint {
                authorize_url: String::new(),
                token_url: server.url.clone(),
            },
            Credentials {
                access_token: "access".to_owned(),
                expiry: go_time::zero(),
                ..Credentials::default()
            },
            directory.path().join("chatgpt.json"),
            CancellationToken::new(),
        );
        assert_eq!(
            source
                .token(&CancellationToken::new())
                .await
                .unwrap()
                .access_token,
            "access"
        );
        assert_eq!(server.count(), 0);
        // `expiry` is only a marker here; the helper keeps the import used.
        assert!(expiry("2030-01-02T03:04:05Z") > go_time::zero());
    }

    fn rotated_token(access: &str) -> oauth::Token {
        oauth::Token {
            access_token: access.to_owned(),
            refresh_token: "refresh-new".to_owned(),
            id_token: String::new(),
            expiry: (chrono::Utc::now() + chrono::TimeDelta::hours(1)).fixed_offset(),
        }
    }

    fn valid_credentials(access: &str, account: &str) -> Credentials {
        Credentials {
            access_token: access.to_owned(),
            refresh_token: "refresh-file".to_owned(),
            account_id: account.to_owned(),
            expiry: (chrono::Utc::now() + chrono::TimeDelta::hours(1)).fixed_offset(),
            ..Credentials::default()
        }
    }

    #[tokio::test]
    async fn a_stale_source_adopts_a_valid_token_from_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        valid_credentials("access-file", "acct-1")
            .save(&path)
            .unwrap();
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let counted = refresh_calls.clone();
        let source = stub_source(
            path,
            expired("access-old", "refresh-old", "acct-1"),
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(rotated_token("access-new"))
            },
        );
        let token = source.token(&CancellationToken::new()).await.unwrap();
        assert_eq!(token.access_token, "access-file");
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_sources_on_one_file_refresh_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let sources: Vec<Arc<TokenSource>> = (0..2)
            .map(|_| {
                let counted = refresh_calls.clone();
                Arc::new(stub_source(
                    path.clone(),
                    expired("access-old", "refresh-old", "acct-1"),
                    move || {
                        counted.fetch_add(1, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(100));
                        Ok(rotated_token("access-new"))
                    },
                ))
            })
            .collect();
        let handles: Vec<_> = sources
            .into_iter()
            .map(|source| {
                tokio::spawn(async move { source.token(&CancellationToken::new()).await })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.await.unwrap().unwrap().access_token, "access-new");
        }
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_rejections_refresh_the_current_token_once() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let counted = refresh_calls.clone();
        let source = Arc::new(stub_source(
            path,
            valid_credentials("access-rejected", "acct-1"),
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(50));
                Ok(rotated_token("access-new"))
            },
        ));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let source = source.clone();
                tokio::spawn(async move {
                    source
                        .refresh_rejected("access-rejected", &CancellationToken::new())
                        .await
                })
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.await.unwrap().unwrap().access_token, "access-new");
        }
        assert_eq!(refresh_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn logout_waits_for_refresh_then_removes_the_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        valid_credentials("access-rejected", "acct-1")
            .save(&path)
            .unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let refresh_entered = entered.clone();
        let refresh_release = release.clone();
        let source = Arc::new(stub_source(
            path.clone(),
            valid_credentials("access-rejected", "acct-1"),
            move || {
                refresh_entered.wait();
                refresh_release.wait();
                Ok(rotated_token("access-new"))
            },
        ));
        let refreshing = {
            let source = source.clone();
            tokio::spawn(async move {
                source
                    .refresh_rejected("access-rejected", &CancellationToken::new())
                    .await
            })
        };
        entered.wait();
        let logging_out = {
            let path = path.clone();
            tokio::spawn(async move {
                crate::auth::service::Service::new(path)
                    .logout(&CancellationToken::new())
                    .await
            })
        };
        release.wait();
        assert_eq!(
            refreshing.await.unwrap().unwrap().access_token,
            "access-new"
        );
        assert!(logging_out.await.unwrap().unwrap());
        assert!(matches!(load(&path), Err(AuthError::NoCredentials)));
    }

    #[tokio::test]
    async fn a_source_captured_before_logout_does_not_recreate_credentials() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        valid_credentials("access-rejected", "acct-1")
            .save(&path)
            .unwrap();
        let source = stub_source(
            path.clone(),
            valid_credentials("access-rejected", "acct-1"),
            || Ok(rotated_token("access-new")),
        );
        assert!(
            crate::auth::service::Service::new(path.clone())
                .logout(&CancellationToken::new())
                .await
                .unwrap()
        );
        assert_eq!(
            source
                .refresh_rejected("access-rejected", &CancellationToken::new())
                .await
                .unwrap()
                .access_token,
            "access-new"
        );
        assert!(matches!(load(&path), Err(AuthError::NoCredentials)));
    }

    #[tokio::test]
    async fn a_file_of_another_account_is_not_overwritten() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let other = Credentials {
            expiry: (chrono::Utc::now() - chrono::TimeDelta::minutes(1)).fixed_offset(),
            ..valid_credentials("access-other", "acct-other")
        };
        other.save(&path).unwrap();
        let source = stub_source(
            path.clone(),
            expired("access-old", "refresh-old", "acct-1"),
            || Ok(rotated_token("access-new")),
        );
        let token = source.token(&CancellationToken::new()).await.unwrap();
        assert_eq!(token.access_token, "access-new");
        assert_eq!(token.account_id, "acct-1");
        let stored = load(&path).unwrap();
        assert_eq!(stored.account_id, "acct-other");
        assert_eq!(stored.access_token, "access-other");
    }

    #[tokio::test]
    async fn a_completed_refresh_is_saved_even_if_the_call_is_cancelled_afterwards() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let cancel = CancellationToken::new();
        let cancelling = cancel.clone();
        let source = stub_source(
            path.clone(),
            expired("access-old", "refresh-old", "acct-1"),
            move || {
                cancelling.cancel();
                Ok(rotated_token("access-new"))
            },
        );
        let token = source.token(&cancel).await.unwrap();
        assert_eq!(token.access_token, "access-new");
        let stored = load(&path).unwrap();
        assert_eq!(stored.access_token, "access-new");
        assert_eq!(stored.refresh_token, "refresh-new");
    }
}
