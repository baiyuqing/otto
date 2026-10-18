//! How a record's scope ID is derived.
//!
//! The default workspace scope ID is `<name>-<12 hex>`: `<name>` is the
//! sanitized last component of the physical workspace path and `<12 hex>` is
//! the first 12 lowercase hex digits of the SHA-256 of the full path, so the
//! same directory always derives the same ID. Markers that already hold a
//! `sha256:<64 hex>` ID keep it.

use sha2::{Digest, Sha256};

use super::contracts::{
    Error, MAX_SCOPE_ID_BYTES, NAMESPACE_WORKSPACE, Result, Scope, invalid_request, valid_opaque_id,
};

/// `stable_override` wins when set. Otherwise the path is made absolute, its
/// symlinks are resolved, and the ID is `workspace_id` of the result.
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
    let id = workspace_id(physical);
    Ok(Scope::new(NAMESPACE_WORKSPACE, id))
}

/// Default workspace ID for a canonical path: `<name>-<12 hex of SHA-256(path)>`.
pub fn workspace_id(physical: &str) -> String {
    named_workspace_id(
        physical,
        &format!("{:x}", Sha256::digest(physical.as_bytes())),
    )
}

/// `<sanitized last component of path>-<first 12 chars of hex>`. The name keeps
/// `[A-Za-z0-9._]`, maps other characters to `-`, collapses and trims `-`,
/// is cut at 64 bytes, and falls back to `workspace` when empty.
pub fn named_workspace_id(path: &str, hex: &str) -> String {
    let base = std::path::Path::new(path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let mapped: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let joined = mapped
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    let name = joined[..joined.len().min(64)].trim_end_matches('-');
    let name = if name.is_empty() { "workspace" } else { name };
    format!("{name}-{}", &hex[..12])
}

fn contains_path_control(path: &str) -> bool {
    path.chars()
        .any(|character| character as u32 <= 31 || (127..=159).contains(&(character as u32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_scope_prefers_the_stable_override() {
        let scope = new_workspace_scope("/nonexistent", "team-shared").expect("scope");
        assert_eq!(scope, Scope::new(NAMESPACE_WORKSPACE, "team-shared"));
        assert!(new_workspace_scope("/nonexistent", "bad override").is_err());
    }

    #[test]
    fn workspace_scope_names_and_digests_the_physical_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().to_str().expect("utf-8");
        let scope = new_workspace_scope(path, "").expect("scope");
        assert_eq!(scope.namespace, NAMESPACE_WORKSPACE);

        let physical = std::fs::canonicalize(path).expect("canonical");
        let digest = Sha256::digest(physical.to_str().expect("utf-8").as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        // tempfile names are `.tmp` plus ASCII alphanumerics, which the
        // sanitizer keeps unchanged.
        let name = physical.file_name().expect("name").to_str().expect("utf-8");
        assert_eq!(scope.id, format!("{name}-{}", &hex[..12]));
    }

    #[test]
    fn workspace_names_are_sanitized_and_bounded() {
        let hex = "0123456789abcdef".repeat(4);
        let cases = [
            ("/a/my project!", "my-project-0123456789ab"),
            ("/a/--x__y..z--", "x__y..z-0123456789ab"),
            ("/a/a - - b", "a-b-0123456789ab"),
            ("/a/项目", "workspace-0123456789ab"),
            ("/a/项目x", "x-0123456789ab"),
            ("/", "workspace-0123456789ab"),
        ];
        for (path, want) in cases {
            assert_eq!(named_workspace_id(path, &hex), want, "{path}");
        }
        let long = format!("/a/{}", "n".repeat(200));
        let id = named_workspace_id(&long, &hex);
        assert_eq!(id, format!("{}-0123456789ab", "n".repeat(64)));
        let dashed = format!("/a/{}-{}", "n".repeat(63), "m".repeat(50));
        let id = named_workspace_id(&dashed, &hex);
        assert_eq!(id, format!("{}-0123456789ab", "n".repeat(63)));
        for (path, _) in cases {
            assert!(valid_opaque_id(
                &named_workspace_id(path, &hex),
                MAX_SCOPE_ID_BYTES
            ));
        }
        assert!(valid_opaque_id(&workspace_id(&long), MAX_SCOPE_ID_BYTES));
    }

    #[test]
    fn workspace_scope_rejects_control_characters() {
        assert!(new_workspace_scope("/tmp/a\u{1}b", "").is_err());
        assert!(new_workspace_scope("", "").is_err());
    }
}
