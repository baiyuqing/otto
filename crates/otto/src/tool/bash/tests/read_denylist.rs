//! Tests for `sandbox_read_path` against the read denylist in effect.

use super::*;
use crate::fixtures::FakeSandbox;
use crate::runtime::info::SandboxInfo;
use crate::sandbox::read_denylist::ReadDenylist;

const ALREADY_READABLE: &str =
    "path is already readable; run the command without sandbox_read_path";

struct Fixture {
    _home: tempfile::TempDir,
    _other: tempfile::TempDir,
    covered: String,
    uncovered: String,
    tool: BashTool,
    executor: Arc<FakeExecutor>,
    approvals: Arc<BashApprovals>,
    _workspace_dir: tempfile::TempDir,
}

fn fixture(with_denylist: bool) -> Fixture {
    let home = tempfile::tempdir().unwrap();
    let secrets = home.path().canonicalize().unwrap().join("secrets");
    std::fs::create_dir(&secrets).unwrap();
    let other = tempfile::tempdir().unwrap();
    let uncovered = other.path().canonicalize().unwrap();
    let (workspace_dir, workspace) = temp_workspace();
    let executor = Arc::new(FakeExecutor::default());
    let approvals = Arc::new(BashApprovals::new(executor.clone(), Vec::new()));
    let control = if with_denylist {
        let denylist = ReadDenylist::new(home.path(), vec![secrets.to_str().unwrap().to_owned()]);
        FakeSandbox::with_read_denylist(SandboxInfo::default(), denylist).0
    } else {
        FakeSandbox::new(SandboxInfo::default(), SandboxInfo::default(), None).0
    };
    let tool = bash(&workspace, executor.clone(), &[], 4096, &[])
        .with_approvals("s1", approvals.clone())
        .with_grant_control(workspace_dir.path().join("config.toml"), control);
    Fixture {
        covered: secrets.to_str().unwrap().to_owned(),
        uncovered: uncovered.to_str().unwrap().to_owned(),
        _home: home,
        _other: other,
        tool,
        executor,
        approvals,
        _workspace_dir: workspace_dir,
    }
}

fn read_args(path: &str) -> String {
    json!({"command": "cat x", "sandbox_read_path": path, "justification": "read x"}).to_string()
}

#[tokio::test]
async fn a_path_below_a_denied_entry_creates_an_approval_request() {
    let fixture = fixture(true);
    let result = fixture
        .tool
        .execute(
            &raw(&read_args(&fixture.covered)),
            &CancellationToken::new(),
        )
        .await;
    let request = parse_approval_request("bash", &result).expect("approval request");
    assert_eq!(request.read_path, fixture.covered);
    assert_eq!(fixture.approvals.pending_count("s1"), 1);
}

#[tokio::test]
async fn an_already_readable_path_is_refused_without_an_approval_request() {
    let fixture = fixture(true);
    let args = read_args(&fixture.uncovered);
    let result = fixture
        .tool
        .execute(&raw(&args), &CancellationToken::new())
        .await;
    assert!(result.is_error);
    assert!(result.content.contains(ALREADY_READABLE), "{result:?}");
    assert!(parse_approval_request("bash", &result).is_none());
    assert_eq!(fixture.approvals.pending_count("s1"), 0);
    assert_eq!(fixture.executor.calls(), 0);
    let Err(refused) = fixture.tool.approval_spec(&raw(&args)) else {
        panic!("approval_spec accepted an already readable path");
    };
    assert!(refused.content.contains(ALREADY_READABLE), "{refused:?}");
}

#[tokio::test]
async fn without_a_denylist_every_existing_path_keeps_the_approval_flow() {
    let fixture = fixture(false);
    let result = fixture
        .tool
        .execute(
            &raw(&read_args(&fixture.uncovered)),
            &CancellationToken::new(),
        )
        .await;
    assert!(
        parse_approval_request("bash", &result).is_some(),
        "{result:?}"
    );
    assert_eq!(fixture.approvals.pending_count("s1"), 1);
}
