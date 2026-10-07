//! Discovery of linked Git worktrees admitted to one sandbox profile.
//!
//! Git itself reports linked worktrees from the current repository's common
//! directory. We canonicalize every reported directory before granting it; a
//! failed discovery deliberately grants nothing beyond the selected workspace.

use std::path::Path;
use std::process::{Command, Stdio};

/// Canonical worktree roots registered with `workspace`'s Git common directory.
///
/// The returned list includes `workspace`. It never trusts a naming convention
/// such as `.worktree`; Git's porcelain listing is the membership authority.
pub fn managed(workspace: &Path) -> Vec<String> {
    let Ok(workspace) = std::fs::canonicalize(workspace) else {
        return Vec::new();
    };
    let mut roots = vec![workspace.to_string_lossy().into_owned()];
    let output = Command::new("/usr/bin/git")
        .current_dir(&workspace)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "worktree",
            "list",
            "--porcelain",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else { return roots };
    if !output.status.success() {
        return roots;
    }
    roots.extend(parse_worktree_list(&String::from_utf8_lossy(
        &output.stdout,
    )));
    roots.sort();
    roots.dedup();
    roots
}

fn parse_worktree_list(output: &str) -> Vec<String> {
    let mut roots = Vec::new();
    for line in output.lines() {
        let Some(path) = line.strip_prefix("worktree ") else {
            continue;
        };
        let Ok(path) = std::fs::canonicalize(path) else {
            continue;
        };
        if path.is_dir() {
            let path = path.to_string_lossy().into_owned();
            roots.push(path);
        }
    }
    roots
}

#[cfg(test)]
mod tests {
    use super::parse_worktree_list;

    #[test]
    fn accepts_only_existing_worktree_records() {
        let root = tempfile::tempdir().expect("temp root");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).expect("first");
        std::fs::create_dir(&second).expect("second");
        let output = format!(
            "worktree {}\nHEAD a\n\nworktree {}\nHEAD b\n\nworktree {}/gone\n",
            first.display(),
            second.display(),
            root.path().display()
        );
        let roots = parse_worktree_list(&output);
        assert_eq!(roots.len(), 2);
        assert!(roots.iter().any(|path| path.ends_with("/first")));
        assert!(roots.iter().any(|path| path.ends_with("/second")));
    }
}
