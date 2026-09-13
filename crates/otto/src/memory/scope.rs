//! Scope derivation ported from Go `internal/memory/scope.go`.
//!
//! The workspace scope ID is a digest of the physical workspace path, so the
//! Go and Rust binaries derive the same ID for the same directory and share
//! the rows in one database file.

use sha2::{Digest, Sha256};

use super::contracts::{
    Error, MAX_SCOPE_ID_BYTES, NAMESPACE_USER, NAMESPACE_WORKSPACE, Result, Scope, invalid_request,
    valid_opaque_id,
};

pub fn new_user_scope(installation_id: &str) -> Result<Scope> {
    if !valid_opaque_id(installation_id, MAX_SCOPE_ID_BYTES) {
        return Err(invalid_request("installation ID"));
    }
    Ok(Scope::new(NAMESPACE_USER, installation_id))
}

/// `stable_override` wins when set. Otherwise the path is made absolute, its
/// symlinks are resolved, and the SHA-256 of the result becomes the ID.
pub fn new_workspace_scope(canonical_path: &str, stable_override: &str) -> Result<Scope> {
    if !stable_override.is_empty() {
        if !valid_opaque_id(stable_override, MAX_SCOPE_ID_BYTES) {
            return Err(invalid_request("workspace scope override"));
        }
        return Ok(Scope::new(NAMESPACE_WORKSPACE, stable_override));
    }
    let bad = || -> Error { invalid_request("canonical workspace path") };
    if canonical_path.is_empty() || contains_path_control(canonical_path) {
        return Err(bad());
    }
    let physical = std::fs::canonicalize(canonical_path).map_err(|_| bad())?;
    let physical = physical.to_str().ok_or_else(bad)?;
    let digest = Sha256::digest(physical.as_bytes());
    let mut id = String::with_capacity(7 + digest.len() * 2);
    id.push_str("sha256:");
    for byte in digest {
        id.push_str(&format!("{byte:02x}"));
    }
    Ok(Scope::new(NAMESPACE_WORKSPACE, id))
}

fn contains_path_control(path: &str) -> bool {
    path.chars()
        .any(|character| character as u32 <= 31 || (127..=159).contains(&(character as u32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_scope_requires_an_opaque_installation_id() {
        assert_eq!(
            new_user_scope("install-1").expect("scope"),
            Scope::new(NAMESPACE_USER, "install-1")
        );
        assert!(new_user_scope("").is_err());
        assert!(new_user_scope("has space").is_err());
    }

    #[test]
    fn workspace_scope_prefers_the_stable_override() {
        let scope = new_workspace_scope("/nonexistent", "team-shared").expect("scope");
        assert_eq!(scope, Scope::new(NAMESPACE_WORKSPACE, "team-shared"));
        assert!(new_workspace_scope("/nonexistent", "bad override").is_err());
    }

    #[test]
    fn workspace_scope_digests_the_physical_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().to_str().expect("utf-8");
        let scope = new_workspace_scope(path, "").expect("scope");
        assert_eq!(scope.namespace, NAMESPACE_WORKSPACE);
        assert!(scope.id.starts_with("sha256:"));
        assert_eq!(scope.id.len(), 7 + 64);

        let physical = std::fs::canonicalize(path).expect("canonical");
        let digest = Sha256::digest(physical.to_str().expect("utf-8").as_bytes());
        let expected: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(scope.id, format!("sha256:{expected}"));
    }

    #[test]
    fn workspace_scope_rejects_control_characters() {
        assert!(new_workspace_scope("/tmp/a\u{1}b", "").is_err());
        assert!(new_workspace_scope("", "").is_err());
    }
}
