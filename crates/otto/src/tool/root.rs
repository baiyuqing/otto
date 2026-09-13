//! A directory handle that confines every operation to one directory tree.
//!
//! Port of Go's `os.Root` (`$GOROOT/src/os/root_openat.go`), which
//! `internal/tool/workspace.go` relies on for the workspace boundary. The
//! resolution rules reproduced here are the security contract:
//!
//! - every component is opened with `openat` relative to the previous one, so
//!   renaming or replacing the root after [`Root::open`] cannot redirect an
//!   operation to another tree;
//! - `..` is resolved lexically against the components already walked and
//!   restarts the walk at the root, so it can never reach the real parent
//!   directory; a `..` that would leave the root is an error;
//! - symbolic links are followed only when their target is relative, and the
//!   target is spliced into the component list so the rules above apply to it
//!   as well; an absolute link target is an error.
//!
//! Ownership: a [`Root`] owns one directory file descriptor and closes it on
//! drop. Returned files and directory listings belong to the caller.
//!
//! Concurrency: every method takes `&self` and performs no interior mutation,
//! so a `Root` may be shared across threads.
//!
//! Errors: all failures are `std::io::Error`. A path that would leave the root
//! fails with [`ErrorKind::InvalidInput`] and the text `path escapes from
//! parent`, matching Go's `errPathEscapes`.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use nix::dir::Dir;
use nix::fcntl::{OFlag, openat, readlinkat, renameat};
use nix::libc;
use nix::sys::stat::{FileStat, Mode, SFlag, fstatat};
use nix::unistd::{UnlinkatFlags, unlinkat};

/// Maximum number of symbolic links followed while resolving one path, the
/// value Go uses (`rootMaxSymlinks`).
const MAX_SYMLINKS: usize = 8;
/// Step and restart limits from Go's `doInRoot`; both must be exceeded before
/// a path is rejected as too long.
const MAX_STEPS: usize = 255;
const MAX_RESTARTS: usize = 8;

/// A directory tree that operations may not leave.
#[derive(Debug)]
pub struct Root {
    fd: OwnedFd,
}

/// One entry of a directory listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: OsString,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub is_regular: bool,
}

pub fn escapes_parent() -> io::Error {
    io::Error::new(ErrorKind::InvalidInput, "path escapes from parent")
}

fn is_symlink_error(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ELOOP) | Some(libc::ENOTDIR) | Some(libc::EMLINK)
    )
}

fn nix_error(error: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

/// The error an operation returns to tell the walker that the component is a
/// symbolic link that must be followed. Go signals this with `errSymlink`.
fn symlink_signal() -> io::Error {
    io::Error::from_raw_os_error(libc::ELOOP)
}

pub fn is_dir(stat: &FileStat) -> bool {
    SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFDIR
}

pub fn is_regular(stat: &FileStat) -> bool {
    SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFREG
}

pub fn is_symlink(stat: &FileStat) -> bool {
    SFlag::from_bits_truncate(stat.st_mode) & SFlag::S_IFMT == SFlag::S_IFLNK
}

/// Splits `path` into components, dropping `.`, keeping `..`, and framing the
/// result with `prefix` and `suffix`. Port of Go's `splitPathInRoot`.
///
/// An empty path, or one starting at the filesystem root, is rejected: an
/// absolute symbolic-link target must never be followed.
fn split_path_in_root(
    path: &Path,
    prefix: &[OsString],
    suffix: &[OsString],
) -> io::Result<Vec<OsString>> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(io::Error::new(ErrorKind::InvalidInput, "empty path"));
    }
    if bytes[0] == b'/' {
        return Err(escapes_parent());
    }
    let mut parts: Vec<OsString> = prefix.to_vec();
    for component in bytes.split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        parts.push(OsString::from_vec(component.to_vec()));
    }
    parts.extend_from_slice(suffix);
    if parts.is_empty() {
        parts.push(OsString::from("."));
    }
    Ok(parts)
}

fn open_dir_at(dirfd: BorrowedFd<'_>, name: &OsStr) -> io::Result<OwnedFd> {
    openat(
        dirfd,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(nix_error)
}

impl Root {
    /// Opens `path` as the root of the tree. Symbolic links in `path` itself
    /// are followed, as in Go's `os.OpenRoot`.
    pub fn open(path: &Path) -> io::Result<Self> {
        let directory = File::open(path)?;
        let stat = nix::sys::stat::fstat(&directory).map_err(nix_error)?;
        if !is_dir(&stat) {
            return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
        }
        Ok(Self {
            fd: OwnedFd::from(directory),
        })
    }

    /// Runs `operation` on the parent directory and final component of `name`.
    ///
    /// `follow_final` decides what happens when `operation` reports `ELOOP`
    /// for the final component: with it set the link is read and spliced into
    /// the path, without it the error is returned to the caller. Operations
    /// that act on a link itself (`lstat`, `readlink`, `remove`) pass `false`.
    fn do_in_root<T>(
        &self,
        name: &Path,
        follow_final: bool,
        operation: &mut dyn FnMut(BorrowedFd<'_>, &OsStr) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut parts = split_path_in_root(name, &[], &[])?;
        let mut current: Option<OwnedFd> = None;
        let mut index = 0usize;
        let mut steps = 0usize;
        let mut restarts = 0usize;
        let mut symlinks = 0usize;

        loop {
            steps += 1;
            if steps > MAX_STEPS && restarts > MAX_RESTARTS {
                return Err(io::Error::from_raw_os_error(libc::ENAMETOOLONG));
            }

            if parts[index] == OsStr::new("..") {
                restarts += 1;
                let mut end = index + 1;
                while end < parts.len() && parts[end] == OsStr::new("..") {
                    end += 1;
                }
                let count = end - index;
                if count > index {
                    return Err(escapes_parent());
                }
                parts.drain(index - count..end);
                if parts.is_empty() {
                    parts.push(OsString::from("."));
                }
                index = 0;
                current = None;
                continue;
            }

            let dirfd = current
                .as_ref()
                .map_or_else(|| self.fd.as_fd(), AsFd::as_fd);
            let last = index == parts.len() - 1;
            let outcome = if last {
                operation(dirfd, &parts[index])
            } else {
                match open_dir_at(dirfd, &parts[index]) {
                    Ok(fd) => {
                        current = Some(fd);
                        index += 1;
                        continue;
                    }
                    Err(error) => Err(error),
                }
            };

            let error = match outcome {
                Ok(value) => return Ok(value),
                Err(error) => error,
            };
            if !(last && !follow_final)
                && is_symlink_error(&error)
                && let Ok(target) = readlinkat(dirfd, parts[index].as_os_str())
            {
                symlinks += 1;
                if symlinks > MAX_SYMLINKS {
                    return Err(io::Error::from_raw_os_error(libc::ELOOP));
                }
                let (head, tail) = parts.split_at(index);
                let (head, tail) = (head.to_vec(), tail[1..].to_vec());
                parts = split_path_in_root(Path::new(&target), &head, &tail)?;
                continue;
            }
            return Err(error);
        }
    }

    /// Opens a file in the tree. Symbolic links in every position, including
    /// the last, are followed as long as they stay inside the tree.
    pub fn open_file(&self, name: &Path, flags: OFlag, mode: Mode) -> io::Result<File> {
        let fd = self.do_in_root(name, true, &mut |dirfd, component| {
            openat(
                dirfd,
                component,
                flags | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
                mode,
            )
            .map_err(nix_error)
        })?;
        Ok(File::from(fd))
    }

    /// Stats `name` without following a final symbolic link.
    pub fn lstat(&self, name: &Path) -> io::Result<FileStat> {
        self.do_in_root(name, false, &mut |dirfd, component| {
            fstatat(dirfd, component, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW).map_err(nix_error)
        })
    }

    /// Stats `name`, following a final symbolic link that stays inside the
    /// tree.
    pub fn stat(&self, name: &Path) -> io::Result<FileStat> {
        self.do_in_root(name, true, &mut |dirfd, component| {
            let stat = fstatat(dirfd, component, nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW)
                .map_err(nix_error)?;
            if is_symlink(&stat) {
                return Err(symlink_signal());
            }
            Ok(stat)
        })
    }

    /// Reads the target of the symbolic link at `name`.
    pub fn read_link(&self, name: &Path) -> io::Result<PathBuf> {
        self.do_in_root(name, false, &mut |dirfd, component| {
            readlinkat(dirfd, component)
                .map(PathBuf::from)
                .map_err(nix_error)
        })
    }

    /// Removes the file or empty directory at `name` without following a
    /// final symbolic link.
    pub fn remove(&self, name: &Path) -> io::Result<()> {
        self.do_in_root(name, false, &mut |dirfd, component| {
            unlinkat(dirfd, component, UnlinkatFlags::NoRemoveDir).map_err(nix_error)
        })
    }

    /// Creates the directory `name` if it is missing.
    fn mkdir(&self, name: &Path, mode: Mode) -> io::Result<()> {
        let result = self.do_in_root(name, false, &mut |dirfd, component| {
            nix::sys::stat::mkdirat(dirfd, component, mode).map_err(nix_error)
        });
        match result {
            Err(error) if error.raw_os_error() == Some(libc::EEXIST) => Ok(()),
            other => other,
        }
    }

    /// Creates `name` and every missing parent directory.
    //
    // ponytail: each prefix is resolved from the root again, so this is
    // O(components^2) syscalls. Paths here are a handful of components deep;
    // add an incremental walk if a deep tree ever shows up in a profile.
    pub fn mkdir_all(&self, name: &Path, mode: Mode) -> io::Result<()> {
        let parts = split_path_in_root(name, &[], &[])?;
        let mut prefix = PathBuf::new();
        for part in &parts {
            prefix.push(part);
            if part == OsStr::new("..") {
                continue;
            }
            match self.mkdir(&prefix, mode) {
                Ok(()) => {}
                Err(error) if error.raw_os_error() == Some(libc::EEXIST) => {}
                Err(error) => {
                    // A symbolic link or an existing file is only a failure if
                    // it does not resolve to a directory, matching os.MkdirAll.
                    if self.stat(&prefix).is_ok_and(|stat| is_dir(&stat)) {
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    /// Renames `from` to `to`, both resolved inside the tree without
    /// following a final symbolic link.
    pub fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        self.do_in_root(from, false, &mut |from_fd, from_name| {
            self.do_in_root(to, false, &mut |to_fd, to_name| {
                renameat(from_fd, from_name, to_fd, to_name).map_err(nix_error)
            })
        })
    }

    /// Lists one directory level, sorted by name, excluding `.` and `..`.
    pub fn read_dir(&self, name: &Path) -> io::Result<Vec<DirEntry>> {
        let directory = self.open_file(
            name,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NONBLOCK,
            Mode::empty(),
        )?;
        let stat_fd = directory.try_clone()?;
        let mut handle = Dir::from_fd(OwnedFd::from(directory)).map_err(nix_error)?;
        let mut entries = Vec::new();
        for entry in handle.iter() {
            let entry = entry.map_err(nix_error)?;
            let name = OsStr::from_bytes(entry.file_name().to_bytes()).to_os_string();
            if name == OsStr::new(".") || name == OsStr::new("..") {
                continue;
            }
            let (mut is_dir_entry, mut is_symlink_entry, mut is_regular_entry) =
                match entry.file_type() {
                    Some(nix::dir::Type::Directory) => (true, false, false),
                    Some(nix::dir::Type::Symlink) => (false, true, false),
                    Some(nix::dir::Type::File) => (false, false, true),
                    Some(_) => (false, false, false),
                    None => (false, false, false),
                };
            if entry.file_type().is_none() {
                let stat = fstatat(
                    &stat_fd,
                    name.as_os_str(),
                    nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
                )
                .map_err(nix_error)?;
                is_dir_entry = is_dir(&stat);
                is_symlink_entry = is_symlink(&stat);
                is_regular_entry = is_regular(&stat);
            }
            entries.push(DirEntry {
                name,
                is_dir: is_dir_entry,
                is_symlink: is_symlink_entry,
                is_regular: is_regular_entry,
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_root() -> (tempfile::TempDir, Root) {
        let directory = tempfile::tempdir().expect("temp dir");
        let root = Root::open(directory.path()).expect("open root");
        (directory, root)
    }

    fn write(base: &Path, name: &str, content: &str) {
        let path = base.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("parents");
        }
        let mut file = File::create(path).expect("create");
        file.write_all(content.as_bytes()).expect("write");
    }

    #[test]
    fn parent_traversal_out_of_the_root_is_rejected() {
        let (_dir, root) = temp_root();
        let error = root
            .open_file(Path::new("../escape.txt"), OFlag::O_RDONLY, Mode::empty())
            .expect_err("escape");
        assert_eq!(error.to_string(), "path escapes from parent");
    }

    #[test]
    fn parent_traversal_inside_the_root_resolves() {
        let (dir, root) = temp_root();
        write(dir.path(), "a/b.txt", "ok");
        let stat = root.stat(Path::new("a/../a/b.txt")).expect("stat");
        assert!(is_regular(&stat));
    }

    #[test]
    fn absolute_symlink_targets_are_rejected() {
        let (dir, root) = temp_root();
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("secret"), "no").expect("write");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");
        let error = root
            .open_file(Path::new("link/secret"), OFlag::O_RDONLY, Mode::empty())
            .expect_err("escape");
        assert_eq!(error.to_string(), "path escapes from parent");
    }

    #[test]
    fn relative_symlinks_inside_the_root_are_followed() {
        let (dir, root) = temp_root();
        write(dir.path(), "inside/file.txt", "ok");
        std::os::unix::fs::symlink("inside", dir.path().join("link")).expect("symlink");
        let stat = root.stat(Path::new("link/file.txt")).expect("stat");
        assert!(is_regular(&stat));
        assert!(is_symlink(&root.lstat(Path::new("link")).expect("lstat")));
        assert_eq!(
            root.read_link(Path::new("link")).expect("readlink"),
            Path::new("inside")
        );
    }

    #[test]
    fn a_symlink_chain_leaving_the_root_is_rejected() {
        let (dir, root) = temp_root();
        std::os::unix::fs::symlink("../outside.txt", dir.path().join("link")).expect("symlink");
        let error = root
            .open_file(Path::new("link"), OFlag::O_RDONLY, Mode::empty())
            .expect_err("escape");
        assert_eq!(error.to_string(), "path escapes from parent");
    }

    #[test]
    fn symlink_loops_stop_after_the_limit() {
        let (dir, root) = temp_root();
        std::os::unix::fs::symlink("b", dir.path().join("a")).expect("symlink");
        std::os::unix::fs::symlink("a", dir.path().join("b")).expect("symlink");
        let error = root
            .open_file(Path::new("a"), OFlag::O_RDONLY, Mode::empty())
            .expect_err("loop");
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
    }

    #[test]
    fn the_root_survives_being_renamed_and_replaced() {
        let parent = tempfile::tempdir().expect("parent");
        let root_path = parent.path().join("workspace");
        std::fs::create_dir(&root_path).expect("mkdir");
        std::fs::write(root_path.join("note.txt"), "inside").expect("write");
        let root = Root::open(&root_path).expect("open root");

        std::fs::rename(&root_path, parent.path().join("moved")).expect("rename");
        let outside = tempfile::tempdir().expect("outside");
        std::fs::write(outside.path().join("note.txt"), "outside secret").expect("write");
        std::os::unix::fs::symlink(outside.path(), &root_path).expect("symlink");

        let mut file = root
            .open_file(Path::new("note.txt"), OFlag::O_RDONLY, Mode::empty())
            .expect("open");
        let mut content = String::new();
        std::io::Read::read_to_string(&mut file, &mut content).expect("read");
        assert_eq!(content, "inside");
    }

    #[test]
    fn mkdir_all_creates_every_level_and_rename_moves_within_the_root() {
        let (dir, root) = temp_root();
        root.mkdir_all(Path::new("a/b/c"), Mode::from_bits_truncate(0o755))
            .expect("mkdir_all");
        assert!(dir.path().join("a/b/c").is_dir());
        root.mkdir_all(Path::new("a/b/c"), Mode::from_bits_truncate(0o755))
            .expect("idempotent");

        std::fs::write(dir.path().join("a/b/c/from"), "x").expect("write");
        root.rename(Path::new("a/b/c/from"), Path::new("a/to"))
            .expect("rename");
        assert!(dir.path().join("a/to").exists());
        root.remove(Path::new("a/to")).expect("remove");
        assert!(!dir.path().join("a/to").exists());
    }

    #[test]
    fn read_dir_sorts_and_classifies_entries() {
        let (dir, root) = temp_root();
        std::fs::write(dir.path().join("b.txt"), "x").expect("write");
        std::fs::create_dir(dir.path().join("a")).expect("mkdir");
        std::os::unix::fs::symlink("b.txt", dir.path().join("c")).expect("symlink");
        let entries = root.read_dir(Path::new(".")).expect("read_dir");
        let names: Vec<_> = entries
            .iter()
            .map(|entry| entry.name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a", "b.txt", "c"]);
        assert!(entries[0].is_dir);
        assert!(entries[1].is_regular);
        assert!(entries[2].is_symlink);
    }
}
