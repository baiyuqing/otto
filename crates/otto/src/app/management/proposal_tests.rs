//! Tests for `/memory propose`: parsing, usage text and the handler without a generator.

use super::{Reply, Request};
use crate::memory::Record;

fn remember_text(controller: &crate::app::Controller, key: &str, text: &str) -> Record {
    let request = Request::parse(
        "remember",
        &format!("--scope user --kind note --key {key} {text}"),
    )
    .expect("parse remember");
    match controller.management(request) {
        Ok(Reply::Remembered(record)) => record,
        other => panic!("expected a remembered record, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn propose_joins_the_evidence_words_with_single_spaces() {
    match Request::parse("memory", "propose r1  the   editor changed  to helix") {
        Ok(Request::MemoryPropose { id, evidence }) => {
            assert_eq!(id, "r1");
            assert_eq!(evidence, "the editor changed to helix");
        }
        other => panic!("parsed as {other:?}"),
    }
}

#[test]
fn propose_without_evidence_or_id_returns_the_usage_text_that_lists_propose() {
    for args in ["propose r1", "propose"] {
        let usage = match Request::parse("memory", args) {
            Err(usage) => usage,
            Ok(request) => panic!("`{args}` parsed as {request:?}"),
        };
        assert!(usage.starts_with("usage: /memory list"), "{args}: {usage}");
        assert!(
            usage.contains("/memory propose <id> <evidence text>"),
            "{args}: {usage}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn propose_without_a_generator_on_the_runner_reports_unavailable() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let mut builder = crate::fixtures::builder(workspace.path(), sessions.path());
    let memory_runtime = otto_core::config::MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: store.path().join("memory").to_string_lossy().into_owned(),
        ..Default::default()
    };
    let (service, user_scope, usable) =
        crate::runtime::wiring::open_memory_service(&memory_runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the test store must be usable");
    builder.shared_mut().memory.service = service;
    builder.shared_mut().memory.user_scope = user_scope;
    builder.shared_mut().memory.usable = usable;
    builder.workspace_scope = crate::runtime::wiring::workspace_memory_scope(
        &memory_runtime,
        &workspace.path().to_string_lossy(),
    )
    .expect("workspace scope");
    let runtime = crate::fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let mut runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    assert!(runner.memory_proposals.is_some());
    runner.memory_proposals = None;
    let info = builder.runtime_info(&runtime);
    let controller = crate::app::Controller::new(builder, true, session, runner, info);
    let record = remember_text(&controller, "editor", "prefers vim");
    let request = Request::parse("memory", &format!("propose {} it is now helix", record.id))
        .expect("parse propose");
    match controller.management(request) {
        Err(error) => assert_eq!(error, "memory proposals are not available"),
        Ok(_) => panic!("propose succeeded without a generator"),
    }
}
