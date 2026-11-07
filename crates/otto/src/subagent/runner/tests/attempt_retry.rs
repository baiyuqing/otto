//! Production Runner retry after a rejected durable attempt append.
use super::*;

#[derive(Debug)]
struct RejectOnce(AtomicBool);

impl crate::session::WriterOwnership for RejectOnce {
    fn check(&self) -> Result<(), String> {
        if self.0.swap(false, Ordering::SeqCst) {
            Err("injected failure before attempt write".into())
        } else {
            Ok(())
        }
    }
    fn release(&self) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn resume_retries_same_child_after_attempt_append_fails_before_write() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent.jsonl");
    let path = interrupted_fixture(
        &parent,
        "t1",
        "original",
        None,
        true,
        false,
        interrupted::INTERRUPTED_STATUS,
    );
    // Add a real acknowledged attempt and its terminal boundary to the fixture.
    let (store, _) = crate::session::Store::open(&path).unwrap();
    store
        .append_custom_entry(
            "otto.task_attempt",
            r#"{"version":1,"taskId":"t1","attemptId":"original-attempt","lineage":"admitted"}"#,
        )
        .unwrap();
    store
        .append_custom_entry("otto.task_result", r#"{"status":"interrupted","error":""}"#)
        .unwrap();
    store.close().unwrap();
    let before = std::fs::read(&path).unwrap();
    let provider = FakeProvider::new();
    provider.add_route(match_any, vec![assistant_text("done", Usage::default())]);
    let tasks = Arc::new(Tasks::new());
    let mut config = config_with_children_dir(&provider, &tasks, parent.with_extension(""));
    let ownership = Arc::new(RejectOnce(AtomicBool::new(true)));
    config.lease = Some(Arc::new(move || Some(ownership.clone())));
    let (runner, _) = runner(config);
    let error = runner.resume("t1", "continue".into()).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("attempt boundary was not persisted")
    );
    let failed = tasks.get("t1").unwrap();
    assert_eq!(failed.status, TaskStatus::Interrupted);
    assert_eq!(failed.attempt_id, "original-attempt");
    assert_eq!(failed.session_path, path.to_string_lossy());
    assert!(
        provider.requests().is_empty(),
        "failed admission must never execute provider work"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        runner.resume("t1", "retry".into()),
    )
    .await
    .expect("retry must release failed store's writer lock")
    .expect("same-process retry");
    let done = wait_final(&tasks, "t1").await;
    assert_eq!(done.status, TaskStatus::Succeeded);
    assert_eq!(done.id, "t1");
    assert_eq!(done.session_path, failed.session_path);
    assert_ne!(done.attempt_id, "original-attempt");
    let after = std::fs::read_to_string(&path).unwrap();
    assert!(after.as_bytes().starts_with(&before));
    assert_eq!(after.matches("otto.task_spec").count(), 1);
    assert_eq!(after.matches("otto.task_attempt").count(), 2);
    assert_eq!(tasks.list().len(), 1);
    assert_eq!(provider.requests().len(), 1);
}

#[derive(Debug)]
struct RejectWithUncertainTail(std::path::PathBuf);

impl crate::session::WriterOwnership for RejectWithUncertainTail {
    fn check(&self) -> Result<(), String> {
        use std::io::Write;
        // Deterministic changed-evidence simulation at the append boundary.
        // This is not a claim of injecting an actual partial syscall write.
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.0)
            .unwrap();
        file.write_all(b"{\"uncertain\":").unwrap();
        Err("injected uncertain attempt persistence".into())
    }
    fn release(&self) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
async fn resume_does_not_roll_back_an_uncertain_changed_transcript() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("parent.jsonl");
    let path = interrupted_fixture(
        &parent,
        "t1",
        "original",
        None,
        true,
        false,
        interrupted::INTERRUPTED_STATUS,
    );
    let before = std::fs::read(&path).unwrap();
    let provider = FakeProvider::new();
    let tasks = Arc::new(Tasks::new());
    let mut config = config_with_children_dir(&provider, &tasks, parent.with_extension(""));
    let ownership = Arc::new(RejectWithUncertainTail(path.clone()));
    config.lease = Some(Arc::new(move || Some(ownership.clone())));
    let (runner, _) = runner(config);
    assert!(runner.resume("t1", "continue".into()).await.is_err());
    assert_eq!(tasks.get("t1").unwrap().status, TaskStatus::Unavailable);
    assert!(provider.requests().is_empty());
    let changed = std::fs::read(&path).unwrap();
    assert!(changed.starts_with(&before));
    assert!(changed.len() > before.len());
    assert!(runner.resume("t1", "retry".into()).await.is_err());
    assert_eq!(
        std::fs::read(&path).unwrap(),
        changed,
        "an unavailable retry must not repair even an incomplete tail"
    );
    assert!(provider.requests().is_empty());
}
