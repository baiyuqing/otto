//! Stale callbacks and captured-request cleanup tests.

use super::*;

#[tokio::test]
async fn stale_callback_allow_or_deny_cannot_decide_reopened_revision() {
    for stale in [ApprovalDecision::Allow, ApprovalDecision::Deny] {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let controller =
            Arc::new(crate::cli::testutil::controller(workspace.path(), sessions.path()).await);
        let scope = controller.approval_scope();
        let service = controller.approval_service().clone();
        service
            .set_mode(&scope, otto_core::approval::ApprovalMode::Manual)
            .unwrap();
        let request = service
            .create(crate::approval::Proposal {
                kind: otto_core::approval::Kind::Authorization,
                authority: otto_core::approval::Authority::Boundary,
                scope: scope.clone(),
                subject: otto_core::approval::Subject {
                    tool_call_id: "call".into(),
                    source: "mcp".into(),
                    id: "prepared".into(),
                    version: "v1".into(),
                    details: "exact prepared call".into(),
                },
                title: "Prepared effect".into(),
                reason: "User choice".into(),
                persistent: false,
                blocking: true,
                expires_at: None,
            })
            .unwrap();
        let (shown, shown_rx) = tokio::sync::oneshot::channel();
        let shown = Arc::new(std::sync::Mutex::new(Some(shown)));
        let release = CancellationToken::new();
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let runner_controller = controller.clone();
        let runner_service = service.clone();
        let runner_request = request.clone();
        let callback_request = request.clone();
        let runner_release = release.clone();
        let runner_executions = executions.clone();
        let job = tokio::spawn(async move {
            let wait_service = runner_service.clone();
            let run = async move {
                let final_request = wait_service
                    .wait(
                        &runner_request.id,
                        &runner_request.scope,
                        &CancellationToken::new(),
                    )
                    .await
                    .map_err(|error| AgentError::Other(error.to_string()))?;
                if final_request.state != otto_core::approval::State::Approved {
                    return Err(AgentError::Other("Wrong rejection".into()));
                }
                runner_executions.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            };
            let (_events, received) = tokio::sync::mpsc::unbounded_channel();
            let mut first = true;
            let mut decide = move |_: ApprovalRequest| {
                let is_first = std::mem::replace(&mut first, false);
                let service = runner_service.clone();
                let scope = scope.clone();
                let request = callback_request.clone();
                let shown = shown.clone();
                let release = runner_release.clone();
                async move {
                    if is_first {
                        service
                            .set_mode(&scope, otto_core::approval::ApprovalMode::Yolo)
                            .unwrap();
                        let automatic = service
                            .get(&request.id, &scope.workspace, Some(&scope.session_id))
                            .unwrap();
                        assert_eq!(automatic.state, otto_core::approval::State::Approved);
                        service
                            .set_mode(&scope, otto_core::approval::ApprovalMode::Manual)
                            .unwrap();
                        assert_eq!(
                            service.revalidate_authorization(&automatic).unwrap().state,
                            otto_core::approval::State::Pending
                        );
                        stale
                    } else {
                        shown.lock().unwrap().take().unwrap().send(()).unwrap();
                        release.cancelled().await;
                        ApprovalDecision::Allow
                    }
                }
            };
            runner_controller
                .step_with_shared_approvals(
                    run,
                    received,
                    &mut |_| {},
                    &CancellationToken::new(),
                    &mut decide,
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), shown_rx)
            .await
            .unwrap()
            .unwrap();
        let current = service
            .get(
                &request.id,
                &request.scope.workspace,
                Some(&request.scope.session_id),
            )
            .unwrap();
        assert_eq!(current.state, otto_core::approval::State::Pending);
        assert!(current.decision.is_none());
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        release.cancel();
        assert!(job.await.unwrap().is_ok());
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
        controller.close().unwrap();
    }
}

#[tokio::test]
async fn model_followups_wait_for_approval_and_are_discarded_on_denial() {
    use otto_core::session::Session;

    for decision in [ApprovalDecision::Allow, ApprovalDecision::Deny] {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let (controller, _, _) =
            scripted_approval_controller(workspace.path(), sessions.path(), false).await;
        let control = controller.runner().unwrap().task_control();
        let mut discard = |_| {};
        let stop = controller
            .run_with_approvals(
                Step::Text("finish the reviewed task"),
                &mut discard,
                &CancellationToken::new(),
                |_| {
                    control.queue_followup("write the final report").unwrap();
                    async move { decision }
                },
            )
            .await
            .unwrap();
        assert_eq!(stop, Stop::EndTurn);
        let history = controller.current_session().messages();
        let followup = history
            .iter()
            .position(|message| message.context_type == "agent_followup");
        match decision {
            ApprovalDecision::Allow => {
                let retry = history
                    .iter()
                    .position(|message| {
                        message.role == Role::User && message.text().contains("The user approved")
                    })
                    .expect("approval retry");
                assert!(followup.expect("accepted follow-up") > retry);
            }
            ApprovalDecision::Deny => {
                assert!(followup.is_none(), "denied work must not continue")
            }
            ApprovalDecision::Cancelled => unreachable!(),
        }
        assert!(control.queue_followup("late work").is_err());
    }
}

#[tokio::test]
async fn replacing_the_runner_during_approval_cleans_only_the_captured_request() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, _) =
        crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
    let controller = Arc::new(controller);
    let session_id = controller.info().session_id;
    let old_id = approvals.request(&session_id, "git push");
    let session = controller.current_session();
    let tasks = Arc::new(Tasks::new());
    let runner = Runner::scripted_with_tools(
        session,
        Arc::new(ApprovalProvider {
            calls: AtomicUsize::new(0),
            duplicate: false,
        }),
        Arc::clone(&tasks),
        vec![Box::new(PendingApprovalTool {
            request_id: old_id.clone(),
        })],
    );
    let runtime = initial_runtime(controller.builder());
    let info = controller.builder().runtime_info(&runtime);
    assert!(
        controller
            .replace_runner_if_current(&session_id, runner, info)
            .unwrap()
    );

    let deciding = Arc::clone(&controller);
    let decisions = Arc::clone(&approvals);
    let run_session = session_id.clone();
    let mut discard = |_| {};
    let result = controller
        .run_with_channel(
            Step::Text("run the command"),
            None,
            None,
            &mut discard,
            &CancellationToken::new(),
            move |_| {
                let deciding = Arc::clone(&deciding);
                let decisions = Arc::clone(&decisions);
                let session_id = run_session.clone();
                async move {
                    let session = deciding.current_session();
                    let runtime = initial_runtime(deciding.builder());
                    let candidate = Runner::scripted(
                        session,
                        Arc::new(ApprovalProvider {
                            calls: AtomicUsize::new(0),
                            duplicate: false,
                        }),
                        Arc::new(Tasks::new()),
                    );
                    let info = deciding.builder().runtime_info(&runtime);
                    assert!(
                        deciding
                            .replace_runner_if_current(&session_id, candidate, info)
                            .unwrap()
                    );
                    decisions.request(&session_id, "new command");
                    ApprovalDecision::Allow
                }
            },
        )
        .await;

    assert!(
        matches!(result, Err(AgentError::Other(ref message)) if message.contains("active session changed")),
        "{result:?}"
    );
    assert!(approvals.pending_command(&session_id, &old_id).is_none());
    assert_eq!(
        approvals
            .pending(&session_id)
            .map(|(_, command, _)| command),
        Some("new command".into())
    );
}

#[tokio::test]
async fn duplicate_tool_calls_share_one_pending_request_cleanup_guard() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, request_id) =
        scripted_approval_controller(workspace.path(), sessions.path(), true).await;
    let session_id = controller.info().session_id;

    let decisions = AtomicUsize::new(0);
    let mut discard = |_| {};
    let stop = controller
        .run_with_channel(
            Step::Text("run the command"),
            None,
            None,
            &mut discard,
            &CancellationToken::new(),
            |_| {
                decisions.fetch_add(1, Ordering::SeqCst);
                assert_eq!(
                    approvals
                        .pending_command(&session_id, &request_id)
                        .as_deref(),
                    Some("git push")
                );
                async { ApprovalDecision::Deny }
            },
        )
        .await
        .unwrap();

    assert_eq!(stop, Stop::EndTurn);
    assert_eq!(decisions.load(Ordering::SeqCst), 1);
    assert!(
        approvals
            .pending_command(&session_id, &request_id)
            .is_none()
    );
}

#[tokio::test]
async fn busy_controller_during_approval_discards_the_captured_request() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, request_id) =
        scripted_approval_controller(workspace.path(), sessions.path(), false).await;
    let session_id = controller.info().session_id;
    let controller = Arc::new(controller);
    let (busy_started_tx, busy_started_rx) = oneshot::channel();
    let (release_busy_tx, release_busy_rx) = oneshot::channel();
    let mut busy_started_tx = Some(busy_started_tx);
    let mut busy_started_rx = Some(busy_started_rx);
    let mut release_busy_rx = Some(release_busy_rx);
    let job = Arc::new(Mutex::new(None));
    let deciding = Arc::clone(&controller);
    let job_slot = Arc::clone(&job);
    let mut discard = |_| {};
    let result = controller
        .run_with_channel(
            Step::Text("run the command"),
            None,
            None,
            &mut discard,
            &CancellationToken::new(),
            move |_| {
                let deciding = Arc::clone(&deciding);
                let job_slot = Arc::clone(&job_slot);
                let busy_started_tx = busy_started_tx.take().unwrap();
                let busy_started_rx = busy_started_rx.take().unwrap();
                let release_busy_rx = release_busy_rx.take().unwrap();
                async move {
                    let busy = Arc::clone(&deciding);
                    let handle = tokio::spawn(async move {
                        let _admission = busy.begin_operation().unwrap();
                        busy_started_tx.send(()).unwrap();
                        release_busy_rx.await.unwrap();
                    });
                    *job_slot.lock().unwrap() = Some(handle);
                    busy_started_rx.await.unwrap();
                    ApprovalDecision::Allow
                }
            },
        )
        .await;

    assert!(matches!(result, Err(AgentError::Other(message)) if message == PROMPT_ACTIVE));
    assert!(
        approvals
            .pending_command(&session_id, &request_id)
            .is_none()
    );
    release_busy_tx.send(()).unwrap();
    let handle = job.lock().unwrap().take().unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn controller_close_during_approval_discards_the_captured_request() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let (controller, approvals, request_id) =
        scripted_approval_controller(workspace.path(), sessions.path(), false).await;
    let session_id = controller.info().session_id;
    let controller = Arc::new(controller);
    let deciding = Arc::clone(&controller);
    let mut discard = |_| {};
    let result = controller
        .run_with_channel(
            Step::Text("run the command"),
            None,
            None,
            &mut discard,
            &CancellationToken::new(),
            move |_| {
                let deciding = Arc::clone(&deciding);
                async move {
                    deciding.request_close();
                    ApprovalDecision::Allow
                }
            },
        )
        .await;

    assert!(matches!(result, Err(AgentError::Other(message)) if message == crate::app::CLOSED));
    assert!(
        approvals
            .pending_command(&session_id, &request_id)
            .is_none()
    );
}
