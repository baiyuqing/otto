//! Tests for stale reads, forks, session replacement and pasted edits.

use super::*;

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
