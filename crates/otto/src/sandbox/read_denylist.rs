//! The sandbox read denylist.
//!
//! A confined command reads every host path except the paths listed here: the
//! built-in entries, the `[sandbox] deny_read_paths` entries and every private
//! sandbox state root under the user cache directory. The `[sandbox]
//! read_paths` entries are exceptions inside that set. The Seatbelt profile
//! renders the list; the bash tool and `/sandbox allow` call
//! [`ReadDenylist::covers`] to decide whether a read grant changes anything.
//! The design record is `docs/specs/2026-11-01-sandbox-read-denylist.md`.

use std::path::{Path, PathBuf};

/// Denied paths relative to the home directory, in the order the profile
/// renders them. Each entry is denied as a subtree whether it exists or not.
pub(crate) const BUILT_IN_DENY_PATHS: &[&str] = &[
    // Credentials of tools and services.
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".config/gcloud",
    ".kube",
    ".docker",
    ".netrc",
    ".git-credentials",
    ".npmrc",
    ".pypirc",
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".gem/credentials",
    ".m2/settings.xml",
    ".terraform.d/credentials.tfrc.json",
    ".config/gh",
    // Credentials of coding agents, Otto's own included.
    ".otto/auth",
    ".claude/.credentials.json",
    ".codex/auth.json",
    // Shell history.
    ".zsh_history",
    ".bash_history",
    ".zsh_sessions",
    // Personal data kept by macOS and browsers.
    "Library/Keychains",
    "Library/Cookies",
    "Library/Safari",
    "Library/Application Support/Google/Chrome",
    "Library/Application Support/Firefox",
    "Library/Messages",
    "Library/Mail",
];

/// Every private sandbox state root under the user cache directory is named
/// with this prefix. Every root is read-denied except the running session's
/// own `tmp` and `cache` directories.
pub(crate) const PRIVATE_ROOT_LEAF_PREFIX: &str = "otto-sandbox-";

/// The user cache directory relative to the home directory; it holds the
/// private state roots.
const PRIVATE_ROOT_CACHE_BASE: [&str; 2] = ["Library", "Caches"];

/// The read denylist of one sandbox: the home directory the built-in entries
/// are relative to and the configured `deny_read_paths`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadDenylist {
    home: PathBuf,
    deny_read_paths: Vec<String>,
}

impl ReadDenylist {
    /// `home` is canonicalized when it exists, so canonical paths compare
    /// against it; `deny_read_paths` are the validated configuration entries.
    pub fn new(home: impl Into<PathBuf>, deny_read_paths: Vec<String>) -> Self {
        let home: PathBuf = home.into();
        let home = std::fs::canonicalize(&home).unwrap_or(home);
        Self {
            home,
            deny_read_paths,
        }
    }

    /// The home directory the built-in entries are relative to.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// The configured `[sandbox] deny_read_paths`, as written.
    pub fn deny_read_paths(&self) -> &[String] {
        &self.deny_read_paths
    }

    /// The user cache directory that holds the private state roots.
    pub(crate) fn cache_base(&self) -> PathBuf {
        PRIVATE_ROOT_CACHE_BASE
            .iter()
            .fold(self.home.clone(), |path, component| path.join(component))
    }

    /// `~/x` relative to the home directory, an absolute path as written, and
    /// `None` for anything else.
    pub(crate) fn expand_entry(&self, entry: &str) -> Option<PathBuf> {
        if let Some(rest) = entry.strip_prefix("~/") {
            return Some(self.home.join(rest));
        }
        Path::new(entry).is_absolute().then(|| PathBuf::from(entry))
    }

    /// Every denied subtree as written: the built-in entries below the home
    /// directory, then the configured entries. The private state roots are not
    /// listed; [`ReadDenylist::covers`] matches them by name prefix.
    pub fn entries(&self) -> Vec<PathBuf> {
        BUILT_IN_DENY_PATHS
            .iter()
            .map(|relative| self.home.join(relative))
            .chain(
                self.deny_read_paths
                    .iter()
                    .filter_map(|entry| self.expand_entry(entry)),
            )
            .collect()
    }

    /// Whether reading `canonical_path` is denied: it lies in a private state
    /// root, or at or below an entry in the entry's written spelling or in the
    /// spelling the entry canonicalizes to when it exists.
    pub fn covers(&self, canonical_path: &Path) -> bool {
        if self.within_private_root(canonical_path) {
            return true;
        }
        self.entries().iter().any(|entry| {
            canonical_path.starts_with(entry)
                || std::fs::canonicalize(entry)
                    .is_ok_and(|canonical| canonical_path.starts_with(&canonical))
        })
    }

    fn within_private_root(&self, canonical_path: &Path) -> bool {
        let cache_base = self.cache_base();
        let cache_base = std::fs::canonicalize(&cache_base).unwrap_or(cache_base);
        let Ok(below) = canonical_path.strip_prefix(&cache_base) else {
            return false;
        };
        below.components().next().is_some_and(|leaf| {
            leaf.as_os_str()
                .to_str()
                .is_some_and(|leaf| leaf.starts_with(PRIVATE_ROOT_LEAF_PREFIX))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical_home() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().expect("temp dir");
        let home = std::fs::canonicalize(temp.path()).expect("canonical temp dir");
        (temp, home)
    }

    #[test]
    fn built_in_entries_are_denied_below_the_home_directory() {
        let (_temp, home) = canonical_home();
        let denylist = ReadDenylist::new(home.clone(), Vec::new());
        assert!(denylist.covers(&home.join(".ssh/id_ed25519")));
        assert!(denylist.covers(&home.join("Library/Application Support/Google/Chrome/Default")));
        assert!(denylist.covers(&home.join(".otto/auth")));
        assert!(!denylist.covers(&home.join(".sshx/key")));
        assert!(!denylist.covers(&home.join(".otto/config.toml")));
        assert!(!denylist.covers(&home.join("project/src/main.rs")));
        assert!(!denylist.covers(Path::new("/usr/bin/true")));
        assert_eq!(denylist.entries().len(), BUILT_IN_DENY_PATHS.len());
    }

    #[test]
    fn configured_entries_match_by_written_and_canonical_spelling() {
        let (_temp, home) = canonical_home();
        let real = home.join("real-secrets");
        std::fs::create_dir_all(&real).expect("real directory");
        let link = home.join("secrets-link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        let link_text = link.to_str().expect("utf-8 link").to_string();
        let denylist = ReadDenylist::new(home.clone(), vec!["~/vault".to_string(), link_text]);

        assert!(denylist.covers(&home.join("vault/token")));
        assert!(denylist.covers(&real.join("key")));
        assert!(denylist.covers(&link.join("key")));
        assert!(!denylist.covers(&home.join("vault-public/readme")));
        assert_eq!(denylist.entries().len(), BUILT_IN_DENY_PATHS.len() + 2);
        assert_eq!(denylist.expand_entry("relative"), None);
        assert_eq!(denylist.expand_entry("~/vault"), Some(home.join("vault")));
    }

    #[test]
    fn private_state_roots_are_denied_and_other_caches_are_not() {
        let (_temp, home) = canonical_home();
        let denylist = ReadDenylist::new(home.clone(), Vec::new());
        let caches = home.join("Library/Caches");
        assert_eq!(denylist.cache_base(), caches);
        assert!(denylist.covers(&caches.join("otto-sandbox-0123abcd/tmp/file")));
        assert!(denylist.covers(&caches.join("otto-sandbox-0123abcd")));
        assert!(!denylist.covers(&caches.join("go-build/object")));
        assert!(!denylist.covers(&caches));
        assert!(!denylist.covers(&home.join("other/otto-sandbox-x/file")));
    }
}
