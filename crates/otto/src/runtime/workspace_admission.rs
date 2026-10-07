//! Canonical workspace admission shared by multi-workspace hosts and delivery.
//!
//! Admission never grants a path because a model requested it. Callers supply
//! only human-configured canonical roots and trusted project paths; this module
//! canonicalizes the requested directory before component-aware containment
//! checks so symlink escapes and lookalike prefixes are rejected.

use std::path::{Path, PathBuf};

use super::sandbox_runtime::canonical_directory;

/// Why a requested workspace path was rejected.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// The path does not resolve to an existing directory.
    Invalid,
    /// A real directory, but outside every admitted root.
    NotAdmitted,
}

/// Canonicalizes `requested` and admits it when it is `startup` or a
/// descendant of one configured root or trusted project. `requested` must be
/// absolute; it is never resolved relative to Otto's process directory.
pub fn admit_workspace(
    requested: &str,
    startup: &Path,
    roots: &[PathBuf],
    trusted: &[PathBuf],
) -> Result<PathBuf, Admission> {
    if !Path::new(requested).is_absolute() {
        return Err(Admission::Invalid);
    }
    let canonical = canonical_directory(Path::new(requested)).map_err(|_| Admission::Invalid)?;
    if canonical == startup
        || roots.iter().any(|root| canonical.starts_with(root))
        || trusted
            .iter()
            .any(|directory| canonical.starts_with(directory))
    {
        Ok(canonical)
    } else {
        Err(Admission::NotAdmitted)
    }
}

/// Canonicalizes configured workspace roots at startup.
pub fn canonicalize_workspace_roots(raw_roots: &[String]) -> Result<Vec<PathBuf>, String> {
    raw_roots
        .iter()
        .map(|root| {
            canonical_directory(Path::new(root))
                .map_err(|error| format!("[server] workspace_roots: \"{root}\": {error}"))
        })
        .collect()
}

/// Canonical directories trusted through `otto trust`. Bad or missing config
/// resolves to no extra admission, never to a broader grant.
pub fn trusted_projects(config_path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(config_path) else {
        return Vec::new();
    };
    let Ok(file) = otto_core::config::parse(&text) else {
        return Vec::new();
    };
    file.projects
        .keys()
        .filter_map(|path| canonical_directory(Path::new(path)).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admits_only_canonical_descendants_of_human_roots() {
        let temp = tempfile::tempdir().unwrap();
        let startup = temp.path().join("startup");
        let root = temp.path().join("root");
        let child = root.join("child");
        let sibling = temp.path().join("root-other");
        for path in [&startup, &root, &child, &sibling] {
            std::fs::create_dir_all(path).unwrap();
        }
        let startup = std::fs::canonicalize(startup).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        assert_eq!(
            admit_workspace(
                &child.to_string_lossy(),
                &startup,
                std::slice::from_ref(&root),
                &[]
            )
            .unwrap(),
            std::fs::canonicalize(child).unwrap()
        );
        assert_eq!(
            admit_workspace(&sibling.to_string_lossy(), &startup, &[root], &[]),
            Err(Admission::NotAdmitted)
        );
    }

    #[test]
    fn rejects_relative_and_symlink_escape_requests() {
        let temp = tempfile::tempdir().unwrap();
        let startup = temp.path().join("startup");
        let root = temp.path().join("root");
        let outside = temp.path().join("outside");
        std::fs::create_dir_all(&startup).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
        let startup = std::fs::canonicalize(startup).unwrap();
        let root = std::fs::canonicalize(root).unwrap();
        assert_eq!(
            admit_workspace("relative", &startup, &[root.clone()], &[]),
            Err(Admission::Invalid)
        );
        #[cfg(unix)]
        assert_eq!(
            admit_workspace(
                &root.join("escape").to_string_lossy(),
                &startup,
                &[root],
                &[]
            ),
            Err(Admission::NotAdmitted)
        );
    }
}
