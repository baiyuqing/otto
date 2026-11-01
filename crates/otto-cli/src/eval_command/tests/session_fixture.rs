//! Session files for the tests, written with the Pi encoder.

use std::path::{Path, PathBuf};

use otto_core::session::{
    PiCompaction, PiEntry, PiHeader, PiMessage, PiRecord, PiUsage, encode_pi_record,
};

fn raw(json: &str) -> Box<serde_json::value::RawValue> {
    serde_json::value::RawValue::from_string(json.to_owned()).unwrap()
}

pub const SESSION_ID: &str = "0123456789abcdef0123456789abcdef";
const TIMESTAMP: &str = "2026-08-27T12:00:00Z";

pub fn user_entry(id: &str, parent: Option<&str>, text: &str) -> PiEntry {
    let mut entry = PiEntry::new("message", id, parent.map(str::to_string), TIMESTAMP);
    entry.message = Some(Box::new(PiMessage {
        role: "user".into(),
        content: Some(raw(&serde_json::json!(text).to_string())),
        timestamp: 1,
        ..PiMessage::default()
    }));
    entry
}

pub fn assistant_entry(id: &str, parent: Option<&str>, text: &str) -> PiEntry {
    let mut entry = PiEntry::new("message", id, parent.map(str::to_string), TIMESTAMP);
    entry.message = Some(Box::new(PiMessage {
        role: "assistant".into(),
        api: "openai-completions".into(),
        provider: "openai-compatible".into(),
        model: "profile-model".into(),
        stop_reason: "stop".into(),
        content: Some(raw(
            &serde_json::json!([{"type": "text", "text": text}]).to_string()
        )),
        usage: Some(PiUsage {
            input: 1,
            output: 1,
            total_tokens: 2,
            ..PiUsage::default()
        }),
        timestamp: 1,
        ..PiMessage::default()
    }));
    entry
}

pub fn compaction_entry(id: &str, parent: &str, first_kept: &str, summary: &str) -> PiEntry {
    let mut entry = PiEntry::new("compaction", id, Some(parent.to_string()), TIMESTAMP);
    entry.compaction = Some(Box::new(PiCompaction {
        summary: summary.into(),
        first_kept_entry_id: Some(first_kept.into()),
        tokens_before: 1,
        retained_tail: Some(Vec::new()),
        ..PiCompaction::default()
    }));
    entry
}

/// Writes `entries` in file order as `<directory>/<SESSION_ID>.jsonl`.
pub fn write_session(directory: &Path, entries: &[PiEntry]) -> PathBuf {
    std::fs::create_dir_all(directory).unwrap();
    let header = PiHeader {
        type_name: "session".into(),
        version: 3,
        id: SESSION_ID.into(),
        timestamp: TIMESTAMP.into(),
        cwd: "/workspace".into(),
        ..PiHeader::default()
    };
    let mut bytes = encode_pi_record(PiRecord::Header(&header)).unwrap();
    bytes.push(b'\n');
    for entry in entries {
        bytes.extend(encode_pi_record(PiRecord::Entry(entry)).unwrap());
        bytes.push(b'\n');
    }
    let path = directory.join(format!("{SESSION_ID}.jsonl"));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// The text of message `index` of a linear session: `text_of` may add a secret or padding.
pub fn message_text(index: usize) -> String {
    format!(
        "message number {index}: {}",
        "filler words for length ".repeat(4)
    )
}

/// A linear session of `count` messages that alternate user and assistant, starting with a user
/// message. `text_of` gives the text of message `index`.
pub fn write_linear_session(
    directory: &Path,
    count: usize,
    text_of: impl Fn(usize) -> String,
) -> PathBuf {
    let mut entries = Vec::new();
    for index in 0..count {
        let id = format!("a{:07x}", index + 1);
        let parent = (index > 0).then(|| format!("a{:07x}", index));
        let text = text_of(index);
        entries.push(if index % 2 == 0 {
            user_entry(&id, parent.as_deref(), &text)
        } else {
            assistant_entry(&id, parent.as_deref(), &text)
        });
    }
    write_session(directory, &entries)
}
