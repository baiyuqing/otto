//! Fault decorator over [`FileDriver`], [`FileRoot`] and [`JournalFile`].
//!
//! [`FaultyFileDriver`] wraps every root and journal it hands out, so a rule
//! reaches journals under nested directories. The plan subject of a root
//! operation is the name passed to it (`OpenRoot` carries the path, `RootSync`
//! and `ListNames` the name the root was opened by). A journal operation
//! carries the journal's name.
//!
//! `Write` counts calls of [`Write::write`]; a caller that uses `write_all`
//! issues one call per record unless the inner write is partial. `metadata` and
//! `is_close_on_exec` are forwarded without a check. `Lock` covers
//! `lock_exclusive`, `try_lock_shared` and `try_lock_exclusive`.
//!
//! [`Fault::Hang`] panics: the file boundary is synchronous, so a hang would
//! block the test thread; a plan that asks for it is a test bug.

use std::fmt;
use std::fs::Metadata;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use super::crash;
use super::{Fault, FaultPlan};
use crate::storage::files::{FileDriver, FileRoot, JournalFile, JournalOpen};

/// The file operations a rule can select.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    OpenRoot,
    OpenDirectory,
    CreateDirectory,
    OpenJournal,
    RemoveJournal,
    PublishJournal,
    ListNames,
    RootSync,
    Read,
    Write,
    Seek,
    FileSync,
    SetLen,
    Lock,
}

type Plan = Arc<FaultPlan<FileOp>>;

/// Applies the fault selected for `op` around `call`. `Short` and `ShortExit`
/// are for the write operations, which handle them themselves.
pub(super) fn run<Op: Copy + Eq + fmt::Debug, T>(
    plan: &FaultPlan<Op>,
    op: Op,
    subject: &str,
    call: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    match plan.check(op, subject) {
        None => call(),
        Some(Fault::Before(error)) => Err(error.io()),
        Some(Fault::After(error)) => {
            call()?;
            Err(error.io())
        }
        Some(Fault::Exit) => crash::exit(),
        Some(Fault::ExitAfter) => {
            let _ = call();
            crash::exit()
        }
        Some(Fault::Hang) => hang(op, subject),
        Some(fault @ (Fault::Short { .. } | Fault::ShortExit { .. })) => {
            panic!("{fault:?} applies to a write operation only, not {op:?} on {subject}")
        }
    }
}

pub(super) fn hang(op: impl fmt::Debug, subject: &str) -> ! {
    panic!(
        "Fault::Hang on {op:?} for {subject}: the file boundary is synchronous, so a hang cannot be ended by a deadline; this plan is a test bug"
    )
}

/// A [`FileDriver`] that applies a [`FaultPlan`] to the driver it wraps.
#[derive(Clone)]
pub struct FaultyFileDriver {
    inner: Arc<dyn FileDriver>,
    plan: Plan,
}

impl FaultyFileDriver {
    pub fn new(inner: Arc<dyn FileDriver>, plan: Arc<FaultPlan<FileOp>>) -> Self {
        Self { inner, plan }
    }
}

impl fmt::Debug for FaultyFileDriver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("FaultyFileDriver")
    }
}

impl FileDriver for FaultyFileDriver {
    fn open_root(&self, path: &Path) -> io::Result<Box<dyn FileRoot>> {
        let name = path.to_string_lossy().into_owned();
        let root = run(&self.plan, FileOp::OpenRoot, &name, || {
            self.inner.open_root(path)
        })?;
        Ok(Box::new(FaultyRoot {
            inner: root,
            plan: Arc::clone(&self.plan),
            name,
        }))
    }
}

struct FaultyRoot {
    inner: Box<dyn FileRoot>,
    plan: Plan,
    name: String,
}

impl fmt::Debug for FaultyRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FaultyRoot")
            .field(&self.name)
            .finish()
    }
}

impl FileRoot for FaultyRoot {
    fn list_names(&self, max_entries: usize) -> io::Result<Vec<String>> {
        run(&self.plan, FileOp::ListNames, &self.name, || {
            self.inner.list_names(max_entries)
        })
    }

    fn open_directory(&self, name: &str) -> io::Result<Box<dyn FileRoot>> {
        let inner = run(&self.plan, FileOp::OpenDirectory, name, || {
            self.inner.open_directory(name)
        })?;
        Ok(Box::new(Self {
            inner,
            plan: Arc::clone(&self.plan),
            name: name.to_owned(),
        }))
    }

    fn create_directory(&self, name: &str) -> io::Result<()> {
        run(&self.plan, FileOp::CreateDirectory, name, || {
            self.inner.create_directory(name)
        })
    }

    fn open_journal(&self, name: &str, mode: JournalOpen) -> io::Result<Box<dyn JournalFile>> {
        let inner = run(&self.plan, FileOp::OpenJournal, name, || {
            self.inner.open_journal(name, mode)
        })?;
        Ok(Box::new(FaultyJournal {
            inner,
            plan: Arc::clone(&self.plan),
            name: name.to_owned(),
        }))
    }

    fn remove_journal(&self, name: &str) -> io::Result<()> {
        run(&self.plan, FileOp::RemoveJournal, name, || {
            self.inner.remove_journal(name)
        })
    }

    fn publish_journal(&self, temp: &str, name: &str) -> io::Result<()> {
        run(&self.plan, FileOp::PublishJournal, name, || {
            self.inner.publish_journal(temp, name)
        })
    }

    fn sync_all(&self) -> io::Result<()> {
        run(&self.plan, FileOp::RootSync, &self.name, || {
            self.inner.sync_all()
        })
    }
}

struct FaultyJournal {
    inner: Box<dyn JournalFile>,
    plan: Plan,
    name: String,
}

impl fmt::Debug for FaultyJournal {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("FaultyJournal")
            .field(&self.name)
            .finish()
    }
}

impl Read for FaultyJournal {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Self { inner, plan, name } = self;
        run(plan, FileOp::Read, name, || inner.read(buf))
    }
}

impl Seek for FaultyJournal {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let Self { inner, plan, name } = self;
        run(plan, FileOp::Seek, name, || inner.seek(position))
    }
}

impl Write for FaultyJournal {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.plan.check(FileOp::Write, &self.name) {
            None => self.inner.write(buf),
            Some(Fault::Before(error)) => Err(error.io()),
            Some(Fault::After(error)) => {
                self.inner.write(buf)?;
                Err(error.io())
            }
            Some(Fault::Short { keep, error }) => {
                self.inner.write_all(&buf[..keep.min(buf.len())])?;
                Err(error.io())
            }
            Some(Fault::ShortExit { keep }) => {
                let _ = self.inner.write_all(&buf[..keep.min(buf.len())]);
                crash::exit()
            }
            Some(Fault::Exit) => crash::exit(),
            Some(Fault::ExitAfter) => {
                let _ = self.inner.write(buf);
                crash::exit()
            }
            Some(Fault::Hang) => hang(FileOp::Write, &self.name),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl JournalFile for FaultyJournal {
    fn metadata(&self) -> io::Result<Metadata> {
        self.inner.metadata()
    }

    fn is_close_on_exec(&self) -> io::Result<bool> {
        self.inner.is_close_on_exec()
    }

    fn lock_exclusive(&self) -> io::Result<()> {
        run(&self.plan, FileOp::Lock, &self.name, || {
            self.inner.lock_exclusive()
        })
    }

    fn try_lock_shared(&self) -> io::Result<()> {
        run(&self.plan, FileOp::Lock, &self.name, || {
            self.inner.try_lock_shared()
        })
    }

    fn try_lock_exclusive(&self) -> io::Result<()> {
        run(&self.plan, FileOp::Lock, &self.name, || {
            self.inner.try_lock_exclusive()
        })
    }

    fn sync_all(&self) -> io::Result<()> {
        run(&self.plan, FileOp::FileSync, &self.name, || {
            self.inner.sync_all()
        })
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        run(&self.plan, FileOp::SetLen, &self.name, || {
            self.inner.set_len(len)
        })
    }
}
