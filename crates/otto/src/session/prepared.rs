//! Preparing a session for activation, and archiving one.
//!
//! Port of `internal/session/prepared.go` and `internal/session/archive.go`.
//!
//! Ownership: a [`Prepared`] owns one verified descriptor until
//! [`Prepared::activate`] transfers it to a [`Store`] or [`Prepared::close`]
//! releases it; both are idempotent and neither leaks the descriptor on
//! failure. Concurrency: the handle is behind a mutex, so racing `activate`
//! calls leave exactly one winner. Errors: an identity that changed between
//! preparation and activation is rejected rather than used.

use std::fs::{File, Metadata};
use std::path::Path;
use std::sync::Mutex;

use otto_core::session::{PiError, SessionInfo, Warning};

use super::fsops;
use super::list::{
    inspect_opened_session, open_session_file_read_only_no_follow_at, open_session_root_no_follow,
    open_workspace_session_directory_no_follow,
};
use super::store::Store;

const ARCHIVE_DIRECTORY_NAME: &str = "archive";

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
#[derive(Debug)]
pub struct Prepared {
    path: String,
    handle: Mutex<Option<(File, Metadata)>>,
    info: SessionInfo,
    identity: Identity,
}

/// A completed archive move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveResult {
    pub path: String,
    pub id: String,
}

impl Prepared {
    /// Pins the session file at `path`, read-write, refusing a symlink.
    pub fn prepare(path: &Path) -> Result<Self, PiError> {
        let (file, metadata) = open_prepared_session_file_no_follow(path)?;
        let path = path.to_string_lossy().into_owned();
        Self::from_opened(path.clone(), file, metadata, Identity::Path(path))
    }

    /// Pins a candidate that `list` returned for this root and workspace.
    ///
    /// Every attacker-controlled component below the root is traversed with
    /// `openat` and `O_NOFOLLOW` before the descriptor is pinned, and the
    /// session's recorded workspace must match the expected one.
    pub fn prepare_listed(root: &Path, workspace: &str, path: &Path) -> Result<Self, PiError> {
        let (root_path, workspace_canonical, basename, candidate_path) =
            validate_listed_candidate_path(root, workspace, path)?;
        let (file, metadata) =
            open_listed_prepared_session_file(&root_path, &workspace_canonical, &basename)?;
        let prepared = Self::from_opened(
            candidate_path,
            file,
            metadata,
            Identity::Listed {
                root: root_path,
                workspace: workspace_canonical.clone(),
                basename,
            },
        )?;
        match fsops::canonical_workspace(Path::new(&prepared.info.cwd)) {
            Ok(candidate) if candidate == workspace_canonical => Ok(prepared),
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
        mut file: File,
        metadata: Metadata,
        identity: Identity,
    ) -> Result<Self, PiError> {
        let (info, _) = inspect_opened_session(&path, &mut file, &metadata)?;
        Ok(Self {
            path,
            handle: Mutex::new(Some((file, metadata))),
            info,
            identity,
        })
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
        let (current_info, _) = inspect_opened_session(&self.path, &mut file, &current_metadata)?;
        if !same_prepared_metadata(&self.info, &current_info) {
            return Err(PiError::invalid(
                "prepared session metadata changed before activation",
            ));
        }
        Store::from_file(file, &self.path)
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
                let Ok((_, current)) = open_listed_prepared_session_file(root, workspace, basename)
                else {
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

    /// Abandons the handle. Safe after activation and idempotent.
    pub fn close(&self) -> Result<(), PiError> {
        self.handle
            .lock()
            .map_err(|_| PiError::other("prepared session mutex is poisoned"))?
            .take();
        Ok(())
    }
}

/// Moves one active session file into the workspace's `archive/` directory.
///
/// The source must be a direct child of the workspace-key directory and a
/// valid Pi v3 session whose recorded workspace matches the current canonical
/// workspace. The move is atomic and never replaces an existing archived
/// session; on success the file exists only under `archive/`.
pub fn archive(root: &Path, workspace: &str, path: &Path) -> Result<ArchiveResult, PiError> {
    let (_, workspace_canonical, basename, _) =
        validate_listed_candidate_path(root, workspace, path)?;

    let root_dir = open_session_root_no_follow(root)?;
    let Some((workspace_dir, directory)) =
        open_workspace_session_directory_no_follow(&root_dir, root, &workspace_canonical)?
    else {
        return Err(PiError::invalid(
            "listed session directory no longer exists",
        ));
    };

    let (mut file, metadata, candidate_path) =
        open_session_file_read_only_no_follow_at(&workspace_dir, &directory, &basename)?;
    let (session_info, _) = inspect_opened_session(&candidate_path, &mut file, &metadata)?;
    match fsops::canonical_workspace(Path::new(&session_info.cwd)) {
        Ok(recorded) if recorded == workspace_canonical => {}
        _ => {
            return Err(PiError::invalid(
                "session workspace does not match expected workspace",
            ));
        }
    }

    match std::fs::symlink_metadata(&candidate_path) {
        Ok(current)
            if current.is_file()
                && fsops::file_identity(&metadata) == fsops::file_identity(&current) => {}
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
    // Archiving ends the session, so its outstanding timers end with it: the
    // sidecar is removed rather than moved. Best-effort, because the session
    // file has already moved and there is no state left to roll back to. A
    // live session also clears its in-process timers in
    // `app::Controller::archive_current_session`.
    let _ = std::fs::remove_file(Path::new(&candidate_path).with_extension("reminders.json"));
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
fn validate_listed_candidate_path(
    root: &Path,
    workspace: &str,
    path: &Path,
) -> Result<(String, String, String, String), PiError> {
    let root_path = fsops::clean_go_path(&absolute(root)?);
    let workspace_canonical = fsops::canonical_workspace(Path::new(workspace))?;
    let key = fsops::workspace_key_of_canonical(&workspace_canonical);
    let candidate_path = fsops::clean_go_path(&absolute(path)?);
    let basename = candidate_path
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    let expected = format!("{root_path}/{key}/{basename}");
    if candidate_path != expected
        || basename == "."
        || basename == ".."
        || !basename.ends_with(".jsonl")
    {
        return Err(PiError::invalid(
            "listed session candidate is outside the expected workspace directory",
        ));
    }
    Ok((root_path, workspace_canonical, basename, candidate_path))
}

/// Go's `filepath.Abs`: the path joined to the working directory when relative.
fn absolute(path: &Path) -> Result<String, PiError> {
    if path.is_absolute() {
        return Ok(path.to_string_lossy().into_owned());
    }
    let working = std::env::current_dir()
        .map_err(|error| PiError::other(format!("resolve session root path: {error}")))?;
    Ok(working.join(path).to_string_lossy().into_owned())
}

fn open_listed_prepared_session_file(
    root: &str,
    workspace: &str,
    basename: &str,
) -> Result<(File, Metadata), PiError> {
    let root_dir = open_session_root_no_follow(Path::new(root))?;
    let Some((workspace_dir, _)) =
        open_workspace_session_directory_no_follow(&root_dir, Path::new(root), workspace)?
    else {
        return Err(PiError::invalid(
            "listed session directory no longer exists",
        ));
    };
    let file =
        fsops::open_at_no_follow(&workspace_dir, basename, libc::O_RDWR).map_err(|error| {
            if fsops::is_eloop(&error) {
                PiError::invalid("listed session file is a symlink")
            } else {
                PiError::other(format!("open listed session file: {error}"))
            }
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

fn open_prepared_session_file_no_follow(path: &Path) -> Result<(File, Metadata), PiError> {
    let file = fsops::open_no_follow(path, libc::O_RDWR).map_err(|error| {
        if fsops::is_eloop(&error) {
            PiError::invalid("session file is a symlink")
        } else {
            PiError::other(format!("open session file: {error}"))
        }
    })?;
    let metadata = file
        .metadata()
        .map_err(|error| PiError::other(format!("stat session file: {error}")))?;
    if !metadata.is_file() {
        return Err(PiError::invalid("session file is not a regular file"));
    }
    Ok((file, metadata))
}

fn same_prepared_metadata(prepared: &SessionInfo, current: &SessionInfo) -> bool {
    prepared.id == current.id
        && prepared.cwd == current.cwd
        && prepared.profile == current.profile
        && prepared.provider == current.provider
        && prepared.model == current.model
        && prepared.created == current.created
}
