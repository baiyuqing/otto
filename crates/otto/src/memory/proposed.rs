//! Canonical proposed memory records, independent of a storage engine.
use super::codec::{
    MAX_SOURCE_JSON_BYTES, encode_provenance, format_timestamp, parse_timestamp, valid_stored_float,
};
use super::json::{encode_float, encode_string, encode_string_map, encode_string_slice};
use super::validate::provenance_zero;
use super::{Error, ErrorKind, Origin, Provenance, Record, Result, Scope};

pub(crate) const MAX_PROPOSED_JSON_BYTES: usize = 32 * 1024;

fn corrupt() -> Error {
    Error::new(ErrorKind::Corrupt)
}

/// The proposed record as stored in `memory_candidates.proposed_json`.
///
/// A wholly empty provenance stores as `{}` rather than the fully populated
/// object [`encode_provenance`] emits.
pub(crate) fn encode_proposed_record(record: &Record) -> Result<String> {
    let source = if provenance_zero(&record.source) {
        "{}".to_string()
    } else {
        let encoded = encode_provenance(&record.source);
        if encoded.len() > MAX_SOURCE_JSON_BYTES {
            return Err(corrupt());
        }
        encoded
    };
    let mut raw = String::from("{\"scope_namespace\":");
    encode_string(&record.scope.namespace, &mut raw);
    raw.push_str(",\"scope_id\":");
    encode_string(&record.scope.id, &mut raw);
    raw.push_str(",\"kind\":");
    encode_string(&record.kind, &mut raw);
    raw.push_str(",\"key\":");
    encode_string(&record.key, &mut raw);
    raw.push_str(",\"text\":");
    encode_string(&record.text, &mut raw);
    raw.push_str(",\"labels\":");
    raw.push_str(&encode_string_slice(&record.labels));
    raw.push_str(",\"metadata\":");
    raw.push_str(&encode_string_map(&record.metadata));
    raw.push_str(",\"source\":");
    raw.push_str(&source);
    raw.push_str(",\"confidence\":");
    raw.push_str(&encode_float(record.confidence));
    raw.push_str(",\"expiry\":");
    match record.expires_at {
        Some(expiry) => encode_string(&format_timestamp(expiry), &mut raw),
        None => raw.push_str("null"),
    }
    raw.push('}');
    if raw.len() > MAX_PROPOSED_JSON_BYTES {
        return Err(corrupt());
    }
    Ok(raw)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProposedWire {
    scope_namespace: String,
    scope_id: String,
    kind: String,
    key: String,
    text: String,
    labels: Vec<String>,
    metadata: std::collections::BTreeMap<String, String>,
    source: serde_json::Value,
    confidence: f64,
    expiry: Option<String>,
}

/// The provenance inside a proposed blob.
///
/// [`super::codec::decode_provenance`] cannot be reused here: it compares its
/// input against its own canonical encoding, and a nested value re-serialized
/// out of `serde_json` has its keys in sorted rather than struct-field order.
/// The whole-blob byte comparison in [`decode_proposed_record`] enforces the
/// same canonicality one level up.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProvenanceWire {
    origin: String,
    session_id: String,
    message_ids: Vec<String>,
    observation_id: String,
    decision_at: Option<String>,
    decision_source: String,
}

fn decode_origin(value: &str) -> Result<Option<Origin>> {
    if value.is_empty() {
        return Ok(None);
    }
    Origin::parse(value).map(Some).ok_or_else(corrupt)
}

fn decode_proposed_provenance(value: serde_json::Value) -> Result<Provenance> {
    if value.as_object().is_some_and(|object| object.is_empty()) {
        return Ok(Provenance::default());
    }
    let wire: ProvenanceWire = serde_json::from_value(value).map_err(|_| corrupt())?;
    Ok(Provenance {
        origin: decode_origin(&wire.origin)?,
        session_id: wire.session_id,
        message_ids: wire.message_ids,
        observation_id: wire.observation_id,
        decision_at: match &wire.decision_at {
            Some(at) => Some(parse_timestamp(at).map_err(|_| corrupt())?),
            None => None,
        },
        decision_source: decode_origin(&wire.decision_source)?,
    })
}

/// Decodes a stored blob and refuses anything that is not its own canonical
/// encoding, so a hand-edited row is corruption rather than silent input.
pub(crate) fn decode_proposed_record(raw: &str) -> Result<Record> {
    if raw.is_empty() || raw.len() > MAX_PROPOSED_JSON_BYTES {
        return Err(corrupt());
    }
    let wire: ProposedWire = serde_json::from_str(raw).map_err(|_| corrupt())?;
    let source = decode_proposed_provenance(wire.source)?;
    let record = Record {
        id: String::new(),
        scope: Scope::new(wire.scope_namespace, wire.scope_id),
        kind: wire.kind,
        key: wire.key,
        text: wire.text,
        labels: wire.labels,
        metadata: wire.metadata,
        source,
        confidence: wire.confidence,
        expires_at: match wire.expiry {
            Some(value) => Some(parse_timestamp(&value).map_err(|_| corrupt())?),
            None => None,
        },
        ..Record::default()
    };
    if encode_proposed_record(&record)? != raw || !valid_stored_float(record.confidence) {
        return Err(corrupt());
    }
    Ok(record)
}
