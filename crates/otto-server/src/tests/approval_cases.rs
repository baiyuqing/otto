//! Approvals decided inside a waiting turn.

use super::*;

// ---- approvals decided inside the waiting turn ----

pub(super) fn approval_harness() -> Harness {
    Harness::with(HarnessOptions {
        script: Script {
            tool_command: Some("echo hi".to_string()),
            deltas: vec!["done".to_string()],
            ..Script::default()
        },
        elevate: true,
        ..HarnessOptions::default()
    })
}

pub(super) async fn decide(
    harness: &Harness,
    session: &str,
    approval: &str,
    decision: &str,
) -> Reply {
    harness
        .send(
            "POST",
            &format!("/v1/sessions/{session}/approvals/{approval}"),
            Some(&serde_json::json!({ "decision": decision }).to_string()),
        )
        .await
}

/// Starts a turn that requests an approval; returns the reader, turn id
/// and approval id once the `approval_requested` frame arrived.
pub(super) async fn waiting_turn(harness: &Harness, session: &str) -> (SseReader, String, String) {
    let response = post_turn(harness, session, "run it", false).await;
    let turn_id = turn_header(&response);
    let mut reader = SseReader::new(response);
    let requested = frame_named(&mut reader, "approval_requested").await;
    let approval = requested["approval_id"].as_str().expect("id").to_string();
    assert_eq!(requested["command"], "echo hi");
    (reader, turn_id, approval)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_turn_waits_for_an_approval_then_retries_in_the_same_turn() {
    let harness = approval_harness();
    let id = harness.create().await;
    let response = post_turn_with_channel(&harness, &id, "run it", Some("telegram"), false).await;
    let turn_id = turn_header(&response);
    let mut reader = SseReader::new(response);
    let requested = frame_named(&mut reader, "approval_requested").await;
    let approval = requested["approval_id"].as_str().expect("id").to_string();

    let waiting = harness
        .send("GET", &format!("/v1/sessions/{id}/turns/{turn_id}"), None)
        .await
        .json();
    assert_eq!(waiting["status"], turn::TURN_RUNNING);
    let status = harness.raw("GET", "/v1/status", None, &[]).await;
    let snapshot = status_snapshot(&mut SseReader::new(status)).await;
    assert_eq!(snapshot["sessions"][0]["approvals"], 1);

    let allowed = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);
    assert_eq!(allowed.json()["decision"], "allow");
    let decided = frame_named(&mut reader, "approval_decided").await;
    assert_eq!(decided["decision"], "allow");
    assert_eq!(decided["approval_id"], approval.as_str());

    let (_, last, end) = ends_of(reader).await;
    assert_eq!((last.as_str(), end.as_str()), ("turn_end", "ok"));
    let summary = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(summary["status"], "ok");
    // The retry ran in the same turn: no second turn exists.
    let session = harness
        .send("GET", &format!("/v1/sessions/{id}"), None)
        .await
        .json();
    assert_eq!(session["turn"]["id"], turn_id.as_str());
    let prompts = harness.provider.system_prompts();
    // Each scripted run makes a tool request and a completion request.
    assert_eq!(prompts.len(), 4);
    assert!(prompts.iter().all(|prompt| prompt.contains("Telegram")));

    let again = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.json()["error"]["code"], "approval_decided");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn model_withdrawal_publishes_denial_and_refuses_the_old_card() {
    let harness = approval_harness();
    let id = harness.create().await;
    let (mut reader, turn_id, approval) = waiting_turn(&harness, &id).await;
    let approvals = harness.factory.builder.bash_approvals.as_ref().unwrap();
    let tool = otto::tool::otto::Approvals {
        session: id.clone(),
        approvals: Arc::clone(approvals),
        revoke: true,
        expected_id: Some(approval.clone()),
    };
    let result = otto::tool::Tool::execute(
        &tool,
        &serde_json::value::RawValue::from_string(serde_json::json!({"id":approval}).to_string())
            .unwrap(),
        &CancellationToken::new(),
    )
    .await;
    assert!(!result.is_error, "{}", result.content);
    let decided = frame_named(&mut reader, "approval_decided").await;
    assert_eq!(decided["decision"], "deny");
    assert_eq!(harness.wait_turn_done(&id, &turn_id).await["status"], "ok");
    let late = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(late.status, StatusCode::CONFLICT);
    assert_eq!(late.json()["error"]["code"], "approval_decided");
    assert_eq!(approvals.pending_count(&id), 0);
    assert_eq!(
        harness.provider.roles.lock().unwrap().len(),
        2,
        "withdrawal must not retry the elevated command"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn approval_dialogue_explains_without_deciding_and_none_means_normal_input() {
    let harness = approval_harness();
    let id = harness.create().await;
    let path = format!("/v1/sessions/{id}/approvals/message");
    let none = harness
        .send("POST", &path, Some(r#"{"text":"hello"}"#))
        .await;
    assert_eq!(none.status, StatusCode::OK);
    assert!(none.json().is_null());
    let (mut reader, turn_id, approval) = waiting_turn(&harness, &id).await;
    let original_turn = harness
        .server
        .lookup(&id)
        .unwrap()
        .find_turn(&turn_id)
        .unwrap();
    let response = harness
        .send(
            "POST",
            &path,
            Some(r#"{"text":"Why does this need approval?","input_id":"http-approval-input","client_source":"tui"}"#),
        )
        .await;
    assert_eq!(response.status, StatusCode::OK, "{}", response.body);
    assert_eq!(response.json()["text"], "done");
    assert_eq!(response.json()["queued"], false);
    assert_eq!(response.json()["input_id"], "http-approval-input");
    let controller = &harness.server.lookup(&id).unwrap().ctrl;
    let notification = controller
        .notification_mailbox()
        .unwrap()
        .queued()
        .into_iter()
        .find(|entry| entry.notification.id == "http-approval-input")
        .expect("HTTP approval human ID")
        .notification;
    let origin = notification.input_provenance.unwrap();
    assert_eq!(origin.source, otto_core::model::InputSource::Http);
    assert_eq!(
        origin.client_source,
        Some(otto_core::model::InputSource::Tui)
    );
    assert_eq!(notification.text, "Why does this need approval?");

    assert!(harness.server.lookup(&id).unwrap().lock().waiting.is_some());
    assert_eq!(
        decide(&harness, &id, &approval, "deny").await.status,
        StatusCode::OK
    );
    let decided = frame_named(&mut reader, "approval_decided").await;
    assert_eq!(decided["decision"], "deny");
    Harness::wait_done(&original_turn).await;
    assert_eq!(original_turn.summary().status, "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn denying_an_approval_ends_the_turn_ok() {
    let harness = approval_harness();
    let id = harness.create().await;
    let (mut reader, turn_id, approval) = waiting_turn(&harness, &id).await;

    let denied = decide(&harness, &id, &approval, "deny").await;
    assert_eq!(denied.status, StatusCode::OK, "{}", denied.body);
    assert_eq!(denied.json()["decision"], "deny");
    let decided = frame_named(&mut reader, "approval_decided").await;
    assert_eq!(decided["decision"], "deny");
    let summary = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(summary["status"], "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unknown_approval_id_answers_409_approval_failed() {
    let harness = approval_harness();
    let id = harness.create().await;
    let (_reader, _turn_id, _approval) = waiting_turn(&harness, &id).await;
    let reply = decide(&harness, &id, "nope", "allow").await;
    assert_eq!(reply.status, StatusCode::CONFLICT, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "approval_failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelling_a_turn_during_an_approval_wait_ends_it_canceled() {
    let harness = approval_harness();
    let id = harness.create().await;
    let (mut reader, turn_id, approval) = waiting_turn(&harness, &id).await;

    let cancel = harness
        .send(
            "POST",
            &format!("/v1/sessions/{id}/turns/{turn_id}/cancel"),
            None,
        )
        .await;
    assert_eq!(cancel.status, StatusCode::ACCEPTED);
    let summary = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(summary["status"], "canceled");
    let mut names = Vec::new();
    while let Some(frame) = reader.next().await {
        names.push(frame.event);
    }
    assert!(!names.iter().any(|name| name == "approval_decided"));
    assert_eq!(names.last().map(String::as_str), Some("turn_end"));
    let late = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(late.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn an_approval_not_decided_in_ten_minutes_times_out_as_a_denial() {
    let harness = approval_harness();
    let id = harness.create().await;
    let (mut reader, turn_id, approval) = waiting_turn(&harness, &id).await;

    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(599)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        harness
            .factory
            .builder
            .bash_approvals
            .as_ref()
            .unwrap()
            .pending_count(&id),
        1,
        "the approval must still be pending before ten minutes"
    );
    tokio::time::advance(Duration::from_secs(2)).await;
    let decided = frame_named_untimed(&mut reader, "approval_decided").await;
    assert_eq!(decided["decision"], "timeout");
    assert_eq!(decided["approval_id"], approval.as_str());
    tokio::time::resume();
    let summary = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(summary["status"], "ok");
    let late = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(late.status, StatusCode::CONFLICT);
    assert_eq!(late.json()["error"]["code"], "approval_decided");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_task_turn_waits_for_an_approval_too() {
    let tasks = Arc::new(otto::subagent::tasks::Tasks::new());
    let harness = Harness::with(HarnessOptions {
        script: Script {
            tool_command: Some("echo hi".to_string()),
            deltas: vec!["done".to_string()],
            ..Script::default()
        },
        elevate: true,
        tasks: Some(Arc::clone(&tasks)),
        ..HarnessOptions::default()
    });
    let id = harness.create().await;
    tasks.notifications().push(notification());

    let turn_id = loop {
        let session = harness
            .send("GET", &format!("/v1/sessions/{id}"), None)
            .await
            .json();
        if let Some(turn_id) = session["turn"]["id"].as_str() {
            break turn_id.to_string();
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let response = harness
        .raw(
            "GET",
            &format!("/v1/sessions/{id}/turns/{turn_id}/events"),
            None,
            &[],
        )
        .await;
    let mut reader = SseReader::new(response);
    let requested = frame_named(&mut reader, "approval_requested").await;
    let approval = requested["approval_id"].as_str().expect("id").to_string();
    let allowed = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(allowed.status, StatusCode::OK, "{}", allowed.body);
    let summary = harness.wait_turn_done(&id, &turn_id).await;
    assert_eq!(summary["trigger"], turn::TRIGGER_TASK);
    assert_eq!(summary["status"], "ok");
}

#[tokio::test]
async fn queued_review_asks_for_the_current_human_revision_without_an_early_prompt() {
    struct AskReviewer {
        started: CancellationToken,
        release: CancellationToken,
    }
    #[async_trait::async_trait]
    impl otto::approval::reviewer::ReviewProvider for AskReviewer {
        async fn assess(
            &self,
            _: &otto_core::approval::Request,
            _: &otto::approval::reviewer::ReviewContext,
            cancel: &CancellationToken,
        ) -> std::result::Result<otto_core::approval::ReviewAssessment, String> {
            self.started.cancel();
            tokio::select! {
                () = cancel.cancelled() => return Err("cancelled".into()),
                () = self.release.cancelled() => {},
            }
            Ok(otto_core::approval::ReviewAssessment {
                risk: otto_core::approval::RiskLevel::Medium,
                user_authorization: otto_core::approval::UserAuthorization::Unknown,
                outcome: otto_core::approval::ReviewOutcome::Ask,
                reason: "Confirm the exact target before running.".into(),
            })
        }
    }
    let harness = Harness::new();
    let id = harness.create().await;
    let session = harness.server.lookup(&id).unwrap();
    let scope = session.ctrl.approval_scope();
    let started = CancellationToken::new();
    let release = CancellationToken::new();
    let service = session.ctrl.approval_service();
    service
        .register_reviewer(
            &scope.workspace,
            &scope.session_id,
            Arc::new(AskReviewer {
                started: started.clone(),
                release: release.clone(),
            }),
        )
        .unwrap();
    service
        .set_review_context(
            &scope.workspace,
            &scope.session_id,
            otto::approval::reviewer::ReviewContext {
                trusted_user_messages: vec!["Run the exact operation.".into()],
                evidence_digest: "evidence".into(),
                policy_digest: "policy".into(),
                instructions: Vec::new(),
            },
        )
        .unwrap();
    let request = service
        .create(otto::approval::Proposal {
            kind: otto_core::approval::Kind::Authorization,
            authority: otto_core::approval::Authority::Boundary,
            scope: scope.clone(),
            subject: otto_core::approval::Subject {
                tool_call_id: "held-call".into(),
                source: "tool".into(),
                id: "held-operation".into(),
                version: "exact-version".into(),
                details: "Run the exact operation".into(),
            },
            title: "Review operation".into(),
            reason: "User requested operation".into(),
            blocking: true,
            persistent: false,
            expires_at: None,
        })
        .unwrap();
    let turn = Arc::new(Turn::new(
        "held-turn".into(),
        TRIGGER_USER,
        CancellationToken::new(),
    ));
    let waiting = {
        let server = harness.server.clone();
        let session = session.clone();
        let turn = turn.clone();
        let request_id = request.id.clone();
        tokio::spawn(async move {
            server
                .wait_for_approval(
                    &session,
                    &turn,
                    &CancellationToken::new(),
                    ApprovalRequest {
                        approval_id: request_id,
                        tool_call_id: "held-call".into(),
                        command: "exact-operation".into(),
                        justification: "Run the exact operation".into(),
                        read_path: String::new(),
                    },
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(10), started.cancelled())
        .await
        .unwrap();
    assert!(
        turn.snapshot(0).0.is_empty(),
        "reviewing must not emit a permission prompt"
    );
    assert!(session.lock().waiting.is_none());
    release.cancel();
    tokio::time::timeout(Duration::from_secs(10), async {
        while session.lock().waiting.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let current = service
        .get(&request.id, &scope.workspace, Some(&scope.session_id))
        .unwrap();
    assert!(current.is_human_pending());
    assert_eq!(current.subject, request.subject);
    assert_eq!(
        session.lock().waiting.as_ref().unwrap().shared_revision,
        Some(current.revision)
    );
    service
        .set_mode(&scope, otto_core::approval::ApprovalMode::Manual)
        .unwrap();
    let fresh = service
        .get(&request.id, &scope.workspace, Some(&scope.session_id))
        .unwrap();
    assert!(fresh.revision > current.revision);
    assert_eq!(fresh.subject, current.subject);
    tokio::time::timeout(Duration::from_secs(10), async {
        while session
            .lock()
            .waiting
            .as_ref()
            .and_then(|waiting| waiting.shared_revision)
            != Some(fresh.revision)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    for option in ["approve", "reject"] {
        let body =
            serde_json::json!({"session_id":id,"revision":current.revision,"option_id":option})
                .to_string();
        assert_eq!(
            harness
                .raw(
                    "POST",
                    &format!("/v1/approvals/{}/decision", request.id),
                    Some(&body),
                    &[("content-type", b"application/json")],
                )
                .await
                .status(),
            StatusCode::CONFLICT
        );
        assert!(
            service
                .get(&request.id, &scope.workspace, Some(&scope.session_id))
                .unwrap()
                .is_human_pending()
        );
    }
    assert_eq!(
        decide(&harness, &id, &request.id, "allow").await.status,
        StatusCode::OK
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .unwrap()
            .unwrap(),
        ApprovalDecision::Allow
    );
}

#[tokio::test]
async fn approval_policy_requires_revision_and_preserves_reviewer_when_disabled() {
    let harness = Harness::new();
    let id = harness.create().await;
    let url = format!("/v1/sessions/{id}/approval-policy");
    let initial = harness.send("GET", &url, None).await.json();
    assert_eq!(initial["policy"], "on_request");
    assert_eq!(initial["reviewer"], "auto_review");
    assert_eq!(initial["revision"], 1);
    let update = r#"{"expected_revision":1,"policy":"never","reviewer":"user"}"#;
    let changed = harness.send("PUT", &url, Some(update)).await;
    assert_eq!(changed.status, StatusCode::OK, "{}", changed.body);
    let body = changed.json();
    assert_eq!(body["policy"], "never");
    assert_eq!(body["reviewer"], "user");
    assert_eq!(body["reviewer_active"], false);
    assert_eq!(body["mode"], "auto");
    assert_eq!(body["automatic"], true);
    assert_eq!(body["revision"], 2);
    assert_eq!(
        harness.send("PUT", &url, Some(update)).await.status,
        StatusCode::CONFLICT
    );
    assert_eq!(
        harness
            .send("PUT", &url, Some(r#"{"policy":"never","reviewer":"user"}"#))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let legacy = harness
        .send("GET", &format!("/v1/sessions/{id}/approval-mode"), None)
        .await
        .json();
    assert_eq!(legacy["policy"], "never");
    assert_eq!(legacy["reviewer_active"], false);
    let manual = harness
        .send(
            "PUT",
            &format!("/v1/sessions/{id}/approval-mode"),
            Some(r#"{"mode":"manual"}"#),
        )
        .await
        .json();
    assert_eq!(manual["policy"], "on_request");
    assert_eq!(manual["reviewer"], "user");
}

#[tokio::test]
async fn approval_mode_api_is_session_scoped_and_events_capture_auto_decisions() {
    let harness = Harness::new();
    let one = harness.create().await;
    let two = harness.create().await;
    let url = format!("/v1/sessions/{one}/approval-mode");
    let body = harness.send("GET", &url, None).await.json();
    assert_eq!(body["automatic"], true);
    assert_eq!(body["mode"], "auto");
    let body = harness
        .send("PUT", &url, Some(r#"{"automatic":false}"#))
        .await
        .json();
    assert_eq!(body["mode"], "manual");
    let body = harness.send("GET", &url, None).await.json();
    assert_eq!(body["automatic"], false);
    assert_eq!(body["mode"], "manual");
    // The mode form selects YOLO, and the response agrees with itself.
    let body = harness
        .send("PUT", &url, Some(r#"{"mode":"yolo"}"#))
        .await
        .json();
    assert_eq!(body["automatic"], true);
    assert_eq!(body["mode"], "yolo");
    assert_eq!(harness.send("GET", &url, None).await.json()["mode"], "yolo");
    // A boolean and a contradicting mode are refused, not guessed.
    assert_eq!(
        harness
            .send("PUT", &url, Some(r#"{"automatic":false,"mode":"yolo"}"#))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(harness.send("GET", &url, None).await.json()["mode"], "yolo");
    assert_eq!(
        harness
            .send("PUT", &url, Some(r#"{"mode":"on"}"#))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        harness
            .send("PUT", &url, Some(r#"{"automatic":true}"#))
            .await
            .json()["mode"],
        "auto"
    );
    assert_eq!(
        harness
            .send("GET", &format!("/v1/sessions/{two}/approval-mode"), None)
            .await
            .json()["mode"],
        "auto"
    );
    assert_eq!(
        harness
            .send("PUT", &url, Some(r#"{"automatic":"yes"}"#))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
    let ctrl = &harness.server.lookup(&two).unwrap().ctrl;
    // A bounded authorization is still decided by AUTO without a dialog.
    let request = ctrl
        .approval_service()
        .create(otto::approval::Proposal {
            kind: otto_core::approval::Kind::Authorization,
            authority: otto_core::approval::Authority::Bounded,
            scope: ctrl.approval_scope(),
            subject: otto_core::approval::Subject {
                source: "plugin".into(),
                id: "api-test".into(),
                version: "1".into(),
                details: "Run a plugin command".into(),
                tool_call_id: String::new(),
            },
            title: "Run a plugin command".into(),
            reason: "Inside the current sandbox".into(),
            blocking: true,
            persistent: false,
            expires_at: None,
        })
        .unwrap();
    assert_eq!(request.state, otto_core::approval::State::Approved);
    // A plan review is a human judgement, so AUTO leaves it pending.
    let review = ctrl
        .approval_service()
        .create(otto::approval::Proposal {
            kind: otto_core::approval::Kind::Confirmation,
            authority: otto_core::approval::Authority::Consent,
            scope: ctrl.approval_scope(),
            subject: otto_core::approval::Subject {
                source: "conversation".into(),
                id: "api-test-plan".into(),
                version: "1".into(),
                details: "Test plan".into(),
                tool_call_id: String::new(),
            },
            title: "Plan approval".into(),
            reason: "Test consent".into(),
            blocking: true,
            persistent: false,
            expires_at: None,
        })
        .unwrap();
    assert_eq!(review.state, otto_core::approval::State::Pending);
    let events = harness
        .send(
            "GET",
            &format!("/v1/sessions/{two}/approval-events?after=0"),
            None,
        )
        .await
        .json();
    let events = events.as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["request"]["state"], "pending");
    assert_eq!(
        events[1]["request"]["decision"]["transport"],
        "bounded-authority"
    );
    assert_eq!(events[2]["request"]["state"], "pending");
    assert!(events[2]["request"]["decision"].is_null());
    let other = harness
        .send(
            "GET",
            &format!("/v1/sessions/{one}/approval-events?after=0"),
            None,
        )
        .await
        .json();
    assert!(other.as_array().unwrap().is_empty());
}

// ---- `always` ----

fn always_harness(command: &str) -> Harness {
    let info = fixtures::seatbelt_info(SandboxNetwork::Denied);
    let (control, _calls) = fixtures::FakeSandbox::new(info.clone(), info, None);
    Harness::with(HarnessOptions {
        script: Script {
            tool_command: Some(command.to_string()),
            deltas: vec!["done".to_string()],
            ..Script::default()
        },
        elevate: true,
        sandbox_control: Some(control),
        ..HarnessOptions::default()
    })
}

async fn decide_always(harness: &Harness, session: &str, approval: &str, decision: &str) -> Reply {
    let body = serde_json::json!({ "decision": decision, "always": true });
    harness
        .send(
            "POST",
            &format!("/v1/sessions/{session}/approvals/{approval}"),
            Some(&body.to_string()),
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_excludes_the_command_and_resumes_the_waiting_turn_once() {
    let harness = always_harness("echo hi");
    let id = harness.create().await;
    let (mut reader, turn_id, approval) = waiting_turn(&harness, &id).await;

    let reply = decide_always(&harness, &id, &approval, "allow").await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let body = reply.json();
    assert_eq!(
        (&body["decision"], &body["always"]),
        (&"allow".into(), &true.into())
    );
    assert!(
        body["followup"]
            .as_str()
            .is_some_and(|text| text.contains("excluded 'echo hi'")),
        "{body}"
    );
    let decided = frame_named(&mut reader, "approval_decided").await;
    assert_eq!(decided["decision"], "allow");
    let (_, last, end) = ends_of(reader).await;
    assert_eq!((last.as_str(), end.as_str()), ("turn_end", "ok"));
    assert_eq!(harness.wait_turn_done(&id, &turn_id).await["status"], "ok");

    let config = std::fs::read_to_string(&harness.factory.builder.shared.config_path)
        .expect("configuration file");
    assert!(
        config.contains("excluded_commands") && config.contains("echo hi"),
        "{config}"
    );
    // Same provider calls as a plain allow: the turn resumed once.
    assert_eq!(harness.provider.system_prompts().len(), 4);
    let again = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(again.json()["error"]["code"], "approval_decided");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_with_deny_is_400_and_the_approval_keeps_waiting() {
    let harness = always_harness("echo hi");
    let id = harness.create().await;
    let (_reader, _turn_id, approval) = waiting_turn(&harness, &id).await;
    let reply = decide_always(&harness, &id, &approval, "deny").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    let plain = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.body);
    assert!(plain.json().get("always").is_none(), "{}", plain.body);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn always_for_a_command_that_cannot_be_excluded_is_400_and_keeps_waiting() {
    let harness = always_harness("make test");
    let id = harness.create().await;
    let response = post_turn(&harness, &id, "run it", false).await;
    let mut reader = SseReader::new(response);
    let requested = frame_named(&mut reader, "approval_requested").await;
    let approval = requested["approval_id"].as_str().expect("id").to_string();

    let reply = decide_always(&harness, &id, &approval, "allow").await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{}", reply.body);
    assert_eq!(reply.json()["error"]["code"], "approval_always_failed");
    assert!(
        reply.json()["error"]["message"]
            .as_str()
            .is_some_and(|text| text.contains("cannot exclude this command")),
        "{}",
        reply.body
    );
    assert!(!harness.factory.builder.shared.config_path.exists());
    // Still undecided: an ordinary allow is accepted.
    let plain = decide(&harness, &id, &approval, "allow").await;
    assert_eq!(plain.status, StatusCode::OK, "{}", plain.body);
}
