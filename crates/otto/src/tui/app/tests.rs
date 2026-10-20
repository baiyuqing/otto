
use super::*;
use crate::tui::interaction::{Action as SurfaceAction, Field, Row};
use otto_core::agent::steering::{Message as SteerMessage, Status};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Probe {
    key: &'static str,
    view: View,
    received: Arc<AtomicUsize>,
}
impl Presenter for Probe {
    fn key(&self) -> String {
        self.key.into()
    }
    fn view(&self) -> View {
        self.view.clone()
    }
    fn activate(&mut self, _: Activation) -> Transition {
        Transition::Stay
    }
    fn receive(&mut self, _: Reply) -> Transition {
        self.received.fetch_add(1, Ordering::SeqCst);
        Transition::Stay
    }
}
fn app() -> (App, Remote) {
    let remote = Remote::offline(Vec::new());
    let app = App::new(&Backend::Attach(&remote));
    (app, remote)
}
#[test]
fn tool_duration_survives_same_session_refresh_but_not_session_replacement() {
    use otto_core::model::{Block, BlockType, Message, Role, ToolResultMetadata};
    let history = vec![Message {
        role: Role::Tool,
        blocks: vec![Block {
            block_type: BlockType::ToolResult,
            tool_call_id: "provider_refresh".into(),
            operation_metadata: Some(ToolResultMetadata {
                operation_id: Some(otto_core::model::OperationId::new("op_refresh").unwrap()),
                disposition: otto_core::model::OperationDisposition::Succeeded,
                effect_certainty: otto_core::model::EffectCertainty::Completed,
                stop_reason: None,
            }),
            ..Default::default()
        }],
        ..Default::default()
    }];
    let remote = Remote::offline(history);
    let mut app = App::new(&Backend::Attach(&remote));
    assert!(app.entries[0].tool_done);
    app.entries[0].tool_elapsed = Some(Duration::from_secs(3));
    app.refresh(&Backend::Attach(&remote));
    assert_eq!(app.entries[0].tool_elapsed, Some(Duration::from_secs(3)));
    app.info.session_id = "other-session".into();
    app.refresh(&Backend::Attach(&remote));
    assert_eq!(app.entries[0].tool_elapsed, None);
}

#[test]
fn attached_tool_frames_cannot_measure_execution_from_replay_delivery() {
    let (mut app, _) = app();
    app.attached = true;
    app.apply_event(&WireEvent {
        event_type: "tool_call_started".into(),
        operation_id: "op_attach".into(),
        ..Default::default()
    });
    assert_eq!(app.entries.last().unwrap().tool_started, None);
}

#[test]
fn provider_recovery_separates_partial_output_in_local_and_attached_tui() {
    for attached in [false, true] {
        let (mut app, _remote) = app();
        app.attached = attached;
        for json in [
            r#"{"type":"text_delta","text":"partial"}"#,
            r#"{"type":"provider_retry","retry":{"attempt":2,"max_attempts":7,"interrupted_output":true}}"#,
            r#"{"type":"text_delta","text":"replacement"}"#,
        ] {
            app.apply_event(&serde_json::from_str(json).unwrap());
        }
        let entries = &app.entries[app.entries.len() - 3..];
        assert_eq!(entries[0].raw, "partial");
        assert_eq!(entries[1].kind, Some(EntryKind::System));
        assert_eq!(entries[1].raw, transcript::INTERRUPTED_OUTPUT_NOTICE);
        assert_eq!(entries[2].raw, "replacement");
    }
}

fn probe(key: &'static str, view: View, received: Arc<AtomicUsize>) -> Box<dyn Presenter> {
    Box::new(Probe {
        key,
        view,
        received,
    })
}
fn approval(id: &str) -> otto_core::approval::Request {
    otto_core::approval::Request {
        options: vec![],
        schema_version: 1,
        id: id.into(),
        revision: 1,
        kind: otto_core::approval::Kind::Confirmation,
        authority: otto_core::approval::Authority::Consent,
        scope: otto_core::approval::Scope {
            workspace: "/work".into(),
            session_id: "session".into(),
            task_id: String::new(),
            run_id: String::new(),
        },
        presentation_origin: String::new(),
        subject: otto_core::approval::Subject {
            tool_call_id: String::new(),
            source: "test".into(),
            id: id.into(),
            version: "1".into(),
            details: "details".into(),
        },
        title: "Confirm".into(),
        reason: String::new(),
        persistent: false,
        blocking: true,
        created_at: 1,
        expires_at: None,
        state: otto_core::approval::State::Pending,
        decision: None,
        application: otto_core::approval::ApplicationState::NotRequired,
        application_error: String::new(),
        review: None,
    }
}
fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, crossterm::event::KeyModifiers::NONE)
}

#[test]
fn back_pops_nested_surface_and_keeps_parent_selection_and_draft() {
    let (mut app, remote) = app();
    let marker = Arc::new(AtomicUsize::new(0));
    let view = View {
        rows: vec![
            Row {
                id: "a".into(),
                ..Default::default()
            },
            Row {
                id: "b".into(),
                ..Default::default()
            },
        ],
        fields: vec![Field {
            id: "feedback".into(),
            max_bytes: 50,
            ..Default::default()
        }],
        ..Default::default()
    };
    app.transition(0, Transition::Push(probe("parent", view, marker.clone())));
    let id = app.surfaces.top().unwrap().id;
    {
        let surface = app.surfaces.top_mut().unwrap();
        surface
            .state
            .handle(&surface.model.view(), interaction::Intent::Move(1));
        surface
            .state
            .drafts
            .insert("feedback".into(), "keep me".into());
    }
    let selected = app.surfaces.top().unwrap().state.selected_row.clone();
    app.transition(
        id,
        Transition::Push(probe("child", View::default(), marker)),
    );
    app.handle_key(
        KeyEvent::new(KeyCode::Esc, crossterm::event::KeyModifiers::NONE),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    let parent = app.surfaces.top().unwrap();
    assert_eq!(parent.model.key(), "parent");
    assert_eq!(parent.state.selected_row, selected);
    assert_eq!(parent.state.drafts["feedback"], "keep me");
}

#[test]
fn dismissed_approval_stays_hidden_until_the_collection_reopens_it() {
    let (mut app, _) = app();
    let request = approval("a1");
    app.hidden_approvals.insert("a1".into());
    app.sync_shared_approval(std::slice::from_ref(&request));
    assert!(!app.has_surface());
    app.transition(
        0,
        Transition::Push(Box::new(features::Approval::shared(&request))),
    );
    assert!(app.has_surface());
    assert!(!app.hidden_approvals.contains("a1"));
}

fn open_trace(app: &mut App) {
    app.transition(0, Transition::Push(Box::new(features::TraceView::new())));
    let _ = app
        .surfaces
        .top_mut()
        .unwrap()
        .model
        .receive(Reply::Trace(features::sample_trace()));
}

fn press(app: &mut App, remote: &Remote, code: KeyCode) {
    app.handle_key(
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        &Backend::Attach(remote),
        &CancellationToken::new(),
    );
}

fn top_key(app: &App) -> String {
    app.surfaces.top().unwrap().model.key()
}

#[test]
fn trace_defers_an_approval_until_a_opens_it() {
    let (mut app, remote) = app();
    open_trace(&mut app);
    let request = approval("a1");

    app.sync_shared_approval(std::slice::from_ref(&request));
    assert_eq!(top_key(&app), "trace");
    assert!(app.surfaces.iter().all(|s| s.model.key() == "trace"));
    assert_eq!(
        app.surfaces
            .top()
            .and_then(|s| s.model.view().notice)
            .as_deref(),
        Some("1 approval pending · a open")
    );

    press(&mut app, &remote, KeyCode::Char('a'));
    assert_eq!(top_key(&app), "approval:a1");
}

#[test]
fn closing_the_trace_presents_the_deferred_approval() {
    let (mut app, remote) = app();
    open_trace(&mut app);
    app.sync_shared_approval(&[approval("a1")]);
    press(&mut app, &remote, KeyCode::Esc);
    assert_eq!(top_key(&app), "approval:a1");
    assert_eq!(app.surfaces.iter().count(), 1);
}

#[test]
fn trace_defers_an_approval_event_until_it_closes() {
    let (mut app, remote) = app();
    open_trace(&mut app);
    app.show_approval(features::Approval::shared(&approval("a1")));
    assert_eq!(top_key(&app), "trace");
    press(&mut app, &remote, KeyCode::Esc);
    assert_eq!(top_key(&app), "approval:a1");
}

#[test]
fn a_decided_approval_is_not_presented_again_from_the_stale_snapshot() {
    let (mut app, _) = app();
    app.sync_shared_approval(&[approval("a1")]);
    let surface = app.surfaces.top().unwrap().id;

    app.transition(surface, Transition::Close);
    assert!(app.surfaces.top().is_none());

    open_trace(&mut app);
    let trace = app.surfaces.top().unwrap().id;
    app.transition(trace, Transition::Close);
    assert!(app.surfaces.top().is_none());
}

#[test]
fn pending_approvals_are_presented_one_at_a_time() {
    let (mut app, _) = app();
    let first = approval("a1");
    let second = approval("a2");

    app.sync_shared_approval(&[first.clone(), second.clone()]);
    let visible: Vec<_> = app
        .surfaces
        .iter()
        .filter(|surface| surface.model.key().starts_with("approval:"))
        .map(|surface| surface.model.key())
        .collect();
    assert_eq!(visible, ["approval:a1"]);

    app.sync_shared_approval(&[second]);
    let visible: Vec<_> = app
        .surfaces
        .iter()
        .filter(|surface| surface.model.key().starts_with("approval:"))
        .map(|surface| surface.model.key())
        .collect();
    assert_eq!(visible, ["approval:a2"]);
}

#[test]
fn memory_review_availability_notifies_and_refreshes_approvals_without_a_surface() {
    let (mut app, _) = app();

    app.memory_review_available();

    assert_eq!(
        app.entries.last().map(|entry| entry.raw.as_str()),
        Some(crate::app::MEMORY_REVIEW_AVAILABLE)
    );
    assert!(!app.has_surface());
    assert!(matches!(
        app.requests.last().map(|(_, request)| request),
        Some(Request::ApprovalSnapshot)
    ));
}

#[test]
fn completing_the_shared_poll_allows_the_next_poll() {
    let (mut app, _) = app();
    app.request_approvals();
    let first = app.requests.last().unwrap().0.clone();
    app.apply_completion(Completion {
        mutation: false,
        ticket: first,
        result: Ok(Reply::ApprovalSnapshot(otto_core::approval::Snapshot {
            requests: vec![],
            cursor: 0,
        })),
    });
    app.request_approvals();
    assert_eq!(app.requests.len(), 2);
    assert!(app.requests[1].0.generation > app.requests[0].0.generation);
}

#[test]
fn obsolete_read_results_are_ignored_after_filter_or_session_changes() {
    let (mut app, _) = app();
    let received = Arc::new(AtomicUsize::new(0));
    app.transition(
        0,
        Transition::Push(probe("reader", View::default(), received.clone())),
    );
    let id = app.surfaces.top().unwrap().id;
    app.enqueue(id, Request::Sessions);
    let old = app.requests.last().unwrap().0.clone();
    app.enqueue(id, Request::Profiles);
    app.apply_completion(Completion {
        mutation: false,
        ticket: old.clone(),
        result: Ok(Reply::Sessions(vec![])),
    });
    assert_eq!(received.load(Ordering::SeqCst), 0);
    app.info.session_id = "changed".into();
    let latest = app.requests.last().unwrap().0.clone();
    app.apply_completion(Completion {
        mutation: false,
        ticket: latest,
        result: Ok(Reply::Profiles(vec![])),
    });
    assert_eq!(received.load(Ordering::SeqCst), 0);
}

#[test]
fn dismissed_mutation_keeps_its_receipt_and_retry_prompt() {
    let (mut app, _) = app();
    let received = Arc::new(AtomicUsize::new(0));
    app.transition(
        0,
        Transition::Push(probe("writer", View::default(), received)),
    );
    let id = app.surfaces.top().unwrap().id;
    app.enqueue(
        id,
        Request::Command(backend::Command::Rename("name".into())),
    );
    let ticket = app.requests.last().unwrap().0.clone();
    app.surfaces.remove(id);
    let info = app.info.clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket,
        result: Ok(Reply::Operation {
            message: "change applied".into(),
            info,
            history: None,
            prompt: Some("retry this".into()),
        }),
    });
    assert!(
        app.entries
            .iter()
            .any(|entry| entry.raw == "change applied")
    );
    assert!(
        app.drain_actions()
            .contains(&Action::Prompt("retry this".into()))
    );
}

#[test]
fn a_fork_puts_its_prompt_in_the_editor_without_sending_it() {
    let (mut app, _) = app();
    app.enqueue(
        0,
        Request::Command(backend::Command::Fork("00000002".into())),
    );
    let ticket = app.requests.last().unwrap().0.clone();
    let mut info = app.info.clone();
    info.session_id = "forked".into();
    app.apply_completion(Completion {
        mutation: true,
        ticket,
        result: Ok(Reply::Forked {
            message: "Forked: forked".into(),
            info,
            history: Vec::new(),
            draft: Some("second".into()),
        }),
    });
    assert_eq!(app.info.session_id, "forked");
    assert_eq!(app.composer.input.iter().collect::<String>(), "second");
    assert!(
        !app.drain_actions()
            .iter()
            .any(|action| matches!(action, Action::Prompt(_)))
    );
}

#[test]
fn resuming_another_session_recalls_its_prompts() {
    let (mut app, remote) = app();
    app.composer.set_input("before");
    app.composer.take();
    app.composer.set_input("draft");
    let mut info = app.info.clone();
    info.session_id = "resumed".into();
    app.apply_receipt(
        OperationReceipt {
            message: String::new(),
            info,
            history: Some(vec![crate::cli::testutil::user("resumed prompt")]),
            prompt: None,
            draft: None,
        },
        false,
    );
    assert_eq!(app.composer.input.iter().collect::<String>(), "draft");
    press(&mut app, &remote, KeyCode::Up);
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "resumed prompt"
    );
    press(&mut app, &remote, KeyCode::Up);
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "resumed prompt"
    );
}

#[test]
fn delivered_queued_input_is_visible_after_steering_read_clears_banner() {
    let text = "我发的 queued input 又看不到了";
    for attached in [false, true] {
        let (mut app, _) = app();
        app.attached = attached;
        app.start_turn();
        app.apply_steering(vec![steer(1, text, Status::Queued)]);
        app.apply_steering(vec![steer(1, text, Status::Delivered)]);
        assert!(app.queued_steer.is_none());
        let event = otto_core::wire::events::to_wire(&otto_core::agent::Event::Notification {
            kind: Some(otto_core::agent::inbox::NotificationKind::UserMessage),
            task_id: String::new(),
            text: text.into(),
            usage: Default::default(),
            present: false,
            cached_input_tokens_present: false,
        });
        app.apply_event(&event);
        assert_eq!(
            app.entries
                .iter()
                .filter(|entry| entry.kind == Some(EntryKind::User) && entry.raw == text)
                .count(),
            1,
            "delivered queued input must remain a user transcript entry (attached={attached})"
        );
        assert!(
            !app.entries
                .iter()
                .any(|entry| entry.kind == Some(EntryKind::System) && entry.raw.contains(text))
        );
    }
}

#[test]
fn user_message_echo_does_not_duplicate_prompt_history() {
    let (mut app, remote) = app();
    app.insert_text("hello");
    assert!(
        matches!(app.handle_key(enter(),&Backend::Attach(&remote),&CancellationToken::new()),Some(Action::Prompt(line)) if line=="hello")
    );
    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: "hello".into(),
        ..Default::default()
    });
    app.composer.edit(Edit::PreviousHistory);
    assert_eq!(app.composer.input.iter().collect::<String>(), "hello");
    app.composer.edit(Edit::NextHistory);
    assert!(app.composer.input.is_empty());
    app.insert_text("queued");
    let line = app.composer.take().unwrap();
    app.queued_input = Some(line.clone());
    app.awaiting_user_echo = Some(line.clone());
    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: line.clone(),
        ..Default::default()
    });
    app.composer.edit(Edit::PreviousHistory);
    assert_eq!(app.composer.input.iter().collect::<String>(), line);
}

#[test]
fn unrelated_user_message_does_not_consume_queued_next_input() {
    let (mut app, _) = app();
    app.attached = false;
    app.start_turn();
    app.queued_input = Some("/memory search vim".into());

    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: "guidance for current turn".into(),
        ..Default::default()
    });

    assert_eq!(app.queued_input.as_deref(), Some("/memory search vim"));
    assert_eq!(app.entries.last().unwrap().raw, "guidance for current turn");
}

#[test]
fn provider_usage_updates_the_footer_during_a_turn() {
    let (mut app, _) = app();
    app.usage = Usage {
        input_tokens: 10,
        output_tokens: 2,
        cached_input_tokens: 4,
    };

    app.apply_event(&WireEvent {
        event_type: "provider_usage".into(),
        usage: Some(Usage {
            input_tokens: 7,
            output_tokens: 3,
            cached_input_tokens: 5,
        }),
        usage_present: Some(true),
        ..Default::default()
    });
    app.apply_event(&WireEvent {
        event_type: "provider_usage".into(),
        usage: Some(Usage::default()),
        usage_present: Some(false),
        ..Default::default()
    });

    assert_eq!(
        app.usage,
        Usage {
            input_tokens: 17,
            output_tokens: 5,
            cached_input_tokens: 9,
        }
    );
}

#[test]
fn a_task_notification_decides_whether_the_cache_share_is_shown() {
    let (mut app, _) = app();
    app.cache_breakdown_complete = Some(true);

    // A finished sub-agent that never reported the cached breakdown: the
    // footer must stop presenting the share as measured, because the
    // session totals now include tokens its cache count cannot explain.
    app.apply_event(&WireEvent {
        event_type: "notification".into(),
        task_id: "t1".into(),
        text: "[task-notification] task t1 succeeded".into(),
        usage: Some(Usage {
            input_tokens: 40,
            output_tokens: 5,
            cached_input_tokens: 0,
        }),
        usage_present: Some(true),
        ..Default::default()
    });
    assert_eq!(app.cache_breakdown_complete, Some(false));
}

#[test]
fn a_measured_task_notification_keeps_the_cache_share() {
    let (mut app, _) = app();
    app.cache_breakdown_complete = Some(true);

    app.apply_event(&WireEvent {
        event_type: "notification".into(),
        task_id: "t1".into(),
        text: "[task-notification] task t1 succeeded".into(),
        usage: Some(Usage {
            input_tokens: 40,
            output_tokens: 5,
            cached_input_tokens: 30,
        }),
        usage_present: Some(true),
        cached_input_tokens_present: Some(true),
        ..Default::default()
    });

    assert_eq!(app.cache_breakdown_complete, Some(true));
}

#[test]
fn paste_edits_focused_surface_feedback_only() {
    let (mut app, _) = app();
    let view = View {
        fields: vec![Field {
            id: "feedback".into(),
            max_bytes: 100,
            ..Default::default()
        }],
        ..Default::default()
    };
    app.transition(
        0,
        Transition::Push(probe("approval:a1", view, Arc::new(AtomicUsize::new(0)))),
    );
    {
        let state = &mut app.surfaces.top_mut().unwrap().state;
        state.focus = interaction::Focus::Field("feedback".into());
    }
    app.insert_text("limit it");
    assert!(app.composer.input.is_empty());
    assert_eq!(
        app.surfaces.top().unwrap().state.drafts["feedback"],
        "limit it"
    );
}

#[test]
fn session_replacement_clears_surfaces_and_reloads_history_while_preserving_draft() {
    let (mut app, remote) = app();
    app.info.session_id = "old".into();
    app.insert_text("draft");
    app.transition(
        0,
        Transition::Push(probe(
            "old-surface",
            View::default(),
            Arc::new(AtomicUsize::new(0)),
        )),
    );
    app.refresh(&Backend::Attach(&remote));
    assert!(!app.has_surface());
    assert!(app.entries.is_empty());
    assert_eq!(app.info.session_id, "");
    assert_eq!(app.composer.input.iter().collect::<String>(), "draft");
}

#[test]
fn unknown_mutation_outcome_keeps_submission_locked() {
    let (mut app, _) = app();
    let view = View {
        actions: vec![SurfaceAction {
            id: "save".into(),
            enabled: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    app.transition(
        0,
        Transition::Push(probe("writer", view, Arc::new(AtomicUsize::new(0)))),
    );
    let id = app.surfaces.top().unwrap().id;
    app.enqueue(
        id,
        Request::Command(backend::Command::Rename("name".into())),
    );
    let ticket = app.requests.last().unwrap().0.clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket,
        result: Err("operation outcome unknown; reconcile state before retrying".into()),
    });
    assert!(app.surfaces.top().unwrap().state.pending);
    assert!(
        app.active_view()
            .unwrap()
            .actions
            .iter()
            .find(|a| a.id == "otto:retry")
            .is_none_or(|a| !a.enabled)
    );
    let view = app.active_view().unwrap();
    assert_eq!(
        app.surfaces
            .top_mut()
            .unwrap()
            .state
            .handle(&view, interaction::Intent::Activate),
        None
    );
}
#[test]
fn model_selection_closes_the_nested_picker_and_keeps_its_receipt_by_the_composer() {
    let (mut app, remote) = app();
    app.attached = false;
    app.info.profile = "current".into();
    app.dispatch_line(
        "/model",
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    let profiles_ticket = app.requests.last().unwrap().0.clone();
    app.apply_completion(Completion {
        mutation: false,
        ticket: profiles_ticket,
        result: Ok(Reply::Profiles(vec![crate::app::ProfileSummary {
            name: "terra".into(),
            provider: "chatgpt".into(),
            model: "gpt-terra".into(),
            thinking: "high".into(),
        }])),
    });

    app.handle_key(
        enter(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    assert_eq!(app.surfaces.iter().count(), 2);
    app.handle_key(
        enter(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    assert!(matches!(
        app.requests.last().map(|(_, request)| request),
        Some(Request::Command(backend::Command::SwitchProfileThinking {
            profile,
            thinking,
            save: false,
        })) if profile == "terra" && thinking == "high"
    ));
    let switch_ticket = app.requests.last().unwrap().0.clone();
    let mut info = app.info.clone();
    info.profile = "terra".into();
    info.model = "gpt-terra".into();
    info.thinking = "high".into();
    let message = "Switched to profile terra (provider chatgpt, model gpt-terra). Set as default profile.\nThinking: high";
    app.apply_completion(Completion {
        mutation: true,
        ticket: switch_ticket,
        result: Ok(Reply::Operation {
            message: message.into(),
            info,
            history: None,
            prompt: None,
        }),
    });

    assert!(
        !app.has_surface(),
        "the complete model picker flow must close"
    );
    assert_eq!(
        app.composer_notices,
        [
            "Switched to profile terra (provider chatgpt, model gpt-terra). Set as default profile.",
            "Thinking: high",
        ]
    );
    assert!(app.entries.iter().all(|entry| entry.raw != message));
}

#[test]
fn command_arguments_survive_completion_and_pending_mutation_defers_plain_input() {
    let (mut app, remote) = app();
    app.attached = false;
    app.composer.set_input("/thinking high --save");
    assert!(app.suggestions().is_empty());
    app.handle_key(
        KeyEvent::from(KeyCode::Enter),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    assert!(
        matches!(&app.requests.last().unwrap().1,Request::Command(backend::Command::SetThinking{thinking,save:true}) if thinking=="high")
    );
    let ticket = app.requests.last().unwrap().0.clone();
    app.surfaces.clear();
    app.composer.set_input("after configuration");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    assert_eq!(app.queued_input.as_deref(), Some("after configuration"));
    app.apply_completion(Completion {
        mutation: true,
        ticket,
        result: Ok(Reply::Operation {
            message: "Applied".into(),
            info: app.info.clone(),
            history: None,
            prompt: None,
        }),
    });
    assert_eq!(app.composer_notices, ["Applied"]);
    assert!(app.entries.iter().all(|entry| entry.raw != "Applied"));
    assert!(!app.has_writes());
    assert_eq!(
        app.submit_queued_input(&Backend::Attach(&remote), &CancellationToken::new()),
        Some(Action::Prompt("after configuration".into()))
    );
}
#[test]
fn sandbox_exclude_dispatch_and_completion_stay_inline_on_success_and_failure() {
    for succeeds in [true, false] {
        let (mut app, remote) = app();
        app.attached = false;
        app.dispatch_line(
            "/sandbox exclude 'gh *'",
            &Backend::Attach(&remote),
            &CancellationToken::new(),
        );
        assert!(
            !app.has_surface(),
            "explicit exclusion must not push a modal"
        );
        let (ticket, request) = app.requests.last().unwrap();
        assert_eq!(ticket.surface, 0);
        assert!(
            matches!(request, Request::Command(backend::Command::SandboxExclude(entry)) if entry == "gh *")
        );
        let ticket = ticket.clone();
        let message = if succeeds {
            "Saved excluded command gh * and reloaded immediately"
        } else {
            "configuration is invalid; no changes made"
        };
        let result = if succeeds {
            Ok(Reply::Operation {
                message: message.into(),
                info: app.info.clone(),
                history: None,
                prompt: Some("retry it now".into()),
            })
        } else {
            Err(message.into())
        };
        app.apply_completion(Completion {
            mutation: true,
            ticket,
            result,
        });
        assert!(!app.has_surface());
        assert!(!app.has_writes());
        assert!(app.entries.iter().any(|entry| entry.raw == message));
        let actions = app.drain_actions();
        assert_eq!(
            actions.contains(&Action::Prompt("retry it now".into())),
            succeeds
        );
    }
}

#[test]
fn reviewing_approval_never_opens_an_approval_surface() {
    let (mut app, _) = app();
    let mut request = approval("reviewing");
    request.review = Some(otto_core::approval::ReviewMetadata {
        status: otto_core::approval::ReviewStatus::Reviewing,
        assessment: None,
        settings_revision: 1,
        evidence_digest: "evidence".into(),
        policy_digest: "policy".into(),
    });
    app.sync_shared_approval(&[request]);
    assert_eq!(app.surfaces.iter().count(), 0);
}

#[test]
fn memory_review_surface_consumes_its_approval_without_an_extra_dialog() {
    let (mut app, remote) = app();
    app.attached = false;
    app.dispatch_line(
        "/memory review",
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    let mut request = approval("memory-1");
    request.subject.source = "memory".into();
    app.sync_shared_approval(&[request]);
    assert_eq!(app.surfaces.iter().count(), 1);
    assert_eq!(app.surfaces.top().unwrap().model.key(), "reviews");
}
#[test]
fn scroll_reverses_immediately_after_reaching_the_document_end() {
    let (mut app, _) = app();
    app.transition(
        0,
        Transition::Push(probe(
            "document",
            View {
                document: "one\ntwo\nthree".into(),
                ..Default::default()
            },
            Arc::new(AtomicUsize::new(0)),
        )),
    );
    app.geometry.borrow_mut().max_scroll = 2;
    app.surfaces.top_mut().unwrap().state.scroll = u16::MAX;
    app.scroll_wheel(true);
    assert_eq!(app.surfaces.top().unwrap().state.scroll, 1);
}

fn steer(id: u64, text: &str, status: otto_core::agent::steering::Status) -> SteerMessage {
    SteerMessage {
        id,
        text: text.into(),
        status,
        input_id: String::new(),
    }
}

fn control_u() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('u'), crossterm::event::KeyModifiers::CONTROL)
}

fn commands(app: &App) -> Vec<Request> {
    app.requests
        .iter()
        .map(|(_, request)| request.clone())
        .collect()
}

/// The run's own read is the only source of the queued guidance: a message
/// that left the queued state disappears without the TUI deciding anything.
#[test]
fn a_steering_read_paints_and_clears_the_tracked_guidance() {
    let (mut app, _) = app();
    app.apply_steering(vec![steer(1, "use the other branch", Status::Queued)]);
    assert_eq!(app.queued_steer.as_ref().map(|steer| steer.id), Some(1));

    // Newest queued wins, and a delivered earlier message is history.
    app.apply_steering(vec![
        steer(1, "first", Status::Delivered),
        steer(2, "second", Status::Queued),
    ]);
    assert_eq!(
        app.queued_steer.as_ref().map(|steer| steer.text.as_str()),
        Some("second")
    );

    app.apply_steering(vec![steer(2, "second", Status::Delivered)]);
    assert!(app.queued_steer.is_none());
}

/// A submission during a busy turn must be admitted, and its guidance must
/// show up promptly: the read follows the admission on the command lock.
#[test]
fn a_busy_submission_admits_guidance_then_reads_the_state_after_it() {
    let (mut app, remote) = app();
    app.start_turn();
    app.composer.set_input("use the other branch");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    assert!(matches!(
        commands(&app).as_slice(),
        [Request::Command(backend::Command::AdmitInput(text)),
         Request::Command(backend::Command::SteeringState)] if text == "use the other branch"
    ));
    let (admission, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: admission,
        result: Ok(Reply::Operation {
            message: "Guidance queued for the active task: use the other branch".into(),
            info: app.info.clone(),
            history: None,
            prompt: None,
        }),
    });
    let (read, _) = app.requests[1].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: read,
        result: Ok(Reply::Steering(vec![steer(
            1,
            "use the other branch",
            Status::Queued,
        )])),
    });
    assert_eq!(app.queued_steer.as_ref().map(|steer| steer.id), Some(1));

    // The read leaves no write pending, so typing again during the same
    // turn is still guidance rather than a queued later turn.
    app.composer.set_input("and stop after that");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    assert!(app.queued_input.is_none());
    let requests = commands(&app);
    assert!(matches!(
        &requests[requests.len() - 2..],
        [Request::Command(backend::Command::AdmitInput(text)),
         Request::Command(backend::Command::SteeringState)] if text == "and stop after that"
    ));
}

/// The read that follows an admission changes nothing, so it must not hold
/// a later submission back. If it took the pending-write slot, typing again
/// while it was in flight would be parked as a queued next turn instead of
/// steering the same turn — the silent later turn the contract forbids.
#[test]
fn a_guidance_read_in_flight_never_defers_the_next_submission() {
    let (mut app, remote) = app();
    app.start_turn();
    app.composer.set_input("use the other branch");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    // The admission's own write resolves; only its read is outstanding.
    let (admission, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: admission,
        result: Ok(Reply::Operation {
            message: "Guidance queued for the active task: use the other branch".into(),
            info: app.info.clone(),
            history: None,
            prompt: None,
        }),
    });
    assert!(
        !app.has_writes(),
        "a read in flight must not look like a pending write"
    );

    app.composer.set_input("and stop after that");
    assert_eq!(
        app.submit_input(&Backend::Attach(&remote), &CancellationToken::new()),
        None
    );
    assert!(
        app.queued_input.is_none(),
        "a second guidance message must not become a queued next turn"
    );
    let requests = commands(&app);
    assert!(
        matches!(
            &requests[2..],
            [Request::Command(backend::Command::AdmitInput(text)),
             Request::Command(backend::Command::SteeringState)] if text == "and stop after that"
        ),
        "{requests:?}"
    );
}

#[test]
fn control_u_recalls_backend_confirmed_guidance_for_editing() {
    let (mut app, remote) = app();
    app.start_turn();
    // Both a queued next input and queued guidance are waiting: Ctrl+U must
    // take back the guidance the run owns and leave the queued next input
    // alone, because only the run can resolve the guidance's state.
    app.queued_input = Some("/memory search vim".into());
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));
    app.awaiting_user_echo = Some("use the other branch".into());
    app.composer.set_input("draft");

    app.handle_key(
        control_u(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );

    assert!(app.composer.input.is_empty(), "the draft is cleared");
    assert_eq!(app.queued_input.as_deref(), Some("/memory search vim"));
    assert!(matches!(
        commands(&app).as_slice(),
        [
            Request::Command(backend::Command::WithdrawSteer(3)),
            Request::Command(backend::Command::SteeringState)
        ]
    ));

    let (withdraw, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: withdraw,
        result: Ok(Reply::Operation {
            message: "Queued input withdrawn.".into(),
            info: app.info.clone(),
            history: None,
            prompt: None,
        }),
    });
    let (read, _) = app.requests[1].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket: read,
        result: Ok(Reply::Steering(vec![steer(
            3,
            "use the other branch",
            Status::Withdrawn,
        )])),
    });

    assert!(app.queued_steer.is_none());
    assert!(
        app.awaiting_user_echo.is_none(),
        "withdrawn guidance cannot still be waiting for its echo"
    );
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "use the other branch",
        "backend-confirmed withdrawal recalls the exact guidance"
    );
}

#[test]
fn recalled_guidance_does_not_overwrite_a_new_draft() {
    let (mut app, remote) = app();
    app.start_turn();
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

    app.handle_key(
        control_u(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    app.composer.set_input("new draft");
    app.apply_steering(vec![steer(3, "use the other branch", Status::Withdrawn)]);

    assert_eq!(app.composer.input.iter().collect::<String>(), "new draft");
    assert!(app.recalling_steer.is_none());
}

#[test]
fn delivered_guidance_is_not_restored_when_recall_loses_the_race() {
    let (mut app, remote) = app();
    app.start_turn();
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

    app.handle_key(
        control_u(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    app.apply_steering(vec![steer(3, "use the other branch", Status::Delivered)]);

    assert!(app.composer.input.is_empty());
    assert!(app.recalling_steer.is_none());
}

/// The still-live queued-next-input path: Ctrl+U clears it locally, with no
/// backend round trip.
#[test]
fn control_u_still_withdraws_a_queued_next_input() {
    let (mut app, remote) = app();
    app.start_turn();
    app.queued_input = Some("/memory search vim".into());
    app.awaiting_user_echo = Some("/memory search vim".into());
    app.composer.set_input("draft");

    app.handle_key(
        control_u(),
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );

    assert!(app.queued_input.is_none());
    assert!(app.awaiting_user_echo.is_none());
    assert!(app.composer.input.is_empty());
    assert!(app.requests.is_empty());
}

#[test]
fn a_turn_end_returns_undelivered_guidance_to_the_composer() {
    let (mut app, _) = app();
    app.start_turn();
    app.awaiting_user_echo = Some("use the other branch".into());
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

    app.end_turn();

    assert!(app.queued_steer.is_none());
    assert!(app.awaiting_user_echo.is_none());
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "use the other branch"
    );
    assert!(matches!(
        app.requests.last().unwrap().1,
        Request::Command(backend::Command::SteeringState)
    ));

    // A draft the user typed meanwhile is never overwritten.
    app.composer.set_input("my own draft");
    app.queued_steer = Some(steer(4, "use the other branch", Status::Queued));
    app.end_turn();
    assert_eq!(
        app.composer.input.iter().collect::<String>(),
        "my own draft"
    );
}

/// Delivery, not the turn end, is what resolves guidance the run took: the
/// echo is the run's own word that the text reached the model.
#[test]
fn a_delivered_guidance_echo_leaves_the_composer_empty_at_the_turn_end() {
    let (mut app, _) = app();
    app.start_turn();
    app.awaiting_user_echo = Some("use the other branch".into());
    app.queued_steer = Some(steer(3, "use the other branch", Status::Queued));

    app.apply_event(&WireEvent {
        event_type: USER_MESSAGE.into(),
        text: "use the other branch".into(),
        ..Default::default()
    });
    assert!(app.queued_steer.is_none());
    assert!(app.awaiting_user_echo.is_none());

    app.end_turn();
    assert!(app.composer.input.is_empty());
}

/// `/steer` is explicit guidance: with nothing running it reports the error
/// the runtime gave instead of silently becoming a prompt.
#[test]
fn an_explicit_steer_command_never_starts_a_turn() {
    let (mut app, remote) = app();
    assert_eq!(
        app.dispatch_line(
            "/steer use the other branch",
            &Backend::Attach(&remote),
            &CancellationToken::new()
        ),
        None
    );
    assert!(matches!(
        commands(&app).as_slice(),
        [Request::Command(backend::Command::Steer(text)),
         Request::Command(backend::Command::SteeringState)] if text == "use the other branch"
    ));
    assert!(
        app.drain_actions().is_empty(),
        "an explicit steer must not dispatch a prompt"
    );
    // The runtime's reason reaches the transcript rather than a turn.
    let (ticket, _) = app.requests[0].clone();
    app.apply_completion(Completion {
        mutation: true,
        ticket,
        result: Err("there is no active task accepting guidance".into()),
    });
    assert!(
        app.entries
            .iter()
            .any(|entry| entry.raw == "there is no active task accepting guidance"),
        "{:?}",
        app.entries
            .iter()
            .map(|e| e.raw.as_str())
            .collect::<Vec<_>>()
    );
}
