//! Host-side fixtures for the Seatbelt startup self-test.
//!
//! Port of `internal/sandbox/seatbelt/selftest_fixture_darwin.go`. The driver
//! proves, before it accepts any caller work, that the generated profile
//! actually allows the reads and writes it promises and actually denies the
//! ones it forbids. Doing that needs real files, and those files are the
//! attack surface of the check itself: a fixture that another process can swap
//! for a symlink would turn the proof into its opposite.
//!
//! Ownership: every fixture keeps its parent directory descriptor and its own
//! file descriptor open for the whole self-test, and every check is made
//! through those descriptors rather than through the path. The path is handed
//! to the child only after the descriptor and the directory entry have both
//! been confirmed to name the same inode.
//!
//! Concurrency: a [`Fixtures`] value is created, used and destroyed inside one
//! call to the driver's startup self-test and is never shared.
//!
//! Errors: every failure is [`Error`], one fixed string, because the reason a
//! fixture could not be trusted must not reach the model.

use std::fs::File;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::FileExt as _;

use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use super::state;

/// The Seatbelt self-test could not build or trust its fixtures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("seatbelt self-test fixture unavailable")]
pub(super) struct Error;

const PREFIX: &str = ".otto-self-test-";
const RANDOM_BYTES: usize = 16;
const MAX_ATTEMPTS: usize = 128;
const MODE: u32 = 0o600;

/// Which promise of the profile one fixture exercises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A file under the private `HOME` the child must be able to read.
    AllowedRead,
    /// A file in the workspace the child must be able to write.
    AllowedWorkspaceWrite,
    /// A file under the private `TMPDIR` the child must be able to write.
    AllowedPrivateWrite,
    /// A file under the profile directory the child must not be able to read.
    DeniedRead,
    /// A file under the profile directory the child must not be able to write.
    DeniedWrite,
}

/// One fixture file, held open for the whole self-test.
pub(super) struct Fixture {
    kind: Kind,
    parent_path: String,
    parent: OwnedFd,
    name: String,
    path: String,
    identity: state::Identity,
    file: File,
}

impl Fixture {
    /// The absolute path handed to the probe child.
    pub(super) fn path(&self) -> &str {
        &self.path
    }

    /// Whether the open descriptor is still the regular, 0600, self-owned,
    /// single-linked file this fixture created.
    fn descriptor_matches(&self) -> bool {
        state::descriptor_stat(&self.file).is_ok_and(|stat| valid_stat(&stat, self.identity))
    }

    /// Whether the directory entry still names that same file.
    ///
    /// Checked separately from the descriptor so a fixture replaced between
    /// two probes is caught: the descriptor would still be the original inode
    /// while the name now resolves to the attacker's file.
    fn edge_matches(&self) -> bool {
        if !state::directory_still_matches(&self.parent_path, &self.parent) {
            return false;
        }
        nix::sys::stat::fstatat(
            self.parent.as_fd(),
            self.name.as_str(),
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .is_ok_and(|stat| valid_stat(&stat, self.identity))
    }

    /// Whether both views still agree.
    fn trusted(&self) -> bool {
        self.descriptor_matches() && self.edge_matches()
    }

    /// The seven arguments the write probe uses to re-identify this file from
    /// inside the sandbox: path, device, inode, expected type, expected owner,
    /// expected permissions and expected link count.
    fn write_probe_arguments(&self) -> Result<Vec<String>, Error> {
        if !self.trusted() {
            return Err(Error);
        }
        Ok(vec![
            self.path.clone(),
            self.identity.device.to_string(),
            self.identity.inode.to_string(),
            u32::from(nix::sys::stat::SFlag::S_IFREG.bits()).to_string(),
            nix::unistd::Uid::effective().as_raw().to_string(),
            MODE.to_string(),
            "1".to_string(),
        ])
    }

    /// Reads the file back through the retained descriptor and compares it to
    /// `contents`, re-checking the identity on both sides of the read.
    ///
    /// Reads one byte more than expected so a longer file is a mismatch.
    pub(super) fn validate_contents(&self, contents: &str) -> Result<(), Error> {
        if !self.trusted() {
            return Err(Error);
        }
        let limit = contents.len() + 1;
        let mut buffer = vec![0u8; limit];
        let mut filled = 0;
        while filled < limit {
            match self.file.read_at(&mut buffer[filled..], filled as u64) {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(Error),
            }
        }
        if buffer[..filled] != *contents.as_bytes() || !self.trusted() {
            return Err(Error);
        }
        Ok(())
    }
}

/// The five fixtures the startup self-test needs, created together.
pub(super) struct Fixtures {
    values: Vec<Fixture>,
}

impl Fixtures {
    /// Creates every fixture, cleaning up whatever was built if any step fails.
    ///
    /// `allowed_read` is the content of the readable fixture and `denied` the
    /// content of both unreadable ones; the writable fixtures start empty
    /// because the probe truncates them.
    pub(super) fn prepare(
        home: &str,
        workspace: &str,
        temp: &str,
        profiles: &str,
        allowed_read: &str,
        denied: &str,
    ) -> Result<Self, Error> {
        let definitions: [(Kind, &str, &str); 5] = [
            (Kind::AllowedRead, home, allowed_read),
            (Kind::AllowedWorkspaceWrite, workspace, ""),
            (Kind::AllowedPrivateWrite, temp, ""),
            (Kind::DeniedRead, profiles, denied),
            (Kind::DeniedWrite, profiles, denied),
        ];
        let mut fixtures = Self {
            values: Vec::with_capacity(definitions.len()),
        };
        for (kind, parent, contents) in definitions {
            match prepare_one(kind, parent, contents.as_bytes()) {
                Ok(fixture) => fixtures.values.push(fixture),
                Err(error) => {
                    let _ = fixtures.cleanup();
                    return Err(error);
                }
            }
        }
        Ok(fixtures)
    }

    pub(super) fn get(&self, kind: Kind) -> Result<&Fixture, Error> {
        self.values
            .iter()
            .find(|fixture| fixture.kind == kind)
            .ok_or(Error)
    }

    /// The fourteen arguments describing both writable fixtures, in the order
    /// the Perl write probe reads them.
    pub(super) fn write_probe_arguments(&self) -> Result<Vec<String>, Error> {
        let mut arguments = Vec::with_capacity(14);
        for kind in [Kind::AllowedWorkspaceWrite, Kind::AllowedPrivateWrite] {
            arguments.extend(self.get(kind)?.write_probe_arguments()?);
        }
        Ok(arguments)
    }

    /// Re-checks both writable fixtures immediately before the probe starts,
    /// closing the window between building the argument list and dispatching.
    pub(super) fn validate_before_write_dispatch(&self) -> Result<(), Error> {
        for kind in [Kind::AllowedWorkspaceWrite, Kind::AllowedPrivateWrite] {
            if !self.get(kind)?.trusted() {
                return Err(Error);
            }
        }
        Ok(())
    }

    /// Confirms the probe wrote `contents` to both writable fixtures.
    pub(super) fn validate_written_contents(&self, contents: &str) -> Result<(), Error> {
        for kind in [Kind::AllowedWorkspaceWrite, Kind::AllowedPrivateWrite] {
            self.get(kind)?.validate_contents(contents)?;
        }
        Ok(())
    }

    /// Unlinks every fixture through its parent descriptor and closes both
    /// descriptors, reporting failure if any file could not be confirmed gone.
    pub(super) fn cleanup(&mut self) -> Result<(), Error> {
        let mut failed = false;
        for fixture in self.values.drain(..) {
            if !fixture.trusted() {
                failed = true;
                continue;
            }
            let unlinked = nix::unistd::unlinkat(
                fixture.parent.as_fd(),
                fixture.name.as_str(),
                nix::unistd::UnlinkatFlags::NoRemoveDir,
            );
            let remaining = nix::sys::stat::fstatat(
                fixture.parent.as_fd(),
                fixture.name.as_str(),
                nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
            );
            let unlink_ok = matches!(unlinked, Ok(()) | Err(nix::errno::Errno::ENOENT));
            if !unlink_ok || !matches!(remaining, Err(nix::errno::Errno::ENOENT)) {
                failed = true;
            }
        }
        if failed { Err(Error) } else { Ok(()) }
    }
}

/// Creates one fixture under `parent_path`, retrying on name collision.
fn prepare_one(kind: Kind, parent_path: &str, contents: &[u8]) -> Result<Fixture, Error> {
    let parent = state::open_verified_directory(parent_path).map_err(|_| Error)?;
    for _ in 0..MAX_ATTEMPTS {
        let name = format!("{PREFIX}{}", random_suffix()?);
        match create(&parent, parent_path, &name, contents) {
            Ok((file, identity)) => {
                return Ok(Fixture {
                    kind,
                    parent_path: parent_path.to_string(),
                    parent,
                    path: super::profile::join(parent_path, &name),
                    name,
                    identity,
                    file,
                });
            }
            Err(CreateError::Collision) => continue,
            Err(CreateError::Fatal) => return Err(Error),
        }
    }
    Err(Error)
}

/// Why one creation attempt did not produce a usable fixture.
enum CreateError {
    /// The name was already taken; the caller retries with a fresh one.
    Collision,
    /// The fixture could not be trusted; retrying would not help.
    Fatal,
}

/// Creates and validates one fixture file, removing it again if anything about
/// it cannot be trusted.
///
/// Every validation happens twice, once before and once after the write, so a
/// file replaced during the write is rejected rather than reported as written.
fn create(
    parent: &OwnedFd,
    parent_path: &str,
    name: &str,
    contents: &[u8],
) -> Result<(File, state::Identity), CreateError> {
    let opened = nix::fcntl::openat(
        parent.as_fd(),
        name,
        OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::from_bits_truncate(MODE as _),
    );
    let file = match opened {
        Ok(fd) => File::from(fd),
        // Only a name collision is retryable; every other errno means the
        // parent directory itself cannot be trusted.
        Err(nix::errno::Errno::EEXIST) => return Err(CreateError::Collision),
        Err(_) => return Err(CreateError::Fatal),
    };

    let mut trusted = false;
    let mut identity = state::Identity::default();
    if let Ok(stat) = state::descriptor_stat(&file) {
        identity = state::identity_from(&stat);
        trusted = valid_identity_stat(&stat, identity)
            && edge_identity_matches(parent, parent_path, name, identity)
            && file
                .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(MODE))
                .is_ok()
            && descriptor_and_edge_match(&file, parent, parent_path, name, identity)
            && write_all(&file, contents)
            && descriptor_and_edge_match(&file, parent, parent_path, name, identity);
    }
    if !trusted {
        let _ = nix::unistd::unlinkat(
            parent.as_fd(),
            name,
            nix::unistd::UnlinkatFlags::NoRemoveDir,
        );
        return Err(CreateError::Fatal);
    }
    Ok((file, identity))
}

fn descriptor_and_edge_match(
    file: &File,
    parent: &OwnedFd,
    parent_path: &str,
    name: &str,
    identity: state::Identity,
) -> bool {
    state::descriptor_stat(file).is_ok_and(|stat| valid_stat(&stat, identity))
        && state::directory_still_matches(parent_path, parent)
        && nix::sys::stat::fstatat(
            parent.as_fd(),
            name,
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .is_ok_and(|stat| valid_stat(&stat, identity))
}

fn edge_identity_matches(
    parent: &OwnedFd,
    parent_path: &str,
    name: &str,
    identity: state::Identity,
) -> bool {
    state::directory_still_matches(parent_path, parent)
        && nix::sys::stat::fstatat(
            parent.as_fd(),
            name,
            nix::fcntl::AtFlags::AT_SYMLINK_NOFOLLOW,
        )
        .is_ok_and(|stat| valid_identity_stat(&stat, identity))
}

/// The kind, owner and link-count check, without the permission bits, used
/// before the explicit `fchmod` has run.
fn valid_identity_stat(stat: &nix::sys::stat::FileStat, identity: state::Identity) -> bool {
    let mode = u32::from(stat.st_mode);
    mode & u32::from(nix::sys::stat::SFlag::S_IFMT.bits())
        == u32::from(nix::sys::stat::SFlag::S_IFREG.bits())
        && stat.st_uid == nix::unistd::Uid::effective().as_raw()
        && stat.st_nlink == 1
        && state::same_identity(state::identity_from(stat), identity)
}

/// The full check: a regular, 0600, self-owned, single-linked file with the
/// expected device and inode.
fn valid_stat(stat: &nix::sys::stat::FileStat, identity: state::Identity) -> bool {
    state::secure_state_stat(stat, false, MODE, nix::unistd::Uid::effective().as_raw())
        && stat.st_nlink == 1
        && state::same_identity(state::identity_from(stat), identity)
}

fn write_all(file: &File, contents: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset < contents.len() {
        match file.write_at(&contents[offset..], offset as u64) {
            Ok(0) => return false,
            Ok(written) => offset += written,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return false,
        }
    }
    true
}

fn random_suffix() -> Result<String, Error> {
    use std::io::Read as _;
    let mut bytes = [0u8; RANDOM_BYTES];
    let mut source = std::fs::File::open("/dev/urandom").map_err(|_| Error)?;
    source.read_exact(&mut bytes).map_err(|_| Error)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}
