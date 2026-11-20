//! Additive built-in evidence metadata; the neutral plugin contract is unchanged.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{Record, Result, invalid_request};

pub const APPLICABILITY: &str = "evidence.applicability";
pub const CONDITION_CHANGED: &str = "evidence.condition_changed";
const PREFIX: &str = "evidence.item.";

/// Attribution describes evidence, not authorization or verified truth.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceProvenance {
    User,
    Tool,
    External,
    Model,
}

/// Record references are implicitly in the conclusion's scope. Observations
/// anchor an immutable event, not a monitored external resource or a copied body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Evidence {
    Record {
        id: String,
        revision: u64,
        provenance: EvidenceProvenance,
    },
    Observation {
        session_id: String,
        event_id: String,
        provenance: EvidenceProvenance,
    },
}

pub fn read(metadata: &BTreeMap<String, String>) -> Result<Vec<Evidence>> {
    if metadata
        .get(CONDITION_CHANGED)
        .is_some_and(|value| value != "true")
    {
        return Err(invalid_request("invalid condition_changed metadata"));
    }
    let mut evidence = Vec::new();
    for (key, value) in metadata {
        if key.starts_with(PREFIX) {
            let index = key.strip_prefix(PREFIX).unwrap_or_default();
            if !matches!(index, "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7") {
                return Err(invalid_request("invalid evidence anchor index"));
            }
            let item: Evidence = serde_json::from_str(value)
                .map_err(|_| invalid_request("invalid evidence metadata"))?;
            match &item {
                Evidence::Record { id, revision, .. } => {
                    if !super::files::valid_evidence_id(id) || *revision == 0 {
                        return Err(invalid_request(
                            "evidence needs a record id and exact revision",
                        ));
                    }
                }
                Evidence::Observation {
                    session_id,
                    event_id,
                    ..
                } => {
                    if !super::validate::valid_text(session_id, 128)
                        || !super::validate::valid_text(event_id, 128)
                        || session_id.trim().is_empty()
                        || event_id.trim().is_empty()
                    {
                        return Err(invalid_request("observation needs session_id and event_id"));
                    }
                }
            }
            evidence.push(item);
        }
    }
    if evidence.len() > 8 {
        return Err(invalid_request(
            "at most eight evidence anchors are supported",
        ));
    }
    // Invalidation must always be able to persist its marker without making
    // an otherwise valid record exceed the existing metadata limits.
    if !evidence.is_empty() && !metadata.contains_key(CONDITION_CHANGED) {
        let mut marked = metadata.clone();
        marked.insert(CONDITION_CHANGED.into(), "true".into());
        if marked.len() > super::MAX_METADATA_ENTRIES
            || super::json::encode_string_map(&marked).len() > super::MAX_METADATA_BYTES
        {
            return Err(invalid_request(
                "evidence requires metadata space for its review marker",
            ));
        }
    }
    if !evidence.is_empty()
        && metadata
            .get(APPLICABILITY)
            .is_none_or(|text| text.trim().is_empty())
    {
        return Err(invalid_request("evidence requires applicability text"));
    }
    Ok(evidence)
}

/// Replace bindings explicitly, retaining unrelated metadata. The old anchors
/// are never silently advanced when their source changes.
pub fn bind(
    metadata: &mut BTreeMap<String, String>,
    evidence: &[Evidence],
    applicability: &str,
    condition_changed: bool,
) -> Result<()> {
    metadata.retain(|key, _| !key.starts_with(PREFIX));
    metadata.insert(APPLICABILITY.into(), applicability.into());
    metadata.remove(CONDITION_CHANGED);
    if condition_changed {
        metadata.insert(CONDITION_CHANGED.into(), "true".into());
    }
    for (index, item) in evidence.iter().enumerate() {
        let value = serde_json::to_string(item).map_err(|_| invalid_request("invalid evidence"))?;
        metadata.insert(format!("{PREFIX}{index}"), value);
    }
    read(metadata)?;
    Ok(())
}

/// JSON schema shared by model-visible built-in remember tools.
pub fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "array", "maxItems": 8, "description": "Exact same-scope record revisions or immutable observation events; attribution is not authority",
        "items": {"oneOf": [
            {"type": "object", "additionalProperties": false,
             "properties": {"type": {"const": "record"}, "id": {"type": "string"}, "revision": {"type": "integer", "minimum": 1},
                            "provenance": {"enum": ["user", "tool", "external", "model"]}},
             "required": ["type", "id", "revision", "provenance"]},
            {"type": "object", "additionalProperties": false,
             "properties": {"type": {"const": "observation"}, "session_id": {"type": "string"}, "event_id": {"type": "string"},
                            "provenance": {"enum": ["user", "tool", "external", "model"]}},
             "required": ["type", "session_id", "event_id", "provenance"]}
        ]}
    })
}

pub(crate) fn condition_changed(record: &Record) -> bool {
    record
        .metadata
        .get(CONDITION_CHANGED)
        .is_some_and(|value| value == "true")
}
