//! Logical operation identity, dispositions, stop reasons and outcomes, and the unset-time marker.

use super::*;

/// Opaque identity shared by every attempt of one logical operation.
///
/// The wire value is 1 to 128 ASCII bytes and may contain only alphanumeric
/// characters plus `.`, `_`, `:`, and `-`. Consumers must not parse meaning
/// from the identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(String);

impl OperationId {
    pub fn new(value: impl Into<String>) -> Result<Self, ValidationError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 128
            || bytes.iter().any(|byte| {
                !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'_' | b':' | b'-')
            })
        {
            return Err(ValidationError(
                "operation id must be 1 to 128 permitted ASCII bytes",
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for OperationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// What the operation's caller observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationDisposition {
    Succeeded,
    Error,
    Cancelled,
    DeadlineExceeded,
    Interrupted,
}

/// What Otto can prove about externally visible effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectCertainty {
    NotStarted,
    KnownNoEffect,
    Completed,
    Unknown,
}

/// Why execution stopped when that reason must cross an API boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStopReason {
    UserCancellation,
    Deadline,
    Shutdown,
    Migration,
    TransportLost,
    ProcessLost,
}

/// Typed operation outcome attached to a tool result.
///
/// `operation_id` is absent for legacy history whose logical operation was
/// never recorded. The remaining fields still make the interruption outcome
/// machine-readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultMetadata {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    pub disposition: OperationDisposition,
    pub effect_certainty: EffectCertainty,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<OperationStopReason>,
}

impl ToolResultMetadata {
    pub fn validate(&self) -> Result<(), ValidationError> {
        OperationOutcome {
            disposition: self.disposition,
            effect_certainty: self.effect_certainty,
            stop_reason: self.stop_reason,
        }
        .validate()
    }
}

/// The implementation-owned safety of repeating an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrySafety {
    ReadOnly,
    Idempotent,
    IdempotentWithKey,
    NonIdempotent,
}

/// Durable terminal classification, intentionally excluding output and errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationOutcome {
    pub disposition: OperationDisposition,
    pub effect_certainty: EffectCertainty,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<OperationStopReason>,
}

impl OperationOutcome {
    /// Rejects combinations whose fields make contradictory claims.
    pub fn validate(&self) -> Result<(), ValidationError> {
        use EffectCertainty::{Completed, NotStarted};
        use OperationDisposition::{Cancelled, DeadlineExceeded, Error, Interrupted, Succeeded};
        use OperationStopReason::{
            Deadline, Migration, ProcessLost, Shutdown, TransportLost, UserCancellation,
        };

        if self.disposition == Succeeded && self.effect_certainty != Completed {
            return Err(ValidationError(
                "successful operation must have completed effects",
            ));
        }
        if self.effect_certainty == Completed && !matches!(self.disposition, Succeeded | Error) {
            return Err(ValidationError(
                "completed effects require a definitive disposition",
            ));
        }
        if self.effect_certainty == NotStarted && self.disposition == Succeeded {
            return Err(ValidationError("an unstarted operation cannot succeed"));
        }
        let valid_reason = match self.stop_reason {
            None => true,
            Some(UserCancellation) => self.disposition == Cancelled,
            Some(Deadline) => self.disposition == DeadlineExceeded,
            Some(Shutdown | Migration | TransportLost | ProcessLost) => {
                self.disposition == Interrupted
            }
        };
        if !valid_reason {
            return Err(ValidationError(
                "operation disposition and stop reason are inconsistent",
            ));
        }
        Ok(())
    }
}

/// The timestamp stored sessions carry for an unset time,
/// `0001-01-01T00:00:00Z`.
///
/// The agent loop uses it as the "not set" marker for [`Message::created_at`].
pub fn zero_time() -> DateTime<Utc> {
    NaiveDate::from_ymd_opt(1, 1, 1)
        .expect("0001-01-01 is a valid date")
        .and_time(NaiveTime::MIN)
        .and_utc()
}
