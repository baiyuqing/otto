//! The Seatbelt driver's private per-session state directory.
//!
//! Port of `internal/sandbox/seatbelt/state_unix.go`. The tree lives beneath a
//! cache base outside the workspace and holds the child's `HOME`, `TMPDIR`,
//! cache root and the generated profile. Every step is performed through
//! directory descriptors with `O_NOFOLLOW` and re-validated against the inode
//! identities recorded at creation, so a concurrent rename or symlink swap can
//! never redirect a write or a removal outside the tree this process built.
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
/// object, mirroring Go's zero `stateIdentity`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

/// The `lstat` facts this module needs: Go's `fs.FileInfo` reduced to the three
/// questions the validation asks of it.
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

/// Everything construction has created so far, so a failure can undo exactly
/// what it made and nothing else.
struct Partial {
    parent_file: Option<OwnedFd>,
    root_name: String,
    root_path: String,
    root_edge: FileStat,
    root_file: Option<OwnedFd>,
    root_identity: Option<Identity>,
    child_identities: HashMap<String, Identity>,
    profile_identity: Option<Identity>,
}

impl Partial {
    /// Removes what construction created.
    ///
    /// Once the root's own identity is known the removal walks the tree and
    /// refuses to descend into anything it did not create. Before that, only
    /// the single edge whose `fstatat` result was captured at creation is
    /// removed, and only if it still names the same object.
    fn discard(&mut self) {
        let Some(parent_file) = self.parent_file.as_ref() else {
            return;
        };
        match (self.root_file.as_ref(), self.root_identity) {
            (Some(root_file), Some(root_identity)) => {
                let _ = cleanup_partial_state_root(
                    parent_file,
                    root_file,
                    &self.root_name,
                    root_identity,
                    &self.child_identities,
                    self.profile_identity,
                );
            }
            _ => {
                let _ = remove_expected_state_edge(
                    parent_file.as_fd(),
                    &self.root_name,
                    self.root_edge,
                );
            }
        }
        self.root_file = None;
        self.parent_file = None;
    }
}

/// The construction steps that run after the root leaf exists.
fn build_state(
    partial: &mut Partial,
    canonical_workspace: &str,
    canonical_cache: &str,
    operations: &Operations,
) -> Result<State, Error> {
    let parent_file = partial.parent_file.as_ref().ok_or(Error::Create)?;
    let uid = (operations.current_uid)();
    let root_name = partial.root_name.clone();
    let root_path = partial.root_path.clone();

    emit(
        operations.event.as_deref(),
        EventKind::RootCreated,
        &root_name,
        &root_path,
    );
    let root_file = open_state_directory_at(parent_file.as_fd(), &root_name)?;
    let root_stat = descriptor_stat(&root_file)?;
    let root_identity = identity_from(&root_stat);
    if !secure_state_stat(&root_stat, true, DIRECTORY_MODE, uid)
        || !same_identity(root_identity, identity_from(&partial.root_edge))
        || !edge_matches(
            parent_file.as_fd(),
            &root_name,
            root_identity,
            true,
            DIRECTORY_MODE,
            uid,
        )
    {
        partial.root_file = Some(root_file);
        return Err(Error::Create);
    }
    partial.root_file = Some(root_file);
    partial.root_identity = Some(root_identity);
    let root_file = partial.root_file.as_ref().ok_or(Error::Create)?;
    emit(
        operations.event.as_deref(),
        EventKind::RootValidated,
        &root_name,
        &root_path,
    );

    let home = profile::join(&root_path, "home");
    let temp = profile::join(&root_path, "tmp");
    let cache = profile::join(&root_path, "cache");
    let profiles = profile::join(&root_path, "profiles");
    let child_paths = [
        ("home", home.as_str()),
        ("tmp", temp.as_str()),
        ("cache", cache.as_str()),
        ("profiles", profiles.as_str()),
    ];

    let mut profiles_file = None;
    for (name, path) in child_paths {
        let (child_file, identity) = create_state_directory_at(root_file, name, path, operations);
        if let Some(identity) = identity {
            partial.child_identities.insert(name.to_string(), identity);
        }
        let child_file = child_file?;
        if name == "profiles" {
            profiles_file = Some(child_file);
            continue;
        }
        nix::unistd::close(child_file).map_err(|_| Error::Create)?;
    }
    let profiles_file = profiles_file.ok_or(Error::Create)?;

    let profile_path = profile::join(&profiles, PROFILE_LEAF);
    let (profile_identity, profile_result) =
        create_state_profile_at(&profiles_file, &profile_path, operations);
    if let Some(profile_identity) = profile_identity {
        partial.profile_identity = Some(profile_identity);
    }
    let profiles_close = nix::unistd::close(profiles_file);
    profile_result?;
    profiles_close.map_err(|_| Error::Create)?;
    let profile_identity = profile_identity.ok_or(Error::Create)?;

    emit(
        operations.event.as_deref(),
        EventKind::FinalValidation,
        &root_name,
        &root_path,
    );
    let final_checks =
        edge_matches(
            parent_file.as_fd(),
            &root_name,
            root_identity,
            true,
            DIRECTORY_MODE,
            uid,
        ) && descriptor_path_matches(canonical_cache, parent_file, &operations.lstat)
            && descriptor_path_matches(&root_path, root_file, &operations.lstat)
            && !profile::path_within(canonical_workspace, &root_path)
            && state_construction_entries_match(
                root_file,
                &partial.child_identities,
                profile_identity,
                uid,
            )
            && edge_matches(
                parent_file.as_fd(),
                &root_name,
                root_identity,
                true,
                DIRECTORY_MODE,
                uid,
            )
            && descriptor_path_matches(canonical_cache, parent_file, &operations.lstat)
            && descriptor_path_matches(&root_path, root_file, &operations.lstat);
    if !final_checks {
        return Err(Error::Create);
    }

    let root_file = partial.root_file.take().ok_or(Error::Create)?;
    let parent_file = partial.parent_file.take().ok_or(Error::Create)?;
    Ok(State {
        directories: PrivateDirectories {
            root: root_path.clone().into(),
            home: home.into(),
            temp: temp.into(),
            cache: cache.into(),
        },
        profiles,
        profile_path,
        root_parent: canonical_cache.to_string(),
        root_text: root_path,
        root_name,
        root_identity,
        child_identities: std::mem::take(&mut partial.child_identities),
        profile_identity,
        event: operations.event.clone(),
        lstat: operations.lstat.clone(),
        close_fd: operations.close_fd.clone(),
        inner: Mutex::new(Inner {
            parent_file: Some(parent_file),
            root_file: Some(root_file),
            closed: false,
            close_result: None,
        }),
    })
}

impl State {
    /// Replaces the generated profile with `profile`.
    ///
    /// The whole chain from the cache base to the profile inode is revalidated
    /// first, and the file is truncated only after its own descriptor has been
    /// proven to be the regular, owner-only, single-linked inode created at
    /// construction. A replacement symlink or a substituted directory is
    /// rejected before any byte is written.
    pub(crate) fn write_profile(&self, profile: &[u8]) -> Result<(), Error> {
        let inner = self.inner.lock().expect("state mutex");
        if inner.closed {
            return Err(Error::Create);
        }
        let (parent_file, root_file) = self.descriptors(&inner)?;
        if !self.valid_shape() || !self.valid_retained_root(root_file) {
            return Err(Error::Create);
        }
        let uid = nix::unistd::Uid::effective().as_raw();

        emit(
            self.event.as_deref(),
            EventKind::ProfileWrite,
            &self.root_name,
            &self.root_text,
        );
        if !edge_matches(
            parent_file.as_fd(),
            &self.root_name,
            self.root_identity,
            true,
            DIRECTORY_MODE,
            uid,
        ) || !descriptor_path_matches(&self.root_parent, parent_file, &self.lstat)
            || !descriptor_path_matches(&self.root_text, root_file, &self.lstat)
        {
            return Err(Error::Create);
        }

        let mut profiles_file = None;
        for name in CHILD_NAMES {
            let identity = *self.child_identities.get(name).ok_or(Error::Create)?;
            let child_file = open_state_directory_at(root_file.as_fd(), name)?;
            if !descriptor_matches_expected(&child_file, identity, true, DIRECTORY_MODE, uid)
                || !edge_matches(root_file.as_fd(), name, identity, true, DIRECTORY_MODE, uid)
            {
                return Err(Error::Create);
            }
            if name == "profiles" {
                profiles_file = Some(child_file);
                continue;
            }
            nix::unistd::close(child_file).map_err(|_| Error::Create)?;
        }
        let profiles_file = profiles_file.ok_or(Error::Create)?;

        let profile_fd = nix::fcntl::openat(
            &profiles_file,
            PROFILE_LEAF,
            OFlag::O_WRONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|_| Error::Create)?;
        let profile_stat = descriptor_stat(&profile_fd)?;
        if !secure_state_stat(&profile_stat, false, PROFILE_FILE_MODE, uid)
            || profile_stat.st_nlink != 1
            || !same_identity(identity_from(&profile_stat), self.profile_identity)
            || !edge_matches(
                profiles_file.as_fd(),
                PROFILE_LEAF,
                self.profile_identity,
                false,
                PROFILE_FILE_MODE,
                uid,
            )
            || !edge_matches(
                parent_file.as_fd(),
                &self.root_name,
                self.root_identity,
                true,
                DIRECTORY_MODE,
                uid,
            )
        {
            return Err(Error::Create);
        }
        nix::unistd::ftruncate(&profile_fd, 0).map_err(|_| Error::Create)?;
        let mut remaining = profile;
        while !remaining.is_empty() {
            match nix::unistd::write(&profile_fd, remaining) {
                Err(Errno::EINTR) => continue,
                Ok(written) if written > 0 => remaining = &remaining[written..],
                _ => return Err(Error::Create),
            }
        }
        (self.close_fd)(profile_fd).map_err(|_| Error::Create)
    }

    /// Removes the tree and closes both descriptors.
    ///
    /// Idempotent: the first call performs the removal and every later call
    /// returns the same result. A tree that no longer matches the identities
    /// recorded at construction is left untouched and reported as
    /// [`Error::Cleanup`], so a substituted root inode is never removed.
    pub(crate) fn close(&self) -> Result<(), Error> {
        let mut inner = self.inner.lock().expect("state mutex");
        if let Some(result) = inner.close_result {
            return result;
        }
        inner.closed = true;

        let mut cleanup_failed = false;
        let root_usable = match (inner.parent_file.as_ref(), inner.root_file.as_ref()) {
            (Some(parent_file), Some(root_file)) => {
                let usable = self.valid_shape() && self.valid_retained_root(root_file);
                emit(
                    self.event.as_deref(),
                    EventKind::Cleanup,
                    &self.root_name,
                    &self.root_text,
                );
                if usable {
                    if !descriptor_path_matches(&self.root_parent, parent_file, &self.lstat)
                        || !descriptor_path_matches(&self.root_text, root_file, &self.lstat)
                    {
                        cleanup_failed = true;
                    }
                    if cleanup_state_root(
                        parent_file,
                        root_file,
                        &self.root_name,
                        self.root_identity,
                        &self.child_identities,
                    )
                    .is_err()
                    {
                        cleanup_failed = true;
                    }
                }
                usable
            }
            _ => {
                emit(
                    self.event.as_deref(),
                    EventKind::Cleanup,
                    &self.root_name,
                    &self.root_text,
                );
                false
            }
        };
        if !root_usable {
            cleanup_failed = true;
        }
        if let Some(root_file) = inner.root_file.take()
            && nix::unistd::close(root_file).is_err()
        {
            cleanup_failed = true;
        }
        if let Some(parent_file) = inner.parent_file.take()
            && nix::unistd::close(parent_file).is_err()
        {
            cleanup_failed = true;
        }
        let result = if cleanup_failed {
            Err(Error::Cleanup)
        } else {
            Ok(())
        };
        inner.close_result = Some(result);
        result
    }

    fn descriptors<'a>(&self, inner: &'a Inner) -> Result<(&'a OwnedFd, &'a OwnedFd), Error> {
        match (inner.parent_file.as_ref(), inner.root_file.as_ref()) {
            (Some(parent), Some(root)) => Ok((parent, root)),
            _ => Err(Error::Create),
        }
    }

    /// Whether the recorded paths are still the fixed shape construction built.
    fn valid_shape(&self) -> bool {
        let root = &self.root_text;
        safe_state_leaf(&self.root_parent, root)
            && self.root_name == base(root)
            && self.directories.home == std::path::Path::new(&profile::join(root, "home"))
            && self.directories.temp == std::path::Path::new(&profile::join(root, "tmp"))
            && self.directories.cache == std::path::Path::new(&profile::join(root, "cache"))
            && self.profiles == profile::join(root, "profiles")
            && self.profile_path == profile::join(&self.profiles, PROFILE_LEAF)
            && self.child_identities.len() == CHILD_NAMES.len()
    }

    /// Whether the retained root descriptor still names the inode created.
    fn valid_retained_root(&self, root_file: &OwnedFd) -> bool {
        descriptor_matches_expected(
            root_file,
            self.root_identity,
            true,
            DIRECTORY_MODE,
            nix::unistd::Uid::effective().as_raw(),
        )
    }
}

fn emit(hook: Option<&EventHook>, kind: EventKind, name: &str, path: &str) {
    if let Some(hook) = hook {
        hook(&Event {
            kind,
            name: name.to_string(),
            path: path.to_string(),
        });
    }
}

/// Canonicalises `path` and proves it is a real directory, not a symlink.
fn canonical_state_directory(
    path: &str,
    operations: &Operations,
) -> Result<(String, Facts), Error> {
    if path.is_empty() || !profile::is_absolute(path) || profile::clean(path) != path {
        return Err(Error::Create);
    }
    let canonical = (operations.canonicalize)(path)?;
    if canonical.is_empty()
        || !profile::is_absolute(&canonical)
        || profile::clean(&canonical) != canonical
    {
        return Err(Error::Create);
    }
    let facts = (operations.lstat)(&canonical)?;
    if facts.symlink || !facts.directory {
        return Err(Error::Create);
    }
    Ok((canonical, facts))
}

/// Opens `path` one component at a time, refusing to follow any symlink.
fn open_state_directory_path(path: &str) -> Result<OwnedFd, Error> {
    if path.is_empty() || !profile::is_absolute(path) || profile::clean(path) != path {
        return Err(Error::Create);
    }
    let mut current = nix::fcntl::open(
        "/",
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| Error::Create)?;
    if path == "/" {
        return Ok(current);
    }
    for component in path.trim_start_matches('/').split('/') {
        let next = open_state_directory_at(current.as_fd(), component)?;
        nix::unistd::close(current).map_err(|_| Error::Create)?;
        current = next;
    }
    Ok(current)
}

/// Opens the directory `name` inside `parent`, refusing symlinks.
fn open_state_directory_at(parent: BorrowedFd<'_>, name: &str) -> Result<OwnedFd, Error> {
    if !valid_state_entry_name(name) {
        return Err(Error::Create);
    }
    nix::fcntl::openat(
        parent,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| Error::Create)
}

/// The same open, reporting the raw errno so the cleanup loop can branch on it.
fn open_state_directory_at_raw(parent: BorrowedFd<'_>, name: &str) -> Result<OwnedFd, Errno> {
    if !valid_state_entry_name(name) {
        return Err(Errno::EINVAL);
    }
    nix::fcntl::openat(
        parent,
        name,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::empty(),
    )
}

/// Creates an `otto-sandbox-` leaf under `parent_file`, retrying on collision.
fn create_random_state_root(
    parent_file: &OwnedFd,
    parent_path: &str,
    operations: &Operations,
) -> Result<(String, String, FileStat), Error> {
    for _ in 0..LEAF_MAX_ATTEMPTS {
        let mut random = [0u8; LEAF_RANDOM_BYTES];
        (operations.random_bytes)(&mut random)?;
        let mut name = String::with_capacity(LEAF_PREFIX.len() + 2 * LEAF_RANDOM_BYTES);
        name.push_str(LEAF_PREFIX);
        for byte in random {
            use std::fmt::Write as _;
            let _ = write!(name, "{byte:02x}");
        }
        match (operations.mkdirat)(parent_file.as_fd(), &name, DIRECTORY_MODE) {
            Ok(()) => {}
            Err(Errno::EEXIST) => continue,
            Err(_) => return Err(Error::Create),
        }
        let Ok(edge) =
            nix::sys::stat::fstatat(parent_file, name.as_str(), AtFlags::AT_SYMLINK_NOFOLLOW)
        else {
            if nix::unistd::unlinkat(parent_file, name.as_str(), UnlinkatFlags::RemoveDir).is_err()
            {
                let _ =
                    nix::unistd::unlinkat(parent_file, name.as_str(), UnlinkatFlags::NoRemoveDir);
            }
            return Err(Error::Create);
        };
        let path = profile::join(parent_path, &name);
        return Ok((name, path, edge));
    }
    Err(Error::Create)
}

/// Creates and validates one fixed child directory of the state root.
///
/// The identity is returned even when validation fails, so the caller can
/// record what it created and remove exactly that inode later.
fn create_state_directory_at(
    parent_file: &OwnedFd,
    name: &str,
    path: &str,
    operations: &Operations,
) -> (Result<OwnedFd, Error>, Option<Identity>) {
    if !valid_state_entry_name(name)
        || (operations.mkdirat)(parent_file.as_fd(), name, DIRECTORY_MODE).is_err()
    {
        return (Err(Error::Create), None);
    }
    let Ok(created_edge) = nix::sys::stat::fstatat(parent_file, name, AtFlags::AT_SYMLINK_NOFOLLOW)
    else {
        return (Err(Error::Create), None);
    };
    let created_identity = identity_from(&created_edge);
    emit(
        operations.event.as_deref(),
        EventKind::DirectoryCreated,
        name,
        path,
    );
    let uid = (operations.current_uid)();
    let Ok(file) = open_state_directory_at(parent_file.as_fd(), name) else {
        return (Err(Error::Create), Some(created_identity));
    };
    let Ok(stat) = descriptor_stat(&file) else {
        return (Err(Error::Create), Some(created_identity));
    };
    let identity = identity_from(&stat);
    if !secure_state_stat(&stat, true, DIRECTORY_MODE, uid)
        || !same_identity(identity, created_identity)
        || !edge_matches(
            parent_file.as_fd(),
            name,
            identity,
            true,
            DIRECTORY_MODE,
            uid,
        )
    {
        return (Err(Error::Create), Some(created_identity));
    }
    (Ok(file), Some(identity))
}

/// Creates the empty profile file with `O_EXCL | O_NOFOLLOW`.
fn create_state_profile_at(
    profiles_file: &OwnedFd,
    path: &str,
    operations: &Operations,
) -> (Option<Identity>, Result<(), Error>) {
    let Ok(fd) = nix::fcntl::openat(
        profiles_file,
        PROFILE_LEAF,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
        Mode::from_bits_truncate(PROFILE_FILE_MODE as _),
    ) else {
        return (None, Err(Error::Create));
    };
    emit(
        operations.event.as_deref(),
        EventKind::ProfileCreated,
        PROFILE_LEAF,
        path,
    );
    let uid = (operations.current_uid)();
    let Ok(stat) = descriptor_stat(&fd) else {
        let _ = (operations.close_fd)(fd);
        return (None, Err(Error::Create));
    };
    let identity = identity_from(&stat);
    if !secure_state_stat(&stat, false, PROFILE_FILE_MODE, uid)
        || stat.st_nlink != 1
        || !edge_matches(
            profiles_file.as_fd(),
            PROFILE_LEAF,
            identity,
            false,
            PROFILE_FILE_MODE,
            uid,
        )
    {
        let _ = (operations.close_fd)(fd);
        return (Some(identity), Err(Error::Create));
    }
    match (operations.close_fd)(fd) {
        Ok(()) => (Some(identity), Ok(())),
        Err(_) => (Some(identity), Err(Error::Create)),
    }
}

/// Re-opens every fixed child and the profile and proves each is what
/// construction created, before and after the profile check.
fn state_construction_entries_match(
    root_file: &OwnedFd,
    child_identities: &HashMap<String, Identity>,
    profile_identity: Identity,
    uid: u32,
) -> bool {
    if child_identities.len() != CHILD_NAMES.len() || profile_identity == Identity::default() {
        return false;
    }
    let mut children = HashMap::with_capacity(CHILD_NAMES.len());
    let mut matches = true;
    for name in CHILD_NAMES {
        let Some(identity) = child_identities.get(name).copied() else {
            matches = false;
            break;
        };
        let Ok(file) = open_state_directory_at(root_file.as_fd(), name) else {
            matches = false;
            break;
        };
        let ok = descriptor_matches_expected(&file, identity, true, DIRECTORY_MODE, uid)
            && edge_matches(root_file.as_fd(), name, identity, true, DIRECTORY_MODE, uid);
        children.insert(name, file);
        if !ok {
            matches = false;
            break;
        }
    }
    if matches {
        let profiles_file = children.get("profiles").expect("profiles descriptor");
        matches = match nix::fcntl::openat(
            profiles_file,
            PROFILE_LEAF,
            OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
            Mode::empty(),
        ) {
            Err(_) => false,
            Ok(profile_fd) => {
                let profile_matches = descriptor_stat(&profile_fd).is_ok_and(|stat| {
                    secure_state_stat(&stat, false, PROFILE_FILE_MODE, uid)
                        && stat.st_nlink == 1
                        && same_identity(identity_from(&stat), profile_identity)
                }) && edge_matches(
                    profiles_file.as_fd(),
                    PROFILE_LEAF,
                    profile_identity,
                    false,
                    PROFILE_FILE_MODE,
                    uid,
                );
                let close_ok = nix::unistd::close(profile_fd).is_ok();
                profile_matches && close_ok
            }
        };
    }
    if matches {
        for name in CHILD_NAMES {
            let identity = child_identities.get(name).copied().unwrap_or_default();
            if !descriptor_matches_expected(
                children.get(name).expect("child descriptor"),
                identity,
                true,
                DIRECTORY_MODE,
                uid,
            ) || !edge_matches(root_file.as_fd(), name, identity, true, DIRECTORY_MODE, uid)
            {
                matches = false;
                break;
            }
        }
    }
    if matches {
        let profiles_file = children.get("profiles").expect("profiles descriptor");
        matches = edge_matches(
            profiles_file.as_fd(),
            PROFILE_LEAF,
            profile_identity,
            false,
            PROFILE_FILE_MODE,
            uid,
        );
    }
    for (_, file) in children {
        if nix::unistd::close(file).is_err() {
            matches = false;
        }
    }
    matches
}

pub(super) fn descriptor_stat<Fd: AsFd>(fd: Fd) -> Result<FileStat, Error> {
    nix::sys::stat::fstat(fd).map_err(|_| Error::Create)
}

/// Whether `stat` is exactly the kind, permission bits and owner expected.
///
/// The permission comparison covers all twelve mode bits, so a set-uid or
/// sticky bit added after creation is a rejection.
pub(super) fn secure_state_stat(
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

pub(super) fn identity_from(stat: &FileStat) -> Identity {
    Identity {
        device: stat.st_dev as u64,
        inode: stat.st_ino,
    }
}

/// Identity equality that treats the unknown value as matching nothing.
pub(super) fn same_identity(left: Identity, right: Identity) -> bool {
    left == right && left != Identity::default()
}

/// Whether the directory entry `name` under `parent_fd` still names `identity`
/// with the expected kind, permissions and owner.
fn edge_matches(
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
fn edge_identity_matches(
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

fn descriptor_matches_expected(
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
fn descriptor_matches_facts(file: &OwnedFd, expected: Facts) -> bool {
    descriptor_stat(file).is_ok_and(|stat| {
        u32::from(stat.st_mode) & u32::from(SFlag::S_IFMT.bits())
            == u32::from(SFlag::S_IFDIR.bits())
            && same_identity(identity_from(&stat), expected.identity)
    })
}

/// Whether `path` still resolves, without following a final symlink, to the
/// object the descriptor holds open.
fn descriptor_path_matches(path: &str, file: &OwnedFd, lstat: &Arc<Lstat>) -> bool {
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
pub(super) fn open_verified_directory(path: &str) -> Result<OwnedFd, Error> {
    let file = open_state_directory_path(path)?;
    let lstat: Arc<Lstat> = Arc::new(lstat_facts);
    if !descriptor_path_matches(path, &file, &lstat) {
        return Err(Error::Create);
    }
    Ok(file)
}

/// Whether `path` still resolves, without following a final symlink, to the
/// directory `file` holds open.
pub(super) fn directory_still_matches(path: &str, file: &OwnedFd) -> bool {
    let lstat: Arc<Lstat> = Arc::new(lstat_facts);
    descriptor_path_matches(path, file, &lstat)
}

/// Go's `filepath.Base` for the cleaned absolute paths this module uses.
fn base(path: &str) -> String {
    match path.rfind('/') {
        Some(index) => path[index + 1..].to_string(),
        None => path.to_string(),
    }
}

/// Whether `root` is a direct `otto-sandbox-` child of `parent`.
fn safe_state_leaf(parent: &str, root: &str) -> bool {
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
fn cleanup_state_root(
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
fn cleanup_partial_state_root(
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

/// Removes everything inside the directory `directory_fd` holds open.
fn remove_state_directory_contents(
    directory_fd: BorrowedFd<'_>,
    expected_children: &HashMap<String, CleanupExpectation>,
    strict: bool,
) -> Result<(), Error> {
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
fn remove_expected_state_edge(
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
fn valid_state_entry_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;

    /// A temporary base directory plus its canonical text.
    fn temp_base() -> (tempfile::TempDir, String) {
        let directory = tempfile::TempDir::new().expect("temp dir");
        let canonical = std::fs::canonicalize(directory.path()).expect("canonical temp dir");
        let text = canonical.to_str().expect("utf-8 temp dir").to_string();
        (directory, text)
    }

    /// Go's `makeStateTestDirectory`.
    fn make_directory(path: &str, mode: u32) -> String {
        std::fs::create_dir(path).expect("create directory");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("set directory mode");
        std::fs::canonicalize(path)
            .expect("canonical directory")
            .to_str()
            .expect("utf-8 directory")
            .to_string()
    }

    /// Go's `makeProfileTestFile`.
    fn make_file(path: &str, mode: u32) -> String {
        std::fs::create_dir_all(profile::dir(path)).expect("create parent");
        std::fs::write(path, b"fixture").expect("write fixture");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("set file mode");
        std::fs::canonicalize(path)
            .expect("canonical file")
            .to_str()
            .expect("utf-8 file")
            .to_string()
    }

    fn symlink(target: &str, link: &str) {
        std::os::unix::fs::symlink(target, link).expect("create symlink");
    }

    fn chmod(path: &str, mode: u32) {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
    }

    fn text(path: &Path) -> String {
        path.to_str().expect("utf-8 path").to_string()
    }

    fn assert_canonical(path: &str) {
        let canonical = std::fs::canonicalize(path).expect("canonicalize");
        assert_eq!(canonical.to_str(), Some(path), "path is not canonical");
        assert!(profile::is_absolute(path));
        assert_eq!(profile::clean(path), path);
    }

    fn assert_entry(path: &str, directory: bool, mode: u32) {
        let info = std::fs::symlink_metadata(path).expect("lstat entry");
        assert!(!info.file_type().is_symlink(), "entry is a symlink");
        assert_eq!(info.permissions().mode() & 0o7777, mode, "entry mode");
        assert_eq!(info.is_dir(), directory, "entry kind");
        assert_eq!(info.is_file(), !directory, "entry kind");
        assert_eq!(info.uid(), nix::unistd::Uid::effective().as_raw());
    }

    fn assert_no_leaves(cache: &str) {
        let leaves: Vec<_> = std::fs::read_dir(cache)
            .expect("read cache base")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(LEAF_PREFIX))
            .collect();
        assert!(leaves.is_empty(), "partial state remains: {leaves:?}");
    }

    /// Runs `hook` on the named event kind, at most once.
    fn once_on(kind: EventKind, hook: impl Fn(&Event) + Send + Sync + 'static) -> Arc<EventHook> {
        let fired = Mutex::new(false);
        Arc::new(move |event: &Event| {
            if event.kind != kind {
                return;
            }
            let mut fired = fired.lock().expect("event guard");
            if *fired {
                return;
            }
            *fired = true;
            hook(event);
        })
    }

    #[test]
    fn creates_an_owner_only_canonical_tree_outside_the_workspace() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let cache_alias = profile::join(&base, "cache-alias");
        symlink(&cache, &cache_alias);
        let workspace_alias = profile::join(&base, "workspace-alias");
        symlink(&workspace, &workspace_alias);

        let state = create(&workspace_alias, &cache_alias).expect("create state");

        assert_eq!(state.root_parent, cache);
        let root = text(&state.directories.root);
        assert_eq!(profile::dir(&root), cache);
        assert!(super::base(&root).starts_with(LEAF_PREFIX));
        assert!(
            !profile::path_within(&workspace, &root),
            "state root is inside the canonical workspace"
        );

        for path in [
            root.clone(),
            text(&state.directories.home),
            text(&state.directories.temp),
            text(&state.directories.cache),
            state.profiles.clone(),
        ] {
            assert_canonical(&path);
            assert_entry(&path, true, 0o700);
        }
        assert_eq!(
            state.directories.home,
            Path::new(&profile::join(&root, "home"))
        );
        assert_eq!(
            state.directories.temp,
            Path::new(&profile::join(&root, "tmp"))
        );
        assert_eq!(
            state.directories.cache,
            Path::new(&profile::join(&root, "cache"))
        );
        assert_eq!(state.profiles, profile::join(&root, "profiles"));
        assert_eq!(
            state.profile_path,
            profile::join(&state.profiles, "profile.sb")
        );
        assert_canonical(&state.profile_path);
        assert_entry(&state.profile_path, false, 0o600);

        state.close().expect("close state");
    }

    #[test]
    fn rejects_a_cache_base_inside_the_canonical_workspace() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&workspace, "user-cache"), 0o700);
        let alias = profile::join(&base, "cache-alias");
        symlink(&cache, &alias);

        assert!(create(&workspace, &alias).is_err());
        assert_no_leaves(&cache);
    }

    #[test]
    fn rejects_a_case_alias_of_a_cache_inside_the_workspace() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "CaseWorkspace"), 0o700);
        let cache = make_directory(&profile::join(&workspace, "CaseCache"), 0o700);
        let alias = cache
            .replacen("CaseWorkspace", "caseworkspace", 1)
            .replacen("CaseCache", "casecache", 1);
        if std::fs::metadata(&alias).is_err() {
            eprintln!("skipping: test volume is case-sensitive");
            return;
        }

        assert!(
            create(&workspace, &alias).is_err(),
            "a differently-cased cache alias inside the workspace was accepted"
        );
        assert_no_leaves(&cache);
    }

    #[test]
    fn rejects_a_symlink_candidate_without_following_it() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let outside = make_directory(&profile::join(&base, "outside"), 0o700);
        let marker = profile::join(&outside, "keep");
        std::fs::write(&marker, b"keep").expect("write marker");

        let mut operations = Operations::default();
        let inner = operations.mkdirat.clone();
        let target = outside.clone();
        operations.mkdirat = Arc::new(move |parent, name, mode| {
            if name.starts_with(LEAF_PREFIX) {
                return nix::unistd::symlinkat(target.as_str(), parent, name);
            }
            inner(parent, name, mode)
        });

        assert!(create_with_operations(&workspace, &cache, &operations).is_err());
        assert_eq!(std::fs::read(&marker).expect("read marker"), b"keep");
        assert_no_leaves(&cache);
    }

    #[test]
    fn fails_closed_when_creation_strips_owner_bits() {
        /// Each case builds a fresh hook so the cases cannot share one
        /// single-shot trigger.
        type HookFactory = Box<dyn Fn() -> Arc<EventHook>>;

        let cases: [(&str, HookFactory); 4] = [
            (
                "root",
                Box::new(|| once_on(EventKind::RootCreated, |event| chmod(&event.path, 0o600))),
            ),
            (
                "root special mode bits",
                Box::new(|| once_on(EventKind::RootCreated, |event| chmod(&event.path, 0o1700))),
            ),
            (
                "child directory",
                Box::new(|| {
                    Arc::new(|event: &Event| {
                        if event.kind == EventKind::DirectoryCreated && event.name == "home" {
                            chmod(&event.path, 0o600);
                        }
                    })
                }),
            ),
            (
                "profile",
                Box::new(|| once_on(EventKind::ProfileCreated, |event| chmod(&event.path, 0o400))),
            ),
        ];

        for (name, hook) in cases {
            let (_base, base) = temp_base();
            let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
            let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
            let operations = Operations {
                event: Some(hook()),
                ..Operations::default()
            };

            assert!(
                create_with_operations(&workspace, &cache, &operations).is_err(),
                "{name}: creation was accepted"
            );
            assert_no_leaves(&cache);
        }
    }

    #[test]
    fn cleans_up_a_partial_construction_failure() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let mut operations = Operations::default();
        let inner = operations.mkdirat.clone();
        operations.mkdirat = Arc::new(move |parent, name, mode| {
            if name == "cache" {
                return Err(Errno::EACCES);
            }
            inner(parent, name, mode)
        });

        assert!(create_with_operations(&workspace, &cache, &operations).is_err());
        assert_no_leaves(&cache);
    }

    #[test]
    fn write_profile_revalidates_the_owner_only_tree() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let state = create(&workspace, &cache).expect("create state");

        chmod(&state.profiles, 0o755);
        let policy = b"(version 1)\n(deny default)\n";
        assert!(
            state.write_profile(policy).is_err(),
            "a group-readable profiles directory was accepted"
        );
        assert!(
            std::fs::read(&state.profile_path)
                .expect("read profile")
                .is_empty(),
            "the profile was modified before its directory chain was validated"
        );

        chmod(&state.profiles, 0o700);
        state.write_profile(policy).expect("write profile");
        assert_eq!(
            std::fs::read(&state.profile_path).expect("read profile"),
            policy
        );
        assert_entry(&state.profile_path, false, 0o600);

        state.close().expect("close state");
    }

    #[test]
    fn write_profile_validates_the_file_before_truncating_it() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let state = create(&workspace, &cache).expect("create state");
        let original = b"original private profile";
        std::fs::write(&state.profile_path, original).expect("seed profile");
        chmod(&state.profile_path, 0o644);

        assert!(state.write_profile(b"replacement").is_err());
        assert_eq!(
            std::fs::read(&state.profile_path).expect("read profile"),
            original,
            "the file was truncated before validation"
        );

        chmod(&state.profile_path, 0o600);
        state.close().expect("close state");
    }

    #[test]
    fn write_profile_does_not_follow_a_replacement_symlink() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let outside = make_file(&profile::join(&base, "outside/keep"), 0o600);
        let state = create(&workspace, &cache).expect("create state");
        std::fs::remove_file(&state.profile_path).expect("remove profile");
        symlink(&outside, &state.profile_path);

        assert!(state.write_profile(b"replace").is_err());
        assert_eq!(std::fs::read(&outside).expect("read outside"), b"fixture");

        state.close().expect("close state");
    }

    #[test]
    fn close_is_idempotent_and_never_follows_symlinks() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let outside = make_directory(&profile::join(&base, "outside"), 0o700);
        let marker = profile::join(&outside, "keep");
        std::fs::write(&marker, b"keep").expect("write marker");

        let state = create(&workspace, &cache).expect("create state");
        let home = text(&state.directories.home);
        std::fs::remove_dir(&home).expect("remove home");
        symlink(&outside, &home);
        symlink(
            &outside,
            &profile::join(&text(&state.directories.cache), "outside-link"),
        );

        state.close().expect("first close");
        state.close().expect("second close");
        assert!(
            std::fs::symlink_metadata(&state.directories.root).is_err(),
            "state root still exists"
        );
        assert_eq!(std::fs::read(&marker).expect("read marker"), b"keep");
    }

    #[test]
    fn construction_does_not_follow_a_root_replacement_after_validation() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let outside = make_directory(&profile::join(&base, "outside"), 0o700);

        let moved = Arc::new(Mutex::new(String::new()));
        let mut operations = Operations::default();
        let recorded = moved.clone();
        let target = outside.clone();
        operations.event = Some(once_on(EventKind::RootValidated, move |event| {
            let replacement = format!("{}.validated", event.path);
            std::fs::rename(&event.path, &replacement).expect("rename root");
            symlink(&target, &event.path);
            *recorded.lock().expect("moved root") = replacement;
        }));

        let created = create_with_operations(&workspace, &cache, &operations);
        assert!(created.is_err(), "a replaced root was accepted");
        for relative in ["home", "tmp", "cache", "profiles", "profiles/profile.sb"] {
            let path = profile::join(&outside, relative);
            assert!(
                std::fs::symlink_metadata(&path).is_err(),
                "external state entry {relative} was created or followed"
            );
        }
        let moved = moved.lock().expect("moved root").clone();
        let _ = std::fs::remove_dir_all(&moved);
    }

    #[test]
    fn final_construction_rejects_a_replaced_profiles_directory_or_profile() {
        type Replace = Box<dyn Fn(&str, &str) + Send + Sync>;
        let cases: [(&str, Replace); 2] = [
            (
                "profiles directory",
                Box::new(|base, root| {
                    let profiles = profile::join(root, "profiles");
                    std::fs::rename(&profiles, profile::join(base, "created-profiles"))
                        .expect("rename profiles");
                    std::fs::create_dir(&profiles).expect("recreate profiles");
                    chmod(&profiles, 0o700);
                    std::fs::write(profile::join(&profiles, "profile.sb"), b"substituted")
                        .expect("write substitute");
                }),
            ),
            (
                "profile file",
                Box::new(|base, root| {
                    let path = profile::join(root, "profiles/profile.sb");
                    std::fs::rename(&path, profile::join(base, "created-profile.sb"))
                        .expect("rename profile");
                    std::fs::write(&path, b"substituted").expect("write substitute");
                }),
            ),
        ];

        for (name, replace) in cases {
            let (_base, base) = temp_base();
            let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
            let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
            let injected = Arc::new(Mutex::new(false));
            let mut operations = Operations::default();
            let fired = injected.clone();
            let injection_base = base.clone();
            operations.event = Some(once_on(EventKind::FinalValidation, move |event| {
                *fired.lock().expect("injected") = true;
                replace(&injection_base, &event.path);
            }));

            let created = create_with_operations(&workspace, &cache, &operations);
            assert!(
                *injected.lock().expect("injected"),
                "{name}: the final-validation event was not emitted"
            );
            assert!(created.is_err(), "{name}: a final replacement was accepted");
        }
    }

    #[test]
    fn partial_cleanup_does_not_traverse_an_unknown_or_substituted_directory() {
        type Insert = Box<dyn Fn(&str, &str) -> String + Send + Sync>;
        let cases: [(&str, Insert); 2] = [
            (
                "unknown directory",
                Box::new(|_base, root| {
                    let nested = profile::join(root, "unknown/nested");
                    std::fs::create_dir_all(&nested).expect("create unknown");
                    std::fs::write(profile::join(&nested, "keep"), b"keep").expect("write marker");
                    "unknown/nested/keep".to_string()
                }),
            ),
            (
                "substituted directory",
                Box::new(|base, root| {
                    let home = profile::join(root, "home");
                    std::fs::rename(&home, profile::join(base, "created-home"))
                        .expect("rename home");
                    std::fs::create_dir_all(profile::join(&home, "nested"))
                        .expect("create substitute");
                    std::fs::write(profile::join(&home, "nested/keep"), b"keep")
                        .expect("write marker");
                    "home/nested/keep".to_string()
                }),
            ),
        ];

        for (name, insert) in cases {
            let (_base, base) = temp_base();
            let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
            let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
            let marker_path = Arc::new(Mutex::new(String::new()));
            let detached = Arc::new(Mutex::new(String::new()));
            let mut operations = Operations::default();
            let recorded_marker = marker_path.clone();
            let recorded_detached = detached.clone();
            let injection_base = base.clone();
            operations.event = Some(once_on(EventKind::FinalValidation, move |event| {
                let marker = insert(&injection_base, &event.path);
                let replacement = format!("{}.detached", event.path);
                std::fs::rename(&event.path, &replacement).expect("detach root");
                *recorded_marker.lock().expect("marker") = profile::join(&replacement, &marker);
                *recorded_detached.lock().expect("detached") = replacement;
            }));

            let created = create_with_operations(&workspace, &cache, &operations);
            assert!(
                created.is_err(),
                "{name}: the injected replacement was accepted"
            );
            let marker = marker_path.lock().expect("marker").clone();
            assert_eq!(
                std::fs::read(&marker).unwrap_or_default(),
                b"keep",
                "{name}: partial cleanup touched untrusted directory contents"
            );
            let detached = detached.lock().expect("detached").clone();
            let _ = std::fs::remove_dir_all(&detached);
        }
    }

    /// Go's `stateCloseReuseProbe`: fails one descriptor close after proving the
    /// number has already been handed to an unrelated open file.
    struct CloseReuseProbe {
        sentinel: String,
        enabled: Mutex<bool>,
        state: Mutex<(usize, Option<OwnedFd>)>,
    }

    impl CloseReuseProbe {
        fn new(sentinel: String) -> Arc<Self> {
            Arc::new(Self {
                sentinel,
                enabled: Mutex::new(false),
                state: Mutex::new((0, None)),
            })
        }

        fn enable(&self) {
            *self.enabled.lock().expect("probe") = true;
        }

        /// Go closes the descriptor and reopens a sentinel, relying on the
        /// kernel handing back the same number. Rust's test harness runs tests
        /// on parallel threads of one process, so the lowest free number is not
        /// reliably the one just released. `dup2` onto the same number gives the
        /// identical end state without the race: the number the production code
        /// still holds now names an unrelated open file, so a second close would
        /// close the sentinel and the assertion would see it.
        fn hook(probe: &Arc<Self>) -> Arc<CloseFd> {
            let probe = probe.clone();
            Arc::new(move |fd: OwnedFd| {
                if !*probe.enabled.lock().expect("probe") {
                    return nix::unistd::close(fd);
                }
                let mut state = probe.state.lock().expect("probe");
                state.0 += 1;
                assert!(
                    state.1.is_none(),
                    "close hook called more than once after descriptor reuse"
                );
                let sentinel = nix::fcntl::open(
                    probe.sentinel.as_str(),
                    OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NOFOLLOW,
                    Mode::empty(),
                )
                .expect("open sentinel in injected hook");
                let mut recycled = fd;
                nix::unistd::dup2(&sentinel, &mut recycled).expect("recycle the descriptor");
                state.1 = Some(recycled);
                Err(Errno::EIO)
            })
        }

        fn assert_reused_fd_open(&self) {
            let state = self.state.lock().expect("probe");
            assert_eq!(state.0, 1, "injected close calls");
            let reused = state.1.as_ref().expect("reused descriptor");
            let descriptor = nix::sys::stat::fstat(reused).expect("reused descriptor was closed");
            let sentinel = nix::sys::stat::stat(self.sentinel.as_str()).expect("stat sentinel");
            assert!(same_identity(
                identity_from(&descriptor),
                identity_from(&sentinel)
            ));
        }
    }

    #[test]
    fn a_profile_close_error_retires_descriptors_before_the_deferred_cleanup() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let probe = CloseReuseProbe::new(make_file(&profile::join(&base, "sentinel"), 0o600));
        probe.enable();
        let operations = Operations {
            close_fd: CloseReuseProbe::hook(&probe),
            ..Operations::default()
        };

        assert!(
            create_with_operations(&workspace, &cache, &operations).is_err(),
            "a synthetic profile close error was accepted"
        );
        probe.assert_reused_fd_open();
        assert_no_leaves(&cache);
    }

    #[test]
    fn a_write_profile_close_error_retires_the_descriptor() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let probe = CloseReuseProbe::new(make_file(&profile::join(&base, "sentinel"), 0o600));
        let operations = Operations {
            close_fd: CloseReuseProbe::hook(&probe),
            ..Operations::default()
        };
        let state = create_with_operations(&workspace, &cache, &operations).expect("create state");

        probe.enable();
        assert!(state.write_profile(b"private profile").is_err());
        probe.assert_reused_fd_open();

        state.close().expect("close state");
    }

    #[test]
    fn write_profile_does_not_use_a_substituted_root_inode() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
        let outside_profile = make_file(&profile::join(&base, "outside/profile.sb"), 0o600);

        let moved = Arc::new(Mutex::new(String::new()));
        let mut operations = Operations::default();
        let recorded = moved.clone();
        let external = outside_profile.clone();
        operations.event = Some(once_on(EventKind::ProfileWrite, move |event| {
            let replacement = format!("{}.validated", event.path);
            std::fs::rename(&event.path, &replacement).expect("rename root");
            make_replacement_tree(&event.path, Some(&external));
            *recorded.lock().expect("moved root") = replacement;
        }));
        let state = create_with_operations(&workspace, &cache, &operations).expect("create state");

        assert!(state.write_profile(b"substituted write").is_err());
        assert_eq!(
            std::fs::read(&outside_profile).expect("read external profile"),
            b"fixture"
        );

        let root = text(&state.directories.root);
        let moved = moved.lock().expect("moved root").clone();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::rename(&moved, &root);
        let _ = state.close();
        let _ = std::fs::remove_dir_all(&moved);
    }

    #[test]
    fn close_does_not_remove_a_substituted_root_inode() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
        let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);

        let moved = Arc::new(Mutex::new(String::new()));
        let mut operations = Operations::default();
        let recorded = moved.clone();
        operations.event = Some(once_on(EventKind::Cleanup, move |event| {
            let replacement = format!("{}.validated", event.path);
            std::fs::rename(&event.path, &replacement).expect("rename root");
            make_replacement_tree(&event.path, None);
            std::fs::write(profile::join(&event.path, "replacement-marker"), b"keep")
                .expect("write marker");
            *recorded.lock().expect("moved root") = replacement;
        }));
        let state = create_with_operations(&workspace, &cache, &operations).expect("create state");

        let root = text(&state.directories.root);
        let alias = profile::join(&base, "outside-root-alias");
        symlink(&root, &alias);

        let first = state.close();
        let second = state.close();
        assert_eq!(first, Err(Error::Cleanup));
        assert_eq!(second, Err(Error::Cleanup));
        assert_eq!(
            std::fs::read(profile::join(&alias, "replacement-marker")).expect("read marker"),
            b"keep"
        );

        let moved = moved.lock().expect("moved root").clone();
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&moved);
        let _ = std::fs::remove_file(&alias);
    }

    #[test]
    fn close_returns_a_stable_bounded_path_free_error() {
        let (_base, base) = temp_base();
        let workspace = make_directory(&profile::join(&base, "workspace-private-name"), 0o700);
        let cache = make_directory(&profile::join(&base, "cache-private-name"), 0o700);
        let state = create(&workspace, &cache).expect("create state");
        let leaf = super::base(&text(&state.directories.root));

        let moved = format!("{cache}-moved");
        std::fs::rename(&cache, &moved).expect("rename cache");

        let first = state.close().expect_err("first close");
        let second = state.close().expect_err("second close");
        assert_eq!(first, second);
        let message = first.to_string();
        assert!(message.len() <= 128, "error is not bounded: {message}");
        assert!(!message.contains(&base));
        assert!(!message.contains(&leaf));
        assert!(!message.contains("profile.sb"));

        let _ = std::fs::remove_dir_all(&moved);
    }

    /// Go's `makeStateReplacementTree`: a look-alike tree with fresh inodes.
    fn make_replacement_tree(root: &str, external_profile: Option<&str>) {
        std::fs::create_dir(root).expect("create replacement root");
        chmod(root, 0o700);
        for name in CHILD_NAMES {
            let child = profile::join(root, name);
            std::fs::create_dir(&child).expect("create replacement child");
            chmod(&child, 0o700);
        }
        let path = profile::join(root, "profiles/profile.sb");
        match external_profile {
            Some(external) => std::fs::hard_link(external, &path).expect("link external profile"),
            None => {
                std::fs::write(&path, b"").expect("write replacement profile");
                chmod(&path, 0o600);
            }
        }
    }
}
