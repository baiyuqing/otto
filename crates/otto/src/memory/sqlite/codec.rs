//! The on-disk encoding of a record's columns.
//!
//! Every blob is written in Go's canonical `encoding/json` form and, on read,
//! re-encoded and compared byte for byte: a row whose stored bytes are not
//! exactly what this encoder would produce is corrupt, not merely unusual.

use chrono::{DateTime, NaiveDateTime, Utc};

use crate::memory::json;
use crate::memory::{Error, ErrorKind, MAX_METADATA_BYTES, Origin, Provenance, Record, Result};

/// The stored timestamp layout, `"2006-01-02T15:04:05.000000000Z"`. Exactly 30
/// bytes, which the schema's `length(...) = 30` checks depend on.
pub const TIMESTAMP_FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.9fZ";
/// Byte length of a value in [`TIMESTAMP_FORMAT`].
pub const TIMESTAMP_BYTES: usize = 30;

pub const MAX_LABELS_JSON_BYTES: usize = 8192;
pub const MAX_SOURCE_JSON_BYTES: usize = 8192;

fn corrupt() -> Error {
    Error::new(ErrorKind::Corrupt)
}

/// The four blob and timestamp columns of one record row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedRecord {
    pub labels: String,
    pub metadata: String,
    pub source: String,
    pub created: String,
    pub updated: String,
    pub expires: Option<String>,
}

fn origin_text(origin: Option<Origin>) -> &'static str {
    origin.map_or("", Origin::as_str)
}

pub fn encode_provenance(source: &Provenance) -> String {
    let mut out = String::from("{\"origin\":");
    json::encode_string(origin_text(source.origin), &mut out);
    out.push_str(",\"session_id\":");
    json::encode_string(&source.session_id, &mut out);
    out.push_str(",\"message_ids\":");
    out.push_str(&json::encode_string_slice(&source.message_ids));
    out.push_str(",\"observation_id\":");
    json::encode_string(&source.observation_id, &mut out);
    out.push_str(",\"decision_at\":");
    match source.decision_at {
        Some(at) => json::encode_string(&format_timestamp(at), &mut out),
        None => out.push_str("null"),
    }
    out.push_str(",\"decision_source\":");
    json::encode_string(origin_text(source.decision_source), &mut out);
    out.push('}');
    out
}

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

pub fn encode_record(record: &Record) -> Result<EncodedRecord> {
    let labels = json::encode_string_slice(&record.labels);
    if labels.len() > MAX_LABELS_JSON_BYTES {
        return Err(corrupt());
    }
    let metadata = json::encode_string_map(&record.metadata);
    if metadata.len() > MAX_METADATA_BYTES {
        return Err(corrupt());
    }
    let source = encode_provenance(&record.source);
    if source.len() > MAX_SOURCE_JSON_BYTES {
        return Err(corrupt());
    }
    Ok(EncodedRecord {
        labels,
        metadata,
        source,
        created: format_timestamp(record.created_at),
        updated: format_timestamp(record.updated_at),
        expires: record.expires_at.map(format_timestamp),
    })
}

pub fn decode_labels(raw: &str) -> Result<Vec<String>> {
    if raw.len() > MAX_LABELS_JSON_BYTES {
        return Err(corrupt());
    }
    let value = json::decode_string_slice(raw).ok_or_else(corrupt)?;
    if json::encode_string_slice(&value) != raw {
        return Err(corrupt());
    }
    Ok(value)
}

pub fn decode_metadata(raw: &str) -> Result<std::collections::BTreeMap<String, String>> {
    if raw.len() > MAX_METADATA_BYTES {
        return Err(corrupt());
    }
    let value = json::decode_string_map(raw).ok_or_else(corrupt)?;
    if json::encode_string_map(&value) != raw {
        return Err(corrupt());
    }
    Ok(value)
}

fn decode_origin(value: &str) -> Result<Option<Origin>> {
    if value.is_empty() {
        return Ok(None);
    }
    Origin::parse(value).map(Some).ok_or_else(corrupt)
}

pub fn decode_provenance(raw: &str) -> Result<Provenance> {
    if raw.len() > MAX_SOURCE_JSON_BYTES {
        return Err(corrupt());
    }
    let wire: ProvenanceWire = serde_json::from_str(raw).map_err(|_| corrupt())?;
    let mut value = Provenance {
        origin: decode_origin(&wire.origin)?,
        session_id: wire.session_id,
        message_ids: wire.message_ids,
        observation_id: wire.observation_id,
        decision_at: None,
        decision_source: decode_origin(&wire.decision_source)?,
    };
    if let Some(at) = &wire.decision_at {
        value.decision_at = Some(parse_timestamp(at)?);
    }
    if encode_provenance(&value) != raw {
        return Err(corrupt());
    }
    Ok(value)
}

pub fn format_timestamp(value: DateTime<Utc>) -> String {
    value.format(TIMESTAMP_FORMAT).to_string()
}

pub fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    if value.len() != TIMESTAMP_BYTES {
        return Err(corrupt());
    }
    let naive = NaiveDateTime::parse_from_str(value, TIMESTAMP_FORMAT).map_err(|_| corrupt())?;
    let parsed = naive.and_utc();
    if format_timestamp(parsed) != value {
        return Err(corrupt());
    }
    Ok(parsed)
}

/// The FTS `labels` column: the labels sorted and joined with newlines, so a
/// label is one token sequence and the order does not affect the index.
pub fn fts_labels(labels: &[String]) -> String {
    let mut sorted: Vec<&str> = labels.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join("\n")
}

/// A valid stored float: finite and inside `[0, 1]`.
pub fn valid_stored_float(value: f64) -> bool {
    value.is_finite() && (0.0..=1.0).contains(&value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(nanos: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 13, 1, 2, 3)
            .unwrap()
            .with_nanosecond(nanos)
            .unwrap()
    }

    use chrono::Timelike;

    #[test]
    fn timestamps_are_thirty_bytes_and_round_trip() {
        let formatted = format_timestamp(at(123_456_789));
        assert_eq!(formatted, "2026-09-13T01:02:03.123456789Z");
        assert_eq!(formatted.len(), TIMESTAMP_BYTES);
        assert_eq!(parse_timestamp(&formatted).expect("parse"), at(123_456_789));
    }

    #[test]
    fn a_zero_nanosecond_timestamp_keeps_nine_fraction_digits() {
        assert_eq!(format_timestamp(at(0)), "2026-09-13T01:02:03.000000000Z");
    }

    #[test]
    fn parsing_rejects_a_wrong_length_or_non_canonical_value() {
        assert!(parse_timestamp("2026-09-13T01:02:03Z").is_err());
        assert!(parse_timestamp("2026-09-13T01:02:03.12345678Z").is_err());
    }

    #[test]
    fn an_empty_provenance_encodes_every_field() {
        assert_eq!(
            encode_provenance(&Provenance::default()),
            r#"{"origin":"","session_id":"","message_ids":[],"observation_id":"","decision_at":null,"decision_source":""}"#
        );
    }

    #[test]
    fn provenance_round_trips_through_its_wire_form() {
        let source = Provenance {
            origin: Some(Origin::Human),
            session_id: "s1".into(),
            message_ids: vec!["m1".into(), "m2".into()],
            observation_id: "o1".into(),
            decision_at: Some(at(0)),
            decision_source: Some(Origin::Model),
        };
        let encoded = encode_provenance(&source);
        assert_eq!(decode_provenance(&encoded).expect("decode"), source);
    }

    #[test]
    fn decoding_rejects_a_non_canonical_blob() {
        assert!(decode_provenance(r#"{"origin":"human"}"#).is_err());
        assert!(decode_labels(r#"[ "a" ]"#).is_err());
        assert!(decode_metadata(r#"{"b":"2","a":"1"}"#).is_err());
    }

    #[test]
    fn fts_labels_sort_and_join_with_newlines() {
        assert_eq!(fts_labels(&["b".into(), "a".into()]), "a\nb");
        assert_eq!(fts_labels(&[]), "");
    }

    #[test]
    fn stored_floats_must_be_finite_and_within_the_unit_interval() {
        assert!(valid_stored_float(0.0) && valid_stored_float(1.0));
        assert!(!valid_stored_float(f64::NAN) && !valid_stored_float(1.5));
    }
}
