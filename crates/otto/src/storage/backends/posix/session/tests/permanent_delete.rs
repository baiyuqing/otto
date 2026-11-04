use super::*;
use crate::session::SessionRepository;
use crate::storage::backends::posix::{lease, session::PosixSessionRepository};

fn repository() -> PosixSessionRepository {
    PosixSessionRepository::new(
        Arc::new(PosixDriver),
        lease::Keeper::new(Box::new(|| {}), std::time::Duration::from_secs(1)),
    )
}

#[test]
fn permanent_delete_repository_cleans_children_without_following_symlinks() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let path = Path::new(&paths[0]);
    let outside = temp.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "safe").unwrap();
    fs::create_dir(path.with_extension("")).unwrap();
    symlink(&outside, path.with_extension("").join("external")).unwrap();
    for suffix in [
        "reminders.json",
        "inbox.json",
        "inputs.json",
        "followups.json",
        "turns.json",
    ] {
        fs::write(path.with_extension(suffix), "[]").unwrap();
    }
    lease::create_lease_dir(path, 30).unwrap();
    repository()
        .permanent_delete(&root, &workspace.to_string_lossy(), &paths[0])
        .unwrap();
    assert!(outside.join("keep").exists());
    assert!(!path.exists());
    assert!(!path.with_extension("").exists());
    assert!(!path.with_extension("lease").exists());
    for suffix in [
        "reminders.json",
        "inbox.json",
        "inputs.json",
        "followups.json",
        "turns.json",
    ] {
        assert!(!path.with_extension(suffix).exists());
    }
}

#[test]
fn permanent_delete_archived_session_cleans_transcripts_sidecars_and_lease() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 2);
    let original = Path::new(&paths[0]);
    fs::create_dir(original.with_extension("")).unwrap();
    fs::write(
        original.with_extension("").join("child.jsonl"),
        "transcript",
    )
    .unwrap();
    lease::create_lease_dir(original, 30).unwrap();
    repository()
        .archive(&root, &workspace.to_string_lossy(), &paths[0])
        .unwrap();
    let archived = original
        .parent()
        .unwrap()
        .join("archive")
        .join(original.file_name().unwrap());
    assert!(archived.with_extension("").join("child.jsonl").exists());
    assert!(archived.with_extension("lease").exists());
    let outside = temp.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), "safe").unwrap();
    symlink(&outside, archived.with_extension("").join("external")).unwrap();
    let suffixes = [
        "reminders.json",
        "inbox.json",
        "inputs.json",
        "followups.json",
        "turns.json",
    ];
    for suffix in suffixes {
        fs::write(archived.with_extension(suffix), "[]").unwrap();
    }
    repository()
        .permanent_delete(
            &root,
            &workspace.to_string_lossy(),
            &archived.to_string_lossy(),
        )
        .unwrap();
    assert!(!archived.exists());
    assert!(!archived.with_extension("").exists());
    assert!(!archived.with_extension("lease").exists());
    for suffix in suffixes {
        assert!(!archived.with_extension(suffix).exists());
    }
    assert!(outside.join("keep").exists());
    assert!(Path::new(&paths[1]).exists(), "active sibling is untouched");
}

#[test]
fn permanent_delete_rejects_nested_sibling_and_parent_escape_paths() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let directory = Path::new(&paths[0]).parent().unwrap();
    for relative in [
        "archive/nested/poison.jsonl",
        "sibling/poison.jsonl",
        "archive/archive/poison.jsonl",
    ] {
        let poison = directory.join(relative);
        fs::create_dir_all(poison.parent().unwrap()).unwrap();
        fs::copy(&paths[0], &poison).unwrap();
        assert!(
            repository()
                .permanent_delete(
                    &root,
                    &workspace.to_string_lossy(),
                    &poison.to_string_lossy()
                )
                .is_err()
        );
        assert!(poison.exists());
    }
    let escape = directory
        .join("archive/../")
        .join(Path::new(&paths[0]).file_name().unwrap());
    assert!(
        repository()
            .permanent_delete(
                &root,
                &workspace.to_string_lossy(),
                &escape.to_string_lossy()
            )
            .is_err()
    );
    assert!(Path::new(&paths[0]).exists());
}

#[test]
fn permanent_delete_rejects_archive_directory_and_journal_symlink_escapes() {
    use std::os::unix::fs::symlink;
    for directory_link in [false, true] {
        let temp = TempDir::new();
        let (root, workspace, paths) = seeded_workspace(&temp, 1);
        let directory = Path::new(&paths[0]).parent().unwrap();
        let archive = directory.join("archive");
        let outside = temp.join("outside");
        fs::create_dir(&outside).unwrap();
        let saved = outside.join("saved.jsonl");
        fs::copy(&paths[0], &saved).unwrap();
        if directory_link {
            symlink(&outside, &archive).unwrap();
        } else {
            fs::create_dir(&archive).unwrap();
            symlink(&saved, archive.join("saved.jsonl")).unwrap();
        }
        assert!(
            repository()
                .permanent_delete(
                    &root,
                    &workspace.to_string_lossy(),
                    &archive.join("saved.jsonl").to_string_lossy()
                )
                .is_err()
        );
        assert!(saved.exists());
        assert!(Path::new(&paths[0]).exists());
    }
}

#[test]
fn permanent_delete_repository_refuses_active_writer_and_lease() {
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let (writer, _) = Store::open(&paths[0]).unwrap();
    assert!(
        repository()
            .permanent_delete(&root, &workspace.to_string_lossy(), &paths[0])
            .is_err()
    );
    assert!(Path::new(&paths[0]).exists());
    writer.close().unwrap();
    lease::create_lease_dir(Path::new(&paths[0]), 30).unwrap();
    let keeper = lease::Keeper::new(Box::new(|| {}), std::time::Duration::from_secs(1));
    let (holder, _) = lease::Lease::acquire_with_keeper(Path::new(&paths[0]), &keeper).unwrap();
    let error = repository()
        .permanent_delete(&root, &workspace.to_string_lossy(), &paths[0])
        .unwrap_err();
    assert!(error.contains("held") || error.contains("busy"), "{error}");
    holder.check().unwrap();
    assert!(Path::new(&paths[0]).exists());
    holder.release().unwrap();
    repository()
        .permanent_delete(&root, &workspace.to_string_lossy(), &paths[0])
        .unwrap();
}

#[test]
fn permanent_delete_repository_failure_never_reports_success() {
    use std::os::unix::fs::PermissionsExt;
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let path = Path::new(&paths[0]);
    let children = path.with_extension("");
    fs::create_dir(&children).unwrap();
    fs::write(children.join("child"), "protected").unwrap();
    fs::set_permissions(&children, fs::Permissions::from_mode(0o000)).unwrap();
    let error = repository()
        .permanent_delete(&root, &workspace.to_string_lossy(), &paths[0])
        .unwrap_err();
    fs::set_permissions(&children, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(error.contains("permanent delete"), "{error}");
    assert!(
        path.exists(),
        "journal remains a retry anchor after incomplete cleanup"
    );
}

#[test]
fn permanent_delete_repository_rejects_symlinked_journal() {
    use std::os::unix::fs::symlink;
    let temp = TempDir::new();
    let (root, workspace, paths) = seeded_workspace(&temp, 1);
    let outside = temp.join("outside.jsonl");
    fs::rename(&paths[0], &outside).unwrap();
    symlink(&outside, &paths[0]).unwrap();
    assert!(
        repository()
            .permanent_delete(&root, &workspace.to_string_lossy(), &paths[0])
            .is_err()
    );
    assert!(outside.exists());
}
