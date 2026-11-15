//! Descriptor-relative history reads and terminal transcript cache validation.
use std::fs::{File, Metadata};
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path};

use crate::fsops::{Dir, open_at_no_follow, open_dir_at_no_follow, open_dir_no_follow};
use crate::subagent::record::TaskRow;
use crate::subagent::tasks::observed::ObserverEntry;
use crate::subagent::tasks::observer::Projection;

const MAX_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq)]
struct Fingerprint {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl Fingerprint {
    fn new(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[derive(Clone)]
pub(super) struct CachedHistory {
    path: String,
    fingerprint: Fingerprint,
    pub(super) entries: Vec<ObserverEntry>,
}

fn descend(mut directory: Dir, relative: &Path) -> Option<Dir> {
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        directory = open_dir_at_no_follow(&directory, name.to_str()?).ok()?;
    }
    Some(directory)
}

fn open_history(session_root: &Path, row: &TaskRow) -> Option<File> {
    if row.task_id.is_empty()
        || !row
            .task_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return None;
    }
    let parent_path = Path::new(&row.parent_session_path);
    let relative_parent = parent_path.parent()?.strip_prefix(session_root).ok()?;
    let child_dir_name = parent_path.file_stem()?.to_str()?;
    let child_dir_path = parent_path.with_extension("");
    let transcript_path = Path::new(&row.session_path);
    let file_name = transcript_path.file_name()?.to_str()?;
    if transcript_path.parent()? != child_dir_path
        || !file_name.starts_with(&format!("{}-", row.task_id))
        || transcript_path.extension()?.to_str()? != "jsonl"
    {
        return None;
    }
    // Only the configured root is trusted to resolve aliases (e.g. macOS /var).
    // Pin that root from /, then reject symlinks in every recorded component.
    let canonical_root = session_root.canonicalize().ok()?;
    let root = descend(
        open_dir_no_follow(Path::new("/")).ok()?,
        canonical_root.strip_prefix("/").ok()?,
    )?;
    let parent = descend(root, relative_parent)?;
    let child = open_dir_at_no_follow(&parent, child_dir_name).ok()?;
    open_at_no_follow(&child, file_name, libc::O_RDONLY | libc::O_NONBLOCK).ok()
}

pub(super) fn read(
    session_root: &Path,
    row: &TaskRow,
    cached: Option<&CachedHistory>,
) -> Option<CachedHistory> {
    let mut file = open_history(session_root, row)?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_TRANSCRIPT_BYTES {
        return None;
    }
    let fingerprint = Fingerprint::new(&metadata);
    if !matches!(row.status.as_str(), "queued" | "running")
        && let Some(cached) = cached
        && cached.path == row.session_path
        && cached.fingerprint == fingerprint
    {
        return Some(cached.clone());
    }
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_TRANSCRIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_TRANSCRIPT_BYTES
        || Fingerprint::new(&file.metadata().ok()?) != fingerprint
    {
        return None;
    }
    let decoded = otto_core::session::decode_pi_file(&bytes).ok()?;
    let leaf = decoded.entries.last().map_or("", |entry| entry.id.as_str());
    let (context, _) = otto_core::session::build_context(&decoded.entries, leaf).ok()?;
    Some(CachedHistory {
        path: row.session_path.clone(),
        fingerprint,
        entries: Projection::transcript(&context.messages).snapshot(),
    })
}
