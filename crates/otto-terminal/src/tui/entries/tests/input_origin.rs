use super::*;
use otto_core::model::{InputDelivery, InputProvenance, InputSource, InputTransport};

#[test]
fn human_origins_identity_and_delivery_render_without_changing_content() {
    for source in [
        InputSource::Tui,
        InputSource::Repl,
        InputSource::OneShot,
        InputSource::Http,
        InputSource::Acp,
        InputSource::Connector,
        InputSource::Unknown,
    ] {
        for delivery in [
            InputDelivery::Direct,
            InputDelivery::Queued,
            InputDelivery::Steering,
        ] {
            let provenance = InputProvenance {
                source,
                transport: InputTransport::Local,
                delivery,
                client_source: None,
            };
            let message = Message {
                role: Role::User,
                input_id: "stable-input".into(),
                input_provenance: Some(provenance),
                blocks: vec![Block::text("unchanged words")],
                ..Default::default()
            };
            let (entries, _) = entries_from_history(&[message]);
            assert_eq!(entries[0].raw, "unchanged words");
            assert!(entries[0].input_label.contains("stable-input"));
            assert!(entries[0].input_label.contains(&provenance.label()));
            let rendered = format!(
                "{:?}",
                crate::tui::transcript::entry_lines(&entries[0], false, 160)
            );
            assert!(rendered.contains("stable-input"));
            assert!(rendered.contains("delivered"));
        }
    }
}

#[test]
fn legacy_unknown_and_runtime_context_are_not_fabricated_human_source() {
    let (entries, _) = entries_from_history(&[
        user("legacy"),
        Message {
            role: Role::Context,
            display: true,
            context_type: "task_notification".into(),
            blocks: vec![Block::text("runtime event")],
            ..Default::default()
        },
    ]);
    assert!(entries[0].input_label.contains("source=unknown"));
    assert!(!entries[0].input_label.contains("Tui"));
    assert!(entries[1].input_label.is_empty());
}

#[test]
fn attached_transport_does_not_promote_unverified_client_source() {
    let origin = InputProvenance::remote(InputTransport::Http, Some(InputSource::Tui));
    let (entries, _) = entries_from_history(&[Message {
        role: Role::User,
        input_id: "remote-id".into(),
        input_provenance: Some(origin),
        blocks: vec![Block::text("words")],
        ..Default::default()
    }]);
    assert!(entries[0].input_label.contains("\"source\":\"http\""));
    assert!(entries[0].input_label.contains("\"client_source\":\"tui\""));
}
