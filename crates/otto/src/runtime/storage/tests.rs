use super::*;
use otto_core::approval::ApprovalMode;

#[test]
fn default_storage_creates_once_and_reopens_the_same_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("new-parent/approvals.db");
    let storage = Storage::default();
    let service = crate::approval::Service::from_store(storage.approvals(&path).unwrap()).unwrap();
    let scope = otto_core::approval::Scope {
        workspace: "/workspace".into(),
        session_id: "session".into(),
        task_id: String::new(),
        run_id: String::new(),
    };
    service.set_mode(&scope, ApprovalMode::Manual).unwrap();
    drop(service);
    let service = crate::approval::Service::from_store(storage.approvals(&path).unwrap()).unwrap();
    assert_eq!(service.mode(&scope).unwrap(), ApprovalMode::Manual);
}
