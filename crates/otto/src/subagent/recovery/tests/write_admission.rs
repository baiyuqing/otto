//! Write admission must reject changed evidence before repair has any effects.
use super::*;

#[test]
fn fifo_inventory_is_bounded_and_reports_incomplete_coverage() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let parent_path = std::path::PathBuf::from(parent.path());
    std::fs::create_dir_all(parent_path.with_extension("")).unwrap();
    let fifo = parent_path.with_extension("").join("t9-fifo.jsonl");
    assert!(
        std::process::Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender.send(interrupted::inventory(&parent_path)).unwrap();
    });
    // A regression leaves a blocked reader only in a detached test thread;
    // never join it on timeout or open a blocking FIFO writer in the harness.
    let inventory = receiver
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("FIFO must not block recovery inventory");
    worker.join().unwrap();
    assert!(!inventory.complete);
    assert!(inventory.records.is_empty());
    assert!(
        inventory
            .diagnostics
            .iter()
            .any(|message| message.contains("not a regular bounded transcript"))
    );
    assert_eq!(inventory.highest_counter, 9);
}

#[test]
fn checked_repair_rejects_replacement_symlink_and_changed_parent_linkage() {
    for change in [
        "replacement",
        "symlink",
        "parent-linkage",
        "task",
        "header",
        "attempt",
    ] {
        let root = tempfile::tempdir().unwrap();
        let parent = parent(root.path());
        let parent_path = std::path::PathBuf::from(parent.path());
        let store = child(&parent_path, "t1");
        let path = std::path::PathBuf::from(store.path());
        drop(store);
        let record = interrupted::inventory(&parent_path).records.remove(0);
        let original = std::fs::read(&path).unwrap();
        let old = path.with_extension("saved");
        if matches!(change, "parent-linkage" | "task" | "header" | "attempt") {
            let text = String::from_utf8(original.clone()).unwrap();
            let changed = match change {
                "parent-linkage" => text.replace(
                    &parent_path.to_string_lossy().to_string(),
                    "/unrelated-parent.jsonl",
                ),
                "task" => text.replace("\"id\":\"t1\"", "\"id\":\"t9\""),
                "header" => text.replace("childt1", "childt9"),
                "attempt" => text.replace("attempt-t1", "attempt-t9"),
                _ => unreachable!(),
            };
            assert_ne!(changed, text);
            std::fs::write(&path, changed).unwrap();
        } else {
            std::fs::rename(&path, &old).unwrap();
            if change == "symlink" {
                std::os::unix::fs::symlink(&old, &path).unwrap();
            } else {
                // A valid replacement with an incomplete tail would otherwise
                // be truncated during Store::open, even before a result append.
                let mut replacement = original.clone();
                replacement.extend_from_slice(b"{\"partial\":");
                std::fs::write(&path, replacement).unwrap();
            }
        }
        let before = std::fs::read(&path).unwrap();
        assert!(
            interrupted::mark_interrupted_with_writer(&record, None).is_err(),
            "{change}"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "must not repair or append: {change}"
        );
        if old.exists() {
            assert_eq!(std::fs::read(&old).unwrap(), original);
        }
        assert!(
            interrupted::open_validated(&record).is_err(),
            "resume admission: {change}"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn checked_repair_rejects_replaced_parent_directory() {
    let root = tempfile::tempdir().unwrap();
    let parent = parent(root.path());
    let parent_path = std::path::PathBuf::from(parent.path());
    let store = child(&parent_path, "t1");
    let path = std::path::PathBuf::from(store.path());
    drop(store);
    let record = interrupted::inventory(&parent_path).records.remove(0);
    let directory = parent_path.with_extension("");
    let saved = root.path().join("saved-children");
    std::fs::rename(&directory, &saved).unwrap();
    std::os::unix::fs::symlink(&saved, &directory).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(interrupted::open_validated(&record).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
}
