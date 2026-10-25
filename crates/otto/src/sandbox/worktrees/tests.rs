use super::{WorktreeRecord, linked_to_common_dir, managed, parse_worktree_list};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const GIT_BINARY: &str = "/usr/bin/git";

fn git_available() -> bool {
    if Path::new(GIT_BINARY).exists() {
        return true;
    }
    println!("skipped: /usr/bin/git missing");
    false
}

fn git_in(directory: &Path, args: &[&str]) -> String {
    let output = Command::new(GIT_BINARY)
        .current_dir(directory)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", directory)
        .args([
            "-c",
            "user.name=otto",
            "-c",
            "user.email=otto@example.invalid",
            "-c",
            "init.defaultBranch=main",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            // `git commit` spawns a detached `git maintenance run --auto`, whose
            // worktree-prune task removes a record whose gitdir target is missing.
            // The forged records below must survive until the listing runs.
            "-c",
            "maintenance.auto=false",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn fixture_repository(directory: &Path) -> PathBuf {
    std::fs::create_dir_all(directory).expect("create repository directory");
    git_in(directory, &["init", "-q"]);
    git_in(directory, &["commit", "-q", "--allow-empty", "-m", "init"]);
    std::fs::canonicalize(directory).expect("canonical repository")
}

fn canonical_sorted(paths: &[&Path]) -> Vec<String> {
    let mut roots: Vec<String> = paths
        .iter()
        .map(|path| {
            std::fs::canonicalize(path)
                .expect("canonical path")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    roots.sort();
    roots
}

/// Writes `.git/worktrees/<id>/{gitdir,HEAD,commondir}` as a sandboxed command could.
fn forge_worktree_record(repository: &Path, id: &str, gitdir_target: &Path) {
    let private_dir = repository.join(".git").join("worktrees").join(id);
    std::fs::create_dir_all(&private_dir).expect("create forged private dir");
    std::fs::write(
        private_dir.join("gitdir"),
        format!("{}\n", gitdir_target.display()),
    )
    .expect("gitdir");
    std::fs::write(private_dir.join("HEAD"), "ref: refs/heads/main\n").expect("HEAD");
    std::fs::write(private_dir.join("commondir"), "../..\n").expect("commondir");
}

/// `listed` is the path git prints for the forged record, not necessarily the victim.
fn assert_listing_contains(repository: &Path, listed: &Path) {
    let listing = git_in(repository, &["worktree", "list", "--porcelain"]);
    let listed_line = format!("worktree {}\n", listed.display());
    assert!(
        listing.contains(&listed_line),
        "git does not list the forged record {}:\n{listing}",
        listed.display()
    );
}

fn assert_only_workspace(repository: &Path, listed: &Path) {
    assert_listing_contains(repository, listed);
    assert_eq!(managed(repository), canonical_sorted(&[repository]));
}

#[test]
fn parses_records_with_attributes() {
    let output = "worktree /srv/bare.git\nbare\n\n\
        worktree /srv/gone\nHEAD abc\nbranch refs/heads/gone\nprunable gitdir file points to non-existent location\n\n\
        worktree /srv/with space/repo\nHEAD def\ndetached\nlocked reason\n";
    let records = parse_worktree_list(output);
    assert_eq!(
        records,
        vec![
            WorktreeRecord {
                path: "/srv/bare.git".into(),
                bare: true,
                prunable: false
            },
            WorktreeRecord {
                path: "/srv/gone".into(),
                bare: false,
                prunable: true
            },
            WorktreeRecord {
                path: "/srv/with space/repo".into(),
                bare: false,
                prunable: false
            },
        ]
    );
}

#[test]
fn admits_a_worktree_git_created() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().expect("temp root");
    let main = fixture_repository(&root.path().join("main"));
    git_in(&main, &["worktree", "add", "-q", "../linked"]);
    let linked = root.path().join("linked");
    let expected = canonical_sorted(&[&main, &linked]);
    assert_eq!(managed(&main), expected);
    assert_eq!(managed(&linked), expected);
}

#[test]
fn rejects_forged_gitdir_record_pointing_outside() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().expect("temp root");
    let main = fixture_repository(&root.path().join("main"));
    let victim = root.path().join("victim");
    std::fs::create_dir(&victim).expect("victim");
    forge_worktree_record(&main, "esc", &victim.join(".git"));
    assert_only_workspace(&main, &victim);
    let common_dir = std::fs::canonicalize(main.join(".git")).expect("common dir");
    assert!(!linked_to_common_dir(
        &std::fs::canonicalize(&victim).expect("victim"),
        &common_dir
    ));
}

#[test]
fn rejects_forged_record_pointing_at_another_repository() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().expect("temp root");
    let main = fixture_repository(&root.path().join("main"));
    let victim = fixture_repository(&root.path().join("victim"));
    forge_worktree_record(&main, "esc", &victim.join(".git"));
    assert_only_workspace(&main, &victim);
}

#[test]
fn rejects_forged_record_whose_gitfile_points_elsewhere() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().expect("temp root");
    let main = fixture_repository(&root.path().join("main"));
    let victim = root.path().join("victim");
    let elsewhere = root.path().join("elsewhere");
    std::fs::create_dir(&victim).expect("victim");
    std::fs::create_dir(&elsewhere).expect("elsewhere");
    std::fs::write(
        victim.join(".git"),
        format!("gitdir: {}\n", elsewhere.display()),
    )
    .expect("gitfile");
    forge_worktree_record(&main, "esc", &victim.join(".git"));
    assert_only_workspace(&main, &victim);
}

#[test]
fn rejects_symlinked_git_entry() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().expect("temp root");
    let main = fixture_repository(&root.path().join("main"));
    let victim = root.path().join("victim");
    std::fs::create_dir(&victim).expect("victim");
    std::os::unix::fs::symlink(main.join(".git"), victim.join(".git")).expect("symlink .git");
    forge_worktree_record(&main, "esc", &victim.join(".git"));
    assert_only_workspace(&main, &victim);
}

#[test]
fn rejects_record_reached_through_workspace_symlink() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().expect("temp root");
    let main = fixture_repository(&root.path().join("main"));
    let victim = root.path().join("victim");
    std::fs::create_dir(&victim).expect("victim");
    std::os::unix::fs::symlink(&victim, main.join("link")).expect("symlink link");
    forge_worktree_record(&main, "esc", &main.join("link").join(".git"));
    assert_only_workspace(&main, &main.join("link"));
    let common_dir = std::fs::canonicalize(main.join(".git")).expect("common dir");
    assert!(!linked_to_common_dir(
        &std::fs::canonicalize(&victim).expect("victim"),
        &common_dir
    ));
}

#[test]
fn ignores_prunable_records() {
    if !git_available() {
        return;
    }
    let root = tempfile::tempdir().expect("temp root");
    let main = fixture_repository(&root.path().join("main"));
    git_in(&main, &["worktree", "add", "-q", "../gone"]);
    let gone = root.path().join("gone");
    std::fs::remove_dir_all(&gone).expect("remove worktree");
    std::fs::create_dir(&gone).expect("recreate empty directory");
    let listing = git_in(&main, &["worktree", "list", "--porcelain"]);
    assert!(
        listing.contains("prunable"),
        "git does not mark the record prunable:\n{listing}"
    );
    assert_eq!(managed(&main), canonical_sorted(&[&main]));
}
