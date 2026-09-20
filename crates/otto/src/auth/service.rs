//! The narrow capability the frontends get.
//!
//! Every failure the sign-in flow can produce collapses to
//! [`AuthError::LoginFailed`] here, so nothing an OAuth endpoint or a callback
//! query said can reach a terminal.

use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use super::login::{LoginError, Opener};
use super::{AuthError, Credentials};

/// How `login` runs. `Production` is the real PKCE flow; `Stub` is what tests
/// substitute. The injected sign-in flow. `login` is a boxed closure so the
/// production path stays a plain function call.
#[cfg(test)]
type LoginFlow = Box<dyn Fn(&Opener<'_>) -> Result<Credentials, LoginError> + Send + Sync>;

enum Flow {
    Production,
    #[cfg(test)]
    Stub(LoginFlow),
}

/// Owns the configured credential file and the operations that mutate or
/// inspect it.
pub struct Service {
    path: PathBuf,
    flow: Flow,
}

impl Service {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            flow: Flow::Production,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_flow(
        path: PathBuf,
        flow: impl Fn(&Opener<'_>) -> Result<Credentials, LoginError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            path,
            flow: Flow::Stub(Box::new(flow)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `open` is `None` when no browser opener is supplied.
    pub async fn login(
        &self,
        cancel: &CancellationToken,
        open: Option<&Opener<'_>>,
    ) -> Result<(), AuthError> {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        let Some(open) = open else {
            return Err(AuthError::LoginFailed);
        };
        let credentials: Result<Credentials, LoginError> = match &self.flow {
            Flow::Production => super::login::login(cancel, open).await,
            #[cfg(test)]
            Flow::Stub(stub) => stub(open),
        };
        let credentials = match credentials {
            Ok(credentials) => credentials,
            Err(LoginError::Cancelled) => return Err(AuthError::Cancelled),
            Err(_) if cancel.is_cancelled() => return Err(AuthError::Cancelled),
            Err(_) => return Err(AuthError::LoginFailed),
        };
        // The token is re-checked here so a cancellation that arrived during
        // the flow cannot still write a credential file.
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        credentials
            .save(&self.path)
            .map_err(|_| AuthError::CredentialsPersistence)
    }

    /// The bool reports whether a file was removed.
    pub fn logout(&self, cancel: &CancellationToken) -> Result<bool, AuthError> {
        if cancel.is_cancelled() {
            return Err(AuthError::Cancelled);
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(AuthError::CredentialsRemoval),
        }
    }

    pub fn status(&self, cancel: &CancellationToken) -> (String, bool) {
        if cancel.is_cancelled() {
            return (AuthError::InteractiveUnavailable.to_string(), false);
        }
        super::status_line(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::load;

    #[tokio::test]
    async fn a_flow_failure_is_reported_as_a_bare_login_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let service = Service::with_flow(path.clone(), |_| Err(LoginError::ExchangeFailed));
        let open: Box<Opener<'static>> = Box::new(|_: &str| Ok(()));
        let error = service
            .login(&CancellationToken::new(), Some(open.as_ref()))
            .await
            .unwrap_err();
        assert_eq!(error, AuthError::LoginFailed);
        assert_eq!(error.to_string(), "chatgpt sign-in failed");
        assert!(!error.to_string().contains("exchange"));
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn login_saves_and_logout_reports_whether_a_file_was_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let service = Service::with_flow(path.clone(), |_| {
            Ok(Credentials {
                access_token: "secret".to_owned(),
                account_id: "acct-2".to_owned(),
                ..Credentials::default()
            })
        });
        let cancel = CancellationToken::new();
        let open: Box<Opener<'static>> = Box::new(|_: &str| Ok(()));
        service.login(&cancel, Some(open.as_ref())).await.unwrap();

        let (line, signed_in) = service.status(&cancel);
        assert!(signed_in);
        assert!(line.contains("Signed in to ChatGPT"), "{line}");
        assert!(!line.contains("acct-2"), "{line}");

        assert!(service.logout(&cancel).unwrap());
        assert!(!service.logout(&cancel).unwrap());
    }

    /// A missing opener and a cancelled token are distinct outcomes, and
    /// cancellation wins.
    #[tokio::test]
    async fn cancellation_precedes_the_missing_opener_check() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path().join("chatgpt.json"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            service.login(&cancel, None).await,
            Err(AuthError::Cancelled)
        );
        assert_eq!(
            service.login(&CancellationToken::new(), None).await,
            Err(AuthError::LoginFailed)
        );
    }

    #[tokio::test]
    async fn credentials_are_not_saved_when_cancellation_arrives_during_the_flow() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let cancel = CancellationToken::new();
        let cancelling = cancel.clone();
        let service = Service::with_flow(path.clone(), move |_| {
            cancelling.cancel();
            Ok(Credentials {
                access_token: "secret".to_owned(),
                ..Credentials::default()
            })
        });
        let open: Box<Opener<'static>> = Box::new(|_: &str| Ok(()));
        assert_eq!(
            service.login(&cancel, Some(open.as_ref())).await,
            Err(AuthError::Cancelled)
        );
        assert_eq!(load(&path), Err(AuthError::NoCredentials));
    }

    /// A save failure is reported as persistence, not as a login failure, so
    /// the CLI can tell the two apart the way `runLogin` does.
    #[tokio::test]
    async fn a_save_failure_is_reported_separately() {
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join("blocked");
        std::fs::write(&parent, "not a directory").unwrap();
        let service =
            Service::with_flow(parent.join("chatgpt.json"), |_| Ok(Credentials::default()));
        let open: Box<Opener<'static>> = Box::new(|_: &str| Ok(()));
        assert_eq!(
            service
                .login(&CancellationToken::new(), Some(open.as_ref()))
                .await,
            Err(AuthError::CredentialsPersistence)
        );
    }

    /// A cancelled token makes status and logout unavailable rather than
    /// reporting "not signed in".
    #[test]
    fn a_cancelled_token_makes_status_and_logout_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::new(directory.path().join("chatgpt.json"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let (line, signed_in) = service.status(&cancel);
        assert!(!signed_in);
        assert_eq!(line, "chatgpt sign-in is unavailable in this session");
        assert_eq!(service.logout(&cancel), Err(AuthError::Cancelled));
    }

    /// The opener the flow receives is the one the caller passed.
    #[tokio::test]
    async fn the_flow_receives_the_callers_opener() {
        let directory = tempfile::tempdir().unwrap();
        let service = Service::with_flow(directory.path().join("chatgpt.json"), |open| {
            open("https://auth.example/authorize?x=1").unwrap();
            Ok(Credentials {
                account_id: "acct-7".to_owned(),
                ..Credentials::default()
            })
        });
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let recorder = std::sync::Arc::clone(&seen);
        let open: Box<Opener<'static>> = Box::new(move |url: &str| {
            *recorder.lock().unwrap() = url.to_owned();
            Ok(())
        });
        service
            .login(&CancellationToken::new(), Some(open.as_ref()))
            .await
            .unwrap();
        assert_eq!(
            seen.lock().unwrap().as_str(),
            "https://auth.example/authorize?x=1"
        );
    }
}
