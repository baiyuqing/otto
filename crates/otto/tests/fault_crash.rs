//! Crash rows of the fault-injection matrix ("Crash harness" in
//! `docs/specs/2026-10-07-fault-injection-testing.md`). Each test re-executes
//! this binary; the child ends at an injected `Exit` fault and the parent
//! reopens what the child left with the real drivers.
//!
//! The rows whose seam is not public live in the owning module's tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use otto::faults::crash::{self, CRASH_EXIT, Role};
use otto::faults::files::{FaultyFileDriver, FileOp};
use otto::faults::{Fault, FaultPlan, Filter, Rule, Trigger};
use otto::session::Store;
use otto_core::model::{Block, BlockType, FinishReason, Message, Role as MessageRole};
use otto_core::session::Header;
use otto_core::session::context::MAY_HAVE_RUN_TOOL_RESULT_TEXT;

fn header(workspace: &Path) -> Header {
    Header {
        id: "550e8400-e29b-41d4-a716-446655440000".into(),
        workspace: workspace.to_string_lossy().into_owned(),
        provider: "openai-compatible".into(),
        profile: "local".into(),
        model: "test-model".into(),
        created_at: chrono::Utc::now(),
        ..Header::default()
    }
}

fn user(text: &str) -> Message {
    Message {
        role: MessageRole::User,
        blocks: vec![Block::text(text)],
        created_at: chrono::Utc::now(),
        ..Message::default()
    }
}

fn tool_result(id: &str, name: &str, text: &str) -> Message {
    Message {
        role: MessageRole::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            text: text.into(),
            tool_call_id: id.into(),
            tool_name: name.into(),
            ..Block::default()
        }],
        created_at: chrono::Utc::now(),
        ..Message::default()
    }
}

fn tool_call(id: &str, name: &str) -> Message {
    Message {
        role: MessageRole::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: id.into(),
            tool_name: name.into(),
            arguments: Some(
                serde_json::value::RawValue::from_string("{}".into()).expect("valid JSON"),
            ),
            ..Block::default()
        }],
        created_at: chrono::Utc::now(),
        finish_reason: Some(FinishReason::ToolCalls),
        ..Message::default()
    }
}

/// A lazy store under `dir` whose journal meets `rules`.
fn store(dir: &Path, rules: Vec<Rule<FileOp>>) -> (Store, Arc<FaultPlan<FileOp>>) {
    let root = dir.join("root");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::create_dir_all(&workspace).expect("workspace");
    let plan = FaultPlan::new(rules);
    let driver = FaultyFileDriver::new(
        Arc::new(otto::storage::drivers::posix::PosixDriver),
        Arc::clone(&plan),
    );
    let store = Store::create_lazy_with_driver(&root, header(&workspace), Arc::new(driver))
        .expect("lazy store");
    (store, plan)
}

/// Every `.jsonl` file under `dir/root`.
fn journals(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "jsonl") {
                out.push(path);
            }
        }
    }
    let mut found = Vec::new();
    walk(&dir.join("root"), &mut found);
    found
}

fn texts(store: &Store) -> Vec<String> {
    store
        .messages()
        .iter()
        .map(|message| message.blocks[0].text.clone())
        .collect()
}

#[test]
fn a_crash_during_a_journal_append_leaves_a_torn_line_that_reopen_truncates_and_a_dangling_call_may_have_run()
 {
    crash::run(crash::name_of(&|| {}), |role| match role {
        Role::Child(dir) => {
            // Writes 1 and 2 are the header and the runtime record, 3 the
            // first message, 4 the tool call, 5 the tool result, which crashes.
            let (store, _plan) = store(
                dir,
                vec![
                    Rule::new(
                        FileOp::Write,
                        Trigger::Nth(5),
                        Fault::ShortExit { keep: 1500 },
                    )
                    .filter(Filter::NameSuffix(".jsonl")),
                ],
            );
            store.append_message(&user("first")).expect("first");
            store
                .append_message(&tool_call("call-1", "bash"))
                .expect("tool call");
            let _ = store.append_message(&tool_result("call-1", "bash", &"x".repeat(2000)));
        }
        Role::Parent(dir, status) => {
            assert_eq!(
                status.code(),
                Some(CRASH_EXIT),
                "child did not crash at the fault"
            );
            let [path] = journals(dir).try_into().expect("one journal");
            let torn = std::fs::read_to_string(&path).expect("read journal");
            assert!(!torn.ends_with('\n'), "the final line has no delimiter");

            let (reopened, warnings) = Store::open(&path).expect("reopen");
            let warnings: Vec<_> = warnings.iter().map(|w| w.message.as_str()).collect();
            assert!(
                warnings.contains(
                    &format!(
                        "truncated incomplete final session line at {}",
                        path.display()
                    )
                    .as_str()
                ),
                "{warnings:?}"
            );
            let messages = reopened.messages();
            assert_eq!(messages.len(), 3, "first, tool call, repaired result");
            assert_eq!(messages[0].blocks[0].text, "first");
            assert_eq!(messages[1].blocks[0].tool_call_id, "call-1");
            assert_eq!(messages[2].blocks[0].text, MAY_HAVE_RUN_TOOL_RESULT_TEXT);
            let repaired = std::fs::read_to_string(&path).expect("read journal");
            assert!(repaired.ends_with('\n'));
            // The torn fragment is longer than the repair record, so a line
            // left behind by a missing truncation is not overwritten.
            for line in repaired.lines() {
                assert!(
                    serde_json::from_str::<serde_json::Value>(line).is_ok(),
                    "every journal line is complete JSON after the reopen: {line:.60}"
                );
            }
        }
    });
}

#[test]
fn a_crash_before_the_session_directory_sync_leaves_no_session_or_a_valid_one() {
    crash::run(crash::name_of(&|| {}), |role| match role {
        Role::Child(dir) => {
            // Root sync 1 follows creating the session directory; root sync 2
            // is the session directory's sync after the journal records are
            // written.
            let (store, _plan) = store(
                dir,
                vec![Rule::new(FileOp::RootSync, Trigger::Nth(2), Fault::Exit)],
            );
            let _ = store.append_message(&user("first"));
        }
        Role::Parent(dir, status) => {
            assert_eq!(
                status.code(),
                Some(CRASH_EXIT),
                "child did not crash at the fault"
            );
            for path in journals(dir) {
                let (reopened, warnings) = Store::open(&path).expect("a session that opens");
                assert!(warnings.is_empty(), "{warnings:?}");
                let texts = texts(&reopened);
                assert!(
                    texts.is_empty() || texts == ["first"],
                    "{}: {texts:?}",
                    path.display()
                );
            }
        }
    });
}

fn workflow_definition() -> otto::workflow::Definition {
    otto::workflow::Definition {
        name: "crash".into(),
        description: String::new(),
        steps: vec![otto::workflow::Step {
            id: "one".into(),
            kind: otto::workflow::StepKind::Agent,
            agent: "worker".into(),
            prompt: "crash".into(),
            needs: Vec::new(),
        }],
        agents: Vec::new(),
        hash: "hash".into(),
    }
}

fn workflow_store(
    address: &str,
    driver: Arc<dyn otto::storage::SqlDriver>,
) -> Arc<dyn otto::workflow::WorkflowStore> {
    use otto::runtime::storage::{Endpoint, RepositoryFactory, SqlRepositories};
    SqlRepositories::new(driver)
        .workflow(Endpoint {
            address,
            policy: otto::storage::OpenPolicy::CreateIfMissing,
        })
        .expect("open workflow store")
}

#[test]
fn a_crash_after_the_step_claim_commit_is_recovered_as_a_paused_run_with_an_interrupted_step() {
    use otto::faults::sql::{FaultySqlDriver, SqlOp};
    use otto::storage::drivers::turso::TursoDriver;
    use otto::workflow::{RunStatus, StepStatus};

    crash::run(crash::name_of(&|| {}), |role| {
        let address = |dir: &Path| dir.join("workflow.db").to_string_lossy().into_owned();
        match role {
            Role::Child(dir) => {
                let address = address(dir);
                let created = workflow_store(&address, Arc::new(TursoDriver));
                created
                    .create_run(
                        "run-1",
                        &workflow_definition(),
                        "/crash",
                        "p",
                        "openai-compatible",
                        "m",
                        "",
                    )
                    .expect("create run");
                drop(created);
                let plan = FaultPlan::new(vec![Rule::new(
                    SqlOp::Commit,
                    Trigger::Nth(1),
                    Fault::ExitAfter,
                )]);
                let faulty = workflow_store(
                    &address,
                    Arc::new(FaultySqlDriver::new(Arc::new(TursoDriver), plan)),
                );
                let _ = faulty.claim_ready("run-1", "one", "transcript.jsonl");
            }
            Role::Parent(dir, status) => {
                assert_eq!(
                    status.code(),
                    Some(CRASH_EXIT),
                    "child did not crash at the fault"
                );
                let store = workflow_store(&address(dir), Arc::new(TursoDriver));
                let before = store.get_run("run-1").expect("get").expect("run exists");
                assert_eq!(
                    before.steps[0].status,
                    StepStatus::Running,
                    "the committed claim is visible before recovery"
                );
                assert_eq!(store.recover("/crash").expect("recover"), 1);
                let after = store.get_run("run-1").expect("get").expect("run exists");
                assert_eq!(after.status, RunStatus::Paused, "run after recovery");
                assert_eq!(
                    after.steps[0].status,
                    StepStatus::Interrupted,
                    "step after recovery"
                );
            }
        }
    });
}
