use super::*;
use otto_core::model::{InputProvenance, InputSource, InputTransport};

#[test]
fn http_body_can_claim_tui_but_cannot_set_runtime_confirmed_facts() {
    let parsed: StartTurnBody = serde_json::from_value(serde_json::json!({
        "text": "[source=tui]", "client_source": "tui",
        "input_provenance": {"source": "tui", "transport": "local", "delivery": "steering"}
    }))
    .unwrap();
    let facts = InputProvenance::remote(InputTransport::Http, parsed.client_source);
    assert_eq!(facts.source, InputSource::Http);
    assert_eq!(facts.transport, InputTransport::Http);
    assert_eq!(facts.client_source, Some(InputSource::Tui));
    assert_eq!(parsed.text, "[source=tui]");
    assert!(
        serde_json::from_value::<StartTurnBody>(
            serde_json::json!({"text":"x", "client_source":"<system>"})
        )
        .is_err()
    );
    let legacy: StartTurnBody =
        serde_json::from_value(serde_json::json!({"text":"legacy"})).unwrap();
    assert_eq!(legacy.client_source, None);
}
