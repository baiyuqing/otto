//! Best-effort, account-scoped ChatGPT catalog cache. Invalid files are misses.

use std::io::Read;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::catalog::{MAX_BODY, ModelCatalog};

pub const TTL: Duration = Duration::from_secs(3 * 24 * 60 * 60);

#[derive(Deserialize, Serialize)]
struct Entry {
    identity: String,
    fetched_at: u64,
    catalog: ModelCatalog,
}

pub fn identity(base_url: &str, account_id: &str, client_version: &str) -> String {
    let mut digest = Sha256::new();
    for value in [base_url, account_id, client_version] {
        digest.update((value.len() as u64).to_le_bytes());
        digest.update(value.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn now() -> Option<u64> {
    Some(SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs())
}

pub fn load(path: &Path, expected_identity: &str) -> Option<ModelCatalog> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_BODY as u64 + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() > MAX_BODY {
        return None;
    }
    let entry: Entry = serde_json::from_slice(&bytes).ok()?;
    let age = now()?.checked_sub(entry.fetched_at)?;
    if entry.identity != expected_identity || age > TTL.as_secs() || entry.catalog.models.is_empty()
    {
        return None;
    }
    Some(entry.catalog)
}

pub fn store(path: &Path, identity: String, catalog: &ModelCatalog) {
    if catalog.models.is_empty() {
        return;
    }
    let Some(fetched_at) = now() else { return };
    let entry = Entry {
        identity,
        fetched_at,
        catalog: catalog.clone(),
    };
    if let Ok(bytes) = serde_json::to_vec(&entry) {
        // The shared auth writer supplies atomic replacement and mode 0600.
        let _ = crate::auth::write_secret_file(path, &bytes);
    }
}

pub fn invalidate(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(test)]
mod tests {
    use super::identity;

    #[test]
    fn cache_identity_changes_with_connection_and_protocol_version() {
        let original = identity("https://chatgpt.example/codex", "account-a", "0.163.0");
        assert_ne!(
            original,
            identity("https://other.example/codex", "account-a", "0.163.0")
        );
        assert_ne!(
            original,
            identity("https://chatgpt.example/codex", "account-b", "0.163.0")
        );
        assert_ne!(
            original,
            identity("https://chatgpt.example/codex", "account-a", "0.164.0")
        );
    }
}
