use super::*;
use crossterm::event::{KeyCode, KeyModifiers};
use std::time::Duration;

#[test]
fn suspend_parks_reader_until_ui_releases_terminal_handoff() {
    let (sender, mut receiver) = mpsc::channel(1);
    let (finished, completion) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let event = TuiEvent::Key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::CONTROL));
        finished.send(forward_event(&sender, event)).unwrap();
    });
    let event = receiver.blocking_recv().expect("suspend event");
    assert!(matches!(event, TuiEvent::Suspend(_)));
    assert_eq!(
        completion.recv_timeout(Duration::from_millis(50)),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
    );
    drop(event);
    assert!(completion.recv_timeout(Duration::from_secs(2)).unwrap());
    reader.join().unwrap();
}

#[test]
fn suspend_does_not_capture_plain_z_or_alt_control_z() {
    for modifiers in [
        KeyModifiers::NONE,
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ] {
        let (sender, mut receiver) = mpsc::channel(1);
        assert!(forward_event(
            &sender,
            TuiEvent::Key(KeyEvent::new(KeyCode::Char('z'), modifiers))
        ));
        assert!(
            matches!(receiver.blocking_recv(), Some(TuiEvent::Key(key)) if key.modifiers == modifiers)
        );
    }
}

#[test]
fn suspend_binding_is_reserved_before_any_observer_screen_can_capture_it() {
    let bindings = otto_core::config::ui::ObserverBindings {
        toggle: "Ctrl+Z".into(),
        ..Default::default()
    };
    let error = match crate::tui::observer::bindings::Bindings::parse(&bindings) {
        Ok(_) => panic!("Ctrl-Z must not be an observer binding"),
        Err(error) => error,
    };
    assert!(error.contains("Ctrl+Z job suspension"), "{error}");
}
