//! ChatGPT credential storage and the "Sign in with ChatGPT" OAuth flow.
//!
//! The credential file at `~/.otto/auth/chatgpt.json` has the byte layout
//! written by Go (Otto was written in Go until #92), and existing files hold
//! it: `encoding/json`'s `MarshalIndent(c, "", "  ")` field order and two-space
//! indent, and `time.RFC3339Nano` for the expiry (see `go_time`).
//!
//! Every error is a fieldless [`AuthError`] variant. The cause is dropped
//! rather than wrapped, so no token, path, or upstream message can reach a log
//! or a terminal.

pub mod claims;
pub mod login;
pub mod oauth;
pub mod service;
pub mod token;

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, FixedOffset};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use otto_core::safetext::{self, SecretCollector};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use otto_platform::random::random_hex;

pub(crate) const MAX_CREDENTIAL_FILE_BYTES: usize = 1 << 20;

/// Takes the cross-process lock that serializes credential mutations.
///
/// Login, logout and token refresh all use the same sibling lock file. The
/// lock file is persistent; only the advisory lock represents ownership.
pub(crate) async fn lock_credentials(
    path: &Path,
    cancel: &CancellationToken,
    base: Option<&CancellationToken>,
) -> Result<Flock<File>, AuthError> {
    let mut lock_name = path.file_name().unwrap_or_default().to_owned();
    lock_name.push(".lock");
    let lock_path = path.with_file_name(lock_name);
    if let Some(parent) = lock_path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
            .map_err(|_| AuthError::CredentialsPersistence)?;
    }
    let mut lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|_| AuthError::CredentialsPersistence)?;
    loop {
        match Flock::lock(lock_file, FlockArg::LockExclusiveNonblock) {
            Ok(lock) => return Ok(lock),
            Err((returned, Errno::EWOULDBLOCK)) => lock_file = returned,
            Err(_) => return Err(AuthError::CredentialsPersistence),
        }
        if cancel.is_cancelled() || base.is_some_and(|token| token.is_cancelled()) {
            return Err(AuthError::Cancelled);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The sentinel errors this module reports. The variants carry no data: a
/// cause would be the only way a secret could escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AuthError {
    #[error("no chatgpt credentials; run 'otto login'")]
    NoCredentials,
    #[error("chatgpt credentials are unavailable; run 'otto login'")]
    CredentialsUnavailable,
    #[error("chatgpt credentials could not be saved")]
    CredentialsPersistence,
    #[error("chatgpt access token refresh failed; run 'otto login'")]
    AccessTokenRefreshFailed,
    #[error("chatgpt sign-in is unavailable in this session")]
    InteractiveUnavailable,
    #[error("chatgpt sign-in failed")]
    LoginFailed,
    #[error("stored chatgpt credentials could not be removed")]
    CredentialsRemoval,
    /// The flows carry a [`tokio_util::sync::CancellationToken`], so
    /// cancellation is a variant rather than a separate error type.
    #[error("context canceled")]
    Cancelled,
}

/// The `time.RFC3339Nano` encoding the stored credential file uses for
/// [`Credentials::expiry`].
///
/// chrono's own serde impl always prints a fixed number of fractional digits;
/// the stored format trims trailing zeros, omits the fraction entirely when it
/// is zero, and writes `Z` rather than `+00:00` for a zero offset. The stored
/// format is Go's `time.RFC3339Nano` layout, so it is written out here.
pub(crate) mod go_time {
    use chrono::{DateTime, FixedOffset, NaiveDate, TimeZone, Timelike};
    use serde::{Deserialize, Deserializer, Serializer};

    /// The zero time, which the stored format writes as `0001-01-01T00:00:00Z`.
    pub fn zero() -> DateTime<FixedOffset> {
        FixedOffset::east_opt(0)
            .expect("UTC is a valid fixed offset")
            .from_utc_datetime(
                &NaiveDate::from_ymd_opt(1, 1, 1)
                    .expect("0001-01-01 is a valid date")
                    .and_hms_opt(0, 0, 0)
                    .expect("midnight is a valid time"),
            )
    }

    /// Formats `value` as `time.RFC3339Nano`, the encoding the credential file
    /// uses.
    pub fn format(value: &DateTime<FixedOffset>) -> String {
        let mut text = value.format("%Y-%m-%dT%H:%M:%S").to_string();
        let nanosecond = value.nanosecond();
        if nanosecond > 0 {
            text.push('.');
            text.push_str(format!("{nanosecond:09}").trim_end_matches('0'));
        }
        if value.offset().local_minus_utc() == 0 {
            text.push('Z');
        } else {
            text.push_str(&value.format("%:z").to_string());
        }
        text
    }

    pub fn serialize<S: Serializer>(
        value: &DateTime<FixedOffset>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format(value))
    }

    /// Accepts RFC 3339 and treats an explicit `null` as the zero time.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<DateTime<FixedOffset>, D::Error> {
        let Some(text) = Option::<String>::deserialize(deserializer)? else {
            return Ok(zero());
        };
        DateTime::parse_from_rfc3339(&text).map_err(serde::de::Error::custom)
    }
}

/// Persisted with 0600 permissions and never logged; the [`std::fmt::Debug`]
/// impl below prints no token material.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub account_id: String,
    #[serde(default = "go_time::zero", with = "go_time")]
    pub expiry: DateTime<FixedOffset>,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            access_token: String::new(),
            refresh_token: String::new(),
            id_token: String::new(),
            account_id: String::new(),
            expiry: go_time::zero(),
        }
    }
}

/// Prints lengths, never values. A derived `Debug` would put three tokens into
/// any `{:?}` or test failure.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("access_token_bytes", &self.access_token.len())
            .field("refresh_token_bytes", &self.refresh_token.len())
            .field("id_token_bytes", &self.id_token.len())
            .field("account_id_bytes", &self.account_id.len())
            .field("expiry", &go_time::format(&self.expiry))
            .finish()
    }
}

/// How far ahead of the clock an access token must stay to count as usable.
/// `golang.org/x/oauth2` applies the same 10 second `expiryDelta`.
const EXPIRY_DELTA: chrono::TimeDelta = chrono::TimeDelta::seconds(10);

impl Credentials {
    /// A non-empty access token that is not within [`EXPIRY_DELTA`] of
    /// expiring. A zero expiry never expires.
    pub(crate) fn token_valid(&self, now: DateTime<FixedOffset>) -> bool {
        if self.access_token.is_empty() {
            return false;
        }
        self.expiry == go_time::zero() || self.expiry > now + EXPIRY_DELTA
    }

    /// Writes the credentials to `path` atomically with 0600 permissions,
    /// creating parent directories with 0700.
    pub fn save(&self, path: &Path) -> Result<(), AuthError> {
        if !self.within_bounds() {
            return Err(AuthError::CredentialsPersistence);
        }
        let credentials_json =
            serde_json::to_vec_pretty(self).map_err(|_| AuthError::CredentialsPersistence)?;
        write_secret_file(path, &credentials_json)
    }

    fn within_bounds(&self) -> bool {
        let mut total = 0usize;
        for value in [
            &self.access_token,
            &self.refresh_token,
            &self.id_token,
            &self.account_id,
        ] {
            if value.len() > MAX_CREDENTIAL_FILE_BYTES - total {
                return false;
            }
            total += value.len();
        }
        true
    }
}

/// Writes `data` to `path` atomically: creates the parent directory (mode
/// 0700, recursively), writes to a mode-0600 temp file in that directory,
/// then renames it onto `path`. Extracted from `Credentials::save`, which
/// this crate's ChatGPT credential file and the MCP OAuth token files
/// (`mcp::oauth`) both use, so the two credential stores share one
/// atomic-write implementation.
pub(crate) fn write_secret_file(path: &Path, data: &[u8]) -> Result<(), AuthError> {
    if data.len() > MAX_CREDENTIAL_FILE_BYTES {
        return Err(AuthError::CredentialsPersistence);
    }
    let directory = path.parent().ok_or(AuthError::CredentialsPersistence)?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|_| AuthError::CredentialsPersistence)?;
    let (temporary_path, mut temporary) = create_temp(directory)?;
    let written = temporary
        .write_all(data)
        .and_then(|()| temporary.sync_all());
    drop(temporary);
    if written.is_err() || std::fs::rename(&temporary_path, path).is_err() {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(AuthError::CredentialsPersistence);
    }
    Ok(())
}

/// Creates an exclusive `.otto-secret-*.tmp` file in `directory` with mode
/// 0600. The suffix comes from `/dev/urandom` so the name cannot be
/// pre-created.
fn create_temp(directory: &Path) -> Result<(PathBuf, File), AuthError> {
    for _ in 0..100 {
        let suffix = random_hex::<8>().map_err(|_| AuthError::CredentialsPersistence)?;
        let candidate = directory.join(format!(".otto-secret-{suffix}.tmp"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(AuthError::CredentialsPersistence),
        }
    }
    Err(AuthError::CredentialsPersistence)
}

pub fn path_for_home(home: &Path) -> PathBuf {
    home.join(".otto").join("auth").join("chatgpt.json")
}

/// A missing file is [`AuthError::NoCredentials`]; every other failure
/// collapses to [`AuthError::CredentialsUnavailable`] with no detail.
pub fn load(path: &Path) -> Result<Credentials, AuthError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Err(AuthError::NoCredentials),
        Err(_) => return Err(AuthError::CredentialsUnavailable),
    };
    let mut credentials_bytes = Vec::new();
    file.take(MAX_CREDENTIAL_FILE_BYTES as u64 + 1)
        .read_to_end(&mut credentials_bytes)
        .map_err(|_| AuthError::CredentialsUnavailable)?;
    if credentials_bytes.len() > MAX_CREDENTIAL_FILE_BYTES {
        return Err(AuthError::CredentialsUnavailable);
    }
    let credentials: Credentials = serde_json::from_slice(&credentials_bytes)
        .map_err(|_| AuthError::CredentialsUnavailable)?;
    if !credentials.within_bounds() {
        return Err(AuthError::CredentialsUnavailable);
    }
    Ok(credentials)
}

/// Reports presence only; no token value is ever part of the line, and the
/// account id is not either.
pub fn status_line(path: &Path) -> (String, bool) {
    match load(path) {
        Err(AuthError::NoCredentials) => (
            "Not signed in to ChatGPT. Run 'otto login'.".to_owned(),
            false,
        ),
        Err(_) => (
            "ChatGPT sign-in state is unavailable. Run 'otto login'.".to_owned(),
            false,
        ),
        Ok(credentials) => {
            let mut line = "Signed in to ChatGPT.".to_owned();
            if credentials.expiry != go_time::zero() {
                // chrono has no abbreviation for a fixed offset, so the format
                // prints the offset.
                line.push_str(&format!(
                    " Access token expires {}.",
                    credentials.expiry.format("%Y-%m-%d %H:%M:%S %Z")
                ));
            }
            (line, true)
        }
    }
}

#[cfg(test)]
pub(crate) fn expiry(text: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(text).expect("valid RFC 3339 timestamp")
}

/// What `captureAuthCredentials` returns: the path it read, the credentials
/// when they loaded, and the values the redaction boundary must mask.
#[derive(Debug, Default)]
pub struct CapturedAuth {
    pub path: String,
    pub credentials: Credentials,
    pub loaded: bool,
    pub complete: bool,
    pub redaction_values: Vec<String>,
}

pub fn capture_auth_credentials(path: &Path) -> CapturedAuth {
    let mut capture = CapturedAuth {
        path: path.to_string_lossy().into_owned(),
        complete: true,
        ..CapturedAuth::default()
    };
    if capture.path.is_empty() {
        capture.complete = false;
        return capture;
    }
    let credentials = match load(path) {
        Ok(credentials) => credentials,
        // A missing file is the normal state before `otto login`: nothing is
        // secret, so the boundary stays open.
        Err(AuthError::NoCredentials) => return capture,
        Err(_) => {
            capture.complete = false;
            return capture;
        }
    };
    let mut redactions = SecretCollector::new();
    let mut exact = SecretCollector::new();
    for value in [
        &credentials.access_token,
        &credentials.refresh_token,
        &credentials.id_token,
        &credentials.account_id,
    ] {
        if value.is_empty() {
            continue;
        }
        if !redactions.add(value)
            || !exact.add_form(value)
            || value.len() > safetext::MAX_DYNAMIC_VALUE_BYTES
        {
            capture.complete = false;
            return capture;
        }
    }
    let values = redactions.values();
    if safetext::dynamic_redaction_marker(&values).is_none() {
        capture.complete = false;
        return capture;
    }
    capture.credentials = credentials;
    capture.loaded = true;
    capture.redaction_values = values;
    capture
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// The exact bytes of a stored credential file: field order as declared,
    /// two-space indent, `": "` after each key, no trailing newline, and
    /// `time.RFC3339Nano` for the expiry. Values are placeholders; no real
    /// credential appears here.
    const GO_FIXTURE: &str = concat!(
        "{\n",
        "  \"access_token\": \"access-token-placeholder\",\n",
        "  \"refresh_token\": \"refresh-token-placeholder\",\n",
        "  \"id_token\": \"header.payload.signature\",\n",
        "  \"account_id\": \"acct-fixture\",\n",
        "  \"expiry\": \"2030-01-02T03:04:05.12345-08:00\"\n",
        "}"
    );

    #[test]
    fn save_then_load_round_trip() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("chatgpt.json");
        let want = Credentials {
            access_token: "access".to_owned(),
            refresh_token: "refresh".to_owned(),
            id_token: "id".to_owned(),
            account_id: "acct-1".to_owned(),
            expiry: expiry("2030-01-02T03:04:05Z"),
        };
        want.save(&path).unwrap();
        assert_eq!(load(&path).unwrap(), want);
    }

    /// Exit criterion: an existing credential file survives load and save
    /// unchanged, byte for byte.
    #[test]
    fn a_previously_written_credential_file_round_trips_byte_for_byte() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        std::fs::write(&path, GO_FIXTURE).unwrap();

        let credentials = load(&path).unwrap();
        assert_eq!(credentials.account_id, "acct-fixture");
        assert_eq!(
            go_time::format(&credentials.expiry),
            "2030-01-02T03:04:05.12345-08:00"
        );

        let rewritten = directory.path().join("rewritten.json");
        credentials.save(&rewritten).unwrap();
        assert_eq!(std::fs::read_to_string(&rewritten).unwrap(), GO_FIXTURE);
    }

    #[test]
    fn go_time_matches_the_rfc3339nano_cases_encoding_json_produces() {
        for (input, want) in [
            ("2030-01-02T03:04:05Z", "2030-01-02T03:04:05Z"),
            ("2030-01-02T03:04:05+00:00", "2030-01-02T03:04:05Z"),
            ("2030-01-02T03:04:05.100Z", "2030-01-02T03:04:05.1Z"),
            (
                "2030-01-02T03:04:05.123456789Z",
                "2030-01-02T03:04:05.123456789Z",
            ),
            (
                "2030-01-02T03:04:05.000000001Z",
                "2030-01-02T03:04:05.000000001Z",
            ),
            ("2030-01-02T03:04:05-08:00", "2030-01-02T03:04:05-08:00"),
            ("2030-01-02T03:04:05+05:30", "2030-01-02T03:04:05+05:30"),
        ] {
            assert_eq!(go_time::format(&expiry(input)), want, "input {input}");
        }
        assert_eq!(go_time::format(&go_time::zero()), "0001-01-01T00:00:00Z");
    }

    /// Covers the file mode and the directory mode.
    #[test]
    fn save_uses_owner_only_permissions_on_the_file_and_the_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("auth").join("chatgpt.json");
        Credentials {
            access_token: "a".to_owned(),
            ..Credentials::default()
        }
        .save(&path)
        .unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file mode {file_mode:o}");
        let directory_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700, "directory mode {directory_mode:o}");
    }

    /// The rewrite case: the rename must not inherit a wider mode from an
    /// existing file.
    #[test]
    fn no_temporary_file_survives_a_save() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        Credentials::default().save(&path).unwrap();
        Credentials::default().save(&path).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name != "chatgpt.json")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// `write_secret_file` is the extracted body `Credentials::save` now
    /// calls; it must produce the same 0700/0600 permissions on its own,
    /// independent of the `Credentials` type.
    #[test]
    fn write_secret_file_uses_owner_only_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested").join("token.json");
        write_secret_file(&path, b"{}").unwrap();
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "file mode {file_mode:o}");
        let directory_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(directory_mode, 0o700, "directory mode {directory_mode:o}");
        assert_eq!(std::fs::read(&path).unwrap(), b"{}");
    }

    #[test]
    fn load_missing_returns_no_credentials() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&directory.path().join("absent.json")),
            Err(AuthError::NoCredentials)
        );
    }

    /// The variant is fieldless, so the rendered message cannot contain the
    /// payload or the path.
    #[test]
    fn load_malformed_returns_a_fixed_unavailable_error() {
        const SECRET: &str = "credential-parse-secret";
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SECRET).join("chatgpt.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("{{\"access_token\":\"{SECRET}")).unwrap();

        let error = load(&path).unwrap_err();
        assert_eq!(error, AuthError::CredentialsUnavailable);
        let rendered = error.to_string();
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert!(
            !rendered.contains(&path.display().to_string()),
            "{rendered}"
        );
    }

    #[test]
    fn save_failure_returns_a_fixed_persistence_error() {
        const SECRET: &str = "credentials-path-secret";
        let directory = tempfile::tempdir().unwrap();
        let parent = directory.path().join(SECRET);
        std::fs::write(&parent, "not a directory").unwrap();
        let path = parent.join("chatgpt.json");

        let error = Credentials {
            access_token: "access".to_owned(),
            ..Credentials::default()
        }
        .save(&path)
        .unwrap_err();
        assert_eq!(error, AuthError::CredentialsPersistence);
        let rendered = error.to_string();
        assert!(!rendered.contains(SECRET), "{rendered}");
    }

    #[test]
    fn load_rejects_a_file_over_the_size_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        std::fs::write(&path, "x".repeat(MAX_CREDENTIAL_FILE_BYTES + 1)).unwrap();
        assert_eq!(load(&path), Err(AuthError::CredentialsUnavailable));
    }

    #[test]
    fn save_rejects_oversized_credentials_without_creating_the_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        let error = Credentials {
            access_token: "a".repeat(MAX_CREDENTIAL_FILE_BYTES + 1),
            ..Credentials::default()
        }
        .save(&path)
        .unwrap_err();
        assert_eq!(error, AuthError::CredentialsPersistence);
        assert!(!path.exists());
    }

    #[test]
    fn path_for_home_has_the_expected_suffix() {
        let path = path_for_home(Path::new("/home/user"));
        assert_eq!(path, Path::new("/home/user/.otto/auth/chatgpt.json"));
    }

    #[test]
    fn status_line_reports_presence_without_naming_the_account() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");

        let (line, signed_in) = status_line(&path);
        assert!(!signed_in);
        assert!(line.contains("Not signed in"), "{line}");

        Credentials {
            account_id: "acct-123".to_owned(),
            expiry: expiry("2030-01-02T03:04:05Z"),
            ..Credentials::default()
        }
        .save(&path)
        .unwrap();
        let (line, signed_in) = status_line(&path);
        assert!(signed_in);
        assert!(line.starts_with("Signed in to ChatGPT."), "{line}");
        assert!(line.contains("expires"), "{line}");
        assert!(!line.contains("acct-123"), "{line}");

        // A zero expiry adds no clause.
        Credentials::default().save(&path).unwrap();
        assert_eq!(status_line(&path).0, "Signed in to ChatGPT.");
    }

    /// A malformed file reports "unavailable", not "not signed in".
    #[test]
    fn status_line_separates_unavailable_from_absent() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("chatgpt.json");
        std::fs::write(&path, "{").unwrap();
        let (line, signed_in) = status_line(&path);
        assert!(!signed_in);
        assert!(line.contains("unavailable"), "{line}");
    }

    /// The `Debug` impl must not print token material.
    #[test]
    fn debug_prints_lengths_rather_than_tokens() {
        let rendered = format!(
            "{:?}",
            Credentials {
                access_token: "access-secret".to_owned(),
                refresh_token: "refresh-secret".to_owned(),
                id_token: "id-secret".to_owned(),
                account_id: "acct-secret".to_owned(),
                expiry: go_time::zero(),
            }
        );
        for secret in [
            "access-secret",
            "refresh-secret",
            "id-secret",
            "acct-secret",
        ] {
            assert!(!rendered.contains(secret), "{rendered}");
        }
    }

    #[test]
    fn token_validity_follows_the_oauth2_expiry_delta() {
        let now = expiry("2030-01-02T03:04:05Z");
        let with = |access: &str, at: &str| Credentials {
            access_token: access.to_owned(),
            expiry: expiry(at),
            ..Credentials::default()
        };
        assert!(with("a", "2030-01-02T03:05:05Z").token_valid(now));
        // Inside the 10 second delta, so already treated as expired.
        assert!(!with("a", "2030-01-02T03:04:10Z").token_valid(now));
        assert!(!with("a", "2030-01-02T03:04:00Z").token_valid(now));
        assert!(!with("", "2030-01-02T03:05:05Z").token_valid(now));
        // A zero expiry never expires.
        assert!(
            Credentials {
                access_token: "a".to_owned(),
                ..Credentials::default()
            }
            .token_valid(now)
        );
    }
}
