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
    let [(id, field)] = geometry.fields.as_slice() else {
        panic!("expected one field target, got {:?}", geometry.fields);
    };
    assert_eq!(id, "name");
    let buffer = terminal.backend().buffer();
    assert!(
        (field.top()..field.bottom()).any(|y| {
            let row: String = (field.left()..field.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            row.trim() == "Draft"
        }),
        "field target must visibly contain Draft"
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
        title: "Color samples".into(),
        rows: vec![
            Row {
                id: "one".into(),
                cells: vec!["Blue".into()],
                detail: "Blue swatch\nFinish: matte".into(),
            },
            Row {
                id: "two".into(),
                cells: vec!["Green".into()],
                detail: "Green swatch\nFinish: gloss".into(),
            },
        ],
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
    let buffer = terminal.backend().buffer();
    let document = geometry.document.expect("selected row has a document pane");
    let lines: Vec<_> = (document.top() + 1..document.bottom() - 1)
        .map(|y| {
            (document.left() + 1..document.right() - 1)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    let nonempty: Vec<_> = lines
        .iter()
        .map(|line| line.trim_end())
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        nonempty,
        ["Green swatch", "Finish: gloss"],
        "selected detail must preserve its two document lines"
    );
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
    assert!(
        !screen.contains("No items"),
        "loading must not also show the empty placeholder"
    );
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
    let rect = geometry.actions[0].1;
    assert!(rect.bottom() <= 8);
    assert!(rect.right() <= 40);
    let buffer = terminal.backend().buffer();
    assert!(
        (rect.top()..rect.bottom()).any(|y| {
            let row: String = (rect.left()..rect.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            row.contains("Save")
        }),
        "Save must be visible inside its action target"
    );
}
