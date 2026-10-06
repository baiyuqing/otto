//! Versioned `otto.turn` records: the start and the end of each agent turn.
//!
//! A turn whose start has no matching end was interrupted by process loss:
//! a turn that fails or is cancelled still writes its end. The records never
//! enter model context.

use serde::{Deserialize, Serialize};

pub const TURN_CUSTOM_TYPE: &str = "otto.turn";
pub const TURN_SCHEMA_VERSION: u32 = 1;

/// One turn lifecycle record. Wire field names are pinned camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum TurnRecord {
    Started {
        #[serde(rename = "schemaVersion")]
        schema_version: u32,
        #[serde(rename = "turnId")]
        turn_id: String,
        trigger: TurnTrigger,
        /// Turns in one recovery chain: 1 unless this turn resumes an
        /// interrupted one, then the interrupted turn's attempt + 1.
        attempt: u32,
        /// The interrupted turn this one resumes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resumes: Option<String>,
    },
    Ended {
        #[serde(rename = "schemaVersion")]
        schema_version: u32,
        #[serde(rename = "turnId")]
        turn_id: String,
        outcome: TurnOutcome,
    },
}

/// What started a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnTrigger {
    /// A user prompt or an agent follow-up.
    User,
    /// An empty-text turn delivering queued notifications.
    Wake,
    /// A wake turn that resumes a turn process loss interrupted.
    Recovery,
}

/// How a turn ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Failed,
    Cancelled,
}

/// The newest started turn with no matching end on the active path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedTurn {
    pub turn_id: String,
    pub attempt: u32,
}

impl TurnRecord {
    pub fn started(
        turn_id: impl Into<String>,
        trigger: TurnTrigger,
        attempt: u32,
        resumes: Option<String>,
    ) -> Self {
        Self::Started {
            schema_version: TURN_SCHEMA_VERSION,
            turn_id: turn_id.into(),
            trigger,
            attempt,
            resumes,
        }
    }

    pub fn ended(turn_id: impl Into<String>, outcome: TurnOutcome) -> Self {
        Self::Ended {
            schema_version: TURN_SCHEMA_VERSION,
            turn_id: turn_id.into(),
            outcome,
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Started {
                schema_version,
                turn_id,
                attempt,
                ..
            } => *schema_version == TURN_SCHEMA_VERSION && !turn_id.is_empty() && *attempt >= 1,
            Self::Ended {
                schema_version,
                turn_id,
                ..
            } => *schema_version == TURN_SCHEMA_VERSION && !turn_id.is_empty(),
        }
    }
}

/// Encodes a record as `otto.turn` data.
pub fn encode_turn_record(record: &TurnRecord) -> String {
    serde_json::to_string(record).expect("turn record serializes")
}

/// Decodes `otto.turn` data; `None` for a malformed record or another schema
/// version. Unknown fields are ignored.
pub fn decode_turn_record(data: &str) -> Option<TurnRecord> {
    serde_json::from_str::<TurnRecord>(data)
        .ok()
        .filter(TurnRecord::valid)
}

/// Folds one record into the open turn: a start opens it, the matching end
/// closes it, and an end for another turn changes nothing.
pub fn apply_turn_record(open: &mut Option<InterruptedTurn>, record: &TurnRecord) {
    match record {
        TurnRecord::Started {
            turn_id, attempt, ..
        } => {
            *open = Some(InterruptedTurn {
                turn_id: turn_id.clone(),
                attempt: *attempt,
            });
        }
        TurnRecord::Ended { turn_id, .. } => {
            if open.as_ref().is_some_and(|turn| &turn.turn_id == turn_id) {
                *open = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_started_and_ended_json() {
        assert_eq!(
            encode_turn_record(&TurnRecord::started("t1", TurnTrigger::User, 1, None)),
            r#"{"event":"started","schemaVersion":1,"turnId":"t1","trigger":"user","attempt":1}"#
        );
        assert_eq!(
            encode_turn_record(&TurnRecord::started(
                "t2",
                TurnTrigger::Recovery,
                2,
                Some("t1".into())
            )),
            r#"{"event":"started","schemaVersion":1,"turnId":"t2","trigger":"recovery","attempt":2,"resumes":"t1"}"#
        );
        assert_eq!(
            encode_turn_record(&TurnRecord::ended("t1", TurnOutcome::Cancelled)),
            r#"{"event":"ended","schemaVersion":1,"turnId":"t1","outcome":"cancelled"}"#
        );
    }

    #[test]
    fn malformed_and_future_records_are_rejected() {
        assert_eq!(decode_turn_record("{}"), None);
        assert_eq!(
            decode_turn_record(
                r#"{"event":"started","schemaVersion":2,"turnId":"t","trigger":"user","attempt":1}"#
            ),
            None
        );
        assert_eq!(
            decode_turn_record(
                r#"{"event":"started","schemaVersion":1,"turnId":"t","trigger":"user","attempt":0}"#
            ),
            None
        );
        assert_eq!(
            decode_turn_record(
                r#"{"event":"ended","schemaVersion":1,"turnId":"","outcome":"failed"}"#
            ),
            None
        );
    }

    #[test]
    fn only_a_start_without_its_end_stays_open() {
        let mut open = None;
        apply_turn_record(
            &mut open,
            &TurnRecord::started("t1", TurnTrigger::User, 1, None),
        );
        apply_turn_record(&mut open, &TurnRecord::ended("other", TurnOutcome::Failed));
        assert_eq!(
            open,
            Some(InterruptedTurn {
                turn_id: "t1".into(),
                attempt: 1
            })
        );
        apply_turn_record(&mut open, &TurnRecord::ended("t1", TurnOutcome::Failed));
        assert_eq!(open, None);
    }
}
