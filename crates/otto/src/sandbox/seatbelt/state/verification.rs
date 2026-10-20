//! Inode identity and descriptor re-validation helpers.

use super::*;

pub(in crate::sandbox::seatbelt) fn descriptor_stat<Fd: AsFd>(fd: Fd) -> Result<FileStat, Error> {
    nix::sys::stat::fstat(fd).map_err(|_| Error::Create)
}

/// Whether `stat` is exactly the kind, permission bits and owner expected.
///
/// The permission comparison covers all twelve mode bits, so a set-uid or
/// sticky bit added after creation is a rejection.
pub(in crate::sandbox::seatbelt) fn secure_state_stat(
    stat: &FileStat,
    directory: bool,
    permissions: u32,
    uid: u32,
) -> bool {
    let expected = if directory {
        SFlag::S_IFDIR
    } else {
        SFlag::S_IFREG
    };
    let mode = u32::from(stat.st_mode);
    mode & u32::from(SFlag::S_IFMT.bits()) == u32::from(expected.bits())
        && mode & 0o7777 == permissions
        && stat.st_uid == uid
}

pub(in crate::sandbox::seatbelt) fn identity_from(stat: &FileStat) -> Identity {
    Identity {
        device: stat.st_dev as u64,
        inode: stat.st_ino,
    }
}

/// Identity equality that treats the unknown value as matching nothing.
pub(in crate::sandbox::seatbelt) fn same_identity(left: Identity, right: Identity) -> bool {
    left == right && left != Identity::default()
}

/// Whether the directory entry `name` under `parent_fd` still names `identity`
/// with the expected kind, permissions and owner.
pub(super) fn edge_matches(
    parent_fd: BorrowedFd<'_>,
    name: &str,
    identity: Identity,
    directory: bool,
    permissions: u32,
    uid: u32,
) -> bool {
    if !valid_state_entry_name(name) {
        return false;
    }
    nix::sys::stat::fstatat(parent_fd, name, AtFlags::AT_SYMLINK_NOFOLLOW).is_ok_and(|stat| {
        secure_state_stat(&stat, directory, permissions, uid)
            && same_identity(identity_from(&stat), identity)
    })
}

/// The same edge check without the permission and owner requirements, used
/// while removing an entry whose mode may already have been tampered with.
pub(super) fn edge_identity_matches(
    parent_fd: BorrowedFd<'_>,
    name: &str,
    identity: Identity,
    directory: bool,
) -> bool {
    if !valid_state_entry_name(name) {
        return false;
    }
    let expected = if directory {
        SFlag::S_IFDIR
    } else {
        SFlag::S_IFREG
    };
    nix::sys::stat::fstatat(parent_fd, name, AtFlags::AT_SYMLINK_NOFOLLOW).is_ok_and(|stat| {
        u32::from(stat.st_mode) & u32::from(SFlag::S_IFMT.bits()) == u32::from(expected.bits())
            && same_identity(identity_from(&stat), identity)
    })
}

pub(super) fn descriptor_matches_expected(
    file: &OwnedFd,
    identity: Identity,
    directory: bool,
    permissions: u32,
    uid: u32,
) -> bool {
    descriptor_stat(file).is_ok_and(|stat| {
        secure_state_stat(&stat, directory, permissions, uid)
            && same_identity(identity_from(&stat), identity)
    })
}

/// Whether the open descriptor is the same directory the `lstat` facts describe.
pub(super) fn descriptor_matches_facts(file: &OwnedFd, expected: Facts) -> bool {
    descriptor_stat(file).is_ok_and(|stat| {
        u32::from(stat.st_mode) & u32::from(SFlag::S_IFMT.bits())
            == u32::from(SFlag::S_IFDIR.bits())
            && same_identity(identity_from(&stat), expected.identity)
    })
}

/// Whether `path` still resolves, without following a final symlink, to the
/// object the descriptor holds open.
pub(super) fn descriptor_path_matches(path: &str, file: &OwnedFd, lstat: &Arc<Lstat>) -> bool {
    let Ok(facts) = lstat(path) else {
        return false;
    };
    if facts.symlink {
        return false;
    }
    descriptor_stat(file).is_ok_and(|stat| same_identity(identity_from(&stat), facts.identity))
}

/// Opens `path` as a directory, refusing to follow any symlink, and verifies
/// that the descriptor still names the object `path` resolves to.
///
/// Used by the Seatbelt self-test fixtures, which need the same
/// symlink-refusing parent handle the state tree itself is built through. The
/// returned descriptor is owned by the caller.
pub(in crate::sandbox::seatbelt) fn open_verified_directory(path: &str) -> Result<OwnedFd, Error> {
    let file = open_state_directory_path(path)?;
    let lstat: Arc<Lstat> = Arc::new(lstat_facts);
    if !descriptor_path_matches(path, &file, &lstat) {
        return Err(Error::Create);
    }
    Ok(file)
}

/// Whether `path` still resolves, without following a final symlink, to the
/// directory `file` holds open.
pub(in crate::sandbox::seatbelt) fn directory_still_matches(path: &str, file: &OwnedFd) -> bool {
    let lstat: Arc<Lstat> = Arc::new(lstat_facts);
    descriptor_path_matches(path, file, &lstat)
}

/// The final component of the cleaned absolute paths this module uses.
pub(super) fn base(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[index + 1..].to_string(),
        None => path.to_string(),
    }
}
