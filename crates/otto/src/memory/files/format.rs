//! The on-disk forms: a record is a Markdown file with JSON frontmatter values,
//! a tombstone and a candidate are one JSON object each.
//!
//! The writer emits the canonical encoding of [`crate::memory::codec`]. The
//! reader accepts any JSON formatting and RFC 3339 timestamps, so a hand edit
//! does not have to reproduce the canonical bytes. Reader errors are reasons
//! shown by `invalid_files`; they never contain file content.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::memory::codec::{encode_record, format_timestamp};
use crate::memory::json::{encode_float, encode_string};
use crate::memory::proposed::{decode_proposed_record, encode_proposed_record};
use crate::memory::validate::{validate_candidate, validate_record, validate_tombstone};
use crate::memory::{
    Candidate, CandidateAction, CandidateState, MAX_ID_BYTES, Origin, Provenance, Record, Result,
    Scope, Tombstone,
};

/// Larger files are not read; a record is at most 8 KiB of text.
pub(super) const MAX_FILE_BYTES: u64 = 128 * 1024;

/// The frontmatter names, in the order the writer emits them.
const NAMES: [&str; 11] = [
    "scope",
    "kind",
    "key",
    "labels",
    "confidence",
    "revision",
    "created_at",
    "updated_at",
    "expires_at",
    "source",
    "metadata",
];

/// A file stem: 1 to 64 characters from `[0-9A-Za-z_-]`.
pub(super) fn valid_file_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn parse_time(value: &str) -> std::result::Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|_| "invalid timestamp".to_string())
}

fn parse_origin(value: &str) -> std::result::Result<Option<Origin>, String> {
    if value.is_empty() {
        return Ok(None);
    }
    Origin::parse(value)
        .map(Some)
        .ok_or_else(|| "invalid origin".to_string())
}

fn origin_text(origin: Option<Origin>) -> &'static str {
    origin.map_or("", Origin::as_str)
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct SourceWire {
    origin: String,
    session_id: String,
    message_ids: Vec<String>,
    observation_id: String,
    decision_at: Option<String>,
    decision_source: String,
}

fn float_text(value: f64) -> String {
    let text = encode_float(value);
    if text.contains(['.', 'e']) {
        text
    } else {
        format!("{text}.0")
    }
}

/// The canonical file form of `record`: frontmatter, then the text and one
/// newline.
pub(super) fn encode_record_file(record: &Record) -> Result<String> {
    let encoded = encode_record(record)?;
    let mut out = String::from("---\nscope: {\"namespace\":");
    encode_string(&record.scope.namespace, &mut out);
    out.push_str(",\"id\":");
    encode_string(&record.scope.id, &mut out);
    out.push_str("}\nkind: ");
    encode_string(&record.kind, &mut out);
    out.push_str("\nkey: ");
    encode_string(&record.key, &mut out);
    out.push_str("\nlabels: ");
    out.push_str(&encoded.labels);
    out.push_str("\nconfidence: ");
    out.push_str(&float_text(record.confidence));
    out.push_str(&format!("\nrevision: {}\ncreated_at: ", record.revision));
    encode_string(&encoded.created, &mut out);
    out.push_str("\nupdated_at: ");
    encode_string(&encoded.updated, &mut out);
    out.push_str("\nexpires_at: ");
    match &encoded.expires {
        Some(expires) => encode_string(expires, &mut out),
        None => out.push_str("null"),
    }
    out.push_str("\nsource: ");
    out.push_str(&encoded.source);
    out.push_str("\nmetadata: ");
    out.push_str(&encoded.metadata);
    out.push_str("\n---\n");
    out.push_str(&record.text);
    out.push('\n');
    Ok(out)
}

fn field<T: serde::de::DeserializeOwned>(
    fields: &BTreeMap<&str, &str>,
    name: &str,
) -> std::result::Result<T, String> {
    let raw = fields
        .get(name)
        .ok_or_else(|| format!("missing frontmatter name {name}"))?;
    serde_json::from_str(raw).map_err(|_| format!("invalid value for {name}"))
}

/// Parses a record file. The record id is the file stem; the caller runs the
/// content guard.
pub(super) fn parse_record(id: &str, content: &str) -> std::result::Result<Record, String> {
    let rest = content.strip_prefix("---\n").ok_or("missing frontmatter")?;
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    let mut body = None;
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        offset += line.len();
        let line = line.strip_suffix('\n').ok_or("unterminated frontmatter")?;
        if line == "---" {
            body = Some(&rest[offset..]);
            break;
        }
        let (name, value) = line.split_once(": ").ok_or("malformed frontmatter line")?;
        if !NAMES.contains(&name) {
            return Err(format!("unknown frontmatter name {name:.32}"));
        }
        if fields.insert(name, value).is_some() {
            return Err(format!("duplicate frontmatter name {name}"));
        }
    }
    let body = body.ok_or("unterminated frontmatter")?;
    let text = body.strip_suffix('\n').unwrap_or(body);

    let source: SourceWire = field(&fields, "source")?;
    let created_at: String = field(&fields, "created_at")?;
    let updated_at: String = field(&fields, "updated_at")?;
    let expires_at: Option<String> = field(&fields, "expires_at")?;
    let record = Record {
        id: id.to_string(),
        scope: field(&fields, "scope")?,
        kind: field(&fields, "kind")?,
        key: field(&fields, "key")?,
        text: text.to_string(),
        labels: field(&fields, "labels")?,
        metadata: field(&fields, "metadata")?,
        source: Provenance {
            origin: parse_origin(&source.origin)?,
            session_id: source.session_id,
            message_ids: source.message_ids,
            observation_id: source.observation_id,
            decision_at: source.decision_at.as_deref().map(parse_time).transpose()?,
            decision_source: parse_origin(&source.decision_source)?,
        },
        confidence: field(&fields, "confidence")?,
        revision: field(&fields, "revision")?,
        created_at: parse_time(&created_at)?,
        updated_at: parse_time(&updated_at)?,
        expires_at: expires_at.as_deref().map(parse_time).transpose()?,
    };
    if !crate::memory::codec::valid_stored_float(record.confidence) {
        return Err("confidence outside [0, 1]".into());
    }
    validate_record(&record).map_err(|error| error.to_string())?;
    Ok(record)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TombstoneWire {
    id: String,
    scope: Scope,
    revision: u64,
    created_at: String,
    updated_at: String,
    forgotten_at: String,
}

pub(super) fn encode_tombstone_file(tombstone: &Tombstone) -> Result<String> {
    let wire = TombstoneWire {
        id: tombstone.id.clone(),
        scope: tombstone.scope.clone(),
        revision: tombstone.revision,
        created_at: format_timestamp(tombstone.created_at),
        updated_at: format_timestamp(tombstone.updated_at),
        forgotten_at: format_timestamp(tombstone.forgotten_at),
    };
    let mut out = serde_json::to_string_pretty(&wire)
        .map_err(|_| crate::memory::Error::new(crate::memory::ErrorKind::Corrupt))?;
    out.push('\n');
    Ok(out)
}

pub(super) fn parse_tombstone(id: &str, content: &str) -> std::result::Result<Tombstone, String> {
    let wire: TombstoneWire =
        serde_json::from_str(content).map_err(|_| "invalid tombstone JSON".to_string())?;
    if wire.id != id {
        return Err("tombstone id differs from file name".into());
    }
    let tombstone = Tombstone {
        id: wire.id,
        scope: wire.scope,
        revision: wire.revision,
        created_at: parse_time(&wire.created_at)?,
        updated_at: parse_time(&wire.updated_at)?,
        forgotten_at: parse_time(&wire.forgotten_at)?,
    };
    validate_tombstone(&tombstone).map_err(|error| error.to_string())?;
    Ok(tombstone)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateWire {
    id: String,
    action: String,
    target_id: String,
    base_revision: u64,
    reason: String,
    state: String,
    created_at: String,
    decided_at: Option<String>,
    decision_source: String,
    result_record_id: String,
    result_revision: u64,
    proposed: Box<RawValue>,
}

pub(super) fn encode_candidate_file(candidate: &Candidate) -> Result<String> {
    let corrupt = || crate::memory::Error::new(crate::memory::ErrorKind::Corrupt);
    let proposed = RawValue::from_string(encode_proposed_record(&candidate.proposed)?)
        .map_err(|_| corrupt())?;
    let wire = CandidateWire {
        id: candidate.id.clone(),
        action: candidate.action.as_str().to_string(),
        target_id: candidate.target_id.clone(),
        base_revision: candidate.base_revision,
        reason: candidate.reason.clone(),
        state: candidate.state.as_str().to_string(),
        created_at: format_timestamp(candidate.created_at),
        decided_at: candidate.decided_at.map(format_timestamp),
        decision_source: origin_text(candidate.decision_source).to_string(),
        result_record_id: candidate.result_record_id.clone(),
        result_revision: candidate.result_revision,
        proposed,
    };
    let mut out = serde_json::to_string_pretty(&wire).map_err(|_| corrupt())?;
    out.push('\n');
    Ok(out)
}

pub(super) fn parse_candidate(id: &str, content: &str) -> std::result::Result<Candidate, String> {
    let wire: CandidateWire =
        serde_json::from_str(content).map_err(|_| "invalid candidate JSON".to_string())?;
    if wire.id != id {
        return Err("candidate id differs from file name".into());
    }
    let candidate = Candidate {
        id: wire.id,
        proposed: decode_proposed_record(wire.proposed.get())
            .map_err(|_| "invalid proposed record".to_string())?,
        action: CandidateAction::parse(&wire.action).ok_or("invalid candidate action")?,
        target_id: wire.target_id,
        base_revision: wire.base_revision,
        reason: wire.reason,
        state: CandidateState::parse(&wire.state).ok_or("invalid candidate state")?,
        created_at: parse_time(&wire.created_at)?,
        decided_at: wire.decided_at.as_deref().map(parse_time).transpose()?,
        decision_source: parse_origin(&wire.decision_source)?,
        result_record_id: wire.result_record_id,
        result_revision: wire.result_revision,
    };
    validate_candidate(&candidate).map_err(|error| error.to_string())?;
    Ok(candidate)
}

/// The content of `identity.json`.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct IdentityWire {
    pub database_id: String,
    pub user_scope_id: String,
}

impl IdentityWire {
    pub(super) fn encode(&self) -> String {
        format!(
            "{{\"database_id\":{},\"user_scope_id\":{}}}\n",
            quoted(&self.database_id),
            quoted(&self.user_scope_id)
        )
    }
}

fn quoted(value: &str) -> String {
    let mut out = String::new();
    encode_string(value, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::files::testsupport::{at, sample_record};

    fn record() -> Record {
        let mut record = sample_record(
            "rec-1",
            &Scope::new("workspace", "sha256:abc"),
            "tone",
            "Reply <tersely>.",
        );
        record.revision = 3;
        record.confidence = 1.0;
        record.expires_at = Some(at(9));
        record.metadata.insert("a".into(), "b&c".into());
        record.labels = vec!["x".into(), "y".into()];
        record
    }

    #[test]
    fn a_record_file_round_trips_byte_for_byte() {
        let record = record();
        let bytes = encode_record_file(&record).expect("encode");
        assert!(bytes.contains("\nconfidence: 1.0\n"), "{bytes}");
        assert!(bytes.ends_with("---\nReply <tersely>.\n"));
        let parsed = parse_record("rec-1", &bytes).expect("parse");
        assert_eq!(parsed, record);
        assert_eq!(encode_record_file(&parsed).expect("encode"), bytes);
    }

    #[test]
    fn non_canonical_spacing_and_a_short_source_are_read() {
        let content = "---\nscope:   { \"namespace\" : \"user\", \"id\":\"u1\" }\nkind: \"preference\"\nkey: \"k\"\nlabels: [ ]\nconfidence: 1\nrevision: 2\ncreated_at: \"2026-03-01T12:00:00Z\"\nupdated_at: \"2026-03-01T13:00:00+01:00\"\nexpires_at: null\nsource: {\"origin\":\"human\"}\nmetadata: {}\n---\nhello";
        let parsed = parse_record("r1", content).expect("parse");
        assert_eq!(parsed.text, "hello");
        assert_eq!(parsed.source.origin, Some(Origin::Human));
        assert_eq!(parsed.updated_at, parsed.created_at);
        assert_eq!(parsed.scope, Scope::new("user", "u1"));
    }

    #[test]
    fn unknown_duplicate_and_missing_names_are_errors() {
        let good = encode_record_file(&record()).expect("encode");
        let unknown = good.replacen("kind:", "extra: 1\nkind:", 1);
        assert!(parse_record("r", &unknown).is_err());
        let duplicate = good.replacen("kind:", "key: \"z\"\nkind:", 1);
        assert!(parse_record("r", &duplicate).is_err());
        let missing = good.replacen("kind: \"preference\"\n", "", 1);
        assert!(parse_record("r", &missing).is_err());
        assert!(parse_record("r", "no frontmatter").is_err());
    }
}
