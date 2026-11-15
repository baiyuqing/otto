//! Build version: the binary reports the commit it was built from, and every
//! file that carries the release version agrees with `crates/otto`.

use std::path::PathBuf;
use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn root_file(path: &str) -> String {
    let full = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path);
    std::fs::read_to_string(&full).unwrap_or_else(|e| panic!("read {}: {e}", full.display()))
}

fn is_build_metadata(text: &str) -> bool {
    text.split('.')
        .all(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
}

fn head_hash() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())
        .flatten()
        .map(|s| s.trim().to_string())
}

#[test]
fn binary_version_flag_prints_semver_with_build_metadata() {
    let out = Command::new(env!("CARGO_BIN_EXE_otto"))
        .arg("--version")
        .output()
        .expect("run otto --version");
    assert!(out.status.success(), "exit status {:?}", out.status);
    let stdout = String::from_utf8(out.stdout).expect("utf-8");
    let metadata = stdout
        .strip_prefix(&format!("otto {VERSION}+"))
        .unwrap_or_else(|| panic!("unexpected output {stdout:?}"))
        .strip_suffix('\n')
        .expect("trailing newline");
    assert!(is_build_metadata(metadata), "invalid metadata {metadata:?}");
    assert_eq!(stdout, format!("otto {}\n", otto::VERSION));
}

/// Fails when build.rs did not rerun after a commit (wrong rerun-if-changed).
#[test]
fn embedded_hash_matches_git_head() {
    let Some(head) = head_hash() else {
        eprintln!("skipped: git unavailable or not a repository");
        return;
    };
    let metadata = otto::VERSION.split_once('+').expect("'+' in VERSION").1;
    let hash = metadata.strip_suffix(".dirty").unwrap_or(metadata);
    assert_eq!(hash, head, "otto::VERSION is stale relative to git HEAD");
}

fn lock_version(lock: &str, package: &str) -> String {
    let doc: toml::Table = lock.parse().expect("parse Cargo.lock");
    doc["package"]
        .as_array()
        .expect("[[package]] entries")
        .iter()
        .find(|p| p["name"].as_str() == Some(package))
        .unwrap_or_else(|| panic!("package {package} not in lock file"))["version"]
        .as_str()
        .expect("version string")
        .to_string()
}

#[test]
fn release_version_is_consistent_across_files() {
    let mut found: Vec<(String, String)> = Vec::new();
    let lock = root_file("Cargo.lock");
    for package in [
        "otto",
        "otto-core",
        "otto-platform",
        "otto-storage",
        "otto-client",
        "otto-terminal",
        "otto-server",
        "otto-acp",
        "otto-eval",
        "otto-cli",
    ] {
        found.push((
            format!("Cargo.lock ({package})"),
            lock_version(&lock, package),
        ));
    }
    for (file, version) in found {
        assert_eq!(
            version, VERSION,
            "{file} has version {version}, crates/otto has {VERSION}; run `make bump VERSION=X.Y.Z`"
        );
    }
}
