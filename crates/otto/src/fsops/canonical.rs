//! Canonical forms of a directory and an executable file.

use std::path::{Path, PathBuf};

/// Resolves `path` to an existing directory with every symlink followed.
pub fn canonical_directory(path: &Path) -> Result<PathBuf, std::io::Error> {
    let canonical = std::fs::canonicalize(path)?;
    if !std::fs::metadata(&canonical)?.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            format!("not a directory: {}", canonical.display()),
        ));
    }
    Ok(canonical)
}

/// Resolves `path` to an existing, regular, executable file.
///
/// The final component is re-checked with `lstat` after canonicalization so a
/// symlink swapped in between the two calls cannot pass as the shell.
pub(crate) fn canonical_executable_file(path: &Path) -> Result<PathBuf, std::io::Error> {
    use std::io::{Error, ErrorKind};
    use std::os::unix::fs::PermissionsExt;

    let invalid = || Error::from(ErrorKind::InvalidInput);
    let Some(text) = path.to_str() else {
        return Err(invalid());
    };
    if text.is_empty() || text.contains('\0') {
        return Err(invalid());
    }
    let canonical = std::fs::canonicalize(path).map_err(|_| invalid())?;
    let metadata = std::fs::symlink_metadata(&canonical).map_err(|_| invalid())?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(invalid());
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shell_must_be_a_regular_executable_file() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(canonical_executable_file(Path::new("/bin/sh")).is_ok());
        assert!(canonical_executable_file(Path::new("")).is_err());
        assert!(canonical_executable_file(dir.path()).is_err());
        assert!(canonical_executable_file(Path::new("/bin/does-not-exist")).is_err());
    }

    #[test]
    fn a_canonical_directory_follows_symlinks_and_rejects_files() {
        let dir = tempfile::tempdir().expect("temp dir");
        let real = dir.path().join("real");
        std::fs::create_dir(&real).expect("mkdir");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("symlink");
        assert_eq!(
            canonical_directory(&link).expect("resolve"),
            std::fs::canonicalize(&real).expect("canonical")
        );
        let file = dir.path().join("file");
        std::fs::write(&file, "").expect("write");
        assert!(canonical_directory(&file).is_err());
    }
}
