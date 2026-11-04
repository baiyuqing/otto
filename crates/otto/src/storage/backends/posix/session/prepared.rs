//! Preparing a session for activation, and archiving one.
//!
//! Ownership: a [`Prepared`] owns one verified descriptor until
//! [`Prepared::activate`] transfers it to a [`Store`] or [`Prepared::close`]
//! releases it; both are idempotent and neither leaks the descriptor on
//! failure. Concurrency: the handle is behind a mutex, so racing `activate`
//! calls leave exactly one winner. Errors: an identity that changed between
//! preparation and activation is rejected rather than used.

use std::fs::Metadata;
use std::path::Path;
use std::sync::{Arc, Mutex};

use otto_core::session::{PiError, SessionInfo, Warning};

use crate::storage::backends::posix::lease::{self, Keeper};
use crate::storage::files::{FileDriver, JournalFile, JournalOpen};

use super::list::{
    inspect_opened_session, open_session_root_no_follow, open_workspace_session_directory_no_follow,
};
use super::store::Store;
use crate::session::fsops;

const ARCHIVE_DIRECTORY_NAME: &str = "archive";

/// The lease [`Prepared::prepare`]/[`Prepared::prepare_listed`] acquired for
/// a session's path, and how it was acquired.
type AcquiredLease = (Arc<lease::Lease>, lease::Acquired);

/// Acquires the lease backing `path`, when it has one.
///
/// `create_lease` is `Some(lease_seconds)` when the caller wants a session
/// with no lease directory yet to get one, e.g. because `[failover]` is
/// enabled; the directory is created only when `path` is not already
/// lease-managed. Whether or not a directory was just created, a session
/// that is lease-managed by the time this returns always has its lease
/// acquired, so a session another host made lease-managed is honored even
/// when this call passed `None`.
fn acquire_lease_if_managed(
    path: &Path,
    create_lease: Option<u64>,
    keeper: &Arc<Keeper>,
) -> Result<Option<AcquiredLease>, PiError> {
    if let Some(lease_seconds) = create_lease
        && !lease::is_lease_managed(path)
            .map_err(|error| PiError::other(format!("check session lease directory: {error}")))?
    {
        lease::create_lease_dir(path, lease_seconds)
            .map_err(|error| PiError::other(format!("create session lease directory: {error}")))?;
    }
    let managed = lease::is_lease_managed(path)
        .map_err(|error| PiError::other(format!("check session lease directory: {error}")))?;
    if !managed {
        return Ok(None);
    }
    match lease::Lease::acquire_with_keeper(path, keeper) {
        Ok((lease, acquired)) => Ok(Some((lease, acquired))),
        Err(error) => Err(PiError::invalid(format!("open session: {error}"))),
    }
}

/// Releases a lease acquired earlier in `prepare`/`prepare_listed` when a
/// later step in the same call fails, so a failed open never leaves a lease
/// held with no `Prepared` or `Store` left to release it.
fn release_on_prepare_failure(lease: &Option<AcquiredLease>) {
    if let Some((lease, _)) = lease.as_ref() {
        let _ = lease.release();
    }
}

/// How a prepared handle re-checks, at activation time, that the path it was
/// opened through still names the same file.
#[derive(Debug)]
enum Identity {
    /// Re-`lstat` the path the caller gave.
    Path(String),
    /// Re-open the candidate through the session root and compare.
    Listed {
        root: String,
        workspace: String,
        basename: String,
    },
}

/// One verified session file descriptor and the metadata read from it.
pub struct Prepared {
    path: String,
    handle: Mutex<Option<(Box<dyn JournalFile>, Metadata)>>,
    info: SessionInfo,
    identity: Identity,
    /// The lease acquired opening `path`, when it is lease-managed. Moved
    /// into the [`Store`] on [`Prepared::activate`]; released by
    /// [`Prepared::close`] or, if neither ran, by `drop`.
    lease: Mutex<Option<AcquiredLease>>,
    /// Which tool names may be run again, instead of settled as interrupted,
    /// when this open takes over a lease epoch. Set by the composition root
    /// through [`Prepared::with_replayable`]; never replays by default.
    replayable: fn(&str) -> bool,
    driver: Arc<dyn FileDriver>,
    keeper: Arc<Keeper>,
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Prepared")
            .field("path", &self.path)
            .field("info", &self.info)
            .finish_non_exhaustive()
    }
}

/// A completed archive move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveResult {
    pub path: String,
    pub id: String,
}

impl Prepared {
    pub(crate) fn prepare_with_driver_and_keeper(
        path: &Path,
        create_lease: Option<u64>,
        driver: Arc<dyn FileDriver>,
        keeper: Arc<Keeper>,
    ) -> Result<Self, PiError> {
        let lease = acquire_lease_if_managed(path, create_lease, &keeper)?;
        let (file, metadata) = match open_prepared_session_file_no_follow(driver.as_ref(), path)
            .and_then(|(file, metadata)| {
                file.lock_exclusive()
                    .map_err(|error| PiError::other(format!("lock session file: {error}")))?;
                Ok((file, metadata))
            }) {
            Ok(value) => value,
            Err(error) => {
                release_on_prepare_failure(&lease);
                return Err(error);
            }
        };
        let path = path.to_string_lossy().into_owned();
        Self::from_opened(
            path.clone(),
            file,
            metadata,
            Identity::Path(path),
            lease,
            driver,
            keeper,
        )
    }

    /// Pins a candidate that `list` returned for this root and workspace.
    ///
    /// Every attacker-controlled component below the root is traversed with
    /// `openat` and `O_NOFOLLOW` before the descriptor is pinned, and the
    /// session's recorded workspace must match the expected one. `create_lease`
    /// is as in [`Prepared::prepare`].
    pub(crate) fn prepare_listed_with_driver_and_keeper(
        root: &Path,
        workspace: &str,
        path: &Path,
        create_lease: Option<u64>,
        driver: Arc<dyn FileDriver>,
        keeper: Arc<Keeper>,
    ) -> Result<Self, PiError> {
        let (root_path, workspace_canonical, basename, candidate_path) =
            validate_listed_candidate_path(root, workspace, path)?;
        let lease = acquire_lease_if_managed(Path::new(&candidate_path), create_lease, &keeper)?;
        let (file, metadata) = match open_listed_prepared_session_file(
            driver.as_ref(),
            &root_path,
            &workspace_canonical,
            &basename,
        )
        .and_then(|(file, metadata)| {
            file.lock_exclusive()
                .map_err(|error| PiError::other(format!("lock session file: {error}")))?;
            Ok((file, metadata))
        }) {
            Ok(value) => value,
            Err(error) => {
                release_on_prepare_failure(&lease);
                return Err(error);
            }
        };
        let prepared = Self::from_opened(
            candidate_path,
            file,
            metadata,
            Identity::Listed {
                root: root_path,
                workspace: workspace_canonical.clone(),
                basename,
            },
            lease,
            driver,
            keeper,
        )?;
        match crate::workspace::owns_session(&workspace_canonical, &prepared.info.cwd) {
            Ok(true) => Ok(prepared),
            _ => {
                prepared.close()?;
                Err(PiError::invalid(
                    "listed session workspace does not match expected workspace",
                ))
            }
        }
    }

    fn from_opened(
        path: String,
        mut file: Box<dyn JournalFile>,
        metadata: Metadata,
        identity: Identity,
        lease: Option<AcquiredLease>,
        driver: Arc<dyn FileDriver>,
        keeper: Arc<Keeper>,
    ) -> Result<Self, PiError> {
        match inspect_opened_session(&path, file.as_mut(), &metadata) {
            Ok((info, _)) => Ok(Self {
                path,
                handle: Mutex::new(Some((file, metadata))),
                info,
                identity,
                lease: Mutex::new(lease),
                replayable: |_| false,
                driver,
                keeper,
            }),
            Err(error) => {
                release_on_prepare_failure(&lease);
                Err(error)
            }
        }
    }

    /// Lets recovery leave calls of the tools `replayable` accepts without a
    /// result, for the agent to run again, instead of settling them as
    /// interrupted. See [`Recovery::replayable`](super::Recovery).
    pub fn with_replayable(mut self, replayable: fn(&str) -> bool) -> Self {
        self.replayable = replayable;
        self
    }

    /// The listing row read while preparing.
    pub fn info(&self) -> SessionInfo {
        self.info.clone()
    }

    /// Consumes the handle and returns a store owning the same descriptor.
    ///
    /// Fails, releasing the descriptor, when the path now names a different
    /// file or the session's identifying metadata changed since preparation.
    pub fn activate(self) -> Result<(Store, Vec<Warning>), PiError> {
        let taken = self
            .handle
            .lock()
            .map_err(|_| PiError::other("prepared session mutex is poisoned"))?
            .take();
        let Some((mut file, metadata)) = taken else {
            return Err(PiError::other("prepared session is no longer available"));
        };
        self.verify_identity(&metadata)?;

        let current_metadata = file
            .metadata()
            .map_err(|error| PiError::other(format!("stat prepared session file: {error}")))?;
        if fsops::file_identity(&metadata) != fsops::file_identity(&current_metadata) {
            return Err(PiError::invalid("prepared session file identity changed"));
        }
        let (current_info, _) =
            inspect_opened_session(&self.path, file.as_mut(), &current_metadata)?;
        if !same_prepared_metadata(&self.info, &current_info) {
            return Err(PiError::invalid(
                "prepared session metadata changed before activation",
            ));
        }
        // Only now, with every check passed, does the lease pass to the
        // `Store`: an `activate` that fails above leaves the lease with
        // `self`, released by `drop` when this `Prepared` goes out of scope.
        let lease = self
            .lease
            .lock()
            .map_err(|_| PiError::other("prepared session mutex is poisoned"))?
            .take();
        Store::from_file(
            file,
            &self.path,
            lease,
            Arc::clone(&self.driver),
            Arc::clone(&self.keeper),
            self.replayable,
        )
    }

    fn verify_identity(&self, prepared: &Metadata) -> Result<(), PiError> {
        const CHANGED: &str = "prepared session path identity changed before activation";
        match &self.identity {
            Identity::Path(path) => {
                let current = std::fs::symlink_metadata(path);
                match current {
                    Ok(current)
                        if current.is_file()
                            && fsops::file_identity(prepared) == fsops::file_identity(&current) =>
                    {
                        Ok(())
                    }
                    _ => Err(PiError::invalid(CHANGED)),
                }
            }
            Identity::Listed {
                root,
                workspace,
                basename,
            } => {
                let Ok((_, current)) = open_listed_prepared_session_file(
                    self.driver.as_ref(),
                    root,
                    workspace,
                    basename,
                ) else {
                    return Err(PiError::invalid(
                        "prepared listed session path identity changed before activation",
                    ));
                };
                if fsops::file_identity(prepared) != fsops::file_identity(&current) {
                    return Err(PiError::invalid(
                        "prepared listed session path identity changed before activation",
                    ));
                }
                Ok(())
            }
        }
    }

    /// Abandons the handle. Safe after activation and idempotent. Releases
    /// the lease, when [`Prepared::activate`] has not already taken it.
    pub fn close(&self) -> Result<(), PiError> {
        self.handle
            .lock()
            .map_err(|_| PiError::other("prepared session mutex is poisoned"))?
            .take();
        let lease = self
            .lease
            .lock()
            .map_err(|_| PiError::other("prepared session mutex is poisoned"))?
            .take();
        if let Some((lease, _)) = lease {
            lease
                .release()
                .map_err(|error| PiError::other(format!("release session lease: {error}")))?;
        }
        Ok(())
    }
}

impl Drop for Prepared {
    /// Best-effort: releases a lease this handle acquired but never
    /// activated or closed, e.g. because `?` returned early from a caller
    /// holding a `Prepared`. Errors are not observable from `drop` and are
    /// discarded.
    fn drop(&mut self) {
        if let Ok(mut guard) = self.lease.lock()
            && let Some((lease, _)) = guard.take()
        {
            let _ = lease.release();
        }
    }
}

pub(crate) fn archive_with_driver_and_keeper(
    root: &Path,
    workspace: &str,
    path: &Path,
    driver: Arc<dyn FileDriver>,
    keeper: Arc<Keeper>,
) -> Result<ArchiveResult, PiError> {
    Prepared::prepare_listed_with_driver_and_keeper(root, workspace, path, None, driver, keeper)?
        .archive(root, workspace)
}

impl Prepared {
    pub(super) fn prepare_delete(
        root: &Path,
        workspace: &str,
        path: &Path,
        driver: Arc<dyn FileDriver>,
        keeper: Arc<Keeper>,
    ) -> Result<Self, PiError> {
        let (root_path, workspace, basename, candidate) =
            validate_listed_candidate_path(root, workspace, path)?;
        let lease = if lease::is_lease_managed(Path::new(&candidate))
            .map_err(|error| PiError::other(error.to_string()))?
        {
            Some(
                lease::acquire_for_delete(Path::new(&candidate), &keeper)
                    .map_err(|error| PiError::other(format!("delete session: {error}")))?,
            )
        } else {
            None
        };
        let opened =
            open_listed_prepared_session_file(driver.as_ref(), &root_path, &workspace, &basename)
                .and_then(|(file, metadata)| {
                    file.lock_exclusive()
                        .map_err(|error| PiError::other(format!("lock session file: {error}")))?;
                    Ok((file, metadata))
                });
        let (file, metadata) = match opened {
            Ok(opened) => opened,
            Err(error) => {
                release_on_prepare_failure(&lease);
                return Err(error);
            }
        };
        let prepared = Self::from_opened(
            candidate,
            file,
            metadata,
            Identity::Listed {
                root: root_path,
                workspace: workspace.clone(),
                basename,
            },
            lease,
            driver,
            keeper,
        )?;
        if !crate::workspace::owns_session(&workspace, &prepared.info.cwd)? {
            prepared.close()?;
            return Err(PiError::invalid(
                "listed session workspace does not match expected workspace",
            ));
        }
        Ok(prepared)
    }

    pub(super) fn delete_permanently(self, root: &Path, workspace: &str) -> Result<(), PiError> {
        let mut handle = self
            .handle
            .lock()
            .map_err(|_| PiError::other("prepared session mutex is poisoned"))?;
        let (file, metadata) = handle
            .as_mut()
            .ok_or_else(|| PiError::other("prepared session is no longer available"))?;
        self.verify_identity(metadata)?;
        let lease = self
            .lease
            .lock()
            .map_err(|_| PiError::other("prepared lease mutex is poisoned"))?;
        let mut action = || {
            super::permanent_delete::delete_open_file(
                root,
                workspace,
                Path::new(&self.path),
                file.as_mut(),
                metadata,
            )
        };
        match lease.as_ref() {
            Some((lease, _)) => lease.deleting(action),
            None => action(),
        }
    }

    /// Archives the file this prepared handle already owns and locks.
    pub(crate) fn archive(self, root: &Path, workspace: &str) -> Result<ArchiveResult, PiError> {
        let path = self.path.clone();
        let (mut file, metadata) = self
            .handle
            .lock()
            .map_err(|_| PiError::other("prepared session mutex is poisoned"))?
            .take()
            .ok_or_else(|| PiError::other("prepared session is no longer available"))?;
        self.verify_identity(&metadata)?;
        archive_open_file(root, workspace, Path::new(&path), file.as_mut(), &metadata)
    }
}

/// Moves a session whose caller already holds its exclusive file lock.
pub(crate) fn archive_open_file(
    root: &Path,
    workspace: &str,
    path: &Path,
    file: &mut dyn JournalFile,
    metadata: &Metadata,
) -> Result<ArchiveResult, PiError> {
    let (_, workspace_canonical, basename, _) =
        validate_listed_candidate_path(root, workspace, path)?;
    if basename.starts_with("archive/") {
        return Err(PiError::invalid(
            "listed session candidate is outside the expected workspace directory",
        ));
    }

    let root_dir = open_session_root_no_follow(root)?;
    let Some((workspace_dir, directory)) =
        open_workspace_session_directory_no_follow(&root_dir, root, &workspace_canonical)?
    else {
        return Err(PiError::invalid(
            "listed session directory no longer exists",
        ));
    };

    let candidate_path = Path::new(&directory)
        .join(&basename)
        .to_string_lossy()
        .into_owned();
    let (session_info, _) = inspect_opened_session(&candidate_path, file, metadata)?;
    match crate::workspace::owns_session(&workspace_canonical, &session_info.cwd)? {
        true => {}
        false => {
            return Err(PiError::invalid(
                "session workspace does not match expected workspace",
            ));
        }
    }

    match std::fs::symlink_metadata(&candidate_path) {
        Ok(current)
            if current.is_file()
                && fsops::file_identity(metadata) == fsops::file_identity(&current) => {}
        _ => {
            return Err(PiError::invalid(
                "session path identity changed before archive",
            ));
        }
    }

    let archive_dir = ensure_archive_directory(&workspace_dir)?;
    let destination = Path::new(&directory)
        .join(ARCHIVE_DIRECTORY_NAME)
        .join(&basename);
    ensure_archive_destination_absent(&archive_dir, &basename)?;

    // RENAME_EXCL makes the move atomic and refuses to replace an existing
    // destination, closing the check-then-rename race.
    fsops::rename_excl(Path::new(&candidate_path), &destination)
        .map_err(|error| PiError::other(format!("archive session file: {error}")))?;
    // Archiving ends the session, so its outstanding timers and queued
    // notifications end with it: both sidecars are removed rather than
    // moved. Best-effort, because the session file has already moved and
    // there is no state left to roll back to. A live session also clears its
    // in-process timers in `app::Controller::archive_current_session`; the
    // in-process inbox is dropped there too, along with the rest of the
    // replaced runner.
    let _ = std::fs::remove_file(Path::new(&candidate_path).with_extension("reminders.json"));
    let _ = std::fs::remove_file(Path::new(&candidate_path).with_extension("inbox.json"));
    let _ = std::fs::remove_file(Path::new(&candidate_path).with_extension("inputs.json"));
    let _ = std::fs::remove_file(Path::new(&candidate_path).with_extension("followups.json"));
    let _ = std::fs::remove_file(Path::new(&candidate_path).with_extension("turns.json"));
    // Sub-agent transcripts live in `<stem>/` beside the session file
    // (`Store::create_child_lazy`) and move with it.
    let children = Path::new(&candidate_path).with_extension("");
    if children.is_dir() {
        fsops::rename_excl(&children, &destination.with_extension("")).map_err(|error| {
            PiError::other(format!(
                "session archived to {}, but moving its sub-agent transcripts failed: {error}",
                destination.display()
            ))
        })?;
    }
    // A lease-managed session's lease directory moves with it, after the
    // session file and its children, so `Lease::release` (called through
    // `Store::close` while a caller still holds the lease past this move)
    // still finds its files afterward.
    let lease_dir = lease::lease_dir(Path::new(&candidate_path));
    if lease_dir.is_dir() {
        fsops::rename_excl(&lease_dir, &lease::lease_dir(&destination)).map_err(|error| {
            PiError::other(format!(
                "session archived to {}, but moving its lease directory failed: {error}",
                destination.display()
            ))
        })?;
    }
    Ok(ArchiveResult {
        path: destination.to_string_lossy().into_owned(),
        id: session_info.id,
    })
}

/// Opens the verified `archive/` directory, creating it 0700 when missing.
fn ensure_archive_directory(workspace_dir: &fsops::Dir) -> Result<fsops::Dir, PiError> {
    if let Err(error) = fsops::mkdir_at(workspace_dir, ARCHIVE_DIRECTORY_NAME, 0o700)
        && error.raw_os_error() != Some(libc::EEXIST)
    {
        return Err(PiError::other(format!("create archive directory: {error}")));
    }
    let archive_dir =
        fsops::open_dir_at_no_follow(workspace_dir, ARCHIVE_DIRECTORY_NAME).map_err(|error| {
            if fsops::is_eloop(&error) {
                PiError::invalid("archive directory is a symlink")
            } else if fsops::is_enotdir(&error) {
                PiError::invalid("archive path is not a directory")
            } else {
                PiError::other(format!("open archive directory: {error}"))
            }
        })?;
    fsops::fchmod_dir(&archive_dir)
        .map_err(|error| PiError::other(format!("chmod archive directory: {error}")))?;
    Ok(archive_dir)
}

/// Rejects an archive move whose destination already exists.
fn ensure_archive_destination_absent(
    archive_dir: &fsops::Dir,
    basename: &str,
) -> Result<(), PiError> {
    match fsops::exists_at_no_follow(archive_dir, basename) {
        Ok(true) => Err(PiError::invalid("archive destination already exists")),
        Ok(false) => Ok(()),
        Err(error) => Err(PiError::other(format!("stat archive destination: {error}"))),
    }
}

/// Checks that `path` is exactly `<root>/<workspace key>/<name>.jsonl`.
///
/// Returns the cleaned root, the canonical workspace, the basename and the
/// cleaned candidate path.
pub(super) fn validate_listed_candidate_path(
    root: &Path,
    workspace: &str,
    path: &Path,
) -> Result<(String, String, String, String), PiError> {
    let root_path = fsops::clean_go_path(&absolute(root)?);
    let workspace_canonical = fsops::canonical_workspace(Path::new(workspace))?;
    let key = crate::workspace::session_key(&workspace_canonical)?;
    let candidate_path = fsops::clean_go_path(&absolute(path)?);
    let basename = candidate_path
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let expected = format!("{root_path}/{key}/{basename}");
    let archived = format!("{root_path}/{key}/archive/{basename}");
    if (candidate_path != expected && candidate_path != archived)
        || path
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        || basename == "."
        || basename == ".."
        || !basename.ends_with(".jsonl")
    {
        return Err(PiError::invalid(
            "listed session candidate is outside the expected workspace directory",
        ));
    }
    let reference = if candidate_path == archived {
        format!("archive/{basename}")
    } else {
        basename
    };
    Ok((root_path, workspace_canonical, reference, candidate_path))
}

/// The path joined to the working directory when relative.
fn absolute(path: &Path) -> Result<String, PiError> {
    if path.is_absolute() {
        return Ok(path.to_string_lossy().into_owned());
    }
    let working = std::env::current_dir()
        .map_err(|error| PiError::other(format!("resolve session root path: {error}")))?;
    Ok(working.join(path).to_string_lossy().into_owned())
}

fn open_listed_prepared_session_file(
    driver: &dyn FileDriver,
    root: &str,
    workspace: &str,
    basename: &str,
) -> Result<(Box<dyn JournalFile>, Metadata), PiError> {
    let root_dir = driver.open_root(Path::new(root)).map_err(|error| {
        map_path_open_error(
            error,
            "open session root",
            "session root is a symlink",
            "session root is not a directory",
        )
    })?;
    let key = crate::workspace::session_key(workspace)?;
    let workspace_dir = root_dir.open_directory(&key).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PiError::invalid("listed session directory no longer exists")
        } else {
            map_path_open_error(
                error,
                "open listed session directory",
                "session directory is a symlink",
                "session directory is not a directory",
            )
        }
    })?;
    let (workspace_dir, basename) = if let Some(basename) = basename.strip_prefix("archive/") {
        let archive = workspace_dir.open_directory("archive").map_err(|error| {
            map_path_open_error(
                error,
                "open listed archive directory",
                "archive directory is a symlink",
                "archive path is not a directory",
            )
        })?;
        (archive, basename)
    } else {
        (workspace_dir, basename)
    };
    let file = workspace_dir
        .open_journal(basename, JournalOpen::ReadWrite)
        .map_err(|error| {
            map_path_open_error(
                error,
                "open listed session file",
                "listed session file is a symlink",
                "listed session file is not a regular file",
            )
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| PiError::other(format!("stat listed session file: {error}")))?;
    if !metadata.is_file() {
        return Err(PiError::invalid(
            "listed session file is not a regular file",
        ));
    }
    Ok((file, metadata))
}

fn open_prepared_session_file_no_follow(
    driver: &dyn FileDriver,
    path: &Path,
) -> Result<(Box<dyn JournalFile>, Metadata), PiError> {
    let parent = path
        .parent()
        .ok_or_else(|| PiError::invalid("session file has no parent"))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| PiError::invalid("session file name is not a valid component"))?;
    let root = driver.open_root(parent).map_err(|error| {
        map_path_open_error(
            error,
            "open session directory",
            "session directory is a symlink",
            "session directory is not a directory",
        )
    })?;
    let file = root
        .open_journal(name, JournalOpen::ReadWrite)
        .map_err(|error| {
            map_path_open_error(
                error,
                "open session file",
                "session file is a symlink",
                "session file is not a regular file",
            )
        })?;
    let metadata = file
        .metadata()
        .map_err(|error| PiError::other(format!("stat session file: {error}")))?;
    if !metadata.is_file() {
        return Err(PiError::invalid("session file is not a regular file"));
    }
    Ok((file, metadata))
}

fn map_path_open_error(
    error: std::io::Error,
    operation: &str,
    symlink: &str,
    non_regular: &str,
) -> PiError {
    match error.raw_os_error() {
        Some(libc::ELOOP) => PiError::invalid(symlink),
        Some(libc::EISDIR) | Some(libc::ENOTDIR) => PiError::invalid(non_regular),
        _ if error.kind() == std::io::ErrorKind::InvalidInput => PiError::invalid(non_regular),
        _ => PiError::other(format!("{operation}: {error}")),
    }
}

fn same_prepared_metadata(prepared: &SessionInfo, current: &SessionInfo) -> bool {
    prepared.id == current.id
        && prepared.cwd == current.cwd
        && prepared.profile == current.profile
        && prepared.provider == current.provider
        && prepared.model == current.model
        && prepared.created == current.created
}
