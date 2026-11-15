use super::*;
use crate::tui::interaction::{Intent, State};
use std::sync::Mutex;

static COPIED: Mutex<Vec<String>> = Mutex::new(Vec::new());
fn record_copy(text: &str) -> std::io::Result<()> {
    COPIED.lock().unwrap().push(text.into());
    Ok(())
}
fn fail_copy(_: &str) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "pbcopy unavailable",
    ))
}

#[test]
fn session_popup_copies_exact_payloads_with_keyboard_and_keeps_open() {
    let info = Info {
        session_id: "own-id".into(),
        session_name: "Current".into(),
        workspace: "/workspace".into(),
        ..Default::default()
    };
    let mut presenter = SessionDetails::new(&info);
    presenter.copy = record_copy;
    let view = presenter.view();
    let mut state = State::new(&view);
    assert_eq!(state.selected_action.as_deref(), Some("session-copy-id"));
    assert_eq!(
        crate::tui::input::hints(&view, &state),
        "↑/↓ select · Enter copy · Esc close"
    );
    let activation = state.handle(&view, Intent::Activate).unwrap();
    assert!(matches!(presenter.activate(activation), Transition::Stay));
    assert_eq!(
        presenter.view().notice.as_deref(),
        Some("Session ID copied.")
    );
    state.handle(&view, Intent::Move(1));
    assert_eq!(
        state.selected_action.as_deref(),
        Some("session-copy-details")
    );
    let activation = state.handle(&view, Intent::Activate).unwrap();
    assert!(matches!(presenter.activate(activation), Transition::Stay));
    let payloads = COPIED.lock().unwrap();
    assert_eq!(payloads.as_slice(), ["own-id", &view.document]);
    assert_eq!(presenter.view().document, view.document);
    drop(payloads);
    state.handle(&view, Intent::Move(-1));
    assert_eq!(state.selected_action.as_deref(), Some("session-copy-id"));
    assert!(matches!(
        presenter.activate(Activation::Back),
        Transition::Close
    ));
}

#[test]
fn session_popup_clipboard_failure_is_actionable_and_retryable() {
    let mut presenter = SessionDetails::new(&Info::default());
    presenter.copy = fail_copy;
    let mut state = State::new(&presenter.view());
    let activation = state.handle(&presenter.view(), Intent::Activate).unwrap();
    assert!(matches!(presenter.activate(activation), Transition::Stay));
    let notice = presenter.view().notice.unwrap();
    assert!(notice.contains("Copy failed: pbcopy unavailable"));
    assert!(notice.contains("terminal host's clipboard helper and retry"));
    assert_eq!(presenter.view().actions.len(), 2);
}

#[test]
fn session_popup_is_client_local_for_both_local_and_attached_sessions() {
    let catalog = super::super::Catalog::default();
    for attached in [false, true] {
        let Transition::Push(mut presenter) = catalog
            .open("/session", &Info::default(), attached)
            .unwrap()
        else {
            panic!("session popup must be a client-side presenter, not a server request");
        };
        assert!(presenter.start().is_none());
        assert!(presenter.refresh().is_none());
        assert!(presenter.retry().is_none());
        assert_eq!(presenter.key(), "session");
        assert_eq!(
            presenter.view().default_action.as_deref(),
            Some("session-copy-id")
        );
    }
}
