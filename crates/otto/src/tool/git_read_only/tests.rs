use super::*;

fn refusal(path: &str) -> io::Result<()> {
    refuse_git_read_only(Path::new(path).iter(), Path::new(path))
}

#[test]
fn git_metadata_at_any_depth_is_refused() {
    for path in [
        ".git",
        ".git/config",
        ".git/hooks/pre-commit",
        "sub/.git",
        "sub/.git/config",
        "sub/deeper/.git/hooks/post-checkout",
        ".git/modules/lib/foo/config",
        ".git/modules/lib/hooks/post-checkout",
        ".git/modules/lib/config.worktree",
        ".git/worktrees/dev/commondir",
        ".git/worktrees/dev/gitdir",
        ".git/worktrees/dev/config.worktree",
        ".GIT/modules/x/CONFIG",
        ".git/./worktrees/dev/gitdir",
    ] {
        let error = refusal(path).expect_err(path);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{path}");
        assert!(error.to_string().contains("read-only"), "{path}: {error}");
    }
}

#[test]
fn other_git_files_and_lookalike_names_are_allowed() {
    for path in [
        ".git/info/exclude",
        ".git/HEAD",
        ".git/worktrees/dev/HEAD",
        ".git/modules/lib/index",
        "src/config",
        "docs/hooks/readme.md",
        "sub/git/config",
        "foo.git/config",
        "config",
        "hooks",
    ] {
        assert!(refusal(path).is_ok(), "{path} must be allowed");
    }
}
