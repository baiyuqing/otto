use std::path::Path;
use std::process::Command;

/// Runs `git <args>` in the crate directory and returns trimmed stdout, or
/// `None` when git is missing, fails, or prints nothing.
/// Cargo starts build scripts in the package directory. `env!` is not used
/// for it: a compiled build script is reused from a shared target directory
/// by other checkouts, and a compile-time path names the checkout that
/// compiled it.
/// `GIT_OPTIONAL_LOCKS=0` stops `git status` from rewriting the index, so a
/// build neither takes `index.lock` from a concurrent git command nor touches
/// the watched index file.
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Emits rerun-if-changed for a git-resolved path, relative to the package
/// directory. Cargo reruns the script on every build when a listed path does
/// not exist, so missing paths are skipped.
fn watch_git_path(name: &str) {
    if let Some(path) = git(&["rev-parse", "--git-path", name]) {
        if Path::new(&path).exists() {
            println!("cargo:rerun-if-changed={path}");
        }
    }
}

fn build_metadata() -> String {
    if let Some(hash) = git(&["rev-parse", "--short=12", "HEAD"]) {
        let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some();
        return if dirty { format!("{hash}.dirty") } else { hash };
    }
    match std::env::var("OTTO_GIT_HASH") {
        Ok(value) if !value.is_empty() => {
            let valid = value.split('.').all(|id| {
                !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            });
            assert!(
                valid,
                "OTTO_GIT_HASH={value:?} is not valid SemVer build metadata: use dot-separated identifiers of [0-9A-Za-z-]"
            );
            value
        }
        _ => "unknown".to_string(),
    }
}

fn main() {
    // The `.dirty` marker depends on the working tree, so rerun after any
    // source edit.
    println!("cargo:rerun-if-changed=../../crates");
    println!("cargo:rerun-if-env-changed=OTTO_GIT_HASH");

    // `--git-path` resolves HEAD, the index and refs for linked worktrees,
    // where `.git` is a file and HEAD is in `.git/worktrees/<name>`.
    watch_git_path("HEAD");
    watch_git_path("index");
    watch_git_path("packed-refs");
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        watch_git_path(&branch);
    }

    println!("cargo:rustc-env=OTTO_BUILD_METADATA={}", build_metadata());
}
