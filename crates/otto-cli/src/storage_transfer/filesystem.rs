//! Path validation, pinned directory access and the destination ownership marker.

use super::*;
use otto::hex::to_hex;

pub(super) fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(invalid(
            "migration ID must contain 1-128 letters, numbers, '-' or '_'",
        ));
    }
    Ok(())
}

pub(super) fn invalid(message: &str) -> migration::Error {
    migration::Error::Invalid(message.into())
}
pub(super) fn unavailable(error: impl std::fmt::Display) -> migration::Error {
    migration::Error::Unavailable(error.to_string())
}
pub(super) fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir().map_err(unavailable)?.join(path)
    };
    if path
        .components()
        .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(invalid(
            "storage paths must not contain '.' or '..' traversal",
        ));
    }
    if path.to_str().is_none() {
        return Err(invalid("storage path is not UTF-8"));
    }
    Ok(path)
}
pub(super) fn pin_directory(path: &Path) -> std::io::Result<fsops::Dir> {
    let mut root = fsops::open_dir_no_follow(Path::new("/"))?;
    for part in path.components() {
        match part {
            Component::RootDir => (),
            Component::Normal(name) => {
                root = fsops::open_dir_at_no_follow(
                    &root,
                    name.to_str().ok_or_else(|| {
                        std::io::Error::new(std::io::ErrorKind::InvalidInput, "non-UTF8 directory")
                    })?,
                )?
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "path traversal",
                ));
            }
        }
    }
    Ok(root)
}
pub(super) fn open_file(path: &Path, flags: libc::c_int) -> std::io::Result<File> {
    let parent = pin_directory(path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no parent")
    })?)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "file has no name"))?;
    let file = fsops::open_at_no_follow(&parent, name, flags | libc::O_NONBLOCK)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "not a regular state file",
        ));
    }
    Ok(file)
}
pub(super) fn present_file(path: &Path) -> Result<bool> {
    match open_file(path, libc::O_RDONLY) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(unavailable(error)),
    }
}
pub(super) fn identity(path: &Path) -> String {
    format!(
        "local:{}",
        to_hex(&Sha256::digest(path.as_os_str().as_encoded_bytes()))
    )
}
pub(super) fn endpoint(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| invalid("non-UTF8 endpoint"))
}
pub(super) fn mkdir(root: &fsops::Dir, name: &str) -> Result<fsops::Dir> {
    match fsops::mkdir_at(root, name, 0o700) {
        Ok(()) => (),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(unavailable(error)),
    }
    fsops::open_dir_at_no_follow(root, name).map_err(unavailable)
}
pub(super) fn sync_directory(root: &fsops::Dir) -> Result<()> {
    let file = fsops::open_at_no_follow(root, ".", libc::O_RDONLY | libc::O_DIRECTORY)
        .map_err(unavailable)?;
    file.sync_all().map_err(unavailable)
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(super) struct Owner {
    pub(super) version: u32,
    pub(super) migration_id: String,
    pub(super) source_identity: String,
    pub(super) destination_identity: String,
}
pub(super) fn private_root(root: &fsops::Dir) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let file = fsops::open_at_no_follow(root, ".", libc::O_RDONLY | libc::O_DIRECTORY)
        .map_err(unavailable)?;
    if file.metadata().map_err(unavailable)?.permissions().mode() & 0o777 != 0o700 {
        return Err(invalid("migration staging directory must have mode 0700"));
    }
    Ok(())
}
pub(super) fn read_owner(root: &fsops::Dir) -> Result<Owner> {
    let mut file = fsops::open_at_no_follow(root, OWNER, libc::O_RDONLY)
        .map_err(|_| invalid("existing destination is not this migration's isolated stage"))?;
    if !file.metadata().map_err(unavailable)?.is_file()
        || file.metadata().map_err(unavailable)?.len() > MAX_CONFIG
    {
        return Err(invalid("invalid destination ownership marker"));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(MAX_CONFIG + 1)
        .read_to_end(&mut bytes)
        .map_err(unavailable)?;
    if bytes.len() as u64 > MAX_CONFIG {
        return Err(invalid("destination ownership marker exceeds limit"));
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid("invalid destination ownership marker"))
}
