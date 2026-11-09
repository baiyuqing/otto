use super::*;
use crossterm::event::{KeyCode, KeyModifiers};
use std::time::Duration;

#[test]
fn wheel_batches_preserve_direction_changes_keys_and_step_counts() {
    let wheel = |up| TuiEvent::Wheel { up, steps: 1 };
    let mut events = [
        wheel(true),
        wheel(true),
        wheel(false),
        TuiEvent::Key(KeyCode::Enter.into()),
    ]
    .into_iter();
    let (batch, pending) = batch_wheel(wheel(true), || events.next());
    assert!(matches!(batch, TuiEvent::Wheel { up: true, steps: 3 }));
    let (batch, pending) = batch_wheel(pending.unwrap(), || events.next());
    assert!(matches!(
        batch,
        TuiEvent::Wheel {
            up: false,
            steps: 1
        }
    ));
    let (key, pending) = batch_wheel(pending.unwrap(), || panic!("never read beyond a key"));
    assert!(matches!(key, TuiEvent::Key(key) if key.code == KeyCode::Enter));
    assert!(pending.is_none());
}

#[test]
fn wheel_batches_are_bounded_and_do_not_drop_remaining_input() {
    let mut remaining = 99;
    let (batch, pending) = batch_wheel(TuiEvent::Wheel { up: true, steps: 1 }, || {
        remaining -= 1;
        Some(TuiEvent::Wheel { up: true, steps: 1 })
    });
    assert!(matches!(
        batch,
        TuiEvent::Wheel {
            up: true,
            steps: 64
        }
    ));
    assert_eq!(remaining, 36);
    assert!(pending.is_none());
}

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
