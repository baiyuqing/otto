//! Complete deletion target discovery through the public repository boundary.
use super::*;
use crate::session::{DeleteCandidate, SessionRepository};
use crate::session::{PosixSessionRepository, lease};
use std::os::unix::fs::{PermissionsExt, symlink};

fn discover(root: &Path, workspace: &Path) -> Result<Vec<DeleteCandidate>, String> {
    let repository = PosixSessionRepository::new(
        Arc::new(PosixDriver),
        lease::Keeper::new(Box::new(|| {}), std::time::Duration::from_secs(1)),
    );
    repository.list_for_delete(root, &workspace.to_string_lossy())
}

#[test]
fn list_for_delete_includes_active_archive_and_more_than_twenty_without_deduplication() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 25);
    let directory = Path::new(&paths[0]).parent().unwrap();
    let archive = directory.join("archive");
    fs::create_dir(&archive).unwrap();
    let archived_path = archive.join("duplicate.jsonl");
    fs::copy(&paths[0], &archived_path).unwrap();
    let duplicate_path = directory.join("another.jsonl");
    fs::copy(&paths[0], &duplicate_path).unwrap();

    let candidates = discover(&root, &workspace).unwrap();
    assert_eq!(candidates.len(), 27);
    assert_eq!(candidates.iter().filter(|row| row.archived).count(), 1);
    let duplicates: Vec<_> = candidates
        .iter()
        .filter(|row| row.id == "session-0000")
        .collect();
    assert_eq!(
        duplicates.len(),
        3,
        "all different paths with one ID remain visible"
    );
    assert!(
        duplicates
            .iter()
            .any(|row| row.path == archived_path.to_string_lossy() && row.archived)
    );
    assert!(
        duplicates
            .iter()
            .any(|row| row.path == duplicate_path.to_string_lossy() && !row.archived)
    );
}

#[test]
fn list_for_delete_preserves_full_exact_names_and_global_name_conflicts() {
    let temp = TempDir::new();
    let workspace = temp.join("workspace");
    fs::create_dir(&workspace).unwrap();
    let root = temp.join("root");
    let full_name = format!("{}\nsecond line", "a".repeat(300));
    for index in 0..2 {
        let mut header = test_header(&workspace);
        header.id = format!("named-{index}");
        let store = Store::create(&root, header).unwrap();
        store.append_message(&user("initialize")).unwrap();
        store.rename(&full_name).unwrap();
        let path = store.path();
        store.close().unwrap();
        if index == 1 {
            let directory = Path::new(&path).parent().unwrap();
            fs::create_dir(directory.join("archive")).unwrap();
            fs::rename(&path, directory.join("archive").join("named.jsonl")).unwrap();
        }
    }
    let candidates = discover(&root, &workspace).unwrap();
    assert_eq!(candidates.len(), 2);
    assert!(candidates.iter().all(|row| row.name == full_name));
    assert_ne!(candidates[0].id, candidates[1].id);
    assert_ne!(candidates[0].archived, candidates[1].archived);
}

#[test]
fn list_for_delete_fails_closed_for_corrupt_active_archive_and_incomplete_tail() {
    for archived in [false, true] {
        for corrupt in [b"not json\n".as_slice(), b"{\"type\":".as_slice()] {
            let temp = TempDir::new();
            let (root, workspace, paths) = seeded_workspace(&temp, 2);
            let directory = Path::new(&paths[0]).parent().unwrap();
            let directory = if archived {
                let archive = directory.join("archive");
                fs::create_dir(&archive).unwrap();
                archive
            } else {
                directory.to_owned()
            };
            let poison = directory.join("poison.jsonl");
            let mut bytes = fs::read(&paths[0]).unwrap();
            bytes.extend_from_slice(corrupt);
            fs::write(&poison, bytes).unwrap();
            assert!(
                discover(&root, &workspace).is_err(),
                "no partial result: {poison:?}"
            );
        }
    }
}

#[test]
fn list_for_delete_fails_closed_for_unreadable_files_and_directories() {
    for target in ["file", "workspace", "archive"] {
        let temp = TempDir::new();
        let (root, workspace, paths) = seeded_workspace(&temp, 2);
        let directory = Path::new(&paths[0]).parent().unwrap();
        let denied = match target {
            "file" => PathBuf::from(&paths[0]),
            "workspace" => directory.to_owned(),
            _ => {
                let archive = directory.join("archive");
                fs::create_dir(&archive).unwrap();
                archive
            }
        };
        let original = fs::metadata(&denied).unwrap().permissions();
        fs::set_permissions(&denied, fs::Permissions::from_mode(0o0)).unwrap();
        let result = discover(&root, &workspace);
        fs::set_permissions(&denied, original).unwrap();
        assert!(result.is_err(), "unreadable {target} must fail closed");
    }
}

#[test]
fn list_for_delete_rejects_symlink_root_workspace_archive_and_journals() {
    for target in ["root", "workspace", "archive", "file", "archived-file"] {
        let temp = TempDir::new();
        let (root, workspace, paths) = seeded_workspace(&temp, 1);
        let directory = Path::new(&paths[0]).parent().unwrap();
        match target {
            "root" => {
                let moved = temp.join("moved-root");
                fs::rename(&root, &moved).unwrap();
                symlink(&moved, &root).unwrap();
            }
            "workspace" => {
                let moved = temp.join("moved-directory");
                fs::rename(directory, &moved).unwrap();
                symlink(&moved, directory).unwrap();
            }
            "archive" => symlink(directory, directory.join("archive")).unwrap(),
            "file" => symlink(&paths[0], directory.join("linked.jsonl")).unwrap(),
            _ => {
                let archive = directory.join("archive");
                fs::create_dir(&archive).unwrap();
                symlink(&paths[0], archive.join("linked.jsonl")).unwrap();
            }
        }
        assert!(
            discover(&root, &workspace).is_err(),
            "must reject {target} symlink"
        );
    }
}

#[test]
fn list_for_delete_fails_closed_for_foreign_workspace_and_non_regular_journal() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let directory = Path::new(&paths[0]).parent().unwrap();
    let other = temp.join("other-workspace");
    fs::create_dir(&other).unwrap();
    let store = Store::create(temp.join("staging"), test_header(&other)).unwrap();
    store.append_message(&user("foreign")).unwrap();
    let foreign = store.path();
    store.close().unwrap();
    let poison = directory.join("foreign.jsonl");
    fs::copy(foreign, &poison).unwrap();
    assert!(discover(&root, &workspace).is_err());
    fs::remove_file(poison).unwrap();
    fs::create_dir(directory.join("not-regular.jsonl")).unwrap();
    assert!(discover(&root, &workspace).is_err());
}

#[test]
fn list_for_delete_missing_workspace_directory_is_empty_and_read_only() {
    let temp = TempDir::new();
    let root = temp.join("root");
    let workspace = temp.join("workspace");
    fs::create_dir(&root).unwrap();
    fs::create_dir(&workspace).unwrap();
    assert!(discover(&root, &workspace).unwrap().is_empty());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
}
