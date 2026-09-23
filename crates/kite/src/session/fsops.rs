//! Filesystem primitives the session store needs and `std` does not offer.
//!
//! Every attacker-controlled component below the session root is traversed with
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
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use kite_core::session::PiError;
use sha2::{Digest, Sha256};

/// An open directory descriptor used as the base of `openat` calls.
#[derive(Debug)]
pub struct Dir(RawFd);

impl Dir {
    /// The raw descriptor. Valid for the lifetime of this `Dir`.
    pub fn fd(&self) -> RawFd {
        self.0
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

/// `openat(dir, name, flags|O_CLOEXEC|O_NOFOLLOW)`. `name` must be a single
/// path component; the caller is responsible for rejecting separators.
pub fn open_at_no_follow(dir: &Dir, name: &str, flags: libc::c_int) -> io::Result<File> {
    let c = c_name(name)?;
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
    let c = c_name(name)?;
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

/// Four random bytes, hex encoded, avoiding any id in `seen`. A fresh Pi entry
/// id, including the 1024-attempt ceiling.
pub fn new_pi_entry_id(seen: &std::collections::HashSet<String>) -> Result<String, PiError> {
    for _ in 0..1024 {
        let mut buffer = [0u8; 4];
        // SAFETY: buffer is 4 writable bytes and 4 is well under the 256-byte
        // getentropy limit.
        if unsafe { libc::getentropy(buffer.as_mut_ptr().cast(), buffer.len()) } < 0 {
            return Err(PiError::other(io::Error::last_os_error()));
        }
        let id = buffer
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if !seen.contains(&id) {
            return Ok(id);
        }
    }
    Err(PiError::other(
        "could not generate a collision-free session entry id",
    ))
}

/// The device and inode pair that identifies a file.
pub fn file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    (metadata.dev(), metadata.ino())
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

/// The first 16 hex characters of the SHA-256 of the canonical workspace path.
pub fn workspace_key(workspace: &Path) -> Result<String, PiError> {
    let canonical = canonical_workspace(workspace)?;
    Ok(hex_sha256(canonical.as_bytes())[..16].to_owned())
}

/// Same as [`workspace_key`] for a path that is already canonical.
pub fn workspace_key_of_canonical(canonical: &str) -> String {
    hex_sha256(canonical.as_bytes())[..16].to_owned()
}

fn hex_sha256(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Go's `path/filepath.Clean` semantics, which the on-disk session paths are
/// written with: `..` is collapsed lexically, which Rust's `std::path` does
/// not do.
pub fn clean_go_path(path: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

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
