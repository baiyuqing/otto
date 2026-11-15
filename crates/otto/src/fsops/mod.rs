//! Compatibility facade for shared pinned POSIX operations.
//! Domain-specific Pi errors, workspace path normalization and atomic file
//! publishing ([`publish_file`]) stay here. `posix` holds the pinned
//! descriptor primitives and the local [`PosixDriver`]; `files` holds the
//! [`files::FileDriver`] contract it implements.
pub mod canonical;
pub mod files;
pub(crate) mod gopath;
pub mod posix;
pub(crate) mod root;
pub use canonical::canonical_directory;
pub(crate) use canonical::canonical_executable_file;
use otto_core::session::PiError;
pub use posix::*;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::{fs::File, io, path::Path};

pub fn lock_directory(dir: &Dir) -> Result<File, PiError> {
    posix::lock_directory(dir)
        .map_err(|_| PiError::other("workspace identity is busy or unavailable"))
}

/// The absolute, symlink-resolved form of a workspace path. A path that does
/// not exist yet is only made absolute and lexically cleaned.
pub fn canonical_workspace(path: &Path) -> Result<String, PiError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| PiError::other(format!("resolve workspace path: {error}")))?
            .join(path)
    };
    match absolute.canonicalize() {
        Ok(canonical) => Ok(canonical.to_string_lossy().into_owned()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok(clean_go_path(&absolute.to_string_lossy()))
        }
        Err(error) => Err(PiError::other(format!(
            "resolve workspace symlinks: {error}"
        ))),
    }
}

/// Go's `path/filepath.Clean` semantics, which the on-disk session paths are
/// written with: `..` is collapsed lexically, which Rust's `std::path` does
/// not do.
pub(crate) fn clean_go_path(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let bytes = path.as_bytes();
    let rooted = bytes[0] == b'/';
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut dotdot = 0usize;
    if rooted {
        out.push(b'/');
        dotdot = 1;
    }
    let mut index = if rooted { 1 } else { 0 };
    while index < bytes.len() {
        match bytes[index] {
            b'/' => index += 1,
            b'.' if index + 1 == bytes.len() || bytes[index + 1] == b'/' => index += 1,
            b'.' if bytes[index + 1] == b'.'
                && (index + 2 == bytes.len() || bytes[index + 2] == b'/') =>
            {
                index += 2;
                if out.len() > dotdot {
                    while out.len() > dotdot && out[out.len() - 1] != b'/' {
                        out.pop();
                    }
                    if out.len() > dotdot {
                        out.pop();
                    }
                } else if !rooted {
                    if !out.is_empty() {
                        out.push(b'/');
                    }
                    out.extend_from_slice(b"..");
                    dotdot = out.len();
                }
            }
            _ => {
                if (rooted && out.len() != 1) || (!rooted && !out.is_empty()) {
                    out.push(b'/');
                }
                while index < bytes.len() && bytes[index] != b'/' {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
        }
    }
    if out.is_empty() {
        return ".".into();
    }
    String::from_utf8(out).unwrap_or_else(|_| path.to_owned())
}

/// One step of [`publish_file`], in the order they run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStep {
    CreateTemp,
    Write,
    FileSync,
    Rename,
    DirSync,
}

/// What [`publish_file`] does when the destination already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishMode {
    /// `renameat`: the destination is replaced atomically.
    Replace,
    /// An exclusive rename: the call fails with `EEXIST` at
    /// [`PublishStep::Rename`] and the destination is unchanged.
    Exclusive,
}

/// The file [`publish_file`] writes: `name`, staged as `temp` in the same
/// directory and given exactly `permissions`.
#[derive(Debug, Clone, Copy)]
pub struct Publish<'a> {
    pub name: &'a str,
    pub temp: &'a str,
    pub permissions: u32,
    pub mode: PublishMode,
}

/// A failed [`publish_file`]. `committed` is `true` once the rename returned
/// `Ok`: the destination then holds the new content, but its directory entry
/// may not be durable.
#[derive(Debug)]
pub struct PublishError {
    pub step: PublishStep,
    pub committed: bool,
    pub source: io::Error,
}

/// The system calls [`publish_file`] makes, one method per [`PublishStep`].
/// [`PosixPublish`] is the only production implementation; tests wrap it to
/// inject faults.
pub trait PublishOps: Send + Sync {
    /// Creates `target.temp` in `dir` exclusively, without following a
    /// symbolic link, with exactly `target.permissions`.
    fn create_temp(&self, dir: &Dir, target: &Publish<'_>) -> io::Result<File>;
    fn write_all(&self, target: &Publish<'_>, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    fn sync_file(&self, target: &Publish<'_>, file: &File) -> io::Result<()>;
    /// Moves `target.temp` to `target.name` in `dir` as `target.mode` says.
    fn rename(&self, dir: &Dir, target: &Publish<'_>) -> io::Result<()>;
    /// `fsync` on `dir` after a change to `name` in it.
    fn sync_dir(&self, dir: &Dir, name: &str) -> io::Result<()>;
}

/// [`PublishOps`] over the pinned POSIX primitives.
#[derive(Debug, Default)]
pub(crate) struct PosixPublish;

impl PublishOps for PosixPublish {
    fn create_temp(&self, dir: &Dir, target: &Publish<'_>) -> io::Result<File> {
        let file = create_at_no_follow(
            dir,
            target.temp,
            libc::O_WRONLY | libc::O_EXCL,
            target.permissions as libc::mode_t,
        )?;
        // The creation mode is masked by the umask; set the exact bits.
        if let Err(error) =
            file.set_permissions(std::fs::Permissions::from_mode(target.permissions))
        {
            let _ = unlink_at(dir, target.temp);
            return Err(error);
        }
        Ok(file)
    }

    fn write_all(&self, _target: &Publish<'_>, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn sync_file(&self, _target: &Publish<'_>, file: &File) -> io::Result<()> {
        file.sync_all()
    }

    fn rename(&self, dir: &Dir, target: &Publish<'_>) -> io::Result<()> {
        match target.mode {
            PublishMode::Replace => rename_at(dir, target.temp, target.name),
            PublishMode::Exclusive => publish_directory(dir, target.temp, target.name, false),
        }
    }

    fn sync_dir(&self, dir: &Dir, _name: &str) -> io::Result<()> {
        sync_dir(dir)
    }
}

/// Writes `bytes` to `target.name` in `dir` so that a reader or a crash sees
/// either the previous file or the whole new one: create `target.temp`,
/// write, `fsync` the file, rename it over the destination, then `fsync`
/// `dir`. A failure before the rename removes the temporary file; a failure
/// to create it leaves any existing file of that name alone.
pub fn publish_file(
    dir: &Dir,
    target: &Publish<'_>,
    bytes: &[u8],
    ops: &dyn PublishOps,
) -> Result<(), PublishError> {
    let failed = |step, committed| {
        move |source| PublishError {
            step,
            committed,
            source,
        }
    };
    let mut file = ops
        .create_temp(dir, target)
        .map_err(failed(PublishStep::CreateTemp, false))?;
    let staged = ops
        .write_all(target, &mut file, bytes)
        .map_err(failed(PublishStep::Write, false))
        .and_then(|()| {
            ops.sync_file(target, &file)
                .map_err(failed(PublishStep::FileSync, false))
        });
    drop(file);
    let renamed = staged.and_then(|()| {
        ops.rename(dir, target)
            .map_err(failed(PublishStep::Rename, false))
    });
    if renamed.is_err() {
        let _ = unlink_at(dir, target.temp);
    }
    renamed?;
    ops.sync_dir(dir, target.name)
        .map_err(failed(PublishStep::DirSync, true))
}

/// Opens the directory `path` lives in, following symbolic links as
/// `std::fs` does, and returns it with `path`'s final component. A bare file
/// name lives in the working directory.
pub(crate) fn open_parent(path: &Path) -> io::Result<(Dir, &str)> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "path has no UTF-8 file name")
        })?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let dir = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(parent)?;
    Ok((Dir::from(OwnedFd::from(dir)), name))
}
