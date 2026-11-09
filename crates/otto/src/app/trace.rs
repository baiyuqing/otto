//! The trace of the current session and its sub-agents, read from session
//! files. See `otto_core::session::trace` for how the spans are built.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use otto_core::model::Message;
use otto_core::session::pi::PiEntry;
use otto_core::session::trace::{self, SpanContent, Trace, TraceSource};

use super::Controller;

/// The parsed content of one session file.
#[derive(Clone)]
struct Parsed {
    entries: Arc<Vec<PiEntry>>,
    messages: Arc<Vec<Message>>,
}

/// A parsed session file and the file state it was parsed at.
struct Cached {
    len: u64,
    modified: Option<SystemTime>,
    parsed: Parsed,
}

/// Parsed session files by path.
///
/// ponytail: one cached read per file; entries are re-parsed whole when the
/// file grows, and a path is never evicted while the controller lives.
#[derive(Default)]
pub(super) struct TraceCache(Mutex<HashMap<PathBuf, Cached>>);

impl TraceCache {
    fn read(&self, path: &Path) -> Option<Parsed> {
        let meta = std::fs::metadata(path).ok()?;
        let (len, modified) = (meta.len(), meta.modified().ok());
        let mut cache = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(hit) = cache.get(path)
            && hit.len == len
            && hit.modified == modified
        {
            return Some(hit.parsed.clone());
        }
        let (entries, messages) = crate::session::Store::read_entries(path).ok()?;
        let parsed = Parsed {
            entries: Arc::new(entries),
            messages: Arc::new(messages),
        };
        cache.insert(
            path.to_path_buf(),
            Cached {
                len,
                modified,
                parsed: parsed.clone(),
            },
        );
        Some(parsed)
    }
}

/// A session file and whether work is still being appended to it.
struct Loaded {
    parsed: Parsed,
    active: bool,
}

impl Loaded {
    fn source(&self) -> TraceSource<'_> {
        TraceSource {
            entries: &self.parsed.entries,
            messages: &self.parsed.messages,
            active: self.active,
        }
    }
}

impl Controller {
    /// The span tree of the current session and its sub-agent tasks. It holds
    /// no message or result text.
    pub fn trace(&self) -> Result<Trace, String> {
        let (id, parent, children) = self.trace_sources()?;
        let children: Vec<TraceSource<'_>> = children.iter().map(Loaded::source).collect();
        Ok(trace::build(&id, &parent.source(), &children))
    }

    /// The text of one span, or `None` when the trace has no such span.
    pub fn trace_span(&self, span_id: &str) -> Result<Option<SpanContent>, String> {
        let (_, parent, children) = self.trace_sources()?;
        let children: Vec<TraceSource<'_>> = children.iter().map(Loaded::source).collect();
        Ok(trace::span_content(&parent.source(), &children, span_id))
    }

    /// The current session id, its file and the files of its sub-agent tasks.
    /// A parent is `active` while an operation holds admission; a child while
    /// its task has not reached a final status. A child file that cannot be
    /// read is skipped.
    fn trace_sources(&self) -> Result<(String, Loaded, Vec<Loaded>), String> {
        if !self.dynamic_content {
            return Err("the session trace is not available".to_string());
        }
        let (id, path, active) = {
            let state = self.lock();
            let current = state.current.as_ref().ok_or("the session is closed")?;
            (current.session.header().id, current.path(), state.is_busy())
        };
        if path.is_empty() {
            return Err(
                "the trace needs a session file; this session runs with --no-session".to_string(),
            );
        }
        let parent = Loaded {
            parsed: self
                .trace_cache
                .read(Path::new(&path))
                .ok_or("the session file cannot be read")?,
            active,
        };
        let running: Vec<String> = self
            .tasks()
            .map(|tasks| tasks.list())
            .unwrap_or_default()
            .into_iter()
            .filter(|task| !task.status.final_status())
            .map(|task| task.session_path)
            .collect();
        let children = child_files(&Path::new(&path).with_extension(""))
            .into_iter()
            .filter_map(|file| {
                Some(Loaded {
                    active: running.iter().any(|path| Path::new(path) == file),
                    parsed: self.trace_cache.read(&file)?,
                })
            })
            .collect();
        Ok((id, parent, children))
    }
}

/// The `*.jsonl` files directly in a session's children directory, the same
/// set `subagent::interrupted::scan` reads. The directory also holds the
/// transcripts of tasks started by an earlier process, which the task
/// registry does not list.
fn child_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_files_lists_transcripts_without_a_registry_entry() {
        let dir = tempfile::tempdir().expect("dir");
        let children = dir.path().join("session");
        std::fs::create_dir(&children).expect("children dir");
        std::fs::write(children.join("t2-child.jsonl"), "").expect("t2");
        std::fs::write(children.join("t1-child.jsonl"), "").expect("t1");
        std::fs::write(children.join("notes.txt"), "").expect("other");

        assert_eq!(
            child_files(&children),
            [
                children.join("t1-child.jsonl"),
                children.join("t2-child.jsonl")
            ]
        );
        assert!(child_files(&dir.path().join("missing")).is_empty());
    }
}
