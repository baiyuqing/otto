//! Fault rows for the POSIX session store. The store is built
//! over a `FaultyFileDriver`; every reopen uses the real driver.
//!
//! A new session file receives its header (write 1) and runtime record
//! (write 2) when the first append creates it, so the first appended message
//! is journal write 3 and the second is write 4. Each write is followed by one
//! `sync_all`, numbered the same way.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto::faults::files::{FaultyFileDriver, FileOp};
use otto::faults::{Fault, FaultError, FaultPlan, Filter, Rule, Trigger};
use otto::session::{Prepared, Store};
use otto_core::model::{Block, Message, Role};
use otto_core::session::{Header, PiErrorKind};
use otto_platform::fsops::PosixDriver;

const EIO: FaultError = FaultError::Os(libc::EIO);

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    workspace: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("temp dir");
        let root = temp.path().join("root");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&root).expect("root");
        std::fs::create_dir_all(&workspace).expect("workspace");
        Self {
            _temp: temp,
            root,
            workspace,
        }
    }

    fn header(&self) -> Header {
        Header {
            id: "550e8400-e29b-41d4-a716-446655440000".into(),
            workspace: self.workspace.to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            profile: "local".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..Header::default()
        }
    }

    fn lazy_store(&self, plan: &Arc<FaultPlan<FileOp>>) -> Store {
        let driver = FaultyFileDriver::new(Arc::new(PosixDriver), Arc::clone(plan));
        Store::create_lazy_with_driver(&self.root, self.header(), Arc::new(driver))
            .expect("lazy store")
    }

    /// Every `.jsonl` file under the session root.
    fn journals(&self) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read dir") {
                let path = entry.expect("entry").path();
                if path.is_dir() {
                    walk(&path, out);
                } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                    out.push(path);
                }
            }
        }
        let mut found = Vec::new();
        walk(&self.root, &mut found);
        found
    }
}

fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block::text(text)],
        created_at: chrono::Utc::now(),
        ..Message::default()
    }
}

fn texts(store: &Store) -> Vec<String> {
    store
        .messages()
        .iter()
        .map(|message| message.blocks[0].text.clone())
        .collect()
}

fn journal_rule(op: FileOp, nth: u32, fault: Fault) -> Rule<FileOp> {
    Rule::new(op, Trigger::Nth(nth), fault).filter(Filter::NameContains(".jsonl"))
}

/// A store whose first message is durable and whose second append meets
/// `fault` on its `op` call number 4.
fn store_failing_second_append(
    fixture: &Fixture,
    op: FileOp,
    fault: Fault,
) -> (Store, Arc<FaultPlan<FileOp>>) {
    let plan = FaultPlan::new(vec![journal_rule(op, 4, fault)]);
    let store = fixture.lazy_store(&plan);
    store.append_message(&user("first")).expect("first append");
    (store, plan)
}

#[test]
fn before_eio_on_journal_write_poisons_the_store_and_reopen_keeps_earlier_records() {
    let fixture = Fixture::new();
    let (store, plan) = store_failing_second_append(&fixture, FileOp::Write, Fault::Before(EIO));
    let path = store.path();

    let error = store
        .append_message(&user("second"))
        .expect_err("append fails");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert_eq!(texts(&store), ["first"]);
    for refused in [store.append_message(&user("third")), store.rename("name")] {
        assert_eq!(
            refused.expect_err("poisoned").kind(),
            PiErrorKind::FatalPersistence
        );
    }
    assert_eq!(texts(&store), ["first"]);
    plan.assert_all_fired();

    drop(store);
    let (reopened, warnings) = Store::open(&path).expect("reopen");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(texts(&reopened), ["first"]);
}

#[test]
fn short_write_leaves_an_incomplete_final_line_that_reopen_truncates_with_a_warning() {
    let fixture = Fixture::new();
    let (store, plan) = store_failing_second_append(
        &fixture,
        FileOp::Write,
        Fault::Short {
            keep: 10,
            error: EIO,
        },
    );
    let path = store.path();

    let error = store
        .append_message(&user("second"))
        .expect_err("append fails");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    plan.assert_all_fired();
    drop(store);

    let torn = std::fs::read_to_string(&path).expect("read journal");
    assert!(!torn.ends_with('\n'), "the final line has no delimiter");
    assert_eq!(
        torn.lines().count(),
        4,
        "header, runtime, first, 10-byte prefix"
    );

    let (reopened, warnings) = Store::open(&path).expect("reopen");
    let messages: Vec<_> = warnings
        .iter()
        .map(|warning| warning.message.as_str())
        .collect();
    assert_eq!(
        messages,
        [format!("truncated incomplete final session line at {path}")]
    );
    assert_eq!(texts(&reopened), ["first"]);
    let repaired = std::fs::read_to_string(&path).expect("read journal");
    assert!(repaired.ends_with('\n'));
    assert_eq!(repaired.lines().count(), 3);
}

#[test]
fn after_eio_on_file_sync_poisons_the_store_and_the_reopened_input_is_not_duplicated() {
    let fixture = Fixture::new();
    let (store, plan) = store_failing_second_append(&fixture, FileOp::FileSync, Fault::After(EIO));
    let path = store.path();

    let error = store
        .append_input_if_absent("input-1", &user("second"))
        .expect_err("append reports failure");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert_eq!(
        store
            .append_message(&user("third"))
            .expect_err("poisoned")
            .kind(),
        PiErrorKind::FatalPersistence
    );
    plan.assert_all_fired();
    drop(store);

    // The record reached the file before the sync reported failure, so the
    // reopened store has it.
    let (reopened, warnings) = Store::open(&path).expect("reopen");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(texts(&reopened), ["first", "second"]);
    assert!(reopened.has_input("input-1"));
    let appended = reopened
        .append_input_if_absent("input-1", &user("second"))
        .expect("retry after reopen");
    assert!(!appended, "the retried input is already present");
    assert_eq!(texts(&reopened), ["first", "second"]);
}

#[test]
fn before_enospc_on_the_session_directory_sync_fails_create_and_removes_the_journal() {
    let fixture = Fixture::new();
    // Root sync 1 follows creating the session directory; root sync 2 is the
    // session directory's own sync after the journal records are written.
    let plan = FaultPlan::new(vec![Rule::new(
        FileOp::RootSync,
        Trigger::Nth(2),
        Fault::Before(FaultError::Os(libc::ENOSPC)),
    )]);
    let store = fixture.lazy_store(&plan);

    let error = store
        .append_message(&user("first"))
        .expect_err("create fails");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert!(
        error.to_string().contains("sync session directory"),
        "{error}"
    );
    assert!(fixture.journals().is_empty(), "{:?}", fixture.journals());
    assert!(store.messages().is_empty());
    plan.assert_all_fired();
}

#[test]
fn before_lock_on_create_fails_and_leaves_no_journal() {
    let fixture = Fixture::new();
    let plan = FaultPlan::new(vec![journal_rule(
        FileOp::Lock,
        1,
        Fault::Before(FaultError::Kind(std::io::ErrorKind::WouldBlock)),
    )]);
    let store = fixture.lazy_store(&plan);

    let error = store
        .append_message(&user("first"))
        .expect_err("create fails");
    assert_eq!(error.kind(), PiErrorKind::FatalPersistence);
    assert!(error.to_string().contains("lock session file"), "{error}");
    assert!(fixture.journals().is_empty(), "{:?}", fixture.journals());
    plan.assert_all_fired();
}

#[test]
fn before_lock_on_open_fails_and_leaves_the_journal_unchanged() {
    let fixture = Fixture::new();
    let store = Store::create(&fixture.root, fixture.header()).expect("create");
    store.append_message(&user("first")).expect("append");
    let path = PathBuf::from(store.path());
    store.close().expect("close");
    drop(store);
    let before = std::fs::read(&path).expect("read journal");

    let plan = FaultPlan::new(vec![journal_rule(
        FileOp::Lock,
        1,
        Fault::Before(FaultError::Kind(std::io::ErrorKind::WouldBlock)),
    )]);
    let driver = FaultyFileDriver::new(Arc::new(PosixDriver), Arc::clone(&plan));
    let error =
        Prepared::prepare_with_driver(&path, None, Arc::new(driver)).expect_err("open fails");
    assert!(error.to_string().contains("lock session file"), "{error}");
    plan.assert_all_fired();

    assert_eq!(std::fs::read(&path).expect("read journal"), before);
    assert_eq!(fixture.journals(), std::slice::from_ref(&path));
    let (reopened, _) = Store::open(&path).expect("reopen");
    assert_eq!(texts(&reopened), ["first"]);
}
