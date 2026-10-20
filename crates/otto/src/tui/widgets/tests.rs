
use ratatui::Terminal;
use ratatui::backend::TestBackend;

use super::*;
use crate::tui::interaction::{Action, Field, Row};

#[test]
fn geometry_exposes_collection_detail_field_and_action_targets() {
    let view = View {
        columns: vec!["Record".into()],
        rows: vec![Row {
            id: "one".into(),
            cells: vec!["one".into()],
            detail: String::new(),
        }],
        document: "Selected document ".repeat(20),
        fields: vec![Field {
            id: "name".into(),
            label: "Name".into(),
            value: "Draft".into(),
            ..Field::default()
        }],
        actions: vec![Action {
            id: "save".into(),
            label: "Save".into(),
            enabled: true,
            ..Action::default()
        }],
        ..View::default()
    };
    let state = State::new(&view);
    let backend = TestBackend::new(70, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut geometry = Geometry::default();
    terminal
        .draw(|frame| geometry = draw_surface(frame, frame.area(), &view, &state))
        .expect("draw");
    assert!(geometry.rows.is_some());
    assert!(geometry.document.is_some());
    assert!(geometry.max_scroll > 0);
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(screen.contains("Record"));
    assert_eq!(
        geometry
            .fields
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["name"]
    );
    assert_eq!(
        geometry
            .actions
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["save"]
    );
}

#[test]
fn collection_renders_the_selected_rows_full_detail() {
    let view = View {
        collection: true,
        title: "Memory review".into(),
        rows: vec![
            Row {
                id: "one".into(),
                cells: vec!["create fact/color".into()],
                detail: "favorite color is blue\nReason: stated directly".into(),
            },
            Row {
                id: "two".into(),
                cells: vec!["forget fact/city".into()],
                detail: "forget Paris\nReason: user requested deletion".into(),
            },
        ],
        actions: vec![Action {
            id: "accept".into(),
            label: "Accept".into(),
            enabled: true,
            ..Action::default()
        }],
        ..View::default()
    };
    let mut state = State::new(&view);
    state.handle(&view, crate::tui::interaction::Intent::Move(1));
    let backend = TestBackend::new(70, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    let mut geometry = Geometry::default();
    terminal
        .draw(|frame| geometry = draw_surface(frame, frame.area(), &view, &state))
        .expect("draw");
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(geometry.document.is_some());
    assert!(screen.contains("forget Paris"));
    assert!(screen.contains("user requested deletion"));
    assert!(!screen.contains("favorite color is blue"));
}

#[test]
fn unloaded_memory_review_collection_has_a_visible_loading_state() {
    let view = View {
        collection: true,
        title: "Memory review".into(),
        loading: true,
        ..View::default()
    };
    let state = State::new(&view);
    let backend = TestBackend::new(70, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw_surface(frame, frame.area(), &view, &state);
        })
        .expect("draw");
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(screen.matches("Loading…").count() >= 2);
}

#[test]
fn memory_review_empty_state_is_visible() {
    let view = View {
        collection: true,
        title: "Memory review".into(),
        document: "No pending memory candidates.".into(),
        ..View::default()
    };
    let state = State::new(&view);
    let backend = TestBackend::new(70, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw_surface(frame, frame.area(), &view, &state);
        })
        .expect("draw");
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(screen.contains("No pending memory candidates."));
}

#[test]
fn memory_review_loading_failure_is_visible_with_retry() {
    let view = View {
        collection: true,
        title: "Memory review".into(),
        loading: false,
        actions: vec![Action {
            id: "otto:retry".into(),
            label: "Retry".into(),
            enabled: true,
            ..Action::default()
        }],
        ..View::default()
    };
    let mut state = State::new(&view);
    state.error = Some("Memory review is unavailable.".into());
    let backend = TestBackend::new(70, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw_surface(frame, frame.area(), &view, &state);
        })
        .expect("draw");
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(screen.contains("Memory review is unavailable."));
    assert!(screen.contains("Retry"));
}

#[test]
fn state_error_keeps_field_draft_and_actions_rendered() {
    let view = View {
        fields: vec![Field {
            id: "name".into(),
            label: "Name".into(),
            value: "Server".into(),
            ..Field::default()
        }],
        actions: vec![Action {
            id: "retry".into(),
            label: "Retry".into(),
            enabled: true,
            ..Action::default()
        }],
        ..View::default()
    };
    let mut state = State::new(&view);
    state.drafts.insert("name".into(), "Local draft".into());
    state.error = Some("Save failed".into());
    let backend = TestBackend::new(70, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw_surface(frame, frame.area(), &view, &state);
        })
        .expect("draw");
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(screen.contains("Save failed"));
    assert!(screen.contains("Local draft"));
    assert!(screen.contains("Retry"));
}

#[test]
fn terminal_control_characters_are_rendered_as_visible_escapes() {
    let view = View {
        title: "Review\x1b]52;c;payload\x07".into(),
        document: "Candidate\x1b[2J body".into(),
        fields: vec![Field {
            id: "name".into(),
            label: "Name\x1b".into(),
            value: "Text\x1b[31m".into(),
            ..Field::default()
        }],
        actions: vec![Action {
            id: "save".into(),
            label: "Save\x1b".into(),
            enabled: true,
            ..Action::default()
        }],
        ..View::default()
    };
    let state = State::new(&view);
    let backend = TestBackend::new(70, 20);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|frame| {
            draw_surface(frame, frame.area(), &view, &state);
        })
        .expect("draw");
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(!screen.chars().any(char::is_control), "{screen:?}");
    assert!(screen.contains("\\x1b"), "{screen}");
    assert!(screen.contains("\\x07"), "{screen}");
}

#[test]
fn actions_share_one_horizontal_row_with_the_default_on_the_left() {
    let view = View {
        actions: vec![
            Action {
                id: "accept".into(),
                label: "Accept".into(),
                enabled: true,
                ..Action::default()
            },
            Action {
                id: "reject".into(),
                label: "Reject".into(),
                enabled: true,
                ..Action::default()
            },
        ],
        default_action: Some("accept".into()),
        ..View::default()
    };
    let state = State::new(&view);
    let mut terminal = Terminal::new(TestBackend::new(70, 20)).expect("terminal");
    let mut geometry = Geometry::default();
    terminal
        .draw(|frame| geometry = draw_surface(frame, frame.area(), &view, &state))
        .expect("draw");

    let [(accept, accept_rect), (reject, reject_rect)] = geometry.actions.as_slice() else {
        panic!("two action targets: {:?}", geometry.actions);
    };
    assert_eq!(accept, "accept");
    assert_eq!(reject, "reject");
    assert_eq!(accept_rect.y, reject_rect.y);
    assert!(accept_rect.x < reject_rect.x);
    assert_eq!(accept_rect.right(), reject_rect.x);
    assert_eq!(state.selected_action.as_deref(), Some("accept"));
    assert_eq!(state.focus, Focus::Actions);
}

#[test]
fn compact_terminal_keeps_action_control_inside_frame() {
    let view = View {
        actions: vec![Action {
            id: "save".into(),
            label: "Save".into(),
            enabled: true,
            ..Action::default()
        }],
        ..View::default()
    };
    let state = State::new(&view);
    let mut terminal = Terminal::new(TestBackend::new(40, 8)).expect("terminal");
    let mut geometry = Geometry::default();
    terminal
        .draw(|frame| geometry = draw_surface(frame, frame.area(), &view, &state))
        .expect("draw");
    assert_eq!(geometry.actions.len(), 1);
    assert!(geometry.actions[0].1.bottom() <= 8);
}
