use super::*;
use crate::fixtures;
use crate::subagent::tasks::{Task, TaskStatus};
use otto_core::config::memory::MemoryRuntime;
use std::path::Path;
use tokio_util::sync::CancellationToken;
mod mcp_commands;
mod skill_commands;
mod task_commands;

fn standalone_service() -> (tempfile::TempDir, crate::memory::Service) {
    let (directory, store) = crate::memory::files::testsupport::open_temp();
    (
        directory,
        crate::memory::Service::from_store(
            std::sync::Arc::new(store),
            crate::memory::decide_default_policy,
        ),
    )
}

#[test]
fn pending_candidates_show_the_advice_line_under_the_candidate() {
    let (_directory, service) = standalone_service();
    let scope = crate::memory::Scope::new(crate::memory::NAMESPACE_USER, "user-1");
    let advised = crate::memory::advice::testing::candidate(&service, scope.clone(), true);

    let text = render_pending_candidates(std::slice::from_ref(&advised));

    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(
        lines[1].starts_with(&format!("id={}", advised.id)),
        "{text}"
    );
    assert_eq!(
        lines[2],
        format!("  {}", crate::memory::advice::testing::LINE)
    );
}

#[test]
fn pending_candidates_without_advice_render_exactly_one_line_each() {
    let (_directory, service) = standalone_service();
    let scope = crate::memory::Scope::new(crate::memory::NAMESPACE_USER, "user-1");
    let plain = crate::memory::advice::testing::candidate(&service, scope, false);

    let text = render_pending_candidates(std::slice::from_ref(&plain));

    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(!text.contains("Advice"), "{text}");
}

#[derive(Clone, Default)]
struct Buffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("buffer").clone()).expect("utf-8")
    }
}

impl std::io::Write for Buffer {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("buffer").extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A controller whose builder carries a usable file-backed memory service.
async fn controller_with_memory(
    workspace: &Path,
    sessions: &Path,
    store_path: &Path,
) -> Controller {
    let mut builder = fixtures::builder(workspace, sessions);
    let runtime = MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: store_path.to_string_lossy().into_owned(),
        ..MemoryRuntime::default()
    };
    let (service, user_scope, usable) =
        crate::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the test store must be usable");
    builder.shared_mut().memory = crate::runtime::wiring::MemoryWiring {
        client: None,
        extension: None,
        plugin: None,
        service,
        usable,
        user_scope,
        recall_limit: 8,
        recall_token_budget: 1000,
        advice_audit: None,
    };
    builder.workspace_scope =
        crate::runtime::wiring::workspace_memory_scope(&runtime, &workspace.to_string_lossy())
            .expect("workspace scope");
    let runtime = fixtures::initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    Controller::new(builder, true, session, runner, info)
}

/// Feeds `input` to a REPL on `controller` and returns its two streams.
async fn session(input: &str, controller: &Controller) -> (String, String) {
    let stdout = Buffer::default();
    let stderr = Buffer::default();
    let mut repl = Repl::new(
        controller,
        Box::new(stdout.clone()),
        Box::new(stderr.clone()),
    );
    repl.run(
        std::io::Cursor::new(input.as_bytes().to_vec()),
        &CancellationToken::new(),
    )
    .await
    .expect("run");
    (stdout.text(), stderr.text())
}

#[test]
fn remember_arguments_parse_flags_and_trailing_text() {
    assert_eq!(
        parse_remember_argument("--scope user --kind fact --key nickname call me Bai"),
        ("user", "fact", "nickname", "call me Bai")
    );
    assert_eq!(
        parse_remember_argument("just some text"),
        ("", "note", "", "just some text")
    );
}

#[test]
fn search_results_render_as_plain_text() {
    assert_eq!(
        render_search_result(&SearchResult::default()),
        "no matching records"
    );
    let record = crate::memory::Record {
        id: "rec-1".into(),
        scope: Scope {
            namespace: "workspace".into(),
            id: "w1".into(),
        },
        kind: "preference".into(),
        key: "editor".into(),
        text: "uses vim".into(),
        revision: 1,
        ..crate::memory::Record::default()
    };
    assert_eq!(
        render_search_result(&SearchResult {
            records: vec![record],
            ..SearchResult::default()
        }),
        "1 records:\nid=rec-1 scope=workspace/w1 kind=preference key=editor revision=1 text=uses vim"
    );
}

#[tokio::test]
async fn the_help_text_lists_the_memory_and_task_commands() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;

    let (stdout, _) = session("/help\n/exit\n", &controller).await;

    for command in ["/memory", "/remember", "/task ", "/agents", "/mcp"] {
        assert!(stdout.contains(command), "{command} missing from {stdout}");
    }
}

#[tokio::test]
async fn memory_commands_without_a_service_are_command_errors() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let controller = fixtures::controller(workspace.path(), sessions.path()).await;

    for (input, command) in [
        ("/memory search vim\n", "/memory"),
        ("/remember prefers dark mode\n", "/remember"),
    ] {
        let mut repl = Repl::new(
            &controller,
            Box::new(Buffer::default()),
            Box::new(Buffer::default()),
        );
        let error = repl
            .run(
                std::io::Cursor::new(input.as_bytes().to_vec()),
                &CancellationToken::new(),
            )
            .await
            .expect_err("command error");
        assert!(
            super::super::repl::is_command_error(&error, command),
            "{error:?}"
        );
        assert_eq!(error.to_string(), MEMORY_UNAVAILABLE);
    }
}

#[tokio::test]
async fn memory_and_remember_round_trip_through_the_store() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let controller = controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;

    let (stdout, stderr) = session(
        "/memory\n/remember --scope user\n/remember --scope bogus text\n\
             /remember --kind preference --key editor vim\n/memory search vim\n/exit\n",
        &controller,
    )
    .await;

    assert!(stderr.contains(MEMORY_USAGE), "{stderr}");
    assert!(stderr.contains(REMEMBER_USAGE), "{stderr}");
    assert!(
        stderr.contains("unknown scope \"bogus\", want user or workspace"),
        "{stderr}"
    );
    assert!(stdout.contains("remembered "), "{stdout}");
    assert!(stdout.contains("kind=preference"), "{stdout}");
    assert!(stdout.contains("1 records:"), "{stdout}");
    assert!(stdout.contains("text=vim"), "{stdout}");
}

#[tokio::test]
async fn forget_resolves_the_revision_and_reports_a_missing_record() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let controller = controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    let (service, _, workspace_scope) = controller.memory_manager().expect("memory");
    let record = service
        .remember(&RememberRequest {
            scope: workspace_scope,
            kind: "note".into(),
            text: "vim".into(),
            ..RememberRequest::default()
        })
        .expect("remember");

    let (stdout, _) = session(
        &format!("/memory forget {}\n/exit\n", record.id),
        &controller,
    )
    .await;
    assert!(
        stdout.contains(&format!("forgot {} (revision 1)", record.id)),
        "{stdout}"
    );

    let mut repl = Repl::new(
        &controller,
        Box::new(Buffer::default()),
        Box::new(Buffer::default()),
    );
    let error = repl
        .run(
            std::io::Cursor::new(b"/memory forget missing\n".to_vec()),
            &CancellationToken::new(),
        )
        .await
        .expect_err("command error");
    assert!(
        super::super::repl::is_command_error(&error, "/memory forget"),
        "{error:?}"
    );
    assert!(error.to_string().contains("not found"), "{error}");
}

fn propose_pending(controller: &Controller, text: &str) -> String {
    let (service, _, workspace_scope) = controller.memory_manager().expect("memory");
    service
        .propose(&crate::memory::ProposeRequest {
            scope: workspace_scope,
            kind: "preference".into(),
            key: "editor".into(),
            text: text.into(),
            reason: "said so".into(),
            confidence: 0.5,
            source: crate::memory::Provenance {
                origin: Some(crate::memory::Origin::Model),
                ..crate::memory::Provenance::default()
            },
            ..crate::memory::ProposeRequest::default()
        })
        .expect("propose")
        .remove(0)
        .id
}

#[tokio::test]
async fn review_lists_accepts_and_rejects_pending_candidates() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let controller = controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;

    let (stdout, _) = session("/memory review\n/exit\n", &controller).await;
    assert!(stdout.contains("no pending candidates"), "{stdout}");

    let accepted = propose_pending(&controller, "uses vim");
    let (stdout, _) = session("/memory review\n/exit\n", &controller).await;
    assert!(stdout.contains("1 pending candidates:"), "{stdout}");
    assert!(stdout.contains(&format!("id={accepted}")), "{stdout}");
    assert!(stdout.contains("origin=model"), "{stdout}");
    assert!(stdout.contains("text=uses vim"), "{stdout}");
    assert!(stdout.contains("reason=said so"), "{stdout}");

    let (stdout, _) = session(
        &format!("/memory review {accepted} accept\n/memory list\n/exit\n"),
        &controller,
    )
    .await;
    assert!(
        stdout.contains(&format!("accepted {accepted} as record ")),
        "{stdout}"
    );
    assert!(stdout.contains("uses vim"), "{stdout}");

    let rejected = propose_pending(&controller, "uses emacs");
    let (stdout, _) = session(
        &format!("/memory review {rejected} reject\n/memory list\n/memory review\n/exit\n"),
        &controller,
    )
    .await;
    assert!(stdout.contains(&format!("rejected {rejected}")), "{stdout}");
    // The unified queue may show the candidate before the rejection. Check
    // the canonical store, rather than confusing review text with a write.
    let (memory, user, workspace) = controller.memory_manager().unwrap();
    let records = memory
        .list(&crate::memory::ListRequest {
            scopes: vec![user, workspace.clone()],
            all_scopes: false,
            kinds: Vec::new(),
            labels: Vec::new(),
            limit: 50,
            cursor: String::new(),
            now: Utc::now(),
            include_expired: false,
        })
        .unwrap()
        .records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].text, "uses vim");
    assert_eq!(
        memory
            .get_candidate(&crate::memory::CandidateRef {
                scope: workspace,
                id: rejected.clone()
            })
            .unwrap()
            .state,
        crate::memory::CandidateState::Rejected
    );
    assert!(stdout.contains("no pending candidates"), "{stdout}");
}

#[tokio::test]
async fn review_reports_unknown_ids_and_usage_mistakes() {
    let workspace = tempfile::tempdir().expect("workspace");
    let sessions = tempfile::tempdir().expect("sessions");
    let store = tempfile::tempdir().expect("store");
    let controller = controller_with_memory(
        workspace.path(),
        sessions.path(),
        &store.path().join("memory"),
    )
    .await;
    let id = propose_pending(&controller, "uses vim");

    let (_, stderr) = session(
        &format!("/memory review {id} maybe\n/memory review {id}\n/memory review a b c\n/exit\n"),
        &controller,
    )
    .await;
    assert_eq!(stderr.matches(MEMORY_USAGE).count(), 3, "{stderr}");

    let mut repl = Repl::new(
        &controller,
        Box::new(Buffer::default()),
        Box::new(Buffer::default()),
    );
    let error = repl
        .run(
            std::io::Cursor::new(b"/memory review missing accept\n".to_vec()),
            &CancellationToken::new(),
        )
        .await
        .expect_err("command error");
    assert!(
        super::super::repl::is_command_error(&error, "/memory review"),
        "{error:?}"
    );
    assert!(error.to_string().contains("not found"), "{error}");
}
