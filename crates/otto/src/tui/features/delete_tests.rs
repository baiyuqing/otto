use super::{
    Presenter, Transition,
    choice::{Choice, ChoiceKind},
};
use crate::tui::{
    backend::{Command, Reply, Request},
    interaction::Activation,
};

#[test]
fn permanent_delete_picker_requires_irreversible_confirmation() {
    let mut picker = Choice::new(ChoiceKind::Delete, "");
    assert!(matches!(picker.start(), Some(Request::Sessions)));
    picker.receive(Reply::Sessions(vec![crate::tui::backend::SessionChoice {
        label: "Session".into(),
        value: "/session.jsonl".into(),
    }]));
    let Transition::Push(mut confirmation) =
        picker.activate(Activation::Open("/session.jsonl".into()))
    else {
        panic!("confirmation required")
    };
    let view = confirmation.view();
    assert_eq!(view.default_action.as_deref(), Some("cancel"));
    assert!(view.document.contains("cannot be undone"));
    assert!(confirmation.start().is_none());
    assert!(matches!(
        confirmation.activate(Activation::Action {
            id: "cancel".into(),
            row: None,
            fields: Default::default()
        }),
        Transition::Close
    ));
    let confirmed = confirmation.activate(Activation::Action {
        id: "apply".into(),
        row: None,
        fields: Default::default(),
    });
    assert!(
        matches!(confirmed, Transition::Request(Request::Command(Command::Delete(path))) if path == "/session.jsonl")
    );
}

#[test]
fn permanent_delete_picker_rejects_unlisted_selection() {
    let mut picker = Choice::new(ChoiceKind::Delete, "");
    assert!(matches!(
        picker.activate(Activation::Open("/outside.jsonl".into())),
        Transition::Stay
    ));
}
