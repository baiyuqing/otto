use super::*;
use otto_core::approval::ApprovalMode;

#[tokio::test]
async fn maps_one_time_approval_and_rejects_unreviewable_input() {
    let directory = tempfile::tempdir().unwrap();
    let service = Arc::new(Service::open(&directory.path().join("approvals.db")).unwrap());
    let scope = Scope {
        workspace: "/workspace".into(),
        session_id: "session".into(),
        task_id: "t1".into(),
        run_id: String::new(),
    };
    let mut root_scope = scope.clone();
    root_scope.task_id.clear();
    service.set_mode(&root_scope, ApprovalMode::Yolo).unwrap();
    let handler = Handler {
        service,
        scope,
        redactor: Redactor::new(&["secret-test-value".into()]),
        cancel: CancellationToken::new(),
    };
    let request = json!({
        "toolCall": {"toolCallId":"call1", "title":"Run command", "rawInput":{"command":"ls"}},
        "options": [{"kind":"allow_once", "optionId":"yes"}, {"kind":"allow_always", "optionId":"always"}]
    });
    assert_eq!(
        handler.decide(request.clone()).await["outcome"]["optionId"],
        "yes"
    );
    let mut secret = request.clone();
    secret["toolCall"]["rawInput"] = json!({"command":"secret-test-value"});
    assert_eq!(
        handler.decide(secret).await["outcome"]["outcome"],
        "cancelled"
    );
    let mut always_only = request;
    always_only["options"] = json!([{"kind":"allow_always","optionId":"always"}]);
    assert_eq!(
        handler.decide(always_only).await["outcome"]["outcome"],
        "cancelled"
    );
}
