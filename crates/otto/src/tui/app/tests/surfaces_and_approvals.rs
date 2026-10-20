//! Tests for nested surfaces and approval presentation.

use super::*;

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
