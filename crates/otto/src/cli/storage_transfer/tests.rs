use super::*;
use crate::hex::to_hex;
use otto_core::approval::ApprovalMode;
use std::os::unix::fs::DirBuilderExt;

fn fixture() -> (tempfile::TempDir, Options) {
    let directory = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    let home = directory.path().join("home");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&home)
        .unwrap();
    (
        directory,
        Options {
            home,
            destination: PathBuf::new(),
            id: "move-1".into(),
        },
    )
}
fn approvals(home: &Path) -> otto_core::approval::Scope {
    let service = crate::approval::Service::open(&home.join(".otto/approvals.db")).unwrap();
    let scope = otto_core::approval::Scope {
        workspace: "/workspace".into(),
        session_id: "parent".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    scope
}
fn journal(root: &Path) -> Vec<u8> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(root.join("workspace"))
        .unwrap();
    let header = serde_json::json!({"type":"session","version":3,"id":"parent","timestamp":"2026-10-06T00:00:00Z","cwd":"/workspace","futureHeader":{"keep":true}});
    let runtime = serde_json::json!({"type":"custom",
        "id":"00000001",
        "parentId":null,
        "timestamp":"2026-10-06T00:00:00Z",
        "customType":"otto.runtime",
        "data":{"profile":"test",
        "provider":"openai-compatible",
        "model":"test"}});
    let entry = serde_json::json!({"type":"future_entry","id":"00000002","parentId":"00000001","timestamp":"2026-10-06T00:00:01Z","unknown":{"verbatim":"yes"}});
    let bytes = format!("{header}\n{runtime}\n{entry}\n").into_bytes();
    let path = root.join("workspace/parent.jsonl");
    std::fs::write(&path, &bytes).unwrap();
    crate::session::Store::read_entries(&path).expect("ordinary native journal replay");
    bytes
}
fn args(options: &Options) -> Vec<String> {
    vec![
        endpoint(&options.home).unwrap(),
        endpoint(&options.destination).unwrap(),
        options.id.clone(),
    ]
}

#[tokio::test]
async fn command_transfers_all_existing_families_and_exact_pi_bytes() {
    let (directory, mut options) = fixture();
    options.destination = directory.path().join("stage");
    let scope = approvals(&options.home);
    let backend = &options.home.join(".otto");
    let memory = backend.join("memory");
    std::fs::create_dir_all(&memory).unwrap();
    std::fs::write(memory.join("identity.json"), b"{}").unwrap();
    drop(
        crate::storage::backends::sqlite::workflow::SqliteWorkflowStore::open(
            &backend.join("workflows.db"),
        )
        .unwrap(),
    );
    drop(
        crate::storage::backends::sqlite::schedule::SqliteScheduleStore::open(
            &backend.join("schedule.db"),
        )
        .unwrap(),
    );
    drop(crate::storage::backends::sqlite::usage::Store::open(&backend.join("usage.db")).unwrap());
    drop(crate::storage::backends::sqlite::tasks::Store::open(&backend.join("tasks.db")).unwrap());
    drop(
        crate::storage::backends::sqlite::skill_checks::Store::open(
            &backend.join("skill-checks.db"),
        )
        .unwrap(),
    );
    drop(
        crate::storage::backends::sqlite::memory_advice::Store::open(
            &backend.join("memory-advice.db"),
        )
        .unwrap(),
    );
    let bytes = journal(&backend.join("sessions"));
    let argv = args(&options);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    assert_eq!(
        run(&argv, &mut stdout, &mut stderr).await,
        0,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(String::from_utf8_lossy(&stdout).contains("transferred 9 domains"));
    assert_eq!(
        std::fs::read(options.destination.join("sessions/workspace/parent.jsonl")).unwrap(),
        bytes
    );
    assert_eq!(
        std::fs::read(backend.join("sessions/workspace/parent.jsonl")).unwrap(),
        bytes
    );
    assert!(!options.destination.join("memory.db").exists());
    assert_eq!(std::fs::read(memory.join("identity.json")).unwrap(), b"{}");
    let runtime = crate::runtime::storage::Storage::open_home(&options.home)
        .await
        .unwrap();
    let service = crate::approval::Service::from_store(
        runtime.approvals(&backend.join("approvals.db")).unwrap(),
    )
    .unwrap();
    assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Manual);
    drop(service);
    drop(runtime);
    assert_eq!(
        run(&argv, &mut Vec::new(), &mut stderr).await,
        0,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(
        abort(
            &[endpoint(&options.home).unwrap(), options.id.clone()],
            &mut Vec::new(),
            &mut stderr
        )
        .await,
        1
    );
    assert!(String::from_utf8_lossy(&stderr).contains("published destination"));
}

#[tokio::test]
async fn live_native_writer_and_unrelated_destination_are_refused() {
    let (directory, mut options) = fixture();
    options.destination = directory.path().join("stage");
    approvals(&options.home);
    let connection = Connection::open(options.home.join(".otto/approvals.db")).unwrap();
    let transaction = connection.unchecked_transaction().unwrap();
    let mut error = Vec::new();
    assert_eq!(run(&args(&options), &mut Vec::new(), &mut error).await, 1);
    assert!(String::from_utf8_lossy(&error).contains("writer"));
    drop(transaction);
    drop(connection);
    options.destination = directory.path().join("unrelated");
    std::fs::create_dir(&options.destination).unwrap();
    let unrelated = options.destination.join("important");
    std::fs::write(&unrelated, b"retain").unwrap();
    assert_eq!(
        run(&args(&options), &mut Vec::new(), &mut Vec::new()).await,
        1
    );
    assert_eq!(std::fs::read(&unrelated).unwrap(), b"retain");
}

#[tokio::test]
async fn bootstrap_resumes_partial_marker_and_symlink_routes_fail_closed() {
    let (directory, mut options) = fixture();
    options.destination = directory.path().join("stage");
    approvals(&options.home);
    let owner = Owner {
        version: 1,
        migration_id: options.id.clone(),
        source_identity: identity(&options.home),
        destination_identity: identity(&options.destination),
    };
    let bootstrap = directory.path().join(format!(
        ".otto-transfer-{}",
        to_hex(&Sha256::digest(serde_json::to_vec(&owner).unwrap()))
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&bootstrap)
        .unwrap();
    std::fs::write(bootstrap.join(OWNER), b"{partial").unwrap();
    let mut stderr = Vec::new();
    assert_eq!(
        run(&args(&options), &mut Vec::new(), &mut stderr).await,
        0,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(!bootstrap.exists());
    let other_home = directory.path().join("other-home");
    std::fs::create_dir_all(other_home.join(".otto")).unwrap();
    std::os::unix::fs::symlink(
        options.destination.join("approvals.db"),
        other_home.join(".otto/approvals.db"),
    )
    .unwrap();
    options.home = other_home;
    options.destination = directory.path().join("refused");
    assert_eq!(
        run(&args(&options), &mut Vec::new(), &mut Vec::new()).await,
        1
    );
    assert!(!options.destination.exists());
}
#[tokio::test]
async fn published_memory_route_is_refused_by_name() {
    let (directory, mut options) = fixture();
    options.destination = directory.path().join("moved");
    approvals(&options.home);
    journal(&options.home.join(".otto/sessions"));
    let control = options.home.join(".otto/storage");
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&control)
        .unwrap();
    let route = |backend: &str, path: PathBuf| StorageRoute {
        backend: backend.into(),
        endpoint: endpoint(&path).unwrap(),
        namespace_identity: None,
    };
    LocalCutover::open(
        &control,
        RouteGroup {
            identity: identity(&options.home),
            generation: "initial".into(),
            writer_epoch: "initial".into(),
            domains: BTreeMap::from([
                (
                    "approvals".into(),
                    route("sqlite", options.home.join(".otto/approvals.db")),
                ),
                (
                    "memory".into(),
                    route("sqlite", options.home.join("routed/memory.db")),
                ),
                (
                    "sessions".into(),
                    route("posix", options.home.join(".otto/sessions")),
                ),
            ]),
        },
    )
    .unwrap();
    let mut stderr = Vec::new();
    assert_eq!(run(&args(&options), &mut Vec::new(), &mut stderr).await, 1);
    assert!(
        String::from_utf8_lossy(&stderr).contains("published storage routes include memory"),
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    assert!(!options.destination.exists());
}
#[tokio::test]
async fn live_memory_approval_transfers_while_memory_stays_in_the_source_home() {
    let (directory, mut options) = fixture();
    options.destination = directory.path().join("moved");
    let scope = approvals(&options.home);
    journal(&options.home.join(".otto/sessions"));
    let service = crate::approval::Service::open(&options.home.join(".otto/approvals.db")).unwrap();
    let request = service
        .create(crate::approval::Proposal {
            kind: otto_core::approval::Kind::Authorization,
            authority: otto_core::approval::Authority::Durable,
            scope: scope.clone(),
            subject: otto_core::approval::Subject {
                tool_call_id: String::new(),
                source: "memory".into(),
                id: "user/workspace/scope/candidate".into(),
                version: "1".into(),
                details: "pending".into(),
            },
            title: "Review".into(),
            reason: "test".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap();
    drop(service);
    let mut stderr = Vec::new();
    assert_eq!(
        run(&args(&options), &mut Vec::new(), &mut stderr).await,
        0,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    let moved = crate::approval::Service::open(&options.destination.join("approvals.db")).unwrap();
    assert_eq!(
        moved
            .get(&request.id, &scope.workspace, Some(&scope.session_id))
            .unwrap()
            .state,
        otto_core::approval::State::Pending
    );
}
#[tokio::test]
async fn command_abort_restores_source_after_failed_closure_without_deleting_staging() {
    let (directory, mut options) = fixture();
    options.destination = directory.path().join("failed-stage");
    let scope = approvals(&options.home);
    journal(&options.home.join(".otto/sessions"));
    let service = crate::approval::Service::open(&options.home.join(".otto/approvals.db")).unwrap();
    let request = service
        .create(crate::approval::Proposal {
            kind: otto_core::approval::Kind::Authorization,
            authority: otto_core::approval::Authority::Durable,
            scope: scope.clone(),
            subject: otto_core::approval::Subject {
                tool_call_id: String::new(),
                source: "unsupported-live-source".into(),
                id: "op".into(),
                version: "1".into(),
                details: "pending".into(),
            },
            title: "Review".into(),
            reason: "test".into(),
            persistent: false,
            blocking: true,
            expires_at: None,
        })
        .unwrap();
    drop(service);
    let mut stderr = Vec::new();
    assert_eq!(run(&args(&options), &mut Vec::new(), &mut stderr).await, 1);
    assert!(String::from_utf8_lossy(&stderr).contains("unsupported live approval subject"));
    let control = LocalCutover::open_existing(&options.home.join(".otto/storage")).unwrap();
    assert!(
        control
            .ensure_writable(&identity(&options.home), "initial")
            .await
            .is_err()
    );
    let staged_before = std::fs::read(options.destination.join("approvals.db")).unwrap();
    let abort_args = vec![endpoint(&options.home).unwrap(), options.id.clone()];
    assert_eq!(
        abort(&abort_args, &mut Vec::new(), &mut stderr).await,
        0,
        "{}",
        String::from_utf8_lossy(&stderr)
    );
    control
        .ensure_writable(&identity(&options.home), "initial")
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(options.destination.join("approvals.db")).unwrap(),
        staged_before
    );
    let original =
        crate::approval::Service::open(&options.home.join(".otto/approvals.db")).unwrap();
    assert_eq!(
        original
            .get(&request.id, &scope.workspace, Some(&scope.session_id))
            .unwrap()
            .state,
        otto_core::approval::State::Pending
    );
    drop(original);
    assert_eq!(abort(&abort_args, &mut Vec::new(), &mut stderr).await, 0);

    let unrelated = directory.path().join("untouched-home");
    std::fs::create_dir(&unrelated).unwrap();
    assert_eq!(
        abort(
            &[endpoint(&unrelated).unwrap(), "missing".into()],
            &mut Vec::new(),
            &mut Vec::new()
        )
        .await,
        1
    );
    assert!(!unrelated.join(".otto").exists());
}
