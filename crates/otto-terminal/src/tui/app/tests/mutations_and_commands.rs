//! Tests for mutation receipts, nested pickers and command dispatch.

use super::*;

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
    app.info.model_selector = "chatgpt/current".into();
    app.dispatch_line(
        "/model",
        &Backend::Attach(&remote),
        &CancellationToken::new(),
    );
    let profiles_ticket = app.requests.last().unwrap().0.clone();
    app.apply_completion(Completion {
        mutation: false,
        ticket: profiles_ticket,
        result: Ok(Reply::Models(vec![otto::app::ModelSummary {
            selector: "chatgpt/gpt-terra".into(),
            provider: "chatgpt".into(),
            provider_type: "chatgpt".into(),
            model_id: "gpt-terra".into(),
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
        Some(Request::Command(backend::Command::SwitchModelThinking {
            model,
            thinking,
            save: false,
        })) if model == "chatgpt/gpt-terra" && thinking == "high"
    ));
    let switch_ticket = app.requests.last().unwrap().0.clone();
    let mut info = app.info.clone();
    info.model_selector = "chatgpt/gpt-terra".into();
    info.model = "gpt-terra".into();
    info.thinking = "high".into();
    let message =
        "Switched to model chatgpt/gpt-terra (type chatgpt). Set as default model.\nThinking: high";
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
            "Switched to model chatgpt/gpt-terra (type chatgpt). Set as default model.",
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
