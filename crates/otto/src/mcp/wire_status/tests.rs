use super::*;

fn status(transport: &'static str, era: Option<Era>, state: ServerState) -> ServerStatus {
    ServerStatus {
        name: "docs".to_string(),
        transport,
        era,
        state,
    }
}

#[test]
fn every_state_and_era_survives_a_wire_round_trip() {
    let cases = [
        status(
            "http",
            Some(Era::Modern),
            ServerState::Connected { tools: 3 },
        ),
        status("stdio", None, ServerState::Connecting),
        status(
            "http",
            Some(Era::Legacy("2025-11-25".into())),
            ServerState::NeedsLogin,
        ),
        status("stdio", None, ServerState::Disabled),
        status("stdio", None, ServerState::Failed("spawn failed".into())),
    ];
    for case in cases {
        let wire = McpServer::from(case.clone());
        let json = serde_json::to_string(&wire).unwrap();
        let decoded: McpServer = serde_json::from_str(&json).unwrap();
        assert_eq!(ServerStatus::try_from(decoded).unwrap(), case);
    }
}

#[test]
fn wire_record_uses_the_documented_field_values() {
    let wire = McpServer::from(status(
        "http",
        Some(Era::Modern),
        ServerState::Connected { tools: 3 },
    ));
    assert_eq!(wire.protocol_version.as_deref(), Some(MODERN_VERSION));
    assert_eq!(
        (wire.state.as_str(), wire.tools, wire.error),
        ("connected", 3, None)
    );
}

#[test]
fn failed_without_an_error_text_maps_to_an_empty_message() {
    let wire = McpServer {
        name: "x".into(),
        transport: "stdio".into(),
        protocol_version: None,
        state: "failed".into(),
        tools: 0,
        error: None,
    };
    assert_eq!(
        ServerStatus::try_from(wire).unwrap().state,
        ServerState::Failed(String::new())
    );
}

#[test]
fn unknown_transport_and_state_are_errors() {
    let base = McpServer {
        name: "x".into(),
        transport: "stdio".into(),
        protocol_version: None,
        state: "connected".into(),
        tools: 0,
        error: None,
    };
    let transport = McpServer {
        transport: "pipe".into(),
        ..base.clone()
    };
    assert!(
        ServerStatus::try_from(transport)
            .unwrap_err()
            .contains("transport")
    );
    let state = McpServer {
        state: "asleep".into(),
        ..base
    };
    assert!(ServerStatus::try_from(state).unwrap_err().contains("state"));
}
