//! The Seatbelt driver's private per-session state directory.
//!
//! The tree lives beneath a cache base outside the workspace and holds the
//! child's `HOME`, `TMPDIR`, cache root and the generated profile. Every step
//! is performed through directory descriptors with `O_NOFOLLOW` and
//! re-validated against the inode identities recorded at creation, so a
//! concurrent rename or symlink swap can never redirect a write or a removal
//! outside the tree this process built. Removal restores the owner's access
//! through those descriptors first, so a tree the child left read-only — a Go
//! module cache entry, an installed plugin package — is still removed.
//!
//! Ownership: [`State`] owns the parent and root directory descriptors and
//! closes them in [`State::close`]. Concurrency: [`State::write_profile`] and
//! [`State::close`] take an internal mutex, so a [`State`] may be shared across
//! threads. Cancellation: none of these operations are cancellable; they are
//! short, local filesystem calls. Errors: every failure collapses to
//! [`Error::Create`] or [`Error::Cleanup`], whose messages are fixed, short and
//! carry no path text.

use std::collections::HashMap;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt as _;
use std::sync::{Arc, Mutex};

use nix::errno::Errno;
use nix::fcntl::{AtFlags, OFlag};
use nix::sys::stat::{FileStat, Mode, SFlag};
use nix::unistd::UnlinkatFlags;

use super::profile;
use crate::sandbox::PrivateDirectories;

mod cleanup;
mod construction;
mod session;
mod verification;
use cleanup::*;
use construction::*;
pub(super) use verification::*;

/// Every private state root is named with this prefix so cleanup can recognise
/// its own leaves and refuse to touch anything else in the cache base.
pub(crate) const LEAF_PREFIX: &str = "otto-sandbox-";

/// Random bytes in a leaf name, hex encoded into 32 characters.
const LEAF_RANDOM_BYTES: usize = 16;

/// How many distinct leaf names are tried before construction gives up.
const LEAF_MAX_ATTEMPTS: usize = 128;

/// Every directory in the tree is owner-only.
const DIRECTORY_MODE: u32 = 0o700;

/// The generated profile is owner read/write and nothing else.
const PROFILE_FILE_MODE: u32 = 0o600;

/// The fixed children of the state root, in creation order.
const CHILD_NAMES: [&str; 4] = ["home", "tmp", "cache", "profiles"];

/// The generated profile's fixed leaf name inside `profiles`.
const PROFILE_LEAF: &str = "profile.sb";

/// Why the private state is unusable.
///
/// Both messages are fixed strings: they are surfaced to the model and must
/// never leak the randomised leaf name or any host path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(crate) enum Error {
    /// The tree could not be created, validated or written.
    #[error("seatbelt private state unavailable")]
    Create,
    /// The tree could not be removed, or was removed only in part.
    #[error("seatbelt private state cleanup failed")]
    Cleanup,
}

/// Which construction step an [`Event`] reports.
///
/// The hook exists so tests can mutate the tree at exactly the moment the
/// production code has finished one step and is about to validate the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    RootCreated,
    RootValidated,
    DirectoryCreated,
    ProfileCreated,
    ProfileWrite,
    Cleanup,
    FinalValidation,
}

/// One construction or teardown step.
///
/// Production only builds events and hands them to an installed hook; the
/// fields exist for the hook, and only the tests install one.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
pub(crate) struct Event {
    pub(crate) kind: EventKind,
    pub(crate) name: String,
    pub(crate) path: String,
}

/// A filesystem object's device and inode pair.
///
/// The all-zero value means "unknown" and never compares equal to a real
/// object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

/// The `lstat` facts this module needs: the three questions the validation asks
/// of a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Facts {
    pub(crate) symlink: bool,
    pub(crate) directory: bool,
    pub(crate) identity: Identity,
}

type Canonicalize = dyn Fn(&str) -> Result<String, Error> + Send + Sync;
type Lstat = dyn Fn(&str) -> Result<Facts, Error> + Send + Sync;
type CurrentUid = dyn Fn() -> u32 + Send + Sync;
type RandomBytes = dyn Fn(&mut [u8]) -> Result<(), Error> + Send + Sync;
type Mkdirat = dyn Fn(BorrowedFd<'_>, &str, u32) -> Result<(), Errno> + Send + Sync;
type CloseFd = dyn Fn(OwnedFd) -> Result<(), Errno> + Send + Sync;
type EventHook = dyn Fn(&Event) + Send + Sync;

/// The injection points construction and teardown go through.
///
/// Production uses [`Operations::default`]. Tests replace individual hooks to
/// simulate a hostile filesystem: `mkdirat` can create a symlink where a
/// directory was asked for, `close_fd` can fail after the descriptor number has
/// been recycled, and `event` can rename or replace the tree mid-construction.
#[derive(Clone)]
pub(crate) struct Operations {
    pub(crate) canonicalize: Arc<Canonicalize>,
    pub(crate) lstat: Arc<Lstat>,
    pub(crate) current_uid: Arc<CurrentUid>,
    pub(crate) random_bytes: Arc<RandomBytes>,
    pub(crate) mkdirat: Arc<Mkdirat>,
    pub(crate) close_fd: Arc<CloseFd>,
    pub(crate) event: Option<Arc<EventHook>>,
}

impl Default for Operations {
    fn default() -> Self {
        Self {
            canonicalize: Arc::new(|path| {
                profile::canonical_filesystem_path(path).map_err(|_| Error::Create)
            }),
            lstat: Arc::new(lstat_facts),
            current_uid: Arc::new(|| nix::unistd::Uid::effective().as_raw()),
            random_bytes: Arc::new(fill_random),
            mkdirat: Arc::new(|parent, name, mode| {
                nix::sys::stat::mkdirat(parent, name, Mode::from_bits_truncate(mode as _))
            }),
            close_fd: Arc::new(|fd: OwnedFd| nix::unistd::close(fd)),
            event: None,
        }
    }
}

impl std::fmt::Debug for Operations {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Operations")
            .field("event", &self.event.is_some())
            .finish_non_exhaustive()
    }
}

/// `lstat` reduced to [`Facts`].
fn lstat_facts(path: &str) -> Result<Facts, Error> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| Error::Create)?;
    Ok(Facts {
        symlink: metadata.file_type().is_symlink(),
        directory: metadata.is_dir(),
        identity: Identity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    })
}

/// Fills `destination` from `/dev/urandom`.
///
/// The leaf name only needs to be unpredictable to another process racing for
/// the same name; the tree's safety rests on the inode checks, not on secrecy.
fn fill_random(destination: &mut [u8]) -> Result<(), Error> {
    use std::io::Read as _;
    let mut source = std::fs::File::open("/dev/urandom").map_err(|_| Error::Create)?;
    source.read_exact(destination).map_err(|_| Error::Create)
}

/// A validated private state tree.
///
/// The struct is created by [`create`] and is valid until [`State::close`]
/// returns. Its path fields are canonical and fixed for the lifetime of the
/// value; the descriptors behind them are re-validated on every operation.
pub(crate) struct State {
    pub(crate) directories: PrivateDirectories,
    pub(crate) profiles: String,
    pub(crate) profile_path: String,
    pub(crate) root_parent: String,
    root_text: String,
    root_name: String,
    root_identity: Identity,
    child_identities: HashMap<String, Identity>,
    profile_identity: Identity,
    event: Option<Arc<EventHook>>,
    lstat: Arc<Lstat>,
    close_fd: Arc<CloseFd>,
    inner: Mutex<Inner>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("State").finish_non_exhaustive()
    }
}

/// The mutable half, guarded by one mutex.
struct Inner {
    parent_file: Option<OwnedFd>,
    root_file: Option<OwnedFd>,
    closed: bool,
    close_result: Option<Result<(), Error>>,
}

/// Creates a private state tree beneath `cache_base` for `workspace`.
///
/// Both arguments must be absolute, cleaned paths naming existing directories.
/// The canonical cache base must lie outside the canonical workspace, so a
/// workspace-relative symlink cannot pull the private tree into reviewed
/// territory.
pub(crate) fn create(workspace: &str, cache_base: &str) -> Result<State, Error> {
    create_with_operations(workspace, cache_base, &Operations::default())
}

/// [`create`] with the filesystem primitives replaced.
pub(crate) fn create_with_operations(
    workspace: &str,
    cache_base: &str,
    operations: &Operations,
) -> Result<State, Error> {
    let (canonical_workspace, _) = canonical_state_directory(workspace, operations)?;
    let (canonical_cache, cache_facts) = canonical_state_directory(cache_base, operations)?;
    if profile::path_within(&canonical_workspace, &canonical_cache) {
        return Err(Error::Create);
    }

    let parent_file = open_state_directory_path(&canonical_cache)?;
    if !descriptor_matches_facts(&parent_file, cache_facts) {
        return Err(Error::Create);
    }

    let (root_name, root_path, root_edge) =
        create_random_state_root(&parent_file, &canonical_cache, operations)?;

    let mut partial = Partial {
        parent_file: Some(parent_file),
        root_name,
        root_path,
        root_edge,
        root_file: None,
        root_identity: None,
        child_identities: HashMap::with_capacity(CHILD_NAMES.len()),
        profile_identity: None,
    };
    match build_state(
        &mut partial,
        &canonical_workspace,
        &canonical_cache,
        operations,
    ) {
        Ok(state) => Ok(state),
        Err(error) => {
            partial.discard();
            Err(error)
        }
    }
}

impl State {}

fn emit(hook: Option<&EventHook>, kind: EventKind, name: &str, path: &str) {
    if let Some(hook) = hook {
        hook(&Event {
            kind,
            name: name.to_string(),
            path: path.to_string(),
        });
    }
}

#[cfg(test)]
mod tests;
