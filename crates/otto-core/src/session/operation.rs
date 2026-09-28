//! Versioned, content-free operation facts and their append-only reducer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::model::{OperationId, OperationOutcome, ValidationError};

pub const OPERATION_CUSTOM_TYPE: &str = "otto.operation";
pub const OPERATION_SCHEMA_VERSION: u32 = 1;

/// One durable lifecycle fact. Wire field names are pinned camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum OperationFact {
    Attempt {
        #[serde(rename = "schemaVersion")]
        schema_version: u32,
        #[serde(rename = "operationId")]
        operation_id: OperationId,
        attempt: u32,
        kind: OperationKind,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
    },
    Terminal {
        #[serde(rename = "schemaVersion")]
        schema_version: u32,
        #[serde(rename = "operationId")]
        operation_id: OperationId,
        attempt: u32,
        kind: OperationKind,
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        #[serde(rename = "toolName")]
        tool_name: String,
        #[serde(flatten)]
        outcome: OperationOutcome,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    ToolCall,
}

impl OperationFact {
    pub fn attempt(
        operation_id: OperationId,
        attempt: u32,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
    ) -> Self {
        Self::Attempt {
            schema_version: OPERATION_SCHEMA_VERSION,
            operation_id,
            attempt,
            kind: OperationKind::ToolCall,
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
        }
    }

    pub fn terminal(
        operation_id: OperationId,
        attempt: u32,
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        outcome: OperationOutcome,
    ) -> Self {
        Self::Terminal {
            schema_version: OPERATION_SCHEMA_VERSION,
            operation_id,
            attempt,
            kind: OperationKind::ToolCall,
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            outcome,
        }
    }

    pub fn operation_id(&self) -> &OperationId {
        match self {
            Self::Attempt { operation_id, .. } | Self::Terminal { operation_id, .. } => {
                operation_id
            }
        }
    }

    pub fn attempt_number(&self) -> u32 {
        match self {
            Self::Attempt { attempt, .. } | Self::Terminal { attempt, .. } => *attempt,
        }
    }

    pub fn tool_call_id(&self) -> &str {
        match self {
            Self::Attempt { tool_call_id, .. } | Self::Terminal { tool_call_id, .. } => {
                tool_call_id
            }
        }
    }

    pub fn tool_name(&self) -> &str {
        match self {
            Self::Attempt { tool_name, .. } | Self::Terminal { tool_name, .. } => tool_name,
        }
    }

    pub fn outcome(&self) -> Option<&OperationOutcome> {
        match self {
            Self::Attempt { .. } => None,
            Self::Terminal { outcome, .. } => Some(outcome),
        }
    }

    pub fn validate(&self) -> Result<(), OperationFactError> {
        let schema_version = match self {
            Self::Attempt { schema_version, .. } | Self::Terminal { schema_version, .. } => {
                *schema_version
            }
        };
        if schema_version != OPERATION_SCHEMA_VERSION {
            return Err(OperationFactError::UnsupportedSchema(schema_version));
        }
        if self.attempt_number() == 0 {
            return Err(OperationFactError::Invalid(
                "operation attempt must be at least 1",
            ));
        }
        if self.tool_call_id().trim().is_empty() {
            return Err(OperationFactError::Invalid(
                "operation tool call id is required",
            ));
        }
        if self.tool_name().trim().is_empty() {
            return Err(OperationFactError::Invalid(
                "operation tool name is required",
            ));
        }
        if let Some(outcome) = self.outcome() {
            outcome.validate().map_err(OperationFactError::Validation)?;
        }
        Ok(())
    }
}

/// Result of decoding an `otto.operation` payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedOperationFact {
    Fact(OperationFact),
    /// A future schema is intentionally opaque and ignored by v1 readers.
    Unsupported {
        schema_version: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OperationFactError {
    #[error("malformed otto.operation v1 fact: {0}")]
    Malformed(String),
    #[error("unsupported otto.operation schema version {0}")]
    UnsupportedSchema(u32),
    #[error("invalid otto.operation fact: {0}")]
    Invalid(&'static str),
    #[error(transparent)]
    Validation(ValidationError),
    #[error("operation history conflict: {0}")]
    Conflict(&'static str),
}

pub fn encode_operation_fact(fact: &OperationFact) -> Result<String, OperationFactError> {
    fact.validate()?;
    serde_json::to_string(fact).map_err(|error| OperationFactError::Malformed(error.to_string()))
}

pub fn decode_operation_fact(raw: &str) -> Result<DecodedOperationFact, OperationFactError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|error| OperationFactError::Malformed(error.to_string()))?;
    let schema = value
        .get("schemaVersion")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| OperationFactError::Malformed("schemaVersion is required".into()))?;
    if schema != u64::from(OPERATION_SCHEMA_VERSION) {
        return Ok(DecodedOperationFact::Unsupported {
            schema_version: schema,
        });
    }
    let fact: OperationFact = serde_json::from_value(value)
        .map_err(|error| OperationFactError::Malformed(error.to_string()))?;
    fact.validate()?;
    Ok(DecodedOperationFact::Fact(fact))
}

pub fn decode_operation_raw(
    raw: Option<&RawValue>,
) -> Result<DecodedOperationFact, OperationFactError> {
    let raw = raw.ok_or_else(|| OperationFactError::Malformed("data is required".into()))?;
    decode_operation_fact(raw.get())
}

/// Folded state for one logical operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRecord {
    pub operation_id: OperationId,
    pub tool_call_id: String,
    pub tool_name: String,
    pub attempts: u32,
    pub terminal: Option<OperationOutcome>,
    pub corrupt: bool,
}

/// Active history, indexed both by operation and by session-unique tool call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OperationLedger {
    operations: BTreeMap<OperationId, OperationRecord>,
    tool_calls: BTreeMap<String, OperationId>,
    warnings: Vec<String>,
}

impl OperationLedger {
    pub fn operation(&self, id: &OperationId) -> Option<&OperationRecord> {
        self.operations.get(id)
    }

    pub fn operation_for_tool_call(&self, tool_call_id: &str) -> Option<&OperationRecord> {
        self.tool_calls
            .get(tool_call_id)
            .and_then(|id| self.operations.get(id))
    }

    pub fn operations(&self) -> impl Iterator<Item = &OperationRecord> {
        self.operations.values()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Strict append validation. On error, the ledger is unchanged.
    pub fn apply(&mut self, fact: OperationFact) -> Result<(), OperationFactError> {
        fact.validate()?;
        self.check(&fact)?;
        self.commit(fact);
        Ok(())
    }

    /// Tolerant disk-history fold. Conflicts mark the relevant operation
    /// corrupt and become warnings; the first valid terminal remains final.
    pub fn fold_history<I>(&mut self, facts: I)
    where
        I: IntoIterator<Item = OperationFact>,
    {
        for fact in facts {
            if let Err(error) = self.apply(fact.clone()) {
                let corrupt_id = self
                    .operations
                    .contains_key(fact.operation_id())
                    .then(|| fact.operation_id().clone())
                    .or_else(|| self.tool_calls.get(fact.tool_call_id()).cloned());
                if let Some(id) = corrupt_id
                    && let Some(record) = self.operations.get_mut(&id)
                {
                    record.corrupt = true;
                }
                self.warnings.push(error.to_string());
            }
        }
    }

    fn check(&self, fact: &OperationFact) -> Result<(), OperationFactError> {
        if let Some(existing_id) = self.tool_calls.get(fact.tool_call_id())
            && existing_id != fact.operation_id()
        {
            return Err(OperationFactError::Conflict(
                "tool call is linked to a different operation",
            ));
        }
        let Some(record) = self.operations.get(fact.operation_id()) else {
            if fact.outcome().is_some() {
                return Err(OperationFactError::Conflict(
                    "terminal fact references a missing attempt",
                ));
            }
            if fact.attempt_number() != 1 {
                return Err(OperationFactError::Conflict(
                    "first operation attempt must be 1",
                ));
            }
            return Ok(());
        };
        if record.tool_call_id != fact.tool_call_id() || record.tool_name != fact.tool_name() {
            return Err(OperationFactError::Conflict(
                "operation link changed tool call or tool name",
            ));
        }
        match fact {
            OperationFact::Attempt { attempt, .. } => {
                if record.terminal.is_some() {
                    return Err(OperationFactError::Conflict(
                        "attempt follows terminal fact",
                    ));
                }
                if *attempt != record.attempts + 1 {
                    return Err(OperationFactError::Conflict(
                        "operation attempts must increase by one",
                    ));
                }
            }
            OperationFact::Terminal {
                attempt, outcome, ..
            } => {
                if *attempt != record.attempts {
                    return Err(OperationFactError::Conflict(
                        "terminal fact must reference the latest existing attempt",
                    ));
                }
                if let Some(existing) = &record.terminal
                    && existing != outcome
                {
                    return Err(OperationFactError::Conflict("conflicting terminal fact"));
                }
            }
        }
        Ok(())
    }

    fn commit(&mut self, fact: OperationFact) {
        match fact {
            OperationFact::Attempt {
                operation_id,
                attempt,
                tool_call_id,
                tool_name,
                ..
            } => {
                self.tool_calls
                    .entry(tool_call_id.clone())
                    .or_insert_with(|| operation_id.clone());
                self.operations
                    .entry(operation_id.clone())
                    .and_modify(|record| record.attempts = attempt)
                    .or_insert(OperationRecord {
                        operation_id,
                        tool_call_id,
                        tool_name,
                        attempts: attempt,
                        terminal: None,
                        corrupt: false,
                    });
            }
            OperationFact::Terminal {
                operation_id,
                outcome,
                ..
            } => {
                let record = self
                    .operations
                    .get_mut(&operation_id)
                    .expect("terminal was checked against an attempt");
                if record.terminal.is_none() {
                    record.terminal = Some(outcome);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{EffectCertainty, OperationDisposition};

    fn id(value: &str) -> OperationId {
        OperationId::new(value).expect("valid id")
    }

    fn success() -> OperationOutcome {
        OperationOutcome {
            disposition: OperationDisposition::Succeeded,
            effect_certainty: EffectCertainty::Completed,
            stop_reason: None,
        }
    }

    #[test]
    fn exact_attempt_and_terminal_json() {
        let attempt = OperationFact::attempt(id("op_1"), 1, "call-1", "read");
        assert_eq!(
            encode_operation_fact(&attempt).expect("encode"),
            r#"{"event":"attempt","schemaVersion":1,"operationId":"op_1","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read"}"#
        );
        let terminal = OperationFact::terminal(id("op_1"), 1, "call-1", "read", success());
        assert_eq!(
            encode_operation_fact(&terminal).expect("encode"),
            r#"{"event":"terminal","schemaVersion":1,"operationId":"op_1","attempt":1,"kind":"tool_call","toolCallId":"call-1","toolName":"read","disposition":"succeeded","effectCertainty":"completed"}"#
        );
    }

    #[test]
    fn malformed_known_schema_errors_and_unknown_schema_is_opaque() {
        assert!(decode_operation_fact(r#"{"schemaVersion":1,"event":"attempt"}"#).is_err());
        assert_eq!(
            decode_operation_fact(r#"{"schemaVersion":2,"anything":{"future":true}}"#)
                .expect("future schema"),
            DecodedOperationFact::Unsupported { schema_version: 2 }
        );
    }

    #[test]
    fn strict_fold_checks_links_attempts_and_terminal_idempotency() {
        let mut ledger = OperationLedger::default();
        let first = OperationFact::attempt(id("op_1"), 1, "call-1", "read");
        ledger.apply(first).expect("first attempt");
        assert!(
            ledger
                .apply(OperationFact::attempt(id("op_1"), 3, "call-1", "read"))
                .is_err()
        );
        let terminal = OperationFact::terminal(id("op_1"), 1, "call-1", "read", success());
        ledger.apply(terminal.clone()).expect("terminal");
        ledger.apply(terminal).expect("identical terminal no-op");
        assert!(
            ledger
                .apply(OperationFact::attempt(id("op_1"), 2, "call-1", "read"))
                .is_err()
        );
        assert!(
            ledger
                .apply(OperationFact::attempt(id("op_2"), 1, "call-1", "read"))
                .is_err()
        );
    }

    #[test]
    fn tolerant_fold_marks_conflicting_terminal_corrupt() {
        let mut ledger = OperationLedger::default();
        let mut conflict = success();
        conflict.disposition = OperationDisposition::Error;
        ledger.fold_history([
            OperationFact::attempt(id("op_1"), 1, "call-1", "read"),
            OperationFact::terminal(id("op_1"), 1, "call-1", "read", success()),
            OperationFact::terminal(id("op_1"), 1, "call-1", "read", conflict),
        ]);
        let record = ledger.operation(&id("op_1")).expect("record");
        assert!(record.corrupt);
        assert_eq!(record.terminal, Some(success()));
        assert_eq!(ledger.warnings().len(), 1);
    }
}
