//! Forking a session: which entries a fork keeps, and the `otto.fork` record
//! that names where the copy came from.
//!
//! A fork copies the source's active path up to a turn boundary. Cutting
//! anywhere else would leave an `otto.turn` start without its end (which
//! reopening resumes on its own) or a tool call without its result. The fork
//! point is the turn-opening user message the copy stops before; that
//! message's text comes back as the prompt a client can offer for editing.
//!
//! Nothing here touches the filesystem, the clock, or randomness.

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::model::{BlockType, Message, Role};

use super::PiError;
use super::context::{active_context_path, index_context_entries, pi_entry_to_context_messages};
use super::pi::PiEntry;
use super::turn::{TURN_CUSTOM_TYPE, TurnRecord, TurnTrigger, decode_turn_record};

pub const FORK_CUSTOM_TYPE: &str = "otto.fork";
pub const FORK_SCHEMA_VERSION: u32 = 1;

/// The `otto.fork` record a fork appends after the copied entries. Wire field
/// names are pinned camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForkRecord {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "sourceSessionId")]
    pub source_session_id: String,
    /// The user message the fork stops before; absent for a fork of the
    /// whole history.
    #[serde(
        rename = "sourceEntryId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub source_entry_id: Option<String>,
    /// The text of that message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Present when an HTTP fork command supplied a stable retry identity.
    #[serde(rename = "requestId", default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
}

/// Encodes a record as `otto.fork` data.
pub fn encode_fork_record(record: &ForkRecord) -> String {
    serde_json::to_string(record).expect("fork record serializes")
}

/// Decodes `otto.fork` data; `None` for a malformed record or another schema
/// version. Unknown fields are ignored.
pub fn decode_fork_record(data: &str) -> Option<ForkRecord> {
    serde_json::from_str::<ForkRecord>(data)
        .ok()
        .filter(|record| {
            record.schema_version == FORK_SCHEMA_VERSION && !record.source_session_id.is_empty()
        })
}

/// The latest valid `otto.fork` record among `entries`.
pub fn fork_origin(entries: &[PiEntry]) -> Option<ForkRecord> {
    entries.iter().rev().find_map(|entry| {
        let custom = entry.custom.as_ref()?;
        if entry.type_name != "custom" || custom.custom_type != FORK_CUSTOM_TYPE {
            return None;
        }
        decode_fork_record(
            custom
                .data
                .as_deref()
                .map(RawValue::get)
                .unwrap_or_default(),
        )
    })
}

/// One place a session can be forked: before the turn the user message
/// `entry_id` opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPoint {
    pub entry_id: String,
    pub prompt: String,
    /// How many active-path entries a fork here keeps.
    kept: usize,
}

/// What a fork copies: the kept active-path entries, in path order, and the
/// fork point it stops before, if any.
#[derive(Debug, Clone)]
pub struct ForkPlan {
    pub entries: Vec<PiEntry>,
    pub point: Option<ForkPoint>,
}

#[derive(Debug)]
pub enum ForkError {
    /// The id names no turn-opening user message on the active path.
    InvalidPoint(String),
    /// The source cannot be resolved.
    Session(PiError),
}

impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPoint(id) => write!(
                f,
                "{id} is not a user prompt that starts a turn on this session's history"
            ),
            Self::Session(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ForkError {}

impl From<PiError> for ForkError {
    fn from(error: PiError) -> Self {
        Self::Session(error)
    }
}

/// Every fork point on the active path ending at `leaf_id`, oldest first.
pub fn fork_points(entries: &[PiEntry], leaf_id: &str) -> Result<Vec<ForkPoint>, PiError> {
    Ok(scan(&path(entries, leaf_id)?)?.points)
}

/// What a fork before `before` keeps; with no `before`, everything up to the
/// last turn boundary, so a running or interrupted turn is left out.
pub fn plan_fork(
    entries: &[PiEntry],
    leaf_id: &str,
    before: Option<&str>,
) -> Result<ForkPlan, ForkError> {
    let mut path = path(entries, leaf_id)?;
    let scan = scan(&path)?;
    let (kept, point) = match before {
        None => (scan.last_boundary, None),
        Some(id) => {
            let point = scan
                .points
                .into_iter()
                .find(|point| point.entry_id == id)
                .ok_or_else(|| ForkError::InvalidPoint(id.to_owned()))?;
            (point.kept, Some(point))
        }
    };
    path.truncate(kept);
    Ok(ForkPlan {
        entries: path,
        point,
    })
}

fn path(entries: &[PiEntry], leaf_id: &str) -> Result<Vec<PiEntry>, PiError> {
    let (index, _) = index_context_entries(entries)?;
    active_context_path(entries, leaf_id, &index)
}

struct Scan {
    points: Vec<ForkPoint>,
    /// The longest prefix that ends outside every turn with every tool call
    /// answered.
    last_boundary: usize,
}

/// A turn opened by an `otto.turn` start that has not ended yet.
struct OpenTurn {
    id: String,
    start: usize,
    /// Whether the turn's first prompt may still be a fork point: a user turn
    /// before any assistant reply.
    awaiting_prompt: bool,
}

fn scan(path: &[PiEntry]) -> Result<Scan, PiError> {
    let mut points = Vec::new();
    let mut open: Option<OpenTurn> = None;
    let mut pending_calls: HashSet<String> = HashSet::new();
    let mut last_boundary = 0;
    for (position, entry) in path.iter().enumerate() {
        if open.is_none() && pending_calls.is_empty() {
            last_boundary = position;
        }
        if let Some(record) = turn_record(entry) {
            match record {
                TurnRecord::Started {
                    turn_id, trigger, ..
                } => {
                    open = Some(OpenTurn {
                        id: turn_id,
                        start: position,
                        awaiting_prompt: trigger == TurnTrigger::User,
                    });
                }
                TurnRecord::Ended { turn_id, .. } => {
                    if open.as_ref().is_some_and(|turn| turn.id == turn_id) {
                        open = None;
                    }
                }
            }
            continue;
        }
        if entry.type_name != "message" {
            continue;
        }
        for message in pi_entry_to_context_messages(entry)? {
            if is_prompt(&message) {
                match open.as_mut() {
                    Some(turn) if turn.awaiting_prompt => {
                        turn.awaiting_prompt = false;
                        points.push(ForkPoint {
                            entry_id: entry.id.clone(),
                            prompt: message.text(),
                            kept: turn.start,
                        });
                    }
                    // Steering, or a prompt inside a wake turn.
                    Some(_) => {}
                    // A session written before turn records.
                    None if pending_calls.is_empty() => points.push(ForkPoint {
                        entry_id: entry.id.clone(),
                        prompt: message.text(),
                        kept: position,
                    }),
                    None => {}
                }
            }
            if message.role == Role::Assistant
                && let Some(turn) = open.as_mut()
            {
                turn.awaiting_prompt = false;
            }
            for block in &message.blocks {
                match block.block_type {
                    BlockType::ToolCall => {
                        pending_calls.insert(block.tool_call_id.clone());
                    }
                    BlockType::ToolResult => {
                        pending_calls.remove(&block.tool_call_id);
                    }
                    _ => {}
                }
            }
        }
    }
    if open.is_none() && pending_calls.is_empty() {
        last_boundary = path.len();
    }
    Ok(Scan {
        points,
        last_boundary,
    })
}

fn turn_record(entry: &PiEntry) -> Option<TurnRecord> {
    let custom = entry.custom.as_ref()?;
    if entry.type_name != "custom" || custom.custom_type != TURN_CUSTOM_TYPE {
        return None;
    }
    decode_turn_record(
        custom
            .data
            .as_deref()
            .map(RawValue::get)
            .unwrap_or_default(),
    )
}

/// A prompt the user typed: a user message that is not injected context.
fn is_prompt(message: &Message) -> bool {
    message.role == Role::User && message.context_type.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::codec::decode_pi_entry;
    use crate::session::turn::{TurnOutcome, encode_turn_record};
    use serde_json::{Value, json};

    struct Builder {
        entries: Vec<PiEntry>,
    }

    impl Builder {
        fn new() -> Self {
            Self {
                entries: Vec::new(),
            }
        }

        /// Appends one entry decoded from its JSONL form, so the payload goes
        /// through the same decoder as a stored session.
        fn push(&mut self, mut line: Value) -> String {
            let id = format!("{:08x}", self.entries.len() + 1);
            line["id"] = json!(id);
            line["parentId"] = json!(self.entries.last().map(|last| last.id.clone()));
            line["timestamp"] = json!("2026-10-06T00:00:00Z");
            self.entries
                .push(decode_pi_entry(line.to_string().as_bytes()).expect("entry decodes"));
            id
        }

        fn custom(&mut self, custom_type: &str, data: &str) -> String {
            let data: Value = serde_json::from_str(data).unwrap();
            self.push(json!({"type":"custom","customType":custom_type,"data":data}))
        }

        fn start(&mut self, turn: &str, trigger: TurnTrigger) -> String {
            self.custom(
                TURN_CUSTOM_TYPE,
                &encode_turn_record(&TurnRecord::started(turn, trigger, 1, None)),
            )
        }

        fn end(&mut self, turn: &str) -> String {
            self.custom(
                TURN_CUSTOM_TYPE,
                &encode_turn_record(&TurnRecord::ended(turn, TurnOutcome::Completed)),
            )
        }

        fn message(&mut self, message: Value) -> String {
            self.push(json!({"type":"message","message":message}))
        }

        fn user(&mut self, text: &str) -> String {
            self.message(json!({
                "role":"user","content":[{"type":"text","text":text}],"timestamp":1
            }))
        }

        fn assistant(&mut self, content: Value, stop: &str) -> String {
            self.message(json!({
                "role":"assistant","content":content,"api":"openai-completions",
                "provider":"openai-compatible","model":"m","stopReason":stop,"timestamp":1,
                "usage":{"input":1,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":2,
                    "cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}}
            }))
        }

        fn reply(&mut self) -> String {
            self.assistant(json!([{"type":"text","text":"done"}]), "stop")
        }

        fn call(&mut self, call_id: &str) -> String {
            self.assistant(
                json!([{"type":"toolCall","id":call_id,"name":"ls","arguments":{}}]),
                "toolUse",
            )
        }

        fn result(&mut self, call_id: &str) -> String {
            self.message(json!({
                "role":"toolResult","toolCallId":call_id,"toolName":"ls",
                "content":[{"type":"text","text":"ok"}],"isError":false,"timestamp":1
            }))
        }

        fn turn(&mut self, turn: &str, prompt: &str) -> String {
            self.start(turn, TurnTrigger::User);
            let id = self.user(prompt);
            self.reply();
            self.end(turn);
            id
        }

        fn leaf(&self) -> &str {
            &self.entries.last().unwrap().id
        }
    }

    fn ids(entries: &[PiEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.id.as_str()).collect()
    }

    #[test]
    fn a_fork_stops_before_the_turn_its_prompt_opens() {
        let mut b = Builder::new();
        let first = b.turn("t1", "first");
        let start = b.entries.len();
        let second = b.turn("t2", "second");

        let points = fork_points(&b.entries, b.leaf()).unwrap();
        assert_eq!(
            points
                .iter()
                .map(|point| (point.entry_id.as_str(), point.prompt.as_str()))
                .collect::<Vec<_>>(),
            [(first.as_str(), "first"), (second.as_str(), "second")]
        );

        let plan = plan_fork(&b.entries, b.leaf(), Some(&second)).unwrap();
        assert_eq!(ids(&plan.entries), ids(&b.entries[..start]));
        assert_eq!(plan.point.unwrap().prompt, "second");

        let plan = plan_fork(&b.entries, b.leaf(), Some(&first)).unwrap();
        assert!(plan.entries.is_empty());
    }

    #[test]
    fn steering_assistant_and_unknown_ids_are_not_fork_points() {
        let mut b = Builder::new();
        b.start("t1", TurnTrigger::User);
        b.user("prompt");
        let call = b.call("c1");
        b.result("c1");
        let steer = b.user("steer");
        b.reply();
        b.end("t1");
        b.start("t2", TurnTrigger::Wake);
        let wake = b.user("wake text");
        b.reply();
        b.end("t2");

        for id in [
            steer.as_str(),
            call.as_str(),
            wake.as_str(),
            "ffffffff",
            "x-tail-0",
        ] {
            assert!(
                matches!(
                    plan_fork(&b.entries, b.leaf(), Some(id)),
                    Err(ForkError::InvalidPoint(_))
                ),
                "{id}"
            );
        }
        assert_eq!(fork_points(&b.entries, b.leaf()).unwrap().len(), 1);
    }

    #[test]
    fn a_whole_fork_leaves_out_an_open_turn() {
        let mut b = Builder::new();
        b.turn("t1", "first");
        let boundary = b.entries.len();
        b.start("t2", TurnTrigger::User);
        b.user("lost");
        b.call("c1");

        let plan = plan_fork(&b.entries, b.leaf(), None).unwrap();
        assert_eq!(ids(&plan.entries), ids(&b.entries[..boundary]));
        assert!(plan.point.is_none());

        b.result("c1");
        b.reply();
        b.end("t2");
        let plan = plan_fork(&b.entries, b.leaf(), None).unwrap();
        assert_eq!(plan.entries.len(), b.entries.len());
    }

    #[test]
    fn sessions_without_turn_records_fork_before_any_answered_prompt() {
        let mut b = Builder::new();
        let first = b.user("first");
        b.call("c1");
        b.result("c1");
        b.reply();
        let before_last = b.entries.len();
        let last = b.user("last");
        b.call("c2");

        let points: Vec<String> = fork_points(&b.entries, b.leaf())
            .unwrap()
            .into_iter()
            .map(|point| point.entry_id)
            .collect();
        assert_eq!(points, [first, last.clone()]);
        let plan = plan_fork(&b.entries, b.leaf(), Some(&last)).unwrap();
        assert_eq!(plan.entries.len(), before_last);
        // A whole fork leaves out the trailing unanswered call.
        let plan = plan_fork(&b.entries, b.leaf(), None).unwrap();
        assert_eq!(plan.entries.len(), before_last + 1);
    }

    #[test]
    fn fork_records_round_trip_and_reject_other_versions() {
        let record = ForkRecord {
            schema_version: FORK_SCHEMA_VERSION,
            source_session_id: "abc".into(),
            source_entry_id: Some("0000000a".into()),
            prompt: Some("hi".into()),
            request_id: None,
        };
        let encoded = encode_fork_record(&record);
        assert_eq!(
            encoded,
            r#"{"schemaVersion":1,"sourceSessionId":"abc","sourceEntryId":"0000000a","prompt":"hi"}"#
        );
        assert_eq!(decode_fork_record(&encoded), Some(record.clone()));
        assert_eq!(
            decode_fork_record(r#"{"schemaVersion":2,"sourceSessionId":"abc"}"#),
            None
        );

        let mut b = Builder::new();
        b.turn("t1", "first");
        assert_eq!(fork_origin(&b.entries), None);
        b.custom(FORK_CUSTOM_TYPE, &encoded);
        assert_eq!(fork_origin(&b.entries), Some(record));
    }
}
