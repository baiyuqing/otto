//! Teardown of a state tree through verified descriptors.

use super::*;

/// Whether `root` is a direct `otto-sandbox-` child of `parent`.
pub(super) fn safe_state_leaf(parent: &str, root: &str) -> bool {
    if parent.is_empty()
        || root.is_empty()
        || !profile::is_absolute(parent)
        || !profile::is_absolute(root)
        || profile::clean(parent) != parent
        || profile::clean(root) != root
        || profile::dir(root) != parent
    {
        return false;
    }
    let leaf = base(root);
    leaf.starts_with(LEAF_PREFIX) && leaf.len() > LEAF_PREFIX.len()
}

/// What a removal is allowed to find at one name, and beneath it.
#[derive(Debug, Clone, Default)]
struct CleanupExpectation {
    identity: Identity,
    children: HashMap<String, CleanupExpectation>,
}

/// Removes a fully constructed tree.
pub(super) fn cleanup_state_root(
    parent_file: &OwnedFd,
    root_file: &OwnedFd,
    root_name: &str,
    root_identity: Identity,
    expected_children: &HashMap<String, Identity>,
) -> Result<(), Error> {
    let expectations = expected_children
        .iter()
        .map(|(name, identity)| {
            (
                name.clone(),
                CleanupExpectation {
                    identity: *identity,
                    children: HashMap::new(),
                },
            )
        })
        .collect();
    cleanup_state_root_with_expectations(
        parent_file,
        root_file,
        root_name,
        root_identity,
        &expectations,
        false,
    )
}

/// Removes a partially constructed tree in strict mode, where any directory the
/// construction did not itself create stops the removal.
pub(super) fn cleanup_partial_state_root(
    parent_file: &OwnedFd,
    root_file: &OwnedFd,
    root_name: &str,
    root_identity: Identity,
    child_identities: &HashMap<String, Identity>,
    profile_identity: Option<Identity>,
) -> Result<(), Error> {
    let expectations = child_identities
        .iter()
        .map(|(name, identity)| {
            let mut children = HashMap::new();
            if name == "profiles"
                && let Some(profile_identity) = profile_identity
            {
                children.insert(
                    PROFILE_LEAF.to_string(),
                    CleanupExpectation {
                        identity: profile_identity,
                        children: HashMap::new(),
                    },
                );
            }
            (
                name.clone(),
                CleanupExpectation {
                    identity: *identity,
                    children,
                },
            )
        })
        .collect();
    cleanup_state_root_with_expectations(
        parent_file,
        root_file,
        root_name,
        root_identity,
        &expectations,
        true,
    )
}

fn cleanup_state_root_with_expectations(
    parent_file: &OwnedFd,
    root_file: &OwnedFd,
    root_name: &str,
    root_identity: Identity,
    expected_children: &HashMap<String, CleanupExpectation>,
    strict: bool,
) -> Result<(), Error> {
    let uid = nix::unistd::Uid::effective().as_raw();
    if !valid_state_entry_name(root_name)
        || !descriptor_matches_expected(root_file, root_identity, true, DIRECTORY_MODE, uid)
    {
        return Err(Error::Cleanup);
    }
    let edge_ok = edge_matches(
        parent_file.as_fd(),
        root_name,
        root_identity,
        true,
        DIRECTORY_MODE,
        uid,
    );
    let contents = remove_state_directory_contents(root_file.as_fd(), expected_children, strict);
    if contents.is_err()
        || !edge_ok
        || !edge_matches(
            parent_file.as_fd(),
            root_name,
            root_identity,
            true,
            DIRECTORY_MODE,
            uid,
        )
    {
        return Err(Error::Cleanup);
    }
    match nix::unistd::unlinkat(parent_file, root_name, UnlinkatFlags::RemoveDir) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(_) => Err(Error::Cleanup),
    }
}

/// Restores the owner's read, write and search bits on a directory whose
/// contents are about to be removed.
///
/// An entry can only be removed through a parent the owner may write and
/// search, and the child can leave directories it created read-only: the Go
/// module cache, otto's own plugin store and `install -m` all do. The
/// descriptor is the one the caller already matched against the identities
/// recorded at creation, so the change cannot reach an entry this process did
/// not build, and the directory is removed by the same cleanup.
fn restore_owner_access(directory_fd: BorrowedFd<'_>) -> Result<(), Error> {
    let stat = nix::sys::stat::fstat(directory_fd).map_err(|_| Error::Cleanup)?;
    let mode = u32::from(stat.st_mode) & 0o777;
    if mode & 0o700 == 0o700 {
        return Ok(());
    }
    nix::sys::stat::fchmod(directory_fd, Mode::from_bits_truncate((mode | 0o700) as _))
        .map_err(|_| Error::Cleanup)
}

/// Removes everything inside the directory `directory_fd` holds open.
fn remove_state_directory_contents(
    directory_fd: BorrowedFd<'_>,
    expected_children: &HashMap<String, CleanupExpectation>,
    strict: bool,
) -> Result<(), Error> {
    restore_owner_access(directory_fd)?;
    let read_fd = nix::fcntl::openat(
        directory_fd,
        ".",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| Error::Cleanup)?;
    let mut directory = nix::dir::Dir::from_fd(read_fd).map_err(|_| Error::Cleanup)?;
    let mut names = Vec::new();
    for entry in directory.iter() {
        let entry = entry.map_err(|_| Error::Cleanup)?;
        let Ok(name) = entry.file_name().to_str() else {
            return Err(Error::Cleanup);
        };
        if name == "." || name == ".." {
            continue;
        }
        names.push(name.to_string());
    }
    drop(directory);

    let mut children_ok = true;
    for name in names {
        let expected = expected_children.get(&name);
        if remove_state_entry(directory_fd, &name, expected, strict).is_err() {
            children_ok = false;
        }
    }
    if children_ok {
        Ok(())
    } else {
        Err(Error::Cleanup)
    }
}

/// Removes one entry only if it is still the exact object `expected` describes.
pub(super) fn remove_expected_state_edge(
    parent_fd: BorrowedFd<'_>,
    name: &str,
    expected: FileStat,
) -> Result<(), Error> {
    if !valid_state_entry_name(name) {
        return Err(Error::Cleanup);
    }
    let current = match nix::sys::stat::fstatat(parent_fd, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
        Ok(current) => current,
        Err(Errno::ENOENT) => return Ok(()),
        Err(_) => return Err(Error::Cleanup),
    };
    if !same_identity(identity_from(&current), identity_from(&expected))
        || u32::from(current.st_mode) & u32::from(SFlag::S_IFMT.bits())
            != u32::from(expected.st_mode) & u32::from(SFlag::S_IFMT.bits())
    {
        return Err(Error::Cleanup);
    }
    let flags = if u32::from(current.st_mode) & u32::from(SFlag::S_IFMT.bits())
        == u32::from(SFlag::S_IFDIR.bits())
    {
        UnlinkatFlags::RemoveDir
    } else {
        UnlinkatFlags::NoRemoveDir
    };
    match nix::unistd::unlinkat(parent_fd, name, flags) {
        Ok(()) | Err(Errno::ENOENT) => Ok(()),
        Err(_) => Err(Error::Cleanup),
    }
}

/// Removes one entry, descending into it only when its inode is one the tree
/// recorded. Three attempts absorb a racing rename or remove.
fn remove_state_entry(
    parent_fd: BorrowedFd<'_>,
    name: &str,
    expected: Option<&CleanupExpectation>,
    strict: bool,
) -> Result<(), Error> {
    if !valid_state_entry_name(name) {
        return Err(Error::Cleanup);
    }
    for _ in 0..3 {
        let stat = match nix::sys::stat::fstatat(parent_fd, name, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::ENOENT) => return Ok(()),
            Err(_) => return Err(Error::Cleanup),
        };
        let stat_identity = identity_from(&stat);
        let is_directory = u32::from(stat.st_mode) & u32::from(SFlag::S_IFMT.bits())
            == u32::from(SFlag::S_IFDIR.bits());
        if is_directory
            && ((strict && expected.is_none())
                || expected
                    .is_some_and(|expected| !same_identity(stat_identity, expected.identity)))
        {
            return Err(Error::Cleanup);
        }
        if !is_directory {
            match nix::unistd::unlinkat(parent_fd, name, UnlinkatFlags::NoRemoveDir) {
                Ok(()) | Err(Errno::ENOENT) => return Ok(()),
                Err(Errno::EISDIR) | Err(Errno::EPERM) => continue,
                Err(_) => return Err(Error::Cleanup),
            }
        }

        let directory_file = match open_state_directory_at_raw(parent_fd, name) {
            Ok(file) => file,
            Err(Errno::ENOENT) | Err(Errno::ENOTDIR) | Err(Errno::ELOOP) => continue,
            Err(Errno::EACCES) if edge_identity_matches(parent_fd, name, stat_identity, true) => {
                match nix::unistd::unlinkat(parent_fd, name, UnlinkatFlags::RemoveDir) {
                    Ok(()) | Err(Errno::ENOENT) => return Ok(()),
                    Err(_) => return Err(Error::Cleanup),
                }
            }
            Err(_) => return Err(Error::Cleanup),
        };
        let Ok(opened_stat) = descriptor_stat(&directory_file) else {
            continue;
        };
        let opened_identity = identity_from(&opened_stat);
        if !same_identity(opened_identity, stat_identity)
            || expected.is_some_and(|expected| !same_identity(opened_identity, expected.identity))
        {
            continue;
        }
        let empty = HashMap::new();
        let expected_contents = expected.map_or(&empty, |expected| &expected.children);
        let remove_result =
            remove_state_directory_contents(directory_file.as_fd(), expected_contents, strict);
        if nix::unistd::close(directory_file).is_err() {
            return Err(Error::Cleanup);
        }
        if remove_result.is_err() {
            if !edge_identity_matches(parent_fd, name, opened_identity, true) {
                return Err(Error::Cleanup);
            }
            return match nix::unistd::unlinkat(parent_fd, name, UnlinkatFlags::RemoveDir) {
                Ok(()) | Err(Errno::ENOENT) => Ok(()),
                Err(_) => Err(Error::Cleanup),
            };
        }
        if !edge_identity_matches(parent_fd, name, opened_identity, true) {
            return Err(Error::Cleanup);
        }
        match nix::unistd::unlinkat(parent_fd, name, UnlinkatFlags::RemoveDir) {
            Ok(()) | Err(Errno::ENOENT) => return Ok(()),
            Err(Errno::ENOTDIR) | Err(Errno::ENOTEMPTY) | Err(Errno::EEXIST) => continue,
            Err(_) => return Err(Error::Cleanup),
        }
    }
    Err(Error::Cleanup)
}

/// Whether `name` is a single path component that is not `.` or `..`.
pub(super) fn valid_state_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}
