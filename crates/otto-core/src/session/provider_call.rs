//! Versioned `otto.provider_call` records: timing and content-free request
//! facts for one provider operation.
//!
//! The record holds no prompt, message, tool argument or result text. The
//! trace builder reads it to place a provider request on a time axis.

use serde::{Deserialize, Serialize};

use crate::model::{FinishReason, OperationOutcome, Usage};

pub const PROVIDER_CALL_CUSTOM_TYPE: &str = "otto.provider_call";
pub const PROVIDER_CALL_SCHEMA_VERSION: u32 = 1;

/// The logical work one provider operation performed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderCallKind {
    Ordinary,
    LocalCompaction,
    RemoteCompaction,
}

/// One provider retry. `at_ms` is the time from the operation start to the
/// moment the retry was announced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderRetryRecord {
    pub attempt: u32,
    pub max_attempts: u32,
    pub delay_ms: u64,
    pub reason: String,
    pub interrupted_output: bool,
    pub at_ms: u64,
}

/// Token estimates of the parts of a request, from the estimators `/context`
/// uses, and the ids of the memory records recalled for the turn.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestEstimate {
    pub system_prompt_tokens: i64,
    pub tool_tokens: i64,
    pub message_tokens: i64,
    pub memory_tokens: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_record_ids: Vec<String>,
}

/// The payload of an `otto.provider_call` entry. Wire field names are pinned
/// camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderCallRecord {
    pub schema_version: u32,
    pub operation_id: String,
    pub kind: ProviderCallKind,
    /// Unix milliseconds at the start of the operation.
    pub started_at: i64,
    pub duration_ms: u64,
    /// Milliseconds from the start to the first reasoning, text or tool call
    /// delta of the attempt that settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_output_ms: Option<u64>,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retries: Vec<ProviderRetryRecord>,
    #[serde(flatten)]
    pub outcome: OperationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestEstimate>,
}

/// Decodes `otto.provider_call` data; `None` for malformed data or another
/// schema version.
pub fn decode_provider_call(data: &str) -> Option<ProviderCallRecord> {
    serde_json::from_str::<ProviderCallRecord>(data)
        .ok()
        .filter(|record| record.schema_version == PROVIDER_CALL_SCHEMA_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EffectCertainty, OperationDisposition};

    fn record() -> ProviderCallRecord {
        ProviderCallRecord {
            schema_version: PROVIDER_CALL_SCHEMA_VERSION,
            operation_id: "op_1".into(),
            kind: ProviderCallKind::LocalCompaction,
            started_at: 1_000,
            duration_ms: 250,
            first_output_ms: Some(40),
            attempts: 2,
            retries: vec![ProviderRetryRecord {
                attempt: 2,
                max_attempts: 3,
                delay_ms: 100,
                reason: "timeout".into(),
                interrupted_output: false,
                at_ms: 90,
            }],
            outcome: OperationOutcome {
                disposition: OperationDisposition::Succeeded,
                effect_certainty: EffectCertainty::Completed,
                stop_reason: None,
            },
            finish_reason: Some(FinishReason::Stop),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 0,
                ..Default::default()
            }),
            request: Some(RequestEstimate {
                system_prompt_tokens: 1,
                tool_tokens: 2,
                message_tokens: 3,
                memory_tokens: 4,
                memory_record_ids: vec!["m1".into()],
            }),
        }
    }

    #[test]
    fn record_round_trips_with_camel_case_fields() {
        let record = record();
        let json = serde_json::to_string(&record).expect("encode");
        let value: serde_json::Value = serde_json::from_str(&json).expect("json");
        for key in [
            "schemaVersion",
            "operationId",
            "startedAt",
            "durationMs",
            "firstOutputMs",
            "finishReason",
            "disposition",
            "effectCertainty",
        ] {
            assert!(value.get(key).is_some(), "missing {key} in {json}");
        }
        assert_eq!(value["kind"], "local_compaction");
        assert_eq!(value["retries"][0]["atMs"], 90);
        assert_eq!(value["request"]["memoryRecordIds"][0], "m1");
        assert_eq!(decode_provider_call(&json), Some(record));
    }

    #[test]
    fn empty_optional_fields_are_omitted_and_other_schemas_rejected() {
        let mut record = record();
        record.first_output_ms = None;
        record.retries.clear();
        record.usage = None;
        record.finish_reason = None;
        record.request = None;
        let json = serde_json::to_string(&record).expect("encode");
        for key in [
            "firstOutputMs",
            "retries",
            "usage",
            "finishReason",
            "request",
        ] {
            assert!(!json.contains(key), "{key} present in {json}");
        }
        let future = json.replace(r#""schemaVersion":1"#, r#""schemaVersion":2"#);
        assert_eq!(decode_provider_call(&future), None);
    }
}
