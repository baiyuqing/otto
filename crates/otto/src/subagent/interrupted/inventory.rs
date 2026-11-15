//! Bounded, confined recovery discovery. Unknown evidence is never an empty scan.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;

use super::ChildRecord;

/// Private transcript evidence must never appear in diagnostic output.
#[derive(PartialEq, Eq)]
pub(crate) struct Evidence {
    identity: (u64, u64),
    bytes: Vec<u8>,
}

impl std::fmt::Debug for Evidence {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("Evidence").finish_non_exhaustive()
    }
}

/// Reopen without following links, acquire the write lock, and compare the
/// descriptor with the exact validated inventory snapshot before any repair.
pub(crate) fn open_validated(record: &ChildRecord) -> Result<otto_session::Store, String> {
    use std::io::SeekFrom;
    let evidence = record
        .evidence
        .as_ref()
        .ok_or("missing pinned recovery evidence")?;
    let prepared =
        otto_session::Prepared::prepare(&record.path, None).map_err(|error| error.to_string())?;
    let (store, _) = prepared
        .activate_checked(|file, metadata| {
            let changed = || {
                otto_core::session::PiError::invalid(
                    "recovery transcript changed before write admission",
                )
            };
            if otto_platform::fsops::file_identity(metadata) != evidence.identity
                || metadata.len() != evidence.bytes.len() as u64
            {
                return Err(changed());
            }
            file.seek(SeekFrom::Start(0)).map_err(|_| changed())?;
            let mut bytes = Vec::new();
            file.take(MAX_CHILD_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| changed())?;
            if bytes != evidence.bytes {
                return Err(changed());
            }
            Ok(())
        })
        .map_err(|error| error.to_string())?;
    Ok(store)
}

const MAX_CHILDREN: usize = 4096;
const MAX_CHILD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SCAN_BYTES: u64 = 64 * 1024 * 1024;

/// Coverage is separate from records: callers must display diagnostics even
/// when no usable child can be reconstructed.
#[derive(Debug, Default)]
pub struct Inventory {
    pub records: Vec<ChildRecord>,
    pub diagnostics: Vec<String>,
    pub complete: bool,
    pub directory_missing: bool,
    /// Highest observed filename counter, not an upper bound if incomplete.
    pub highest_counter: u64,
}

/// Scans only the selected parent's direct child directory using pinned POSIX
/// descriptors. Never follows stored parent paths. Duplicate identities are
/// excluded, not arbitrarily selected as execution authority.
pub fn inventory(parent: &Path) -> Inventory {
    let directory = parent.with_extension("");
    let mut result = Inventory {
        complete: true,
        ..Inventory::default()
    };
    let dir = match otto_platform::fsops::open_dir_no_follow(&directory) {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            result.directory_missing = true;
            return result;
        }
        Err(error) => {
            result.complete = false;
            result.diagnostics.push(format!(
                "recovery: cannot scan {}: {error}",
                directory.display()
            ));
            return result;
        }
    };
    let mut names = match otto_platform::fsops::read_directory_names(&dir, MAX_CHILDREN) {
        Ok(names) => names,
        Err(error) => {
            result.complete = false;
            result
                .diagnostics
                .push(format!("recovery: directory coverage unavailable: {error}"));
            return result;
        }
    };
    names.sort();
    let mut task_ids = HashSet::new();
    let mut conflicts = HashSet::new();
    let mut child_ids = HashMap::new();
    let mut scan_bytes = 0;
    for name in names {
        let path = directory.join(&name);
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        if let Some(counter) = name
            .strip_prefix('t')
            .and_then(|name| name.split_once('-'))
            .and_then(|(digits, _)| digits.parse::<u64>().ok())
        {
            result.highest_counter = result.highest_counter.max(counter);
        }
        let checked = (|| -> Result<ChildRecord, String> {
            let file = otto_platform::fsops::open_at_no_follow(
                &dir,
                &name,
                libc::O_RDONLY | libc::O_NONBLOCK,
            )
            .map_err(|error| error.to_string())?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !metadata.is_file() || metadata.len() > MAX_CHILD_BYTES {
                return Err("not a regular bounded transcript".into());
            }
            scan_bytes += metadata.len();
            if scan_bytes > MAX_SCAN_BYTES {
                return Err("aggregate scan byte limit exceeded".into());
            }
            let mut bytes = Vec::new();
            file.take(MAX_CHILD_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| error.to_string())?;
            if bytes.len() as u64 > MAX_CHILD_BYTES {
                return Err("child grew beyond scan limit".into());
            }
            let (decoded, messages) = otto_session::Store::decode_recovery_bytes(&bytes)
                .map_err(|error| error.to_string())?;
            let mut has_spec = false;
            for entry in &decoded.entries {
                let Some(custom) = &entry.custom else {
                    continue;
                };
                if !matches!(
                    custom.custom_type.as_str(),
                    "otto.task_spec" | "otto.task_attempt" | "otto.task_result"
                ) {
                    continue;
                }
                let data = custom.data.as_ref().ok_or("missing lifecycle payload")?;
                match custom.custom_type.as_str() {
                    "otto.task_spec" => {
                        if has_spec {
                            return Err("multiple task specifications".into());
                        }
                        has_spec = true;
                        serde_json::from_str::<super::TaskSpecData>(data.get())
                            .map_err(|error| error.to_string())?;
                    }
                    "otto.task_attempt" => {
                        let data: serde_json::Value =
                            serde_json::from_str(data.get()).map_err(|error| error.to_string())?;
                        if data.get("version").and_then(|value| value.as_u64()) != Some(1)
                            || data
                                .get("attemptId")
                                .and_then(|value| value.as_str())
                                .is_none_or(str::is_empty)
                        {
                            return Err("unsupported or malformed attempt fact".into());
                        }
                    }
                    _ => {
                        let data: serde_json::Value =
                            serde_json::from_str(data.get()).map_err(|error| error.to_string())?;
                        if data
                            .get("status")
                            .and_then(|value| value.as_str())
                            .is_none_or(|status| {
                                !matches!(
                                    status,
                                    "succeeded" | "failed" | "canceled" | "interrupted"
                                )
                            })
                        {
                            return Err("unsupported or malformed terminal fact".into());
                        }
                    }
                }
            }
            let expected = otto_platform::fsops::clean_go_path(&parent.to_string_lossy());
            let linked = decoded
                .header
                .parent_session
                .as_deref()
                .ok_or("missing parentSession linkage")?;
            if otto_platform::fsops::clean_go_path(linked) != expected {
                return Err("parentSession does not match selected parent".into());
            }
            let child_id = &decoded.header.id;
            let mut record = super::record_from_entries(&path, &decoded.entries, &messages)
                .ok_or("missing/invalid task spec")?;
            let counter = record
                .task_id
                .strip_prefix('t')
                .and_then(|id| id.parse::<u64>().ok());
            if counter.is_none()
                || child_id.is_empty()
                || path.file_stem().and_then(|name| name.to_str())
                    != Some(format!("{}-{child_id}", record.task_id).as_str())
            {
                return Err("task/header/filename identity mismatch".into());
            }
            record.evidence = Some(std::sync::Arc::new(Evidence {
                identity: otto_platform::fsops::file_identity(&metadata),
                bytes,
            }));
            if !task_ids.insert(record.task_id.clone()) {
                conflicts.insert(record.task_id.clone());
            }
            if let Some(previous) = child_ids.insert(child_id.clone(), record.task_id.clone()) {
                conflicts.insert(previous);
                conflicts.insert(record.task_id.clone());
            }
            Ok(record)
        })();
        match checked {
            Ok(record) => result.records.push(record),
            Err(error) => {
                result.complete = false;
                result
                    .diagnostics
                    .push(format!("recovery: {}: {error}", path.display()));
            }
        }
        if scan_bytes > MAX_SCAN_BYTES {
            break;
        }
    }
    result
        .records
        .retain(|record| !conflicts.contains(&record.task_id));
    for id in conflicts {
        result.complete = false;
        result.diagnostics.push(format!(
            "recovery: conflicting identity for {id}; resume unavailable"
        ));
    }
    result
        .records
        .sort_by_key(|record| super::task_counter(&record.task_id));
    result
}
