//! Fork tests.

use super::*;

// ---------------------------------------------------------------------------
// fork
// ---------------------------------------------------------------------------

pub(super) const FORK_ID: &str = "0123456789abcdef0123456789abcdef";

pub(super) fn write_turn(store: &Store, record: &otto_core::session::TurnRecord) {
    store
        .append_custom_entry(
            otto_core::session::TURN_CUSTOM_TYPE,
            &otto_core::session::encode_turn_record(record),
        )
        .expect("turn record");
}

/// Appends one completed user turn and returns its prompt's entry id.
pub(super) fn completed_turn(store: &Store, turn: &str, prompt: &str) -> String {
    use otto_core::session::{TurnOutcome, TurnRecord, TurnTrigger};
    write_turn(
        store,
        &TurnRecord::started(turn, TurnTrigger::User, 1, None),
    );
    store.append_message(&user(prompt)).expect("prompt");
    let id = json_lines(Path::new(&store.path()))
        .last()
        .and_then(|line| line["id"].as_str().map(str::to_owned))
        .expect("prompt id");
    store.append_message(&assistant("reply")).expect("reply");
    write_turn(store, &TurnRecord::ended(turn, TurnOutcome::Completed));
    id
}

pub(super) fn session_files(directory: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(directory)
        .expect("read session directory")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_fork_copies_the_history_before_a_turn_and_leaves_the_source_alone() {
    use otto_core::session::Session;
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    completed_turn(&store, "t1", "first");
    let kept_messages = store.messages();
    let second = completed_turn(&store, "t2", "second");
    store.rename("dev").expect("rename");
    let source = PathBuf::from(store.path());
    let before = fs::read(&source).expect("read source");

    let forked = super::super::fork::fork(
        &crate::storage::drivers::posix::PosixDriver,
        &source,
        Some(&second),
        FORK_ID,
    )
    .expect("fork");
    assert_eq!(fs::read(&source).expect("read source"), before);
    assert_eq!(
        forked.path,
        source.with_file_name(format!("{FORK_ID}.jsonl"))
    );
    assert_eq!(forked.point.as_ref().expect("point").prompt, "second");

    let lines = json_lines(&forked.path);
    assert_eq!(lines[0]["id"], FORK_ID);
    assert_eq!(lines[0]["parentSession"], source.to_string_lossy().as_ref());

    let (fork, warnings) = Store::open(&forked.path).expect("open fork");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(fork.take_recovery().is_none());
    assert_eq!(fork.interrupted_turn(), None);
    assert_eq!(fork.messages(), kept_messages);
    assert_eq!(fork.name(), "dev (fork)");
    let origin = fork.forked_from().expect("fork record");
    assert_eq!(origin.source_session_id, store.header().id);
    assert_eq!(origin.source_entry_id.as_deref(), Some(second.as_str()));
    assert_eq!(origin.prompt.as_deref(), Some("second"));
    assert!(store.forked_from().is_none());
    fork.close().expect("close fork");
    store.close().expect("close");
}

#[test]
fn a_fork_keeps_a_compaction_before_the_cut() {
    use otto_core::session::Session;
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    completed_turn(&store, "t1", "one");
    let anchor = completed_turn(&store, "t2", "two");
    store
        .append_compaction(&checkpoint(&anchor))
        .expect("compact");
    let model_messages = store.model_messages();
    let third = completed_turn(&store, "t3", "three");

    let forked = super::super::fork::fork(
        &crate::storage::drivers::posix::PosixDriver,
        Path::new(&store.path()),
        Some(&third),
        FORK_ID,
    )
    .expect("fork");
    let (fork, warnings) = Store::open(&forked.path).expect("open fork");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(fork.model_messages(), model_messages);
    assert!(fork.latest_compaction().is_some());
    fork.close().expect("close fork");
    store.close().expect("close");
}

#[test]
fn fork_context_is_what_a_fork_before_the_prompt_opens_with() {
    use otto_core::session::{build_context, fork_points, plan_fork};
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    completed_turn(&store, "t1", "first");
    let after_first = store.messages();
    let second = completed_turn(&store, "t2", "second");
    completed_turn(&store, "t3", "third");
    let source = PathBuf::from(store.path());
    let before = fs::read(&source).expect("read source");

    let (entries, _) = Store::read_entries(&source).expect("entries");
    let leaf = entries.last().expect("leaf").id.clone();
    let points = fork_points(&entries, &leaf).expect("points");
    assert_eq!(points[1].entry_id, second);
    let plan = plan_fork(&entries, &leaf, Some(&second)).expect("plan");
    let kept_leaf = plan.entries.last().expect("kept").id.clone();
    let expected = build_context(&plan.entries, &kept_leaf)
        .expect("context")
        .0
        .messages;

    let reference = source.to_string_lossy();
    let context = super::super::fork::fork_context(&reference, " second\n").expect("context");
    assert_eq!(context, expected);
    assert_eq!(context, after_first);
    assert_eq!(
        super::super::fork::fork_context(&reference, "first").expect("first"),
        Vec::new()
    );
    let error = super::super::fork::fork_context(&reference, "missing").expect_err("no match");
    assert!(error.starts_with("no user prompt matches;"), "{error}");
    assert_eq!(fs::read(&source).expect("read source"), before);
    store.close().expect("close");
}

#[test]
fn fork_context_ignores_a_compaction_after_the_prompt_and_keeps_one_before() {
    use otto_core::model::Role;
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    completed_turn(&store, "t1", "one");
    let after_one = store.messages();
    let anchor = completed_turn(&store, "t2", "two");
    let after_two = store.messages();
    store
        .append_compaction(&checkpoint(&anchor))
        .expect("compact");
    completed_turn(&store, "t3", "three");
    let reference = store.path();

    // The checkpoint came after "two": the original messages come back.
    let context = super::super::fork::fork_context(&reference, "two").expect("context");
    assert_eq!(context, after_one);
    assert!(
        context
            .iter()
            .all(|message| message.context_type.is_empty() && !message.text().contains("summary"))
    );

    // The checkpoint came before "three": its summary replaces "one".
    let context = super::super::fork::fork_context(&reference, "three").expect("context");
    assert_eq!(context.len(), 3);
    assert_eq!(context[0].role, Role::Context);
    assert!(context[0].text().contains("summary"), "{:?}", context[0]);
    assert_eq!(context[1..], after_two[2..]);
    assert!(!context.iter().any(|message| message.text() == "one"));
    store.close().expect("close");
}

#[test]
fn a_whole_fork_leaves_out_an_interrupted_turn() {
    use otto_core::session::{Session, TurnRecord, TurnTrigger};
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    completed_turn(&store, "t1", "first");
    let kept_messages = store.messages();
    write_turn(
        &store,
        &TurnRecord::started("t2", TurnTrigger::User, 1, None),
    );
    store.append_message(&user("lost")).expect("prompt");
    store
        .append_message(&tool_call("call-1", "bash"))
        .expect("call");
    assert!(store.interrupted_turn().is_some());

    let forked = super::super::fork::fork(
        &crate::storage::drivers::posix::PosixDriver,
        Path::new(&store.path()),
        None,
        FORK_ID,
    )
    .expect("fork");
    assert!(forked.point.is_none());
    let (fork, _) = Store::open(&forked.path).expect("open fork");
    assert!(fork.take_recovery().is_none());
    assert_eq!(fork.interrupted_turn(), None);
    assert_eq!(fork.messages(), kept_messages);
    let origin = fork.forked_from().expect("fork record");
    assert_eq!(origin.source_entry_id, None);
    assert_eq!(origin.prompt, None);
    fork.close().expect("close fork");
    store.close().expect("close");
}

#[test]
fn an_invalid_fork_point_or_a_taken_id_writes_nothing() {
    use otto_core::session::{ForkError, TurnRecord, TurnTrigger};
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    completed_turn(&store, "t1", "first");
    write_turn(
        &store,
        &TurnRecord::started("t2", TurnTrigger::User, 1, None),
    );
    store.append_message(&user("prompt")).expect("prompt");
    store
        .append_message(&tool_call("call-1", "bash"))
        .expect("call");
    store
        .append_message(&tool_result("call-1", "bash", "ok"))
        .expect("result");
    store.append_message(&user("steer")).expect("steer");
    let steer = message_entry_id(&store.path(), 5);
    let source = PathBuf::from(store.path());
    let directory = source.parent().expect("directory").to_path_buf();
    let files = session_files(&directory);

    for before in [steer.as_str(), "ffffffff"] {
        let error = super::super::fork::fork(
            &crate::storage::drivers::posix::PosixDriver,
            &source,
            Some(before),
            FORK_ID,
        )
        .expect_err("invalid fork point");
        assert!(matches!(error, ForkError::InvalidPoint(_)), "{error}");
        assert_eq!(session_files(&directory), files);
    }

    // The destination is never replaced, and the temporary file is removed.
    let taken = store.header().id;
    let taken_path = directory.join(format!("{taken}.jsonl"));
    fs::copy(&source, &taken_path).expect("occupy id");
    let files = session_files(&directory);
    super::super::fork::fork(
        &crate::storage::drivers::posix::PosixDriver,
        &source,
        None,
        &taken,
    )
    .expect_err("taken id");
    assert_eq!(session_files(&directory), files);
    store.close().expect("close");
}

pub(super) fn fork_failing_at(
    step: crate::fsops::PublishStep,
) -> (TempDir, Store, PathBuf, Result<(), String>) {
    let temp = TempDir::new();
    let (store, _) = new_store(&temp);
    completed_turn(&store, "t1", "first");
    let source = PathBuf::from(store.path());
    let plan = FaultPlan::new(vec![Rule::new(
        step,
        Trigger::Always,
        Fault::Before(FaultError::Os(libc::EIO)),
    )]);
    let result = super::super::fork::fork_with(
        &crate::storage::drivers::posix::PosixDriver,
        &source,
        None,
        FORK_ID,
        None,
        &crate::faults::publish::FaultyPublishOps::new(plan.clone()),
    )
    .map(|_| ())
    .map_err(|error| error.to_string());
    plan.assert_all_fired();
    (temp, store, source, result)
}

#[test]
fn a_fork_rename_failure_publishes_nothing_and_removes_the_temporary_file() {
    let (_temp, store, source, result) = fork_failing_at(crate::fsops::PublishStep::Rename);
    let error = result.expect_err("rename fails");
    assert!(error.contains("publish fork file"), "{error}");
    let names = session_files(source.parent().expect("directory"));
    assert!(
        !names.iter().any(|name| name.starts_with(FORK_ID)),
        "{names:?}"
    );
    store.close().expect("close");
}

#[test]
fn a_fork_directory_sync_failure_is_reported_after_the_fork_is_in_place() {
    let (_temp, store, source, result) = fork_failing_at(crate::fsops::PublishStep::DirSync);
    let error = result.expect_err("directory sync fails");
    assert!(error.contains("sync session directory"), "{error}");
    let fork_path = source.with_file_name(format!("{FORK_ID}.jsonl"));
    let (fork, _) = Store::open(&fork_path).expect("open fork");
    assert!(fork.forked_from().is_some());
    fork.close().expect("close fork");
    store.close().expect("close");
}

#[test]
fn a_crash_after_the_fork_rename_leaves_a_complete_fork_and_no_temporary_file() {
    use crate::faults::crash::{self, CRASH_EXIT, Role as Part};
    crash::run(crash::name_of(&|| {}), |part| match part {
        Part::Child(dir) => {
            let workspace = dir.join("workspace");
            fs::create_dir_all(&workspace).expect("create workspace");
            let store =
                Store::create(dir.join("root"), test_header(&workspace)).expect("create store");
            completed_turn(&store, "t1", "first");
            // The rename has happened and the directory sync has not.
            let plan = FaultPlan::new(vec![Rule::new(
                crate::fsops::PublishStep::DirSync,
                Trigger::Always,
                Fault::Exit,
            )]);
            let _ = super::super::fork::fork_with(
                &PosixDriver,
                Path::new(&store.path()),
                None,
                FORK_ID,
                None,
                &crate::faults::publish::FaultyPublishOps::new(plan),
            );
        }
        Part::Parent(dir, status) => {
            assert_eq!(
                status.code(),
                Some(CRASH_EXIT),
                "child did not crash at the fault"
            );
            let directory = list::session_directory(
                &dir.join("root"),
                &dir.join("workspace").to_string_lossy(),
            )
            .expect("session directory");
            let source = directory.join("550e8400-e29b-41d4-a716-446655440000.jsonl");

            let fork_path = directory.join(format!("{FORK_ID}.jsonl"));
            assert!(
                !session_files(&directory)
                    .iter()
                    .any(|name| name.ends_with(".fork-tmp")),
                "{:?}",
                session_files(&directory)
            );
            let (fork, warnings) = Store::open(&fork_path).expect("the fork is present and opens");
            assert!(warnings.is_empty(), "{warnings:?}");
            assert!(fork.forked_from().is_some());
            let (original, _) = Store::open(&source).expect("source");
            assert_eq!(fork.messages().len(), original.messages().len());
        }
    });
}

#[test]
fn listing_shows_a_fork_and_ignores_a_leftover_temporary_file() {
    let temp = TempDir::new();
    let (store, root) = new_store(&temp);
    completed_turn(&store, "t1", "first");
    let source = PathBuf::from(store.path());
    fs::write(
        source.with_file_name(format!("{FORK_ID}.jsonl.fork-tmp")),
        "partial",
    )
    .expect("leftover");
    super::super::fork::fork(
        &crate::storage::drivers::posix::PosixDriver,
        &source,
        None,
        "fedcba9876543210fedcba9876543210",
    )
    .expect("fork");
    let listed = list::list(&root, &store.header().workspace, "", 10).expect("list");
    assert_eq!(listed.sessions.len(), 2, "{listed:?}");
    assert_eq!(listed.skipped, 0);
    store.close().expect("close");
}
