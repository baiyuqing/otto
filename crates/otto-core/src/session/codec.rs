//! The Pi v3 JSONL decoder and encoder.
//!
//! Every validation rule, every field path, and every error string is fixed by
//! the Pi v3 files on disk.
//!
//! Ownership: decoding copies the bytes it keeps, so a decoded record does not
//! borrow the input buffer. Encoding returns a fresh buffer.
//!
//! Concurrency: every function here is pure and takes no shared state.
//!
//! Errors: every failure is a [`PiError`]; callers branch on its
//! [`PiErrorKind`] rather than on the message text.
//!
//! Trust boundary: session files are read from disk and may have been written
//! by another tool or corrupted. Nothing is accepted on shape alone; each field
//! is checked against the type the domain expects before it is used.

use std::collections::BTreeMap;

use serde_json::value::RawValue;

use super::pi::{
    MAX_SESSION_ENTRY_BYTES, MAX_SESSION_FILE_BYTES, PI_SESSION_VERSION, PiBranchSummary,
    PiCompaction, PiContentBlock, PiContextEdit, PiCost, PiCustom, PiCustomMessage, PiEntry,
    PiFile, PiHeader, PiLabel, PiMessage, PiModelChange, PiSessionInfo, PiThinkingLevelChange,
    PiUsage,
};
use super::{PiError, PiErrorKind};

/// A JSON object kept as raw values, so unknown fields survive untouched.
/// `BTreeMap` also gives the sorted key order the stored files are written in.
pub(crate) type Object = BTreeMap<String, Box<RawValue>>;

/// One record of a session file, for [`encode_pi_record`].
#[derive(Debug, Clone, Copy)]
pub enum PiRecord<'a> {
    Header(&'a PiHeader),
    Entry(&'a PiEntry),
}

// -- file level ------------------------------------------------------------

/// Decodes a whole session file.
///
/// The file is one JSON object per line: the header on line 1 and entries
/// after it. A final record without a trailing newline is accepted, matching
/// what a process killed mid-append leaves behind.
///
/// Rejects an empty file, an empty line, a record over
/// [`MAX_SESSION_ENTRY_BYTES`], and a file over [`MAX_SESSION_FILE_BYTES`].
pub fn decode_pi_file(data: &[u8]) -> Result<PiFile, PiError> {
    if data.len() > MAX_SESSION_FILE_BYTES {
        return Err(PiError::size(
            PiErrorKind::FileTooLarge,
            MAX_SESSION_FILE_BYTES,
        ));
    }
    if data.is_empty() {
        return Err(PiError::invalid("session file is empty"));
    }
    let mut records: Vec<&[u8]> = data.split(|byte| *byte == b'\n').collect();
    if data.last() == Some(&b'\n') {
        records.pop();
    }

    let mut decoded = PiFile::default();
    for (index, record) in records.iter().enumerate() {
        if record.len() > MAX_SESSION_ENTRY_BYTES {
            return Err(PiError::size(
                PiErrorKind::EntryTooLarge,
                MAX_SESSION_ENTRY_BYTES,
            ));
        }
        decode_pi_file_record(&mut decoded, index + 1, record)?;
    }
    Ok(decoded)
}

fn decode_pi_file_record(
    decoded: &mut PiFile,
    line_number: usize,
    record: &[u8],
) -> Result<(), PiError> {
    if record.is_empty() {
        return Err(PiError::invalid(format!("line {line_number} is empty")));
    }
    if line_number == 1 {
        decoded.header =
            decode_pi_header(record).map_err(|error| error.context("session line 1"))?;
        return Ok(());
    }
    let entry = decode_pi_entry(record)
        .map_err(|error| error.context(format!("session line {line_number}")))?;
    decoded.entries.push(entry);
    Ok(())
}

// -- header ----------------------------------------------------------------

/// Decodes line 1 of a session file.
///
/// A record that is not a Pi session header, or carries a version other than
/// [`PI_SESSION_VERSION`], fails with [`PiErrorKind::UnsupportedFormat`] so
/// callers can tell "not ours" apart from "ours and corrupt".
pub fn decode_pi_header(raw: &[u8]) -> Result<PiHeader, PiError> {
    let object = decode_object(raw, "session header")?;
    let type_name = required_string(&object, "type", "session header.type")?;
    if type_name != "session" {
        return Err(PiError::new(
            PiErrorKind::UnsupportedFormat,
            "first record is not a Pi session header",
        ));
    }

    let Some(version_raw) = object.get("version") else {
        return Err(PiError::new(
            PiErrorKind::UnsupportedFormat,
            "Pi session version is missing",
        ));
    };
    let version = decode_scalar::<i64>(version_raw)
        .ok_or_else(|| invalid_field("session header.version", "an integer"))?;
    if version != PI_SESSION_VERSION {
        return Err(PiError::new(
            PiErrorKind::UnsupportedFormat,
            "Pi session version is not supported",
        ));
    }

    Ok(PiHeader {
        type_name,
        version,
        id: required_string(&object, "id", "session header.id")?,
        timestamp: required_string(&object, "timestamp", "session header.timestamp")?,
        cwd: required_string(&object, "cwd", "session header.cwd")?,
        parent_session: optional_string(
            &object,
            "parentSession",
            "session header.parentSession",
            false,
        )?,
        raw: raw.to_vec(),
    })
}

// -- encoding --------------------------------------------------------------

/// Encodes one record as the exact bytes of a session file line, without the
/// trailing newline.
///
/// A record decoded from a file is re-emitted from its captured bytes so
/// unknown fields survive; the bytes are size-checked and re-decoded first, so
/// a caller cannot smuggle unvalidated JSON back into a file. A record built
/// in memory is encoded field by field and then decoded again as a self-check.
pub fn encode_pi_record(record: PiRecord<'_>) -> Result<Vec<u8>, PiError> {
    let encoded = match record {
        PiRecord::Header(header) => encode_pi_header(header),
        PiRecord::Entry(entry) => encode_pi_entry(entry),
    }
    .map_err(|error| error.context("encode Pi session record"))?;
    if encoded.len() > MAX_SESSION_ENTRY_BYTES {
        return Err(PiError::size(
            PiErrorKind::EntryTooLarge,
            MAX_SESSION_ENTRY_BYTES,
        ));
    }
    decode_object(&encoded, "encoded session record")?;
    Ok(encoded)
}

fn encode_pi_header(header: &PiHeader) -> Result<Vec<u8>, PiError> {
    if !header.raw.is_empty() {
        if header.raw.len() > MAX_SESSION_ENTRY_BYTES {
            return Err(PiError::size(
                PiErrorKind::EntryTooLarge,
                MAX_SESSION_ENTRY_BYTES,
            ));
        }
        decode_pi_header(&header.raw)?;
        return Ok(header.raw.clone());
    }
    let encoded = serde_json::to_vec(header)
        .map_err(|error| PiError::other(format!("encode session header: {error}")))?;
    decode_pi_header(&encoded)?;
    Ok(encoded)
}

fn encode_pi_entry(entry: &PiEntry) -> Result<Vec<u8>, PiError> {
    if !entry.raw.is_empty() {
        if entry.raw.len() > MAX_SESSION_ENTRY_BYTES {
            return Err(PiError::size(
                PiErrorKind::EntryTooLarge,
                MAX_SESSION_ENTRY_BYTES,
            ));
        }
        decode_pi_entry(&entry.raw)?;
        return Ok(entry.raw.clone());
    }

    let mut object = Object::new();
    object.insert("type".into(), to_raw(&entry.type_name)?);
    object.insert("id".into(), to_raw(&entry.id)?);
    object.insert("parentId".into(), to_raw(&entry.parent_id)?);
    object.insert("timestamp".into(), to_raw(&entry.timestamp)?);

    let payload: Option<Box<RawValue>> = match entry.type_name.as_str() {
        "message" => {
            let Some(message) = &entry.message else {
                return Err(PiError::invalid("message payload is required"));
            };
            object.insert("message".into(), to_raw(message)?);
            None
        }
        "model_change" => entry.model_change.as_ref().map(to_raw).transpose()?,
        "thinking_level_change" => entry
            .thinking_level_change
            .as_ref()
            .map(to_raw)
            .transpose()?,
        "compaction" => entry.compaction.as_ref().map(to_raw).transpose()?,
        "branch_summary" => entry.branch_summary.as_ref().map(to_raw).transpose()?,
        "custom" => entry.custom.as_ref().map(to_raw).transpose()?,
        "custom_message" => entry.custom_message.as_ref().map(to_raw).transpose()?,
        "context_edit" => entry.context_edit.as_ref().map(to_raw).transpose()?,
        "label" => entry.label.as_ref().map(to_raw).transpose()?,
        "session_info" => entry.session_info.as_ref().map(to_raw).transpose()?,
        _ => {
            return Err(PiError::invalid(
                "raw JSON is required to encode an unknown entry type",
            ));
        }
    };
    if entry.type_name != "message" {
        let Some(payload) = payload else {
            return Err(PiError::invalid(format!(
                "{} payload is required",
                entry.type_name
            )));
        };
        let fields = decode_object(
            payload.get().as_bytes(),
            &format!("{} payload", entry.type_name),
        )?;
        object.extend(fields);
    }

    let encoded = serde_json::to_vec(&object)
        .map_err(|error| PiError::other(format!("encode session entry: {error}")))?;
    decode_pi_entry(&encoded)?;
    Ok(encoded)
}

fn to_raw<T: serde::Serialize + ?Sized>(value: &T) -> Result<Box<RawValue>, PiError> {
    serde_json::value::to_raw_value(value)
        .map_err(|error| PiError::other(format!("encode session field: {error}")))
}

// -- shape helpers ---------------------------------------------------------

pub(crate) fn decode_object(raw: &[u8], path: &str) -> Result<Object, PiError> {
    serde_json::from_slice(raw)
        .map_err(|_| PiError::invalid(format!("{path} must be one JSON object")))
}

fn required_object_raw<'a>(
    object: &'a Object,
    field: &str,
    path: &str,
) -> Result<&'a RawValue, PiError> {
    let Some(raw) = object.get(field) else {
        return Err(invalid_field(path, "an object"));
    };
    decode_object(raw.get().as_bytes(), path)?;
    Ok(raw)
}

pub(crate) fn required_string(object: &Object, field: &str, path: &str) -> Result<String, PiError> {
    match object.get(field) {
        Some(raw) if !is_json_null(raw) => {
            serde_json::from_str::<String>(raw.get()).map_err(|_| invalid_field(path, "a string"))
        }
        _ => Err(invalid_field(path, "a string")),
    }
}

pub(crate) fn optional_string(
    object: &Object,
    field: &str,
    path: &str,
    allow_null: bool,
) -> Result<Option<String>, PiError> {
    let Some(raw) = object.get(field) else {
        return Ok(None);
    };
    if is_json_null(raw) {
        if allow_null {
            return Ok(None);
        }
        return Err(invalid_field(path, "a string"));
    }
    serde_json::from_str::<String>(raw.get())
        .map(Some)
        .map_err(|_| invalid_field(path, "a string"))
}

fn required_nullable_string(
    object: &Object,
    field: &str,
    path: &str,
) -> Result<Option<String>, PiError> {
    let Some(raw) = object.get(field) else {
        return Err(invalid_field(path, "a string or null"));
    };
    if is_json_null(raw) {
        return Ok(None);
    }
    serde_json::from_str::<String>(raw.get())
        .map(Some)
        .map_err(|_| invalid_field(path, "a string or null"))
}

pub(crate) fn required_i64(object: &Object, field: &str, path: &str) -> Result<i64, PiError> {
    object
        .get(field)
        .and_then(|raw| decode_scalar::<i64>(raw))
        .ok_or_else(|| invalid_field(path, "an integer"))
}

fn required_f64(object: &Object, field: &str, path: &str) -> Result<f64, PiError> {
    object
        .get(field)
        .and_then(|raw| decode_scalar::<f64>(raw))
        .ok_or_else(|| invalid_field(path, "a number"))
}

fn required_bool(object: &Object, field: &str, path: &str) -> Result<bool, PiError> {
    match object.get(field) {
        Some(raw) => decode_bool(raw, path),
        None => Err(invalid_field(path, "a boolean")),
    }
}

fn decode_bool(raw: &RawValue, path: &str) -> Result<bool, PiError> {
    decode_scalar::<bool>(raw).ok_or_else(|| invalid_field(path, "a boolean"))
}

/// Decodes a non-null JSON scalar, returning `None` for `null` and for any
/// value of the wrong type.
fn decode_scalar<T: serde::de::DeserializeOwned>(raw: &RawValue) -> Option<T> {
    if is_json_null(raw) {
        return None;
    }
    serde_json::from_str::<T>(raw.get()).ok()
}

pub(crate) fn invalid_field(path: &str, shape: &str) -> PiError {
    PiError::invalid(format!("{path} must be {shape}"))
}

pub(crate) fn is_json_null(raw: &RawValue) -> bool {
    raw.get().trim() == "null"
}

fn is_json_string(raw: &RawValue) -> bool {
    raw.get().trim_start().starts_with('"')
}

fn is_json_array(raw: &RawValue) -> bool {
    raw.get().trim_start().starts_with('[')
}

/// Elements of a JSON array, or `None` when the value is not an array.
fn json_array_elements(raw: &RawValue) -> Option<Vec<Box<RawValue>>> {
    if !is_json_array(raw) {
        return None;
    }
    serde_json::from_str(raw.get()).ok()
}

mod content;
mod entries;

pub(crate) use content::*;
pub use entries::*;

#[cfg(test)]
pub(crate) mod tests;
