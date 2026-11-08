use super::*;
use crate::sandbox::{Denial, Error as SandboxError, ExitStatus};
use std::sync::Mutex;

struct RecordingExecutor {
    requests: Mutex<Vec<SandboxRequest>>,
    outcomes: Mutex<Vec<(ExitStatus, Result<(), SandboxError>)>>,
}

impl RecordingExecutor {
    fn new(outcomes: Vec<(ExitStatus, Result<(), SandboxError>)>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            outcomes: Mutex::new(outcomes),
        })
    }
}

#[async_trait]
impl CommandExecutor for RecordingExecutor {
    async fn execute(
        &self,
        request: SandboxRequest,
        streams: Streams<'_>,
        _cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), SandboxError>) {
        self.requests.lock().unwrap().push(request);
        let _ = streams.stdout.write_all(b"must not reach model");
        self.outcomes.lock().unwrap().remove(0)
    }
}

fn test_plan() -> (tempfile::TempDir, WorktreePlan) {
    let temp = tempfile::tempdir().unwrap();
    let target = Target {
        repository: temp.path().to_path_buf(),
        common_dir: temp.path().to_path_buf(),
    };
    let plan = plan(target, "d1", "agent/test".into()).unwrap();
    (temp, plan)
}

/// One scripted command result: exit status, execution outcome and stdout.
type ScriptedOutput = (ExitStatus, Result<(), SandboxError>, String);

struct InspectorExecutor {
    requests: Mutex<Vec<SandboxRequest>>,
    outputs: Mutex<Vec<ScriptedOutput>>,
}

impl InspectorExecutor {
    fn new(outputs: Vec<ScriptedOutput>) -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            outputs: Mutex::new(outputs),
        })
    }
}

#[async_trait]
impl CommandExecutor for InspectorExecutor {
    async fn execute(
        &self,
        request: SandboxRequest,
        streams: Streams<'_>,
        _cancel: &CancellationToken,
    ) -> (ExitStatus, Result<(), SandboxError>) {
        self.requests.lock().unwrap().push(request);
        let (status, result, output) = self.outputs.lock().unwrap().remove(0);
        let _ = streams.stdout.write_all(output.as_bytes());
        (status, result)
    }
}

struct FakePreflight {
    previews: Mutex<usize>,
}

#[async_trait]
impl Preflight for FakePreflight {
    async fn preview(
        &self,
        source_workspace: &str,
        request: &Request,
        _cancel: &CancellationToken,
    ) -> Result<Preview, String> {
        *self.previews.lock().unwrap() += 1;
        let root = PathBuf::from("/target");
        Ok(Preview {
            source_workspace: source_workspace.into(),
            plan: WorktreePlan {
                target: Target {
                    repository: root.clone(),
                    common_dir: PathBuf::from("/git/common"),
                },
                delivery_id: "pending".into(),
                branch: branch(request.branch.as_deref(), &request.objective)?,
                path: root.join(".otto/delivery-worktrees/pending"),
            },
            base: "a".repeat(40),
            handoff: handoff(
                &request.objective,
                request
                    .source_refs
                    .iter()
                    .enumerate()
                    .map(|(index, value)| (format!("source-ref-{}", index + 1), value.clone())),
            )?,
        })
    }
}

struct FakeService {
    prepares: Mutex<Vec<String>>,
    starts: Mutex<Vec<String>>,
}

impl FakeService {
    fn new() -> Self {
        Self {
            prepares: Mutex::new(Vec::new()),
            starts: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl Service for FakeService {
    async fn prepare(&self, approval_id: &str, preview: &Preview) -> Result<Receipt, String> {
        self.prepares.lock().unwrap().push(approval_id.into());
        Ok(Receipt {
            id: format!("receipt-{approval_id}"),
            preview: preview.clone(),
            base: preview.base.clone(),
        })
    }

    async fn start(
        &self,
        receipt: &Receipt,
        _cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.starts.lock().unwrap().push(receipt.id.clone());
        Ok(format!("started {}", receipt.id))
    }
}

fn raw(value: &str) -> Box<RawValue> {
    RawValue::from_string(value.into()).unwrap()
}

fn approval(id: &str, spec: &ApprovalSpec) -> otto_core::approval::Request {
    otto_core::approval::Request {
        options: Vec::new(),
        schema_version: 1,
        id: id.into(),
        revision: 1,
        kind: otto_core::approval::Kind::Authorization,
        authority: otto_core::approval::Authority::Boundary,
        scope: otto_core::approval::Scope {
            workspace: "/source".into(),
            session_id: "s1".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: otto_core::approval::Subject {
            tool_call_id: "call".into(),
            source: "delivery".into(),
            id: "op".into(),
            version: spec.version.clone(),
            details: spec.details.clone(),
        },
        title: spec.title.clone(),
        reason: spec.reason.clone(),
        persistent: false,
        blocking: true,
        created_at: 1,
        expires_at: Some(2),
        state: otto_core::approval::State::Approved,
        decision: None,
        application: otto_core::approval::ApplicationState::Applying,
        application_error: String::new(),
        review: None,
    }
}

#[tokio::test]
async fn tool_binds_stable_subject_then_starts_only_prepared_receipt() {
    let service = Arc::new(FakeService::new());
    let preflight = Arc::new(FakePreflight {
        previews: Mutex::new(0),
    });
    let tool = StartDeliveryTask::new("/source".into(), service.clone(), preflight.clone());
    let arguments =
        raw(r#"{"target":"/target","objective":"Ship diagram","source_refs":["message:1"]}"#);
    let spec = tool
        .approval_spec_async(&arguments, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap();
    assert!(spec.details.contains("/target"));
    let request = approval("a1", &spec);
    tool.apply_approval(&arguments, &CancellationToken::new(), &request)
        .await
        .unwrap();
    let operation_id = serde_json::from_str(r#""op_delivery_test""#).unwrap();
    let call = ToolCall {
        tool_call_id: "call",
        operation_id: &operation_id,
        name: TOOL_NAME,
        arguments: &arguments,
        attempt: 1,
    };

    let result = tool
        .execute_approved(call, &CancellationToken::new(), &request)
        .await;
    assert!(!result.is_error, "{}", result.content);
    assert_eq!(result.content, "started receipt-a1");
    assert_eq!(service.prepares.lock().unwrap().as_slice(), ["a1"]);
    assert_eq!(service.starts.lock().unwrap().as_slice(), ["receipt-a1"]);
    assert_eq!(*preflight.previews.lock().unwrap(), 2);
    assert!(tool.receipts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn tool_rejects_unknown_or_unsafe_arguments_before_async_preflight() {
    let preflight = Arc::new(FakePreflight {
        previews: Mutex::new(0),
    });
    let tool = StartDeliveryTask::new(
        "/source".into(),
        Arc::new(FakeService::new()),
        preflight.clone(),
    );
    assert!(
        tool.approval_spec(&raw(r#"{"target":"/target","objective":"x"}"#))
            .is_err()
    );
    assert!(
        tool.approval_spec_async(
            &raw(r#"{"target":"relative","objective":"x"}"#),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert!(
        tool.approval_spec_async(
            &raw(r#"{"target":"/target","objective":"x","extra":true}"#),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert_eq!(*preflight.previews.lock().unwrap(), 0);
}

#[test]
fn generated_branch_is_safe_and_stable() {
    assert_eq!(
        branch(None, "Build the Otto diagram!").unwrap(),
        "agent/build-the-otto-diagram"
    );
    assert!(branch(Some("agent/../bad"), "x").is_err());
    assert!(branch(Some("-bad"), "x").is_err());
}

#[test]
fn rendered_handoff_contains_only_frozen_content() {
    let handoff = handoff(
        "Ship the target change",
        [("source-ref-1".into(), "Selected summary".into())],
    )
    .unwrap();
    let rendered = render_handoff_prompt(&handoff);
    assert!(rendered.contains("Ship the target change"));
    assert!(rendered.contains("Selected summary"));
    assert!(!rendered.contains("/source"));
}

#[test]
fn handoff_is_frozen_bounded_and_hashed() {
    let packet = handoff("Deliver it", [("design.md".into(), "diagram".into())]).unwrap();
    assert_eq!(
        packet.bytes,
        "Deliver it".len() + "design.md".len() + "diagram".len()
    );
    assert_eq!(packet.items[0].sha256, hash("diagram"));
    assert_eq!(packet.digest.len(), 64);
    assert!(handoff("x", [("a".into(), "x".repeat(HANDOFF_MAX_BYTES))]).is_err());
}

#[test]
fn plan_stays_under_target_and_refuses_invalid_id() {
    let temp = tempfile::tempdir().unwrap();
    let target = Target {
        repository: temp.path().to_path_buf(),
        common_dir: temp.path().to_path_buf(),
    };
    let prepared = plan(target, "d1", "agent/test".into()).unwrap();
    assert_eq!(
        prepared.path,
        temp.path().join(".otto/delivery-worktrees/d1")
    );
    let other = tempfile::tempdir().unwrap();
    let target = Target {
        repository: other.path().to_path_buf(),
        common_dir: other.path().to_path_buf(),
    };
    assert!(plan(target, "bad/id", "agent/test".into()).is_err());
}

pub(super) fn binding(approval_id: &str) -> ReceiptBinding {
    ReceiptBinding {
        approval_id: approval_id.into(),
        source_workspace: "/source".into(),
        target_repository: "/target".into(),
        target_common_dir: "/git/common".into(),
        worktree: "/target/.otto/delivery-worktrees/d1".into(),
        branch: "agent/test".into(),
        base: "a".repeat(40),
        handoff_digest: "b".repeat(64),
    }
}

#[tokio::test]
async fn sandbox_inspector_uses_only_fixed_read_only_git_requests() {
    let target = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(target.path()).unwrap();
    let git_dir = root.join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    let head = "a".repeat(40);
    let executor = InspectorExecutor::new(vec![
        (
            ExitStatus::default(),
            Ok(()),
            format!("{}\n", root.display()),
        ),
        (
            ExitStatus::default(),
            Ok(()),
            format!("{}\n", git_dir.display()),
        ),
        (
            ExitStatus::default(),
            Ok(()),
            format!("{}\n", git_dir.display()),
        ),
        (ExitStatus::default(), Ok(()), format!("{head}\n")),
    ]);
    let inspector = SandboxTargetInspector::new(executor.clone(), vec!["PATH=/usr/bin".into()]);
    let (identity, base) = inspector
        .inspect(&root, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(identity.repository, root);
    assert_eq!(identity.common_dir, git_dir);
    assert_eq!(base, head);
    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    for request in requests.iter() {
        assert_eq!(request.dir, identity.repository);
        assert_eq!(request.env, vec!["PATH=/usr/bin"]);
        assert!(request.report_denials);
        assert_eq!(
            &request.argv[..7],
            [
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "protocol.ext.allow=never"
            ]
        );
    }
    assert_eq!(&requests[0].argv[7..], ["rev-parse", "--show-toplevel"]);
    assert_eq!(
        &requests[1].argv[7..],
        ["rev-parse", "--path-format=absolute", "--git-common-dir"]
    );
    assert_eq!(
        &requests[2].argv[7..],
        ["rev-parse", "--path-format=absolute", "--git-dir"]
    );
    assert_eq!(&requests[3].argv[7..], ["rev-parse", "HEAD"]);
}

#[tokio::test]
async fn sandbox_inspector_rejects_command_failure_and_large_output() {
    let target = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(target.path()).unwrap();
    let failed = InspectorExecutor::new(vec![(
        ExitStatus {
            code: 1,
            ..ExitStatus::default()
        },
        Ok(()),
        String::new(),
    )]);
    assert!(
        SandboxTargetInspector::new(failed, Vec::new())
            .inspect(&root, &CancellationToken::new())
            .await
            .unwrap_err()
            .contains("inspection failed")
    );

    let oversized = InspectorExecutor::new(vec![(
        ExitStatus::default(),
        Ok(()),
        "x".repeat(MAX_GIT_INSPECTION_BYTES + 1),
    )]);
    assert!(
        SandboxTargetInspector::new(oversized, Vec::new())
            .inspect(&root, &CancellationToken::new())
            .await
            .unwrap_err()
            .contains("output exceeds limit")
    );
}

#[tokio::test]
async fn sandbox_preflight_refuses_unadmitted_target_before_git_inspection() {
    let startup = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let executor = InspectorExecutor::new(Vec::new());
    let preflight = SandboxPreflight::new(
        AdmissionPolicy {
            startup: std::fs::canonicalize(startup.path()).unwrap(),
            roots: Vec::new(),
            trusted: Vec::new(),
        },
        SandboxTargetInspector::new(executor.clone(), Vec::new()),
    );
    let error = preflight
        .preview(
            "/source",
            &Request {
                target: std::fs::canonicalize(outside.path())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
                objective: "ship it".into(),
                branch: None,
                source_refs: Vec::new(),
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error, "delivery target is not an admitted workspace");
    assert!(executor.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn sandbox_preflight_binds_admitted_sandbox_identity_to_preview() {
    let target = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(target.path()).unwrap();
    let git_dir = root.join(".git");
    std::fs::create_dir(&git_dir).unwrap();
    let head = "a".repeat(40);
    let executor = InspectorExecutor::new(vec![
        (
            ExitStatus::default(),
            Ok(()),
            format!("{}\n", root.display()),
        ),
        (
            ExitStatus::default(),
            Ok(()),
            format!("{}\n", git_dir.display()),
        ),
        (
            ExitStatus::default(),
            Ok(()),
            format!("{}\n", git_dir.display()),
        ),
        (ExitStatus::default(), Ok(()), format!("{head}\n")),
    ]);
    let preflight = SandboxPreflight::new(
        AdmissionPolicy {
            startup: root.clone(),
            roots: Vec::new(),
            trusted: Vec::new(),
        },
        SandboxTargetInspector::new(executor, Vec::new()),
    );
    let preview = preflight
        .preview(
            "/source",
            &Request {
                target: root.to_string_lossy().into_owned(),
                objective: "Ship diagram".into(),
                branch: None,
                source_refs: vec!["message:1".into()],
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(preview.plan.target.repository, root);
    assert_eq!(preview.base, head);
    assert_eq!(preview.plan.branch, "agent/ship-diagram");
    assert_eq!(preview.handoff.digest.len(), 64);
    assert!(
        preview
            .plan
            .path
            .ends_with(".otto/delivery-worktrees/".to_string() + &preview.plan.delivery_id)
    );
}

#[test]
fn receipt_binding_is_idempotent_and_never_overwritten() {
    let store = ReceiptStore::in_memory();
    let original = binding("approval-1");
    assert_eq!(
        store.prepare(original.clone()).unwrap().state,
        ReceiptState::Prepared
    );
    assert_eq!(store.prepare(original.clone()).unwrap().binding, original);
    let mut conflicting = original;
    conflicting.branch = "agent/other".into();
    assert!(
        store
            .prepare(conflicting)
            .unwrap_err()
            .contains("conflicts")
    );
}

#[test]
fn receipt_persists_immutable_binding_across_reopen() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private/delivery-receipts.db");
    let original = binding("approval-1");
    ReceiptStore::open(&path)
        .unwrap()
        .prepare(original.clone())
        .unwrap();
    let reopened = ReceiptStore::open(&path).unwrap();
    assert_eq!(
        reopened.get("approval-1").unwrap().unwrap().binding,
        original
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}

#[test]
fn delivery_task_ledger_is_terminal_once_and_notifies_source_once() {
    let store = ReceiptStore::in_memory();
    let worktree = Path::new("/target/.otto/delivery-worktrees/delivery-1");
    store.prepare(binding("approval-1")).unwrap();
    let task = store
        .begin_task("approval-1", "source-session", "target-session", worktree)
        .unwrap();
    assert_eq!(task.state, TaskState::Running);
    assert_eq!(
        store
            .finish_task("approval-1", TaskState::Succeeded, "done", "")
            .unwrap()
            .state,
        TaskState::Succeeded
    );
    assert!(
        store
            .finish_task("approval-1", TaskState::Failed, "", "changed")
            .unwrap_err()
            .contains("already terminal")
    );
    assert!(
        store
            .claim_source_notification("approval-1")
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .claim_source_notification("approval-1")
            .unwrap()
            .is_none()
    );
}

#[test]
fn receipt_session_binding_is_durable_and_never_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("private/delivery-receipts.db");
    let worktree = directory.path().join("worktree");
    let store = ReceiptStore::open(&path).unwrap();
    store.prepare(binding("approval-1")).unwrap();
    store
        .bind_session("approval-1", "session-1", &worktree)
        .unwrap();
    store
        .bind_session("approval-1", "session-1", &worktree)
        .unwrap();
    assert_eq!(
        store.session("approval-1").unwrap(),
        Some(("session-1".into(), worktree.to_string_lossy().into_owned()))
    );
    assert!(
        store
            .bind_session("approval-1", "session-2", &worktree)
            .unwrap_err()
            .contains("conflicts")
    );
    drop(store);
    let reopened = ReceiptStore::open(&path).unwrap();
    assert_eq!(
        reopened.session("approval-1").unwrap(),
        Some(("session-1".into(), worktree.to_string_lossy().into_owned()))
    );
}

#[test]
fn receipt_requires_prepare_then_single_direction_transitions() {
    let store = ReceiptStore::in_memory();
    assert!(store.begin("missing").is_err());
    store.prepare(binding("approval-1")).unwrap();
    assert!(
        store
            .started("approval-1")
            .unwrap_err()
            .contains("invalid state")
    );
    assert_eq!(
        store.begin("approval-1").unwrap().state,
        ReceiptState::Applying
    );
    assert_eq!(
        store.started("approval-1").unwrap().state,
        ReceiptState::Started
    );
    assert_eq!(
        store.started("approval-1").unwrap().state,
        ReceiptState::Started
    );
    assert!(
        store
            .begin("approval-1")
            .unwrap_err()
            .contains("invalid state")
    );
}

#[tokio::test]
async fn constrained_executor_uses_only_fixed_plan_request() {
    let (_temp, plan) = test_plan();
    let executor = RecordingExecutor::new(vec![(ExitStatus::default(), Ok(()))]);
    let runner = WorktreeExecutor::new(executor.clone(), vec!["PATH=/usr/bin".into()]);
    runner
        .add(&plan, &"a".repeat(40), &CancellationToken::new())
        .await
        .unwrap();
    let requests = executor.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].dir, plan.target.repository);
    assert_eq!(requests[0].env, vec!["PATH=/usr/bin"]);
    assert!(requests[0].report_denials);
    assert_eq!(
        requests[0].argv,
        worktree_add_argv(&plan, &"a".repeat(40)).unwrap()
    );
}

#[tokio::test]
async fn constrained_executor_never_accepts_denial_or_failure() {
    let (_temp, plan) = test_plan();
    let denied = RecordingExecutor::new(vec![(
        (ExitStatus {
            code: 1,
            denials: vec![Denial {
                operation: "file-write-data".into(),
                target: ".git/worktrees".into(),
            }],
            ..ExitStatus::default()
        }),
        Ok(()),
    )]);
    let error = WorktreeExecutor::new(denied, Vec::new())
        .add(&plan, &"a".repeat(40), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.contains("denied by the sandbox"));

    let failed = RecordingExecutor::new(vec![(
        (ExitStatus {
            code: 23,
            ..ExitStatus::default()
        }),
        Ok(()),
    )]);
    let error = WorktreeExecutor::new(failed, Vec::new())
        .add(&plan, &"a".repeat(40), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(error.contains("exit status 23"));
}

#[test]
fn worktree_command_is_fixed_and_rejects_invalid_base_or_path() {
    let temp = tempfile::tempdir().unwrap();
    let target = Target {
        repository: temp.path().to_path_buf(),
        common_dir: temp.path().to_path_buf(),
    };
    let prepared = plan(target, "d1", "agent/test".into()).unwrap();
    let base = "a".repeat(40);
    let argv = worktree_add_argv(&prepared, &base).unwrap();
    assert_eq!(
        argv,
        vec![
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.ext.allow=never",
            "worktree",
            "add",
            "-b",
            "agent/test",
            prepared.path.to_str().unwrap(),
            base.as_str(),
        ]
    );
    assert!(worktree_add_argv(&prepared, "HEAD;rm -rf /").is_err());
    std::fs::create_dir_all(&prepared.path).unwrap();
    assert!(worktree_add_argv(&prepared, &base).is_err());
}

#[test]
fn existing_delivery_path_is_never_reused() {
    let temp = tempfile::tempdir().unwrap();
    let target = Target {
        repository: temp.path().to_path_buf(),
        common_dir: temp.path().to_path_buf(),
    };
    std::fs::create_dir_all(temp.path().join(".otto/delivery-worktrees/d1")).unwrap();
    assert!(plan(target, "d1", "agent/test".into()).is_err());
}

#[test]
fn branch_rejects_git_ref_hazards() {
    for candidate in ["agent/a b", "agent/a@{b", "agent/a.lock", "agent//b"] {
        assert!(branch(Some(candidate), "x").is_err(), "{candidate}");
    }
}
