use super::*;

fn row(id: &str) -> Row {
    Row {
        id: id.into(),
        ..Row::default()
    }
}
fn action(id: &str) -> Action {
    Action {
        id: id.into(),
        enabled: true,
        ..Action::default()
    }
}

#[test]
fn selection_tracks_id_and_missing_row_uses_neighbor() {
    let mut view = View {
        rows: vec![row("a"), row("b"), row("c")],
        ..View::default()
    };
    let mut state = State::new(&view);
    state.handle(&view, Intent::Move(1));
    view.rows = vec![row("c"), row("b"), row("a")];
    state.reconcile(&view);
    assert_eq!(state.selected_row.as_deref(), Some("b"));
    view.rows.remove(1);
    state.reconcile(&view);
    assert_eq!(state.selected_row.as_deref(), Some("a"));
}

#[test]
fn initial_default_row_is_selected_but_refresh_preserves_user_selection() {
    let mut view = View {
        rows: vec![row("a"), row("b"), row("c")],
        default_row: Some("b".into()),
        ..View::default()
    };
    let mut state = State::new(&view);
    assert_eq!(state.selected_row.as_deref(), Some("b"));
    state.handle(&view, Intent::Move(1));
    assert_eq!(state.selected_row.as_deref(), Some("c"));
    view.rows.reverse();
    state.reconcile(&view);
    assert_eq!(state.selected_row.as_deref(), Some("c"));
}

#[test]
fn loading_choice_keeps_collection_focus_until_rows_arrive() {
    let mut view = View {
        collection: true,
        actions: vec![Action {
            id: "apply".into(),
            label: "Apply".into(),
            enabled: false,
            ..Action::default()
        }],
        ..View::default()
    };
    let mut state = State::new(&view);
    assert_eq!(state.focus, Focus::Rows);
    assert_eq!(state.handle(&view, Intent::Activate), None);

    view.rows = vec![row("high"), row("low")];
    view.default_row = Some("high".into());
    view.actions[0].enabled = true;
    state.reconcile(&view);
    assert_eq!(state.focus, Focus::Rows);
    assert_eq!(
        state.handle(&view, Intent::Activate),
        Some(Activation::Open("high".into()))
    );
    state.handle(&view, Intent::Move(1));
    assert_eq!(state.selected_row.as_deref(), Some("low"));
}

#[test]
fn loading_review_keeps_default_decision_focus_without_implicit_submit() {
    let mut view = View {
        collection: true,
        actions: vec![Action {
            id: "accept".into(),
            label: "Accept".into(),
            enabled: false,
            ..Action::default()
        }],
        default_action: Some("accept".into()),
        ..View::default()
    };
    let mut state = State::new(&view);
    assert_eq!(state.focus, Focus::Actions);
    assert_eq!(state.selected_action.as_deref(), Some("accept"));
    assert_eq!(state.handle(&view, Intent::Activate), None);

    view.rows = vec![row("candidate")];
    view.actions[0].enabled = true;
    state.reconcile(&view);
    assert_eq!(state.focus, Focus::Actions);
    assert_eq!(state.selected_row.as_deref(), Some("candidate"));
    assert_eq!(
        state.handle(&view, Intent::Activate),
        Some(Activation::Action {
            id: "accept".into(),
            row: Some("candidate".into()),
            fields: BTreeMap::new(),
        })
    );
}

#[test]
fn focus_cycles_and_back_is_always_available() {
    let view = View {
        rows: vec![row("r")],
        actions: vec![action("ok")],
        ..View::default()
    };
    let mut state = State::new(&view);
    assert_eq!(state.handle(&view, Intent::Back), Some(Activation::Back));
    state.handle(&view, Intent::NextFocus);
    assert_eq!(state.focus, Focus::Actions);
}

#[test]
fn drafts_survive_refresh_and_utf8_limit_is_bytes() {
    let mut view = View {
        fields: vec![Field {
            id: "name".into(),
            value: "a".into(),
            max_bytes: 3,
            ..Field::default()
        }],
        ..View::default()
    };
    let mut state = State::new(&view);
    state.handle(&view, Intent::End);
    state.handle(&view, Intent::Text("éé".into()));
    assert_eq!(state.drafts["name"], "aé");
    view.fields[0].value = "new server value".into();
    state.reconcile(&view);
    assert_eq!(state.drafts["name"], "aé");
}

#[test]
fn backspace_at_start_of_utf8_field_is_a_noop() {
    let view = View {
        fields: vec![Field {
            id: "name".into(),
            value: "猫".into(),
            max_bytes: 8,
            ..Field::default()
        }],
        ..View::default()
    };
    let mut state = State::new(&view);
    state.handle(&view, Intent::Home);
    state.handle(&view, Intent::Backspace);
    assert_eq!(state.drafts["name"], "猫");
    assert_eq!(state.field_cursor("name"), 0);
}

#[test]
fn reconcile_preserves_host_failure_and_actions_remain_navigable() {
    let view = View {
        actions: vec![action("accept"), action("reject")],
        default_action: Some("accept".into()),
        ..View::default()
    };
    let mut state = State::new(&view);
    state.error = Some("operation failed".into());
    state.reconcile(&view);
    assert_eq!(state.error.as_deref(), Some("operation failed"));
    state.handle(&view, Intent::MoveAction(1));
    assert_eq!(state.selected_action.as_deref(), Some("reject"));
    state.handle(&view, Intent::MoveAction(-1));
    assert_eq!(state.selected_action.as_deref(), Some("accept"));
}

#[test]
fn readonly_document_never_takes_focus_from_the_default_action() {
    let view = View {
        document: "Read-only approval details".into(),
        actions: vec![action("accept"), action("reject")],
        default_action: Some("accept".into()),
        ..View::default()
    };
    let mut state = State::new(&view);
    assert_eq!(state.focus, Focus::Actions);
    assert_eq!(state.selected_action.as_deref(), Some("accept"));

    state.handle(&view, Intent::NextFocus);
    assert_eq!(state.focus, Focus::Actions);
    state.handle(&view, Intent::Move(1));
    assert_eq!(state.focus, Focus::Actions);
    assert_eq!(state.selected_action.as_deref(), Some("accept"));
    assert_eq!(state.scroll, 1);
}

#[test]
fn host_removes_and_iterates_surfaces_by_stable_id() {
    let view = View::default();
    let mut host = Host::default();
    let first = host.push("first", &view);
    let second = host.push("second", &view);
    assert_eq!(host.iter_mut().count(), 2);
    assert_eq!(
        host.remove(first).map(|surface| surface.model),
        Some("first")
    );
    assert_eq!(host.top().map(|surface| surface.id), Some(second));
}

#[test]
fn pending_blocks_submission_and_default_action_only_selects() {
    let view = View {
        rows: vec![row("r")],
        actions: vec![action("accept")],
        default_action: Some("accept".into()),
        ..View::default()
    };
    let mut state = State::new(&view);
    assert_eq!(state.focus, Focus::Actions);
    assert_eq!(state.selected_action.as_deref(), Some("accept"));
    assert_eq!(
        state.handle(&view, Intent::Activate),
        Some(Activation::Action {
            id: "accept".into(),
            row: Some("r".into()),
            fields: BTreeMap::new()
        })
    );
    state.pending = true;
    assert_eq!(state.handle(&view, Intent::Activate), None);
}

#[test]
fn page_moves_selection_by_geometry_and_empty_rows_do_not_open() {
    let view = View {
        rows: vec![row("a"), row("b"), row("c"), row("d")],
        ..View::default()
    };
    let mut state = State::new(&view);
    state.page_rows = 2;
    state.handle(&view, Intent::Page(1));
    assert_eq!(state.selected_row.as_deref(), Some("c"));
    let empty = View::default();
    assert_eq!(State::new(&empty).handle(&empty, Intent::Activate), None);
}
#[test]
fn designated_field_submits_but_feedback_returns_to_actions_without_deciding() {
    let mut view = View {
        fields: vec![Field {
            id: "name".into(),
            value: "中文".into(),
            max_bytes: 100,
            submit: true,
            ..Default::default()
        }],
        actions: vec![Action {
            id: "save".into(),
            enabled: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut state = State::new(&view);
    assert!(
        matches!(state.handle(&view,Intent::Activate),Some(Activation::Action{id,fields,..}) if id=="save"&&fields["name"]=="中文")
    );
    view.fields[0].submit = false;
    state.focus = Focus::Field("name".into());
    assert_eq!(state.handle(&view, Intent::Activate), None);
    assert_eq!(state.focus, Focus::Actions);
    state.pending = true;
    state.focus = Focus::Rows;
    state.selected_row = Some("one".into());
    view.rows = vec![Row {
        id: "one".into(),
        ..Default::default()
    }];
    assert_eq!(state.handle(&view, Intent::Activate), None);
}

#[test]
fn a_space_extends_the_search_unless_an_action_uses_it() {
    let query_of = |view: &View| {
        let mut state = State::new(view);
        match state.handle(view, Intent::Shortcut(' ')) {
            Some(Activation::Action { id, fields, .. }) if id == "otto:search" => {
                Some(fields["query"].clone())
            }
            _ => None,
        }
    };
    let mut view = View {
        search: Some("ab".into()),
        collection: true,
        ..View::default()
    };
    assert_eq!(query_of(&view).as_deref(), Some("ab "));
    view.actions = vec![Action {
        shortcut: Some(' '),
        ..action("toggle")
    }];
    assert_eq!(query_of(&view), None);
}
