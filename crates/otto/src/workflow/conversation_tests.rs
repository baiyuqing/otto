use super::*;
use crate::runtime::builder::{MemoryHandle, SharedSession};
use otto_core::mailbox::{Inbox, Mailbox};
use otto_core::session::{CURRENT_VERSION, Header, Session};
use std::sync::Arc;

pub(crate) struct FakeExecutor;
#[async_trait::async_trait]
impl Executor for FakeExecutor {
    async fn execute(
        &self,
        _: Attempt,
        _: &otto_platform::deadline::Control,
    ) -> Result<String, String> {
        Ok("review findings".into())
    }
}
pub(crate) fn fixture() -> (tempfile::TempDir, Catalog) {
    let manual = include_str!("../../../../docs/manual/automation.md");
    let section = manual
        .split("### Set up a first conversational workflow")
        .nth(1)
        .expect("manual setup section");
    fn fence<'a>(section: &'a str, language: &str) -> &'a str {
        section
            .split(&format!("```{language}\n"))
            .nth(1)
            .expect("documented fence")
            .split("```")
            .next()
            .unwrap()
    }
    let dir = tempfile::tempdir().unwrap();
    let agents = dir.path().join(".otto/agents");
    let workflows = dir.path().join(".otto/workflows");
    std::fs::create_dir_all(agents.join("review")).unwrap();
    std::fs::create_dir_all(&workflows).unwrap();
    std::fs::write(agents.join("review/AGENT.md"), fence(section, "markdown")).unwrap();
    std::fs::write(
        workflows.join("documentation-review.toml"),
        fence(section, "toml"),
    )
    .unwrap();
    let (agents, warnings) = crate::subagent::Catalog::discover(&[agents]);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        agents.lookup("review").unwrap().write_policy,
        crate::subagent::WritePolicy::ReadOnly
    );
    let (catalog, warnings) = Catalog::discover(&[workflows], &agents);
    assert!(warnings.is_empty(), "{warnings:?}");
    (dir, catalog)
}
pub(crate) fn controller(
    store: Arc<dyn WorkflowStore>,
    catalog: Catalog,
    workspace: &str,
    root: &std::path::Path,
) -> Arc<Controller> {
    Controller::new(
        store,
        catalog,
        Arc::new(FakeExecutor),
        workspace.into(),
        root.join("transcripts"),
        RuntimeIdentity::default(),
        1,
    )
}
pub(crate) fn session(id: &str) -> SharedSession {
    SharedSession::new(Arc::new(MemoryHandle::new(Header {
        version: CURRENT_VERSION,
        id: id.into(),
        workspace: crate::fixtures::shared_workspace().into(),
        profile: String::new(),
        provider: String::new(),
        model: String::new(),
        created_at: chrono::Utc::now(),
    })))
}
#[test]
fn documented_review_then_human_gate_discovers_without_fixture_drift() {
    let (_dir, catalog) = fixture();
    let definition = catalog.lookup("documentation-review").unwrap();
    assert_eq!(definition.steps.len(), 2);
    assert_eq!(definition.steps[0].id, "review");
    assert_eq!(definition.steps[1].id, "accept");
    assert_eq!(definition.steps[1].kind, StepKind::Approval);
    assert_eq!(definition.steps[1].needs, ["review"]);
    assert!(
        definition
            .source
            .ends_with(".otto/workflows/documentation-review.toml")
    );
}
#[test]
fn empty_invalid_and_workspace_override_catalog_are_distinct() {
    let (dir, catalog) = fixture();
    let agents = crate::subagent::Catalog::default();
    let empty = dir.path().join("missing");
    let (none, warnings) = Catalog::discover(&[empty], &agents);
    assert!(none.lookup("anything").is_none());
    assert!(warnings.is_empty());
    let local = dir.path().join("local");
    std::fs::create_dir(&local).unwrap();
    std::fs::write(local.join("bad.toml"), "invalid = true").unwrap();
    let gate = "version=1\ndescription='workspace override'\n[[steps]]\nid='gate'\nkind='approval'\nprompt='Human review'\n";
    std::fs::write(local.join("documentation-review.toml"), gate).unwrap();
    let (resolved, warnings) =
        Catalog::discover(&[dir.path().join(".otto/workflows"), local], &agents);
    assert_eq!(
        resolved.lookup("documentation-review").unwrap().description,
        "workspace override"
    );
    assert!(!warnings.is_empty());
    assert_eq!(
        catalog.lookup("documentation-review").unwrap().steps.len(),
        2,
        "construction-time snapshot must not refresh implicitly"
    );
}
#[tokio::test]
async fn receipt_atomicity_reconciliation_workspace_and_input_guards() {
    let (dir, catalog) = fixture();
    let store = Arc::new(Store::open_in_memory());
    let definition = catalog.lookup("documentation-review").unwrap().clone();
    let owner = controller(
        store.clone(),
        catalog,
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let first = owner
        .start_parent("documentation-review", "scope", "source", "operation-1")
        .await
        .unwrap();
    owner.wait(&first.id).await.unwrap();
    let same = owner
        .start_parent("documentation-review", "scope", "source", "operation-1")
        .await
        .unwrap();
    assert_eq!(first.id, same.id);
    assert_eq!(same.status, RunStatus::Waiting);
    assert_eq!(
        store
            .origin_runs(crate::fixtures::shared_workspace(), "source")
            .unwrap(),
        std::slice::from_ref(&first.id)
    );
    assert!(
        owner
            .start_parent("documentation-review", "changed", "source", "operation-1")
            .await
            .is_err()
    );
    assert!(
        owner
            .start_parent("documentation-review", "scope", "", "operation-2")
            .await
            .is_err()
    );
    let (second, created) = store
        .create_origin_run(
            "second",
            &definition,
            "/other",
            &RuntimeIdentity::default(),
            "scope",
            "source",
            "operation-1",
        )
        .unwrap();
    assert!(created);
    assert!(owner.get(&second.id).is_err());
    assert!(
        store
            .origin_runs(
                crate::fixtures::shared_workspace(),
                "replacement-new-session"
            )
            .unwrap()
            .is_empty()
    );
    let next = owner
        .start_parent("documentation-review", "scope", "source", "operation-2")
        .await
        .unwrap();
    assert_ne!(
        first.id, next.id,
        "a distinct explicit tool operation is a distinct start"
    );
    owner.close().await;
}
#[tokio::test]
async fn lifecycle_replay_restart_gate_dedup_and_no_new_session_redirect() {
    let (dir, catalog) = fixture();
    let store = Arc::new(Store::open(&dir.path().join("workflows.db")).unwrap());
    let owner = controller(
        store.clone(),
        catalog.clone(),
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let run = owner
        .start_parent("documentation-review", "scope", "source", "op")
        .await
        .unwrap();
    owner.wait(&run.id).await.unwrap();
    let source = session("source");
    let inbox = Inbox::default();
    let mut updates = crate::runtime::workflow_notifications::updates(&inbox);
    let mut seen = std::collections::HashSet::new();
    crate::runtime::workflow_notifications::replay(&owner, &source, &inbox, &mut seen);
    assert!(updates.has_changed().unwrap());
    updates.borrow_and_update();
    let notices = inbox.snapshot();
    assert!(notices.iter().any(|n| n.text.contains("run_started")));
    assert!(
        notices
            .iter()
            .any(|n| n.text.contains("approval_requested"))
    );
    assert!(
        notices
            .iter()
            .all(|n| n.id.starts_with(&format!("workflow:{}:", run.id)))
    );
    // Simulate crash after enqueue before acknowledgement: queued IDs dedup.
    crate::runtime::workflow_notifications::replay(
        &owner,
        &source,
        &inbox,
        &mut Default::default(),
    );
    assert_eq!(notices.len(), inbox.len());
    // Simulate append-before-ack crash: transcript input ID dedup remains authoritative.
    for entry in inbox.queued() {
        source
            .append_input_if_absent(
                &entry.notification.id,
                otto_core::model::Message {
                    role: otto_core::model::Role::Context,
                    context_type: "task_report".into(),
                    blocks: vec![otto_core::model::Block::text(&entry.notification.text)],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        inbox.ack(entry.seq);
    }
    crate::runtime::workflow_notifications::replay(
        &owner,
        &source,
        &inbox,
        &mut Default::default(),
    );
    assert!(inbox.is_empty());
    let replacement = session("replacement");
    crate::runtime::workflow_notifications::replay(
        &owner,
        &replacement,
        &inbox,
        &mut Default::default(),
    );
    assert!(inbox.is_empty());
    owner.close().await;
    drop(owner);
    drop(store);
    let store = Arc::new(Store::open(&dir.path().join("workflows.db")).unwrap());
    store.recover(crate::fixtures::shared_workspace()).unwrap();
    let reopened = controller(
        store.clone(),
        catalog,
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    assert_eq!(
        reopened.get(&run.id).unwrap().status,
        RunStatus::Waiting,
        "recovery must preserve genuine human gates"
    );
    let fresh_inbox = Inbox::default();
    crate::runtime::workflow_notifications::replay(
        &reopened,
        &session("source"),
        &fresh_inbox,
        &mut Default::default(),
    );
    assert!(
        fresh_inbox.is_empty(),
        "durably enqueued events must not regenerate on reopen"
    );
    reopened.store.cancel_run(&run.id).unwrap();
    crate::runtime::workflow_notifications::replay(
        &reopened,
        &session("source"),
        &fresh_inbox,
        &mut Default::default(),
    );
    assert!(
        !fresh_inbox.is_empty(),
        "new closed-source events remain replayable"
    );
    reopened.close().await;
}

#[tokio::test]
async fn receipt_survives_crash_before_launch_and_does_not_retry_interrupted_effects() {
    let (dir, catalog) = fixture();
    let store = Arc::new(Store::open(&dir.path().join("receipt.db")).unwrap());
    let definition = catalog.lookup("documentation-review").unwrap();
    let (run, created) = store
        .create_origin_run(
            "crash-run",
            definition,
            crate::fixtures::shared_workspace(),
            &RuntimeIdentity::default(),
            "scope",
            "source",
            "same-op",
        )
        .unwrap();
    assert!(created);
    // Receipt and run are committed before a response or driver can exist.
    store
        .claim_ready(&run.id, "review", "controlled-transcript")
        .unwrap();
    drop(store);
    let reopened = Arc::new(Store::open(&dir.path().join("receipt.db")).unwrap());
    reopened
        .recover(crate::fixtures::shared_workspace())
        .unwrap();
    let owner = controller(
        reopened.clone(),
        catalog,
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let reconciled = owner
        .start_parent("documentation-review", "scope", "source", "same-op")
        .await
        .unwrap();
    assert_eq!(reconciled.id, run.id);
    assert_eq!(reconciled.status, RunStatus::Paused);
    assert_eq!(reconciled.steps[0].status, StepStatus::Interrupted);
    assert_eq!(reconciled.steps[0].attempt, 1);
    assert_eq!(
        reopened
            .origin_runs(crate::fixtures::shared_workspace(), "source")
            .unwrap(),
        [run.id]
    );
    owner.close().await;
}
#[tokio::test]
async fn same_operation_returns_original_snapshot_after_catalog_restart() {
    let (dir, catalog) = fixture();
    let store = Arc::new(Store::open_in_memory());
    let owner = controller(
        store.clone(),
        catalog,
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let run = owner
        .start_parent("documentation-review", "scope", "source", "op")
        .await
        .unwrap();
    owner.wait(&run.id).await.unwrap();
    owner.close().await;
    let changed = controller(
        store,
        Catalog::default(),
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let replay = changed
        .start_parent("documentation-review", "scope", "source", "op")
        .await
        .unwrap();
    assert_eq!(replay.id, run.id);
    assert_eq!(replay.definition, run.definition);
    assert_eq!(replay.status, RunStatus::Waiting);
    changed.close().await;
}

#[tokio::test]
async fn mailbox_signal_wakes_without_a_subagent_registry() {
    let inbox = Inbox::default();
    let mut signal = crate::runtime::workflow_notifications::updates(&inbox);
    inbox
        .enqueue(otto_core::mailbox::Notification {
            id: "workflow:run:1".into(),
            text: "workflow lifecycle".into(),
            ..Default::default()
        })
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), signal.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(inbox.len(), 1);
}

#[test]
fn pending_ledger_pages_ownership_restart_and_constant_idle_query() {
    let (dir, catalog) = fixture();
    let path = dir.path().join("pages.db");
    let store = Store::open(&path).unwrap();
    let definition = catalog.lookup("documentation-review").unwrap();
    for i in 0..80 {
        store
            .create_origin_run(
                &format!("run-{i}"),
                definition,
                crate::fixtures::shared_workspace(),
                &RuntimeIdentity::default(),
                "",
                "source",
                &format!("op-{i}"),
            )
            .unwrap();
    }
    let mut previous = 0;
    let mut count = 0;
    loop {
        let page = store
            .pending_events(crate::fixtures::shared_workspace(), "source", usize::MAX)
            .unwrap();
        assert!(page.len() <= 64);
        if page.is_empty() {
            break;
        }
        for event in page {
            assert!(event.seq > previous);
            previous = event.seq;
            store
                .acknowledge_event("/other", "source", event.seq)
                .unwrap();
            store
                .acknowledge_event(
                    crate::fixtures::shared_workspace(),
                    "replacement",
                    event.seq,
                )
                .unwrap();
            assert_eq!(
                store
                    .pending_events(crate::fixtures::shared_workspace(), "source", 1)
                    .unwrap()[0]
                    .seq,
                event.seq
            );
            store
                .acknowledge_event(crate::fixtures::shared_workspace(), "source", event.seq)
                .unwrap();
            count += 1;
        }
    }
    assert!(
        count > 128,
        "must cross multiple SQL pages, including non-lifecycle events"
    );
    for _ in 0..10 {
        assert!(
            store
                .pending_events(crate::fixtures::shared_workspace(), "source", 64)
                .unwrap()
                .is_empty()
        );
    }
    drop(store);
    let reopened = Store::open(&path).unwrap();
    assert!(
        reopened
            .pending_events(crate::fixtures::shared_workspace(), "source", 64)
            .unwrap()
            .is_empty(),
        "reopen must not backfill acknowledged history"
    );
}

#[tokio::test]
async fn failed_enqueue_keeps_ordered_page_pending() {
    let (dir, catalog) = fixture();
    let store = Arc::new(Store::open_in_memory());
    let owner = controller(
        store.clone(),
        catalog,
        crate::fixtures::shared_workspace(),
        dir.path(),
    );
    let run = owner
        .start_parent("documentation-review", "scope", "source", "op")
        .await
        .unwrap();
    owner.wait(&run.id).await.unwrap();
    let before = store
        .pending_events(crate::fixtures::shared_workspace(), "source", 64)
        .unwrap();
    let inbox = Inbox::default();
    let failing = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let persist_failing = failing.clone();
    inbox.set_persist(Box::new(move |_| {
        if persist_failing.load(std::sync::atomic::Ordering::SeqCst) {
            Err("injected disk failure".into())
        } else {
            Ok(())
        }
    }));
    crate::runtime::workflow_notifications::replay(
        &owner,
        &session("source"),
        &inbox,
        &mut Default::default(),
    );
    let after = store
        .pending_events(crate::fixtures::shared_workspace(), "source", 64)
        .unwrap();
    assert_eq!(
        before.iter().map(|e| e.seq).collect::<Vec<_>>(),
        after.iter().map(|e| e.seq).collect::<Vec<_>>()
    );
    assert_eq!(
        inbox.len(),
        1,
        "failed entry stays in memory but is not durable evidence"
    );
    crate::runtime::workflow_notifications::replay(
        &owner,
        &session("source"),
        &inbox,
        &mut Default::default(),
    );
    assert_eq!(
        store
            .pending_events(crate::fixtures::shared_workspace(), "source", 64)
            .unwrap()[0]
            .seq,
        before[0].seq,
        "duplicate memory ID cannot bypass persistence failure"
    );
    failing.store(false, std::sync::atomic::Ordering::SeqCst);
    crate::runtime::workflow_notifications::replay(
        &owner,
        &session("source"),
        &inbox,
        &mut Default::default(),
    );
    assert!(!inbox.is_empty());
    assert!(
        store
            .pending_events(crate::fixtures::shared_workspace(), "source", 64)
            .unwrap()
            .is_empty()
    );
    owner.close().await;
}
