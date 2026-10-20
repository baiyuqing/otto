use super::*;
use crate::memory::{NAMESPACE_USER, NAMESPACE_WORKSPACE, RememberRequest, SearchRequest};
use otto_core::agent::inbox::Inbox;

#[test]
fn children_of_an_in_memory_parent_stay_in_memory() {
    let build = child_sessions(
        SharedSession::memory(otto_core::session::Header::default()),
        Arc::new(Default::default()),
    );
    assert!(build("t1").expect("build").is_none());
}

#[tokio::test]
async fn a_child_store_refuses_to_append_after_the_parent_lease_is_lost() {
    let root = tempfile::tempdir().expect("root");
    let workspace = tempfile::tempdir().expect("workspace");
    let lease = crate::failover::lease::Lease::for_test(workspace.path());
    let store = crate::session::Store::create(
        root.path(),
        otto_core::session::Header {
            id: "parent".into(),
            workspace: workspace.path().to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("parent");
    store.set_lease_check(Arc::clone(&lease) as Arc<dyn crate::session::WriterOwnership>);
    let build = child_sessions(
        SharedSession::new(Arc::new(store)),
        Arc::new(Default::default()),
    );

    let (child, _path) = build("t1").expect("build").expect("persisted");

    lease.mark_lost_for_test("test fence");

    let message = otto_core::model::Message {
        role: otto_core::model::Role::User,
        blocks: vec![otto_core::model::Block::text("hello")],
        ..otto_core::model::Message::default()
    };
    let error = child
        .append(message)
        .await
        .expect_err("append must be rejected once the parent lease is lost");
    assert!(error.to_string().contains("session lease lost"), "{error}");
}

#[tokio::test]
async fn children_of_a_session_file_are_written_beside_it() {
    let root = tempfile::tempdir().expect("root");
    let workspace = tempfile::tempdir().expect("workspace");
    let store = crate::session::Store::create(
        root.path(),
        otto_core::session::Header {
            id: "parent".into(),
            workspace: workspace.path().to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("parent");
    let parent_path = store.path();
    let build = child_sessions(
        SharedSession::new(Arc::new(store)),
        Arc::new(Default::default()),
    );

    let (_, path) = build("t1").expect("build").expect("persisted");

    let prefix = std::path::Path::new(&parent_path)
        .with_extension("")
        .join("t1-");
    assert!(path.starts_with(&*prefix.to_string_lossy()), "{path}");
    assert!(path.ends_with(".jsonl"), "{path}");
}

#[test]
fn a_reopened_inbox_loads_its_sidecar_and_the_file_is_updated_after_delivery() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("s1.inbox.json");
    std::fs::write(
        &path,
        r#"[
                {"seq":0,"task_id":"t1","kind":"task_finished","text":"first","usage":null},
                {"seq":1,"task_id":"t2","kind":"task_finished","text":"second","usage":null}
            ]"#,
    )
    .expect("write sidecar");

    let inbox = Inbox::new(None);
    assert_eq!(super::super::sidecar_mailbox::load(&inbox, &path), None);
    let hook_path = path.clone();
    super::super::sidecar_mailbox::install(&inbox, ".inbox.json", move || Some(hook_path.clone()));

    let queued = inbox.queued();
    assert_eq!(
        queued
            .iter()
            .map(|entry| entry.notification.text.as_str())
            .collect::<Vec<_>>(),
        ["first", "second"]
    );

    // Delivery removes each loaded item by sequence number, exactly like
    // `deliver_notifications`, so every one is handled exactly once.
    let mut delivered = Vec::new();
    for entry in inbox.queued() {
        delivered.push(entry.notification.text.clone());
        inbox.remove_seq(entry.seq);
    }
    assert_eq!(delivered, ["first", "second"]);
    assert!(inbox.is_empty());
    assert!(
        !path.exists(),
        "the sidecar is removed once every loaded notification has been delivered"
    );
}

/// A database path whose parent is a regular file, so every open fails
/// deterministically.
fn unopenable_path(directory: &std::path::Path) -> String {
    let blocker = directory.join("blocker-file");
    std::fs::write(&blocker, b"not a directory").expect("write blocker");
    blocker.join("memory").to_string_lossy().into_owned()
}

/// A store directory below the temporary directory.
fn store_path(directory: &std::path::Path) -> String {
    directory.join("memory").to_string_lossy().into_owned()
}

fn enabled(path: String) -> MemoryRuntime {
    MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: path,
        ..MemoryRuntime::default()
    }
}

#[test]
fn disabled_memory_returns_an_unusable_null_service() {
    let mut stderr = Vec::new();
    let (service, scope, usable) =
        open_memory_service(&MemoryRuntime::default(), &[], &mut stderr).expect("open");
    assert!(!usable);
    assert_eq!(scope, Scope::default());
    assert_eq!(
        service
            .search(&SearchRequest::default())
            .expect_err("disabled")
            .kind,
        ErrorKind::Disabled
    );
    assert!(stderr.is_empty(), "{stderr:?}");
    service.close().expect("close");
}

#[test]
fn an_open_failure_degrades_to_a_null_service_unless_required() {
    let directory = tempfile::tempdir().expect("directory");
    let mut config = enabled(unopenable_path(directory.path()));

    let mut stderr = Vec::new();
    let (service, scope, usable) = open_memory_service(&config, &[], &mut stderr).expect("degrade");
    assert!(!usable);
    assert_eq!(scope, Scope::default());
    assert_eq!(
        service
            .search(&SearchRequest::default())
            .expect_err("unavailable")
            .kind,
        ErrorKind::Unavailable
    );
    assert!(
        String::from_utf8_lossy(&stderr).contains("warning"),
        "{stderr:?}"
    );
    service.close().expect("close");

    config.required = true;
    assert!(open_memory_service(&config, &[], &mut Vec::new()).is_err());
}

#[test]
fn an_invalid_record_file_is_reported_once_and_the_service_stays_usable() {
    let directory = tempfile::tempdir().expect("directory");
    let path = store_path(directory.path());
    let config = enabled(path.clone());
    let (service, _, usable) = open_memory_service(&config, &[], &mut Vec::new()).expect("open");
    assert!(usable);
    service.close().expect("close");
    std::fs::write(
        std::path::Path::new(&path).join("records").join("bad.md"),
        "not a record\n",
    )
    .expect("write bad record");

    let mut stderr = Vec::new();
    let (service, _, usable) = open_memory_service(&config, &[], &mut stderr).expect("reopen");
    assert!(usable);
    let stderr = String::from_utf8_lossy(&stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with(SKIPPED_FILE_PREFIX))
        .collect();
    assert_eq!(lines.len(), 1, "{stderr:?}");
    assert!(lines[0].contains("bad.md"), "{}", lines[0]);
    assert!(!stderr.contains("not a record"), "{stderr:?}");
    service.close().expect("close");
}

#[test]
fn a_successful_open_returns_a_stable_user_scope() {
    let directory = tempfile::tempdir().expect("directory");
    let config = enabled(store_path(directory.path()));

    let (service, scope, usable) =
        open_memory_service(&config, &[], &mut Vec::new()).expect("open");
    assert!(usable);
    assert_eq!(scope.namespace, NAMESPACE_USER);
    assert!(!scope.id.is_empty());
    service.close().expect("close");

    let (reopened, again, usable) =
        open_memory_service(&config, &[], &mut Vec::new()).expect("reopen");
    assert!(usable);
    assert_eq!(again, scope, "the installation id must survive a reopen");
    reopened.close().expect("close");
}

#[test]
fn require_encryption_fails_because_turso_never_encrypts_at_rest() {
    let directory = tempfile::tempdir().expect("directory");
    let config = MemoryRuntime {
        require_encryption: true,
        ..enabled(store_path(directory.path()))
    };
    let error = match open_memory_service(&config, &[], &mut Vec::new()) {
        Err(error) => error,
        Ok(_) => panic!("require_encryption must fail startup"),
    };
    assert!(error.contains("encrypt"), "{error}");
}

#[test]
fn configured_secret_values_are_rejected_in_remembered_text() {
    let directory = tempfile::tempdir().expect("directory");
    let config = enabled(store_path(directory.path()));
    let secrets = ["sk-configured-secret".to_string()];

    let (service, scope, usable) =
        open_memory_service(&config, &secrets, &mut Vec::new()).expect("open");
    assert!(usable);
    let error = service
        .remember(&RememberRequest {
            scope,
            kind: "preference".into(),
            key: "credentials".into(),
            text: "the api key is sk-configured-secret".into(),
            ..RememberRequest::default()
        })
        .expect_err("sensitive");
    assert_eq!(error.kind, ErrorKind::SensitiveMemory);
    service.close().expect("close");
}

#[test]
fn an_invalid_secret_value_fails_the_open() {
    let directory = tempfile::tempdir().expect("directory");
    let config = enabled(store_path(directory.path()));
    assert!(
        open_memory_service(&config, &[String::new()], &mut Vec::new()).is_err(),
        "an empty guard value must be rejected"
    );
}

#[test]
fn the_workspace_scope_uses_a_configured_override() {
    let config = MemoryRuntime {
        workspace_ids: std::collections::HashMap::from([(
            "/work/otto".to_string(),
            "custom-id".to_string(),
        )]),
        ..MemoryRuntime::default()
    };
    let scope = workspace_memory_scope(&config, "/work/otto").expect("scope");
    assert_eq!(scope.namespace, NAMESPACE_WORKSPACE);
    assert_eq!(scope.id, "custom-id");
}

#[test]
fn the_workspace_scope_is_derived_from_the_path_without_an_override() {
    let directory = tempfile::tempdir().expect("directory");
    let scope = workspace_memory_scope(
        &MemoryRuntime::default(),
        &directory.path().to_string_lossy(),
    )
    .expect("scope");
    assert_eq!(scope.namespace, NAMESPACE_WORKSPACE);
    let canonical = std::fs::canonicalize(directory.path()).expect("canonical");
    assert_eq!(
        scope.id,
        crate::memory::scope::workspace_id(canonical.to_str().expect("utf-8"))
    );
}

// -----------------------------------------------------------------
// R12: build_subagents reports a takeover through the recovery push.
//
// A takeover is simulated the same way `session::tests` does: a lease
// directory is written by hand (no live `Lease` of this process's own),
// with `lease_seconds` = 6. `Prepared::prepare`/`activate` then performs
// a real takeover, populating the returned store's `take_recovery()` and
// handing this process a live `Keeper::global` lease. `L` must stay at
// or above 6: the watchdog fences a live global-keeper lease at 5L/6
// staleness, and the renewal thread ticks every 1s, so L < 6 leaves no
// margin and calls `libc::_exit(75)` mid-test. `Prepared::prepare`'s
// 7L/6 takeover wait is therefore about 7s per test.
// -----------------------------------------------------------------

fn recovery_test_header(workspace: &Path) -> otto_core::session::Header {
    otto_core::session::Header {
        id: "parent".into(),
        workspace: workspace.to_string_lossy().into_owned(),
        provider: "openai-compatible".into(),
        model: "test-model".into(),
        created_at: chrono::Utc::now(),
        ..otto_core::session::Header::default()
    }
}

fn recovery_dangling_call(id: &str, name: &str) -> otto_core::model::Message {
    otto_core::model::Message {
        role: otto_core::model::Role::Assistant,
        blocks: vec![otto_core::model::Block {
            block_type: otto_core::model::BlockType::ToolCall,
            tool_call_id: id.into(),
            tool_name: name.into(),
            arguments: Some(
                serde_json::value::RawValue::from_string("{}".into()).expect("valid JSON"),
            ),
            ..otto_core::model::Block::default()
        }],
        created_at: chrono::Utc::now(),
        finish_reason: Some(otto_core::model::FinishReason::ToolCalls),
        ..otto_core::model::Message::default()
    }
}

fn recovery_assistant_text(text: &str) -> otto_core::model::Message {
    otto_core::model::Message {
        role: otto_core::model::Role::Assistant,
        blocks: vec![otto_core::model::Block::text(text)],
        created_at: chrono::Utc::now(),
        finish_reason: Some(otto_core::model::FinishReason::Stop),
        ..otto_core::model::Message::default()
    }
}

/// The `lease.json` + `epoch-<epoch>` + `heartbeat` layout
/// `failover::lease` writes, reproduced by hand so no live `Lease` of
/// this process's own is ever created; see the section header above and
/// `session::tests`'s helper of the same shape.
fn recovery_write_lease_dir_by_hand(session_path: &Path, host: &str, pid: u32) {
    let dir = session_path.with_extension("lease");
    std::fs::create_dir_all(&dir).expect("create lease dir by hand");
    std::fs::write(
        dir.join("lease.json"),
        serde_json::json!({ "lease_seconds": 6u64 }).to_string(),
    )
    .expect("write lease.json by hand");
    std::fs::write(dir.join("epoch-1"), b"{}").expect("write epoch marker by hand");
    let json = serde_json::json!({
        "epoch": 1u64,
        "host": host,
        "pid": pid,
        "released": false,
        "seq": 1u64,
    })
    .to_string();
    let mut bytes = vec![b' '; 512];
    bytes[..json.len()].copy_from_slice(json.as_bytes());
    bytes[511] = b'\n';
    std::fs::write(dir.join("heartbeat"), bytes).expect("write heartbeat by hand");
}

/// Builds a `Builder` and its `build_subagents` call args over a fresh
/// workspace/session root, with sub-agents unconditionally enabled.
fn recovery_test_builder(root: &Path) -> (Builder, Runtime, ProviderClient, CatalogWiring) {
    let session_root = root.join("sessions");
    let builder = crate::cli::testutil::builder(root, &session_root);
    let runtime = Runtime {
        provider: "openai-compatible".to_string(),
        base_url: "http://127.0.0.1:0".to_string(),
        model: "test-model".to_string(),
        max_output_bytes: 4096,
        ..Runtime::default()
    };
    let client = ProviderClient::Compat {
        client: Arc::new(crate::provider::openaicompat::Client::new(
            &runtime.base_url,
            "test-key",
        )),
        timeout: runtime.resilience.deadlines.provider_timeout,
        cancellation_grace: runtime.resilience.deadlines.cancellation_grace,
    };
    let catalogs = CatalogWiring {
        discovered_skills: skill::Catalog::default(),
        skills: skill::Catalog::default(),
        skill_section: String::new(),
        agents: AgentsRuntime {
            enabled: true,
            roots: Vec::new(),
            max_parallel: 4,
        },
        agent_catalog: subagent::Catalog::default(),
        agent_section: String::new(),
    };
    (builder, runtime, client, catalogs)
}

#[allow(clippy::too_many_arguments)]
fn recovery_call_build_subagents(
    builder: &Builder,
    runtime: &Runtime,
    client: &ProviderClient,
    catalogs: &CatalogWiring,
    session: &SharedSession,
) -> SubagentWiring {
    let mut tools = Vec::new();
    let mut stderr = Vec::new();
    let redactor = Redactor::with_completeness(&[], true);
    let prompt_for = builder.child_prompt_for(runtime, "host", "");
    builder
        .build_subagents(
            &mut tools,
            catalogs,
            client,
            &[],
            &redactor,
            runtime,
            session,
            prompt_for,
            Vec::new(),
            None,
            None,
            &mut stderr,
        )
        .expect("build_subagents")
}

#[test]
fn a_takeover_with_a_dangling_call_and_an_interrupted_child_pushes_one_recovery_notification() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let session_root = root.path().join("sessions");
    std::fs::create_dir_all(&session_root).expect("create session root");

    let parent = crate::session::Store::create(&session_root, recovery_test_header(&workspace))
        .expect("create parent");
    parent
        .append_message(&recovery_dangling_call("call-1", "read"))
        .expect("append dangling call");
    let parent_path = parent.path();
    parent.close().expect("close before simulating a takeover");

    // A pre-existing inbox sidecar: build_subagents must load this and
    // keep it queued alongside the recovery push, not replace it.
    let inbox_path = Path::new(&parent_path).with_extension("inbox.json");
    std::fs::write(
        &inbox_path,
        serde_json::to_vec_pretty(&[otto_core::agent::inbox::Entry {
            seq: 0,
            notification: otto_core::agent::inbox::Notification {
                task_id: "t0".to_string(),
                kind: Some(otto_core::agent::inbox::NotificationKind::TaskFinished),
                text: "already queued".to_string(),
                usage: None,
                ..Default::default()
            },
        }])
        .expect("encode inbox"),
    )
    .expect("write inbox sidecar");

    // An interrupted child transcript: `otto.task_spec` with no
    // `otto.task_result`.
    let child = crate::session::Store::create_child_lazy(
        &parent_path,
        "t1-child",
        otto_core::session::Header {
            id: "child".into(),
            workspace: workspace.to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("create child");
    child
        .append_custom_entry(
            crate::subagent::runner::TASK_SPEC_CUSTOM_TYPE,
            &serde_json::json!({
                "id": "t1",
                "name": "worker",
                "description": "",
                "model": "test-model",
                "context": "fresh",
                "prompt": "do it",
                "definition": null,
            })
            .to_string(),
        )
        .expect("append task_spec");
    child.close().expect("close child");
    let children_dir = Path::new(&parent_path).with_extension("");

    recovery_write_lease_dir_by_hand(Path::new(&parent_path), "stopped-host", 4242);

    let (activated, _warnings) = crate::failover::lease::without_acquire_wait(|| {
        crate::session::Prepared::prepare(Path::new(&parent_path), None)
    })
    .expect("prepare takes over")
    .activate()
    .expect("activate");
    let session = SharedSession::new(Arc::new(activated));

    let (builder, runtime, client, catalogs) = recovery_test_builder(root.path());
    let wiring = recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);

    let inbox = wiring.inbox.expect("inbox");
    let queued = inbox.queued();
    assert_eq!(queued.len(), 2, "{queued:?}");
    assert_eq!(queued[0].notification.text, "already queued");
    assert!(
        queued[1].notification.text.contains("stopped-host"),
        "{}",
        queued[1].notification.text
    );
    assert!(
        queued[1].notification.text.contains("read"),
        "{}",
        queued[1].notification.text
    );
    assert!(
        queued[1].notification.text.contains("t1"),
        "{}",
        queued[1].notification.text
    );

    let on_disk: Vec<otto_core::agent::inbox::Entry> =
        serde_json::from_str(&std::fs::read_to_string(&inbox_path).expect("read inbox file"))
            .expect("decode inbox file");
    assert_eq!(on_disk.len(), 2, "{on_disk:?}");

    let records = crate::subagent::interrupted::scan(&children_dir);
    let record = records
        .iter()
        .find(|record| record.task_id == "t1")
        .expect("the child transcript was scanned");
    assert_eq!(
        record.final_status,
        Some(crate::subagent::interrupted::INTERRUPTED_STATUS.to_string()),
        "notify's mark_interrupted call must have run after the push"
    );
}

#[test]
fn a_plain_reopen_after_process_loss_notifies_and_marks_children_interrupted() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let session_root = root.path().join("sessions");
    std::fs::create_dir_all(&session_root).expect("create session root");

    let parent = crate::session::Store::create(&session_root, recovery_test_header(&workspace))
        .expect("create parent");
    parent
        .append_custom_entry(
            otto_core::session::TURN_CUSTOM_TYPE,
            &otto_core::session::encode_turn_record(&otto_core::session::TurnRecord::started(
                "op_lost",
                otto_core::session::TurnTrigger::User,
                1,
                None,
            )),
        )
        .expect("turn start");
    parent
        .append_message(&recovery_dangling_call("call-1", "bash"))
        .expect("append dangling call");
    let parent_path = parent.path();
    let child = crate::session::Store::create_child_lazy(
        &parent_path,
        "t1-child",
        otto_core::session::Header {
            id: "child".into(),
            workspace: workspace.to_string_lossy().into_owned(),
            provider: "openai-compatible".into(),
            model: "test-model".into(),
            created_at: chrono::Utc::now(),
            ..otto_core::session::Header::default()
        },
    )
    .expect("create child");
    child
        .append_custom_entry(
            crate::subagent::runner::TASK_SPEC_CUSTOM_TYPE,
            &serde_json::json!({
                "id": "t1",
                "name": "worker",
                "description": "",
                "model": "test-model",
                "context": "fresh",
                "prompt": "do it",
                "definition": null,
            })
            .to_string(),
        )
        .expect("append task_spec");
    child.close().expect("close child");
    // The process stops: the parent is closed without ending its turn,
    // and no lease directory exists.
    parent.close().expect("close without ending the turn");
    let children_dir = Path::new(&parent_path).with_extension("");

    let (activated, _warnings) =
        crate::session::Store::open(Path::new(&parent_path)).expect("reopen");
    let session = SharedSession::new(Arc::new(activated));
    let (builder, runtime, client, catalogs) = recovery_test_builder(root.path());
    let wiring = recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);

    let queued = wiring.inbox.expect("inbox").queued();
    assert_eq!(queued.len(), 1, "{queued:?}");
    let text = &queued[0].notification.text;
    assert!(text.contains("previous Otto process"), "{text}");
    assert!(text.contains("Continue the interrupted turn"), "{text}");
    assert!(text.contains("bash"), "{text}");
    assert!(text.contains("t1"), "{text}");
    let record = crate::subagent::interrupted::scan(&children_dir)
        .into_iter()
        .find(|record| record.task_id == "t1")
        .expect("the child transcript was scanned");
    assert_eq!(
        record.final_status,
        Some(crate::subagent::interrupted::INTERRUPTED_STATUS.to_string()),
    );
}

#[test]
fn a_takeover_with_no_dangling_state_and_no_children_pushes_nothing() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let session_root = root.path().join("sessions");
    std::fs::create_dir_all(&session_root).expect("create session root");

    let parent = crate::session::Store::create(&session_root, recovery_test_header(&workspace))
        .expect("create parent");
    parent
        .append_message(&recovery_assistant_text("all done"))
        .expect("append finished turn");
    let parent_path = parent.path();
    parent.close().expect("close before simulating a takeover");

    recovery_write_lease_dir_by_hand(Path::new(&parent_path), "stopped-host", 4242);

    let (activated, _warnings) = crate::failover::lease::without_acquire_wait(|| {
        crate::session::Prepared::prepare(Path::new(&parent_path), None)
    })
    .expect("prepare takes over")
    .activate()
    .expect("activate");
    assert!(
        activated.take_recovery().is_some(),
        "sanity: the parent must actually have been taken over"
    );
    // Re-open to get an un-taken `take_recovery()` back for the build,
    // since the sanity check above just consumed it.
    activated.close().expect("close");
    let (activated, _warnings) = crate::session::Store::open(Path::new(&parent_path))
        .expect("reopen without a lease dir race: no other holder exists");

    let session = SharedSession::new(Arc::new(activated));
    let (builder, runtime, client, catalogs) = recovery_test_builder(root.path());
    let wiring = recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);

    let inbox = wiring.inbox.expect("inbox");
    assert!(inbox.is_empty(), "{:?}", inbox.queued());
}

#[test]
fn a_second_build_subagents_for_the_same_store_pushes_nothing_more() {
    let root = tempfile::tempdir().expect("root");
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).expect("create workspace");
    let session_root = root.path().join("sessions");
    std::fs::create_dir_all(&session_root).expect("create session root");

    let parent = crate::session::Store::create(&session_root, recovery_test_header(&workspace))
        .expect("create parent");
    parent
        .append_message(&recovery_dangling_call("call-1", "read"))
        .expect("append dangling call");
    let parent_path = parent.path();
    parent.close().expect("close before simulating a takeover");

    recovery_write_lease_dir_by_hand(Path::new(&parent_path), "stopped-host", 4242);

    let (activated, _warnings) = crate::failover::lease::without_acquire_wait(|| {
        crate::session::Prepared::prepare(Path::new(&parent_path), None)
    })
    .expect("prepare takes over")
    .activate()
    .expect("activate");
    let session = SharedSession::new(Arc::new(activated));

    let (builder, runtime, client, catalogs) = recovery_test_builder(root.path());
    let first = recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);
    assert_eq!(first.inbox.expect("inbox").len(), 1);

    // The second call builds a fresh `Tasks`/`Inbox` that reloads the
    // sidecar the first call persisted, so it still shows that one
    // entry; the assertion is that the count does not grow to 2, which
    // it would if `notify` pushed again from a takeover that
    // `take_recovery()` had already consumed on the first call.
    let second = recovery_call_build_subagents(&builder, &runtime, &client, &catalogs, &session);
    assert_eq!(
        second.inbox.expect("inbox").len(),
        1,
        "take_recovery() already consumed the record on the first call, so the second call must not push again"
    );
}
