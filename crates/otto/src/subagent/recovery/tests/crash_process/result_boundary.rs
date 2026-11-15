//! Test-only pinned file decorator: tool returned, result not yet recorded.

use otto_platform::fsops::files::{FileDriver, FileRoot, JournalFile, JournalOpen};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug)]
pub(super) struct ResultBoundary {
    pub marker: PathBuf,
}
impl FileDriver for ResultBoundary {
    fn open_root(&self, path: &Path) -> io::Result<Box<dyn FileRoot>> {
        Ok(Box::new(BoundaryRoot {
            inner: otto_platform::fsops::PosixDriver.open_root(path)?,
            marker: self.marker.clone(),
        }))
    }
}
#[derive(Debug)]
struct BoundaryRoot {
    inner: Box<dyn FileRoot>,
    marker: PathBuf,
}
impl FileRoot for BoundaryRoot {
    fn list_names(&self, max: usize) -> io::Result<Vec<String>> {
        self.inner.list_names(max)
    }
    fn open_directory(&self, name: &str) -> io::Result<Box<dyn FileRoot>> {
        Ok(Box::new(Self {
            inner: self.inner.open_directory(name)?,
            marker: self.marker.clone(),
        }))
    }
    fn create_directory(&self, name: &str) -> io::Result<()> {
        self.inner.create_directory(name)
    }
    fn open_journal(&self, name: &str, mode: JournalOpen) -> io::Result<Box<dyn JournalFile>> {
        Ok(Box::new(BoundaryJournal {
            inner: self.inner.open_journal(name, mode)?,
            marker: self.marker.clone(),
        }))
    }
    fn remove_journal(&self, name: &str) -> io::Result<()> {
        self.inner.remove_journal(name)
    }
    fn publish_journal(&self, temp: &str, name: &str) -> io::Result<()> {
        self.inner.publish_journal(temp, name)
    }
    fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
}
#[derive(Debug)]
struct BoundaryJournal {
    inner: Box<dyn JournalFile>,
    marker: PathBuf,
}
impl Read for BoundaryJournal {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buffer)
    }
}
impl Seek for BoundaryJournal {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}
impl Write for BoundaryJournal {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let needle = b"effect recorded once";
        if buffer.windows(needle.len()).any(|window| window == needle) {
            // No byte in this result record has reached the underlying journal.
            std::fs::write(&self.marker, "tool returned; result write not started")?;
            loop {
                std::thread::park();
            }
        }
        self.inner.write(buffer)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
impl JournalFile for BoundaryJournal {
    fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.inner.metadata()
    }
    fn is_close_on_exec(&self) -> io::Result<bool> {
        self.inner.is_close_on_exec()
    }
    fn lock_exclusive(&self) -> io::Result<()> {
        self.inner.lock_exclusive()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.inner.sync_all()
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.inner.set_len(len)
    }
}

pub(super) fn driver(marker: PathBuf) -> Arc<dyn FileDriver> {
    Arc::new(ResultBoundary { marker })
}
