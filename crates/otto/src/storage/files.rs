//! Confined local file capability used by append-only journals.
//!
//! Names are single components under pinned directory handles. This interface
//! promises no-follow traversal, exclusive writer locks and durable file/directory
//! sync; it is not an object store or a multi-file transaction abstraction.
use std::{
    fmt::Debug,
    fs::Metadata,
    io::{self, Read, Seek, Write},
    path::Path,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalOpen {
    ReadOnly,
    ReadWrite,
    CreateNew,
}

pub trait JournalFile: Debug + Read + Write + Seek + Send {
    fn metadata(&self) -> io::Result<Metadata>;
    fn is_close_on_exec(&self) -> io::Result<bool>;
    fn lock_exclusive(&self) -> io::Result<()>;
    /// Nonblocking lifetime admission locks; dropping the handle releases them.
    fn try_lock_shared(&self) -> io::Result<()>;
    fn try_lock_exclusive(&self) -> io::Result<()>;
    fn sync_all(&self) -> io::Result<()>;
    fn set_len(&self, len: u64) -> io::Result<()>;
}

pub trait FileRoot: Debug + Send + Sync {
    fn list_names(&self, max_entries: usize) -> io::Result<Vec<String>>;
    fn open_directory(&self, name: &str) -> io::Result<Box<dyn FileRoot>>;
    fn create_directory(&self, name: &str) -> io::Result<()>;
    fn open_journal(&self, name: &str, mode: JournalOpen) -> io::Result<Box<dyn JournalFile>>;
    fn remove_journal(&self, name: &str) -> io::Result<()>;
    fn sync_all(&self) -> io::Result<()>;
}

/// Driver selection occurs at composition; consumers receive a confined root.
/// Implementations must satisfy all guarantees documented above.
pub trait FileDriver: Debug + Send + Sync {
    fn open_root(&self, path: &Path) -> io::Result<Box<dyn FileRoot>>;
}
