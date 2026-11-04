//! Pinned, no-follow filesystem primitives shared by native persistent stores.
//!
//! Every attacker-controlled component below the pinned root is traversed with
//! `openat` and `O_NOFOLLOW`. No safe-wrapper crate covers the exclusive
//! rename, `mkdirat`, or `fstatat(AT_SYMLINK_NOFOLLOW)`, so these are raw
//! `libc` calls confined to this module. The exclusive rename is the one
//! primitive whose spelling differs per platform; see [`rename_excl`].
//!
//! Ownership: [`Dir`] owns a directory descriptor and closes it on drop.
//! Concurrency: every function is a single syscall or a short sequence of them;
//! no shared state. Errors: `io::Error` carrying the raw `errno`, so callers
//! can match `ErrorKind` or `raw_os_error`.

#![allow(unsafe_code)]

use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// An open directory descriptor used as the base of `openat` calls.
#[derive(Debug)]
pub struct Dir(RawFd);

impl Dir {
    /// The raw descriptor. Valid for the lifetime of this `Dir`.
    pub fn fd(&self) -> RawFd {
        self.0
    }
}

impl From<OwnedFd> for Dir {
    fn from(fd: OwnedFd) -> Self {
        Self(fd.into_raw_fd())
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        // SAFETY: self.0 came from an open(2)/openat(2) that succeeded and is
        // closed exactly once, here.
        unsafe { libc::close(self.0) };
    }
}

fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

fn c_name(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "name must be one path component",
        ));
    }
    CString::new(name)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))
}

fn open_result(fd: libc::c_int) -> io::Result<RawFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

/// `open(path, flags|O_CLOEXEC|O_NOFOLLOW)`. A symlink at the final component
/// fails with `ELOOP` instead of being followed.
pub fn open_no_follow(path: &Path, flags: libc::c_int) -> io::Result<File> {
    let c = c_path(path)?;
    // SAFETY: c is a NUL-terminated path that outlives the call.
    let fd =
        open_result(unsafe { libc::open(c.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) })?;
    // SAFETY: fd is a fresh descriptor this function now owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// How long [`lock_exclusive`] retries a busy `flock`.
///
/// `fork`, including the `clone` inside Linux `posix_spawn`, duplicates every
/// open file description into the child. The session descriptor is
/// `CLOEXEC`, so that duplicate disappears at `exec`, but until then a
/// non-blocking lock still returns `EWOULDBLOCK` even though no other Otto
/// process holds the session. A genuine holder keeps the lock for the whole
/// time the session is open, which is longer than this grace.
pub(crate) const SESSION_LOCK_SPAWN_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// Nonblocking shared writer-admission lease on a stable pinned file.
pub fn try_lock_shared(file: &File) -> io::Result<()> {
    try_lock(file, libc::LOCK_SH)
}

/// Nonblocking exclusive freeze on the same writer-admission file.
pub fn try_lock_exclusive(file: &File) -> io::Result<()> {
    try_lock(file, libc::LOCK_EX)
}

fn try_lock(file: &File, mode: libc::c_int) -> io::Result<()> {
    // SAFETY: the borrowed descriptor stays open through this call.
    if unsafe { libc::flock(file.as_raw_fd(), mode | libc::LOCK_NB) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Acquires the session's non-blocking advisory write lock.
pub fn lock_exclusive(file: &File) -> io::Result<()> {
    let deadline = std::time::Instant::now() + SESSION_LOCK_SPAWN_GRACE;
    loop {
        // SAFETY: the borrowed descriptor stays open through this call.
        let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file is already locked by another process",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// `openat(dir, name, flags|O_CLOEXEC|O_NOFOLLOW)`. `name` must be a single
/// path component; the caller is responsible for rejecting separators.
pub fn open_at_no_follow(dir: &Dir, name: &str, flags: libc::c_int) -> io::Result<File> {
    let c = if name == "." && flags & libc::O_DIRECTORY != 0 {
        c_path(Path::new("."))?
    } else {
        c_name(name)?
    };
    // SAFETY: dir.fd() is open for the duration of the call and c is a valid
    // NUL-terminated name.
    let fd = open_result(unsafe {
        libc::openat(
            dir.fd(),
            c.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    })?;
    // SAFETY: fd is a fresh descriptor this function now owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// `openat(dir, name, flags|O_CREAT|O_CLOEXEC|O_NOFOLLOW, mode)`. Add
/// `O_EXCL` to `flags` to fail with `EEXIST` on an existing name. `name` must
/// be a single path component.
pub fn create_at_no_follow(
    dir: &Dir,
    name: &str,
    flags: libc::c_int,
    mode: libc::mode_t,
) -> io::Result<File> {
    let c = c_name(name)?;
    // SAFETY: dir.fd() is open for the duration of the call and c is a valid
    // NUL-terminated name.
    let fd = open_result(unsafe {
        libc::openat(
            dir.fd(),
            c.as_ptr(),
            flags | libc::O_CREAT | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            libc::c_uint::from(mode),
        )
    })?;
    // SAFETY: fd is a fresh descriptor this function now owns.
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// Opens a directory with `O_NOFOLLOW|O_DIRECTORY`.
pub fn open_dir_no_follow(path: &Path) -> io::Result<Dir> {
    let c = c_path(path)?;
    // SAFETY: as in open_no_follow.
    let fd = open_result(unsafe {
        libc::open(
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    })?;
    Ok(Dir(fd))
}

/// Opens a subdirectory relative to `dir` with `O_NOFOLLOW|O_DIRECTORY`.
pub fn open_dir_at_no_follow(dir: &Dir, name: &str) -> io::Result<Dir> {
    let c = if name == "." {
        c_path(Path::new("."))?
    } else {
        c_name(name)?
    };
    // SAFETY: as in open_at_no_follow.
    let fd = open_result(unsafe {
        libc::openat(
            dir.fd(),
            c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    })?;
    Ok(Dir(fd))
}

/// `mkdirat(dir, name, mode)`. `EEXIST` is returned to the caller.
pub fn mkdir_at(dir: &Dir, name: &str, mode: libc::mode_t) -> io::Result<()> {
    let c = c_name(name)?;
    // SAFETY: dir.fd() is open and c is a valid NUL-terminated name.
    if unsafe { libc::mkdirat(dir.fd(), c.as_ptr(), mode) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// True when `name` exists under `dir`, without following a symlink at the
/// final component. An `AT_SYMLINK_NOFOLLOW` stat probe.
pub fn exists_at_no_follow(dir: &Dir, name: &str) -> io::Result<bool> {
    let c = c_name(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: dir.fd() is open, c is NUL-terminated, and stat points at
    // writable storage of exactly one libc::stat.
    let result = unsafe {
        libc::fstatat(
            dir.fd(),
            c.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        return Ok(false);
    }
    Err(error)
}

/// Pins a directory lock to the same descriptor used for relative operations.
pub fn lock_directory(dir: &Dir) -> io::Result<File> {
    // SAFETY: dir is open; F_DUPFD_CLOEXEC returns a separately owned descriptor.
    let fd = open_result(unsafe { libc::fcntl(dir.fd(), libc::F_DUPFD_CLOEXEC, 0) })?;
    // SAFETY: fd is fresh and owned by this function.
    let file = unsafe { File::from_raw_fd(fd) };
    lock_exclusive(&file)?;
    Ok(file)
}

/// Atomically replaces a name inside a pinned directory.
pub fn rename_at(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    let (from, to) = (c_name(from)?, c_name(to)?);
    // SAFETY: dir is open and both names outlive the syscall.
    if unsafe { libc::renameat(dir.fd(), from.as_ptr(), dir.fd(), to.as_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Publish a staged directory inside one pinned root. Creation is exclusive;
/// replacement exchanges both names atomically, leaving the old tree at `from`.
/// Exclusive creation also works for a file.
/// No fallback to a non-atomic sequence on unsupported filesystems.
pub fn publish_directory(dir: &Dir, from: &str, to: &str, replace: bool) -> io::Result<()> {
    let (from, to) = (c_name(from)?, c_name(to)?);
    // SAFETY: the directory descriptor and both single-component names live
    // through the call. Callers validate the names and own the staged directory.
    #[cfg(target_os = "linux")]
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            dir.fd(),
            from.as_ptr(),
            dir.fd(),
            to.as_ptr(),
            if replace { 2u32 } else { 1u32 }, // RENAME_EXCHANGE / RENAME_NOREPLACE
        )
    };
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            dir.fd(),
            from.as_ptr(),
            dir.fd(),
            to.as_ptr(),
            if replace { 0x0000_0002 } else { 0x0000_0004 }, // RENAME_SWAP / RENAME_EXCL
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `fsync` on a pinned directory, which makes the names created, renamed or
/// removed in it durable.
pub fn sync_dir(dir: &Dir) -> io::Result<()> {
    // SAFETY: the pinned directory descriptor remains live through fsync.
    if unsafe { libc::fsync(dir.fd()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Removes a file name inside a pinned directory.
pub fn unlink_at(dir: &Dir, name: &str) -> io::Result<()> {
    let name = c_name(name)?;
    // SAFETY: dir is open and name outlives the syscall.
    if unsafe { libc::unlinkat(dir.fd(), name.as_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Recursively removes one name without following symbolic links. Missing names
/// are accepted; every other error, including entry limits, is propagated.
pub fn remove_tree_at_no_follow(dir: &Dir, name: &str) -> io::Result<()> {
    match open_dir_at_no_follow(dir, name) {
        Ok(child) => {
            for child_name in read_directory_names(&child, 100_000)? {
                remove_tree_at_no_follow(&child, &child_name)?;
            }
            sync_dir(&child)?;
            let name = c_name(name)?;
            // SAFETY: dir is pinned and name is a validated single component.
            if unsafe { libc::unlinkat(dir.fd(), name.as_ptr(), libc::AT_REMOVEDIR) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
        Err(error) if is_enoent(&error) => Ok(()),
        Err(error) if is_enotdir(&error) || is_eloop(&error) => unlink_at(dir, name),
        Err(error) => Err(error),
    }
}

/// Gives the file `from` the second name `to` inside a pinned directory;
/// fails with `EEXIST` when `to` exists.
fn link_at(dir: &Dir, from: &str, to: &str) -> io::Result<()> {
    let (from, to) = (c_name(from)?, c_name(to)?);
    // SAFETY: dir is open and both names outlive the syscall.
    if unsafe { libc::linkat(dir.fd(), from.as_ptr(), dir.fd(), to.as_ptr(), 0) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// An atomic move that fails with `EEXIST` rather than replacing an existing
/// destination, closing the check-then-rename race.
///
/// Each platform spells it differently and neither spelling has a safe
/// wrapper: macOS has `renamex_np(RENAME_EXCL)`, Linux has
/// `renameat2(RENAME_NOREPLACE)`. The Linux call goes through `syscall`
/// rather than the glibc wrapper, which only exists from glibc 2.28; a
/// kernel older than 3.15 reports `ENOSYS`, and that is returned to the
/// caller rather than falling back to a non-atomic link-and-unlink.
pub fn rename_excl(from: &Path, to: &Path) -> io::Result<()> {
    let (from, to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both paths are NUL-terminated and outlive the call.
    if unsafe { rename_exclusive(from.as_ptr(), to.as_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `renamex_np(from, to, RENAME_EXCL)`.
///
/// # Safety
/// `from` and `to` must be valid NUL-terminated paths that outlive the call.
#[cfg(target_os = "macos")]
unsafe fn rename_exclusive(from: *const libc::c_char, to: *const libc::c_char) -> libc::c_int {
    const RENAME_EXCL: libc::c_uint = 0x0000_0004;
    unsafe { libc::renamex_np(from, to, RENAME_EXCL) }
}

/// `renameat2(AT_FDCWD, from, AT_FDCWD, to, RENAME_NOREPLACE)`.
///
/// # Safety
/// `from` and `to` must be valid NUL-terminated paths that outlive the call.
#[cfg(target_os = "linux")]
unsafe fn rename_exclusive(from: *const libc::c_char, to: *const libc::c_char) -> libc::c_int {
    const RENAME_NOREPLACE: libc::c_uint = 1;
    // `syscall` returns `long`; every outcome this cares about fits `c_int`.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from,
            libc::AT_FDCWD,
            to,
            RENAME_NOREPLACE,
        )
    };
    if result < 0 { -1 } else { 0 }
}

/// `fchmod(dir, 0700)`.
pub fn fchmod_dir(dir: &Dir) -> io::Result<()> {
    // SAFETY: dir.fd() is a valid open descriptor for the duration of the call.
    if unsafe { libc::fchmod(dir.fd(), 0o700) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The device and inode pair that identifies a file.
pub fn file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
}

/// True when the `errno` is `ELOOP`, the signal that a no-follow open refused
/// to traverse a symlink.
pub fn is_eloop(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ELOOP)
}

/// True when the `errno` is `ENOTDIR`.
pub fn is_enotdir(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOTDIR)
}

/// True when the `errno` is `ENOENT`.
pub fn is_enoent(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOENT)
}

/// Enumerate a pinned directory with an independent cursor and a strict bound.
/// No path is reopened; names that cannot be represented exactly are refused.
pub fn read_directory_names(dir: &Dir, max_entries: usize) -> io::Result<Vec<String>> {
    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            // SAFETY: fdopendir created this owned stream and it closes once.
            unsafe { libc::closedir(self.0) };
        }
    }
    let pinned = open_dir_at_no_follow(dir, ".")?;
    // SAFETY: pinned is a live fresh descriptor; ownership transfers on success.
    let stream = unsafe { libc::fdopendir(pinned.fd()) };
    if stream.is_null() {
        return Err(io::Error::last_os_error());
    }
    std::mem::forget(pinned);
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        // SAFETY: libc exposes the current thread's errno cell on each platform.
        #[cfg(target_os = "macos")]
        let errno = unsafe { libc::__error() };
        #[cfg(target_os = "linux")]
        let errno = unsafe { libc::__errno_location() };
        // SAFETY: stream is owned and live; readdir's entry lives until next call.
        let entry = unsafe {
            *errno = 0;
            libc::readdir(stream.0)
        };
        if entry.is_null() {
            // SAFETY: errno points at this thread's live errno cell.
            if unsafe { *errno } != 0 {
                return Err(io::Error::last_os_error());
            }
            break;
        }
        // SAFETY: readdir returns a NUL-terminated d_name for this live entry.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        let name = name.to_str().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "directory name is not UTF-8")
        })?;
        if name == "." || name == ".." {
            continue;
        }
        if names.len() == max_entries {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory entry limit exceeded",
            ));
        }
        names.push(name.to_owned());
    }
    Ok(names)
}

/// Local POSIX driver; all relative operations use owned pinned descriptors.
#[derive(Debug, Default)]
pub struct PosixDriver;

impl crate::storage::files::FileDriver for PosixDriver {
    fn open_root(&self, path: &Path) -> io::Result<Box<dyn crate::storage::files::FileRoot>> {
        Ok(Box::new(open_dir_no_follow(path)?))
    }
}

impl crate::storage::files::FileRoot for Dir {
    fn list_names(&self, max_entries: usize) -> io::Result<Vec<String>> {
        read_directory_names(self, max_entries)
    }
    fn open_directory(&self, name: &str) -> io::Result<Box<dyn crate::storage::files::FileRoot>> {
        Ok(Box::new(open_dir_at_no_follow(self, name)?))
    }
    fn create_directory(&self, name: &str) -> io::Result<()> {
        mkdir_at(self, name, 0o700)
    }
    fn open_journal(
        &self,
        name: &str,
        mode: crate::storage::files::JournalOpen,
    ) -> io::Result<Box<dyn crate::storage::files::JournalFile>> {
        use crate::storage::files::JournalOpen;
        let file = match mode {
            JournalOpen::ReadOnly => {
                open_at_no_follow(self, name, libc::O_RDONLY | libc::O_NONBLOCK)?
            }
            JournalOpen::ReadWrite => {
                open_at_no_follow(self, name, libc::O_RDWR | libc::O_NONBLOCK)?
            }
            JournalOpen::CreateNew => {
                create_at_no_follow(self, name, libc::O_RDWR | libc::O_EXCL, 0o600)?
            }
        };
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "journal is not a regular file",
            ));
        }
        Ok(Box::new(file))
    }
    fn remove_journal(&self, name: &str) -> io::Result<()> {
        unlink_at(self, name)
    }
    fn publish_journal(&self, temp: &str, name: &str) -> io::Result<()> {
        // `link` instead of an exclusive rename: the Linux NFS and CephFS
        // clients return `EINVAL` for `renameat2` flags, and session failover
        // supports both. A failed unlink leaves `temp` as a second name of
        // the published file; no session scan reads it.
        link_at(self, temp, name)?;
        let _ = unlink_at(self, temp);
        Ok(())
    }
    fn sync_all(&self) -> io::Result<()> {
        sync_dir(self)
    }
}

impl crate::storage::files::JournalFile for File {
    fn metadata(&self) -> io::Result<std::fs::Metadata> {
        File::metadata(self)
    }
    fn is_close_on_exec(&self) -> io::Result<bool> {
        // SAFETY: the borrowed descriptor remains live through fcntl.
        let flags = unsafe { libc::fcntl(self.as_raw_fd(), libc::F_GETFD) };
        if flags < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(flags & libc::FD_CLOEXEC != 0)
        }
    }
    fn lock_exclusive(&self) -> io::Result<()> {
        lock_exclusive(self)
    }
    fn try_lock_shared(&self) -> io::Result<()> {
        try_lock_shared(self)
    }
    fn try_lock_exclusive(&self) -> io::Result<()> {
        try_lock_exclusive(self)
    }
    fn sync_all(&self) -> io::Result<()> {
        File::sync_all(self)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        File::set_len(self, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_driver_confines_journals_to_pinned_root() {
        use crate::storage::files::{FileDriver, JournalOpen};
        use std::io::{Read, Seek, SeekFrom, Write};
        let home = tempfile::tempdir().unwrap();
        let original = home.path().join("root");
        std::fs::create_dir(&original).unwrap();
        let root = PosixDriver.open_root(&original).unwrap();
        std::fs::rename(&original, home.path().join("pinned")).unwrap();
        std::fs::create_dir(&original).unwrap();
        let mut journal = root
            .open_journal("entries", JournalOpen::CreateNew)
            .unwrap();
        journal.lock_exclusive().unwrap();
        journal.write_all(b"record\n").unwrap();
        journal.sync_all().unwrap();
        root.sync_all().unwrap();
        journal.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        journal.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"record\n");
        assert!(!original.join("entries").exists());
        assert!(home.path().join("pinned/entries").exists());
        for name in ["", ".", "..", "../escape", "child/name"] {
            assert_eq!(
                root.open_journal(name, JournalOpen::CreateNew)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
        std::os::unix::fs::symlink(home.path().join("outside"), home.path().join("pinned/link"))
            .unwrap();
        assert!(root.open_journal("link", JournalOpen::ReadOnly).is_err());
    }

    #[test]
    fn writer_admission_is_shared_and_freeze_is_exclusive() {
        use crate::storage::files::{FileDriver, JournalOpen};
        let home = tempfile::tempdir().unwrap();
        let root = PosixDriver.open_root(home.path()).unwrap();
        let first = root
            .open_journal("admission", JournalOpen::CreateNew)
            .unwrap();
        let second = root
            .open_journal("admission", JournalOpen::ReadWrite)
            .unwrap();
        let freeze = root
            .open_journal("admission", JournalOpen::ReadWrite)
            .unwrap();
        first.try_lock_shared().unwrap();
        second.try_lock_shared().unwrap();
        assert_eq!(
            freeze.try_lock_exclusive().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(first);
        assert!(freeze.try_lock_exclusive().is_err());
        drop(second);
        freeze.try_lock_exclusive().unwrap();
        let writer = root
            .open_journal("admission", JournalOpen::ReadWrite)
            .unwrap();
        assert_eq!(
            writer.try_lock_shared().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(freeze);
        writer.try_lock_shared().unwrap();
    }

    #[test]
    fn directory_publication_is_exclusive_and_exchanges_complete_trees() {
        let home = tempfile::tempdir().expect("home");
        for name in ["staged", "existing"] {
            std::fs::create_dir(home.path().join(name)).expect("directory");
            std::fs::write(home.path().join(name).join("content"), name).expect("content");
        }
        let root = open_dir_no_follow(home.path()).expect("root");
        assert_eq!(
            publish_directory(&root, "staged", "existing", false)
                .expect_err("collision")
                .raw_os_error(),
            Some(libc::EEXIST)
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("existing/content")).expect("old"),
            "existing"
        );
        publish_directory(&root, "staged", "existing", true).expect("exchange");
        assert_eq!(
            std::fs::read_to_string(home.path().join("existing/content")).expect("new"),
            "staged"
        );
        assert_eq!(
            std::fs::read_to_string(home.path().join("staged/content")).expect("displaced"),
            "existing"
        );
        publish_directory(&root, "staged", "created", false).expect("create");
        assert!(!home.path().join("staged").exists());
        assert_eq!(
            std::fs::read_to_string(home.path().join("created/content")).expect("created"),
            "existing"
        );
    }

    /// The archive move: the session file lands at its destination and the
    /// source is gone.
    #[test]
    fn rename_excl_moves_a_file_to_an_absent_destination() {
        let dir = tempfile::tempdir().expect("temp dir");
        let from = dir.path().join("session.jsonl");
        let to = dir.path().join("archived.jsonl");
        std::fs::write(&from, b"one line\n").expect("write source");

        rename_excl(&from, &to).expect("rename");

        assert!(!from.exists(), "the source is gone");
        assert_eq!(std::fs::read(&to).expect("read destination"), b"one line\n");
    }

    /// The safety property `RENAME_EXCL` buys: an occupied destination is
    /// refused rather than replaced, so the check-then-rename race cannot
    /// destroy an archived session. Both files must survive intact.
    #[test]
    fn rename_excl_refuses_an_occupied_destination() {
        let dir = tempfile::tempdir().expect("temp dir");
        let from = dir.path().join("session.jsonl");
        let to = dir.path().join("archived.jsonl");
        std::fs::write(&from, b"new\n").expect("write source");
        std::fs::write(&to, b"existing\n").expect("write destination");

        let error = rename_excl(&from, &to).expect_err("refused");

        assert_eq!(error.raw_os_error(), Some(libc::EEXIST), "{error}");
        assert_eq!(std::fs::read(&from).expect("source kept"), b"new\n");
        assert_eq!(std::fs::read(&to).expect("destination kept"), b"existing\n");
    }
}
