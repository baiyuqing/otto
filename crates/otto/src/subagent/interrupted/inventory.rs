//! Bounded, confined recovery discovery. Unknown evidence is never an empty scan.

use std::collections::HashSet;
use std::io::BufRead;
use std::path::Path;

use super::ChildRecord;

const MAX_CHILDREN: usize = 4096;
const MAX_CHILD_BYTES: u64 = 16 * 1024 * 1024;
const MAX_HEADER_BYTES: u64 = 64 * 1024;

/// Coverage is separate from records: callers must display diagnostics even
/// when no usable child can be reconstructed.
#[derive(Debug, Default)]
pub struct Inventory {
    pub records: Vec<ChildRecord>,
    pub diagnostics: Vec<String>,
    pub complete: bool,
    pub directory_missing: bool,
}

/// Scans only the selected parent's direct child directory. Symlinks, oversized
/// files, conflicting ids and invalid linkage are unavailable, never resumable.
/// `parent` is the selected saved transcript, not a path taken from a child.
pub fn inventory(parent: &Path) -> Inventory {
    let directory = parent.with_extension("");
    let mut result = Inventory {
        complete: true,
        ..Inventory::default()
    };
    let entries = match std::fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            result.directory_missing = true;
            return result;
        }
        Ok(metadata) if metadata.file_type().is_dir() => std::fs::read_dir(&directory),
        Ok(_) => {
            result.diagnostics.push(format!(
                "recovery: {} is not a real directory",
                directory.display()
            ));
            result.complete = false;
            return result;
        }
        Err(error) => Err(error),
    };
    let entries = match entries {
        Ok(entries) => entries,
        Err(error) => {
            result.complete = false;
            result.diagnostics.push(format!(
                "recovery: cannot scan {}: {error}",
                directory.display()
            ));
            return result;
        }
    };
    let mut task_ids = HashSet::new();
    let mut conflicts = HashSet::new();
    let mut child_ids = HashSet::new();
    for (index, entry) in entries.enumerate() {
        if index >= MAX_CHILDREN {
            result.complete = false;
            result.diagnostics.push(format!(
                "recovery: scan truncated at {MAX_CHILDREN} directory entries"
            ));
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result.complete = false;
                result
                    .diagnostics
                    .push(format!("recovery: directory entry unavailable: {error}"));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }
        let checked = (|| -> Result<ChildRecord, String> {
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
            if !metadata.file_type().is_file() || metadata.len() > MAX_CHILD_BYTES {
                return Err("not a regular bounded transcript (symlinks are not followed)".into());
            }
            use std::io::Read;
            let file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
            let mut header = String::new();
            std::io::BufReader::new(file.take(MAX_HEADER_BYTES))
                .read_line(&mut header)
                .map_err(|error| error.to_string())?;
            let header: serde_json::Value =
                serde_json::from_str(&header).map_err(|error| error.to_string())?;
            let expected = parent.canonicalize().map_err(|error| error.to_string())?;
            let linked = header
                .get("parentSession")
                .and_then(|value| value.as_str())
                .ok_or("missing parentSession linkage")?;
            if Path::new(linked)
                .canonicalize()
                .map_err(|error| error.to_string())?
                != expected
            {
                return Err("parentSession does not match selected parent".into());
            }
            let child_id = header
                .get("id")
                .and_then(|value| value.as_str())
                .ok_or("missing child id")?;
            let record = super::scan_one(&path)
                .ok_or("unreadable transcript or missing/invalid task spec")?;
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
            if !task_ids.insert(record.task_id.clone()) {
                conflicts.insert(record.task_id.clone());
            }
            if !child_ids.insert(child_id.to_owned()) {
                return Err("duplicate child session identity".into());
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
    }
    result
        .records
        .retain(|record| !conflicts.contains(&record.task_id));
    for id in conflicts {
        result.complete = false;
        result.diagnostics.push(format!(
            "recovery: conflicting transcripts for {id}; resume unavailable"
        ));
    }
    result
        .records
        .sort_by_key(|record| super::task_counter(&record.task_id));
    result
}
