//! Forking a session file: a new session beside the source whose history is
//! the source's active path up to a turn boundary. The source is only read.
//!
//! The copy is written to a temporary name, synced and moved into place with
//! an exclusive rename, so a crash leaves either no session or a whole one.
//! The temporary name does not end in `.jsonl`, so listing ignores a leftover.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::Utc;
use otto_core::session::context::format_persisted_timestamp;
use otto_core::session::pi::{PI_SESSION_VERSION, PiCustom, PiHeader, PiSessionInfo};
use otto_core::session::{
    FORK_CUSTOM_TYPE, FORK_SCHEMA_VERSION, ForkError, ForkPoint, ForkRecord, PiEntry, PiError,
    PiRecord, encode_fork_record, encode_pi_record, plan_fork,
};

use super::store::{decode_pi_file_read_only, open_path_journal, reject_oversized_session_file};
use crate::session::fsops;
use crate::storage::files::{FileDriver, JournalOpen};

/// A session created by [`fork`].
#[derive(Debug, Clone)]
pub struct Forked {
    pub path: PathBuf,
    pub id: String,
    /// The fork point the copy stops before; `None` for a whole fork.
    pub point: Option<ForkPoint>,
}

/// Writes a fork of the session at `source` as `<new_id>.jsonl` in the same
/// directory. `before` names the turn-opening user message to stop before;
/// `None` keeps everything up to the last turn boundary.
pub fn fork(
    driver: &dyn FileDriver,
    source: &Path,
    before: Option<&str>,
    new_id: &str,
) -> Result<Forked, ForkError> {
    let mut file = open_path_journal(driver, source, JournalOpen::ReadOnly)?;
    reject_oversized_session_file(file.as_ref())?;
    let decoded = decode_pi_file_read_only(file.as_mut())?;
    drop(file);

    let leaf = decoded
        .entries
        .last()
        .map(|entry| entry.id.clone())
        .unwrap_or_default();
    let plan = plan_fork(&decoded.entries, &leaf, before)?;
    let name = decoded
        .entries
        .iter()
        .rev()
        .find_map(|entry| entry.session_info.as_ref()?.name.clone())
        .filter(|name| !name.trim().is_empty());

    let timestamp = format_persisted_timestamp(Utc::now(), "session fork")?;
    let header = PiHeader {
        type_name: "session".into(),
        version: PI_SESSION_VERSION,
        id: new_id.to_owned(),
        timestamp: timestamp.clone(),
        cwd: decoded.header.cwd.clone(),
        parent_session: Some(source.to_string_lossy().into_owned()),
        raw: Vec::new(),
    };
    let mut records = vec![encode_pi_record(PiRecord::Header(&header))?];
    let mut seen: HashSet<String> = HashSet::new();
    let mut parent = None;
    for entry in &plan.entries {
        records.push(encode_pi_record(PiRecord::Entry(entry))?);
        seen.insert(entry.id.clone());
        parent = Some(entry.id.clone());
    }
    if let Some(name) = name {
        let id = fsops::new_pi_entry_id(&seen)?;
        let mut entry = PiEntry::new("session_info", &id, parent.take(), &timestamp);
        entry.session_info = Some(PiSessionInfo {
            name: Some(format!("{name} (fork)")),
        });
        records.push(encode_pi_record(PiRecord::Entry(&entry))?);
        seen.insert(id.clone());
        parent = Some(id);
    }
    let record = ForkRecord {
        schema_version: FORK_SCHEMA_VERSION,
        source_session_id: decoded.header.id.clone(),
        source_entry_id: plan.point.as_ref().map(|point| point.entry_id.clone()),
        prompt: plan.point.as_ref().map(|point| point.prompt.clone()),
    };
    let id = fsops::new_pi_entry_id(&seen)?;
    let mut entry = PiEntry::new("custom", &id, parent, &timestamp);
    entry.custom = Some(PiCustom {
        custom_type: FORK_CUSTOM_TYPE.into(),
        data: Some(
            serde_json::value::RawValue::from_string(encode_fork_record(&record))
                .map_err(|error| PiError::other(format!("encode fork record: {error}")))?,
        ),
    });
    records.push(encode_pi_record(PiRecord::Entry(&entry))?);

    let directory = source
        .parent()
        .ok_or_else(|| PiError::invalid("session file has no parent directory"))?;
    let path = directory.join(format!("{new_id}.jsonl"));
    write_new(driver, directory, &path, &records)?;
    Ok(Forked {
        path,
        id: new_id.to_owned(),
        point: plan.point,
    })
}

fn write_new(
    driver: &dyn FileDriver,
    directory: &Path,
    path: &Path,
    records: &[Vec<u8>],
) -> Result<(), PiError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PiError::invalid("session file name is not a valid component"))?;
    let temporary = format!("{file_name}.fork-tmp");
    let root = driver
        .open_root(directory)
        .map_err(|error| PiError::other(format!("open session directory: {error}")))?;
    let written = root
        .open_journal(&temporary, JournalOpen::CreateNew)
        .map_err(|error| PiError::other(format!("create fork file: {error}")))
        .and_then(|mut file| {
            let mut bytes = Vec::new();
            for record in records {
                bytes.extend_from_slice(record);
                bytes.push(b'\n');
            }
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| PiError::other(format!("write fork file: {error}")))
        })
        .and_then(|()| {
            crate::storage::drivers::posix::rename_excl(&directory.join(&temporary), path)
                .map_err(|error| PiError::other(format!("publish fork file: {error}")))
        })
        .and_then(|()| {
            root.sync_all()
                .map_err(|error| PiError::other(format!("sync session directory: {error}")))
        });
    if written.is_err() {
        let _ = root.remove_journal(&temporary);
    }
    written
}
