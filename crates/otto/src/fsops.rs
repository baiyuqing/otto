//! Compatibility facade for shared pinned POSIX operations.
//! Domain-specific Pi errors and workspace path normalization stay here.
pub use crate::storage::drivers::posix::*;
use otto_core::session::PiError;
use std::{fs::File, io, path::Path};

pub fn lock_directory(dir: &Dir) -> Result<File, PiError> {
    crate::storage::drivers::posix::lock_directory(dir)
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
