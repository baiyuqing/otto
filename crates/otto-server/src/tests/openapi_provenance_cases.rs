//! `openapi.yaml` documents the input-provenance request and event fields.
use otto_core::model::{InputDelivery, InputSource, InputTransport};

fn openapi_document() -> String {
    String::from_utf8_lossy(crate::OPENAPI_YAML).into_owned()
}

/// The text of one top-level path item, up to the next path item.
fn path_item_text(document: &str, path: &str) -> String {
    let header = format!("\n  {path}:\n");
    let start = document
        .find(&header)
        .unwrap_or_else(|| panic!("openapi.yaml is missing {path}"));
    let body = &document[start + header.len()..];
    let end = body.find("\n  /").unwrap_or(body.len());
    body[..end].to_owned()
}

/// The text of one `components.schemas` entry, up to the next schema.
fn schema_text(document: &str, name: &str) -> String {
    let header = format!("\n    {name}:\n");
    let start = document
        .find(&header)
        .unwrap_or_else(|| panic!("openapi.yaml is missing schema {name}"));
    let body = &document[start + header.len()..];
    let end = body
        .match_indices('\n')
        .map(|(index, _)| index)
        .find(|&index| {
            let next_line = &body[index + 1
                ..body[index + 1..]
                    .find('\n')
                    .map_or(body.len(), |n| index + 1 + n)];
            let indent = next_line.len() - next_line.trim_start_matches(' ').len();
            !next_line.trim().is_empty() && indent <= 4
        })
        .unwrap_or(body.len());
    body[..end].to_owned()
}

fn wire_name<T: serde::Serialize>(value: T) -> String {
    serde_json::to_value(value)
        .expect("provenance enums serialize")
        .as_str()
        .expect("provenance enums serialize as strings")
        .to_owned()
}

/// Every variant, by exhaustive match: a new variant fails to compile here.
fn every_input_source() -> Vec<String> {
    let all = [
        InputSource::Unknown,
        InputSource::Tui,
        InputSource::Repl,
        InputSource::OneShot,
        InputSource::Http,
        InputSource::Acp,
        InputSource::Connector,
        InputSource::Runtime,
        InputSource::Agent,
    ];
    for source in all {
        match source {
            InputSource::Unknown
            | InputSource::Tui
            | InputSource::Repl
            | InputSource::OneShot
            | InputSource::Http
            | InputSource::Acp
            | InputSource::Connector
            | InputSource::Runtime
            | InputSource::Agent => {}
        }
    }
    all.into_iter().map(wire_name).collect()
}

fn every_input_transport() -> Vec<String> {
    let all = [
        InputTransport::Unknown,
        InputTransport::Local,
        InputTransport::Http,
        InputTransport::Acp,
    ];
    for transport in all {
        match transport {
            InputTransport::Unknown
            | InputTransport::Local
            | InputTransport::Http
            | InputTransport::Acp => {}
        }
    }
    all.into_iter().map(wire_name).collect()
}

fn every_input_delivery() -> Vec<String> {
    let all = [
        InputDelivery::Direct,
        InputDelivery::Unknown,
        InputDelivery::Steering,
        InputDelivery::Queued,
    ];
    for delivery in all {
        match delivery {
            InputDelivery::Direct
            | InputDelivery::Unknown
            | InputDelivery::Steering
            | InputDelivery::Queued => {}
        }
    }
    all.into_iter().map(wire_name).collect()
}

#[test]
fn openapi_enumerates_every_input_provenance_value() {
    let document = openapi_document();
    let source_enum = format!("enum: [{}]", every_input_source().join(", "));
    assert!(
        schema_text(&document, "InputSource").contains(&source_enum),
        "InputSource must list {source_enum}"
    );
    let provenance = schema_text(&document, "InputProvenance");
    for wanted in [
        format!("enum: [{}]", every_input_transport().join(", ")),
        format!("enum: [{}]", every_input_delivery().join(", ")),
        "required: [source, transport, delivery]".to_owned(),
        "client_source:".to_owned(),
    ] {
        assert!(
            provenance.contains(&wanted),
            "InputProvenance must contain {wanted}"
        );
    }
}

#[test]
fn openapi_documents_client_source_on_every_input_route() {
    let document = openapi_document();
    for path in [
        "/v1/sessions/{id}/turns",
        "/v1/sessions/{id}/inputs",
        "/v1/sessions/{id}/tasks/steer",
        "/v1/sessions/{id}/tasks/control",
        "/v1/sessions/{id}/approvals/message",
    ] {
        let item = path_item_text(&document, path);
        for field in ["client_source:", "input_id:"] {
            assert!(
                item.contains(field),
                "{path} request body is missing {field}"
            );
        }
    }
    let turns = path_item_text(&document, "/v1/sessions/{id}/turns");
    assert!(
        turns.contains("queued_input:"),
        "POST /turns is missing queued_input"
    );
}

#[test]
fn openapi_documents_user_message_provenance_on_wire_events() {
    let wire_event = schema_text(&openapi_document(), "WireEvent");
    for field in [
        "input_id:",
        "input_provenance:",
        "#/components/schemas/InputProvenance",
    ] {
        assert!(wire_event.contains(field), "WireEvent is missing {field}");
    }
}
