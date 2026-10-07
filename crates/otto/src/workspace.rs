//! Persistent workspace ownership, independent of runtime processes and paths.
//!
//! The local marker travels with a workspace. Legacy session keys and memory
//! scopes are adopted without rewriting state. Writes are serialized by a
//! directory lock and atomically published through pinned, no-follow handles.
//! Recorded paths identify historical transcripts, never execution authority.

use std::io::Read;
use std::path::Path;

use otto_core::session::PiError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::fsops;

pub const MARKER: &str = ".otto/workspace.json";
/// Protect the marker and its parent entry from model-originated replacement.
pub(crate) const READ_ONLY_PATHS: [(&str, bool); 2] = [(".otto", false), (MARKER, false)];
const NAME: &str = "workspace.json";
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    pub version: u8,
    pub id: String,
    pub session_key: String,
    pub memory_scope: String,
    pub paths: Vec<String>,
}

fn invalid() -> PiError {
    PiError::invalid("workspace identity is invalid or conflicts with memory.workspace_ids")
}

fn io_error(error: std::io::Error) -> PiError {
    PiError::other(format!("workspace identity: {error}"))
}

fn validate(identity: &Identity) -> Result<(), PiError> {
    let uuid = identity.id.as_bytes();
    if identity.version != 1
        || uuid.len() != 36
        || !uuid.iter().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                *b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
        || uuid[14] != b'4'
        || !matches!(uuid[19], b'8' | b'9' | b'a' | b'b')
        || ![16, 32].contains(&identity.session_key.len())
        || !identity.session_key.bytes().all(|b| b.is_ascii_hexdigit())
        || crate::memory::scope::new_workspace_scope("", &identity.memory_scope).is_err()
        || identity.paths.is_empty()
        || identity.paths.iter().any(|p| {
            !Path::new(p).is_absolute()
                || p.chars().any(char::is_control)
                || fsops::clean_go_path(p) != *p
        })
    {
        return Err(invalid());
    }
    Ok(())
}

fn read_at(dir: &fsops::Dir) -> Result<Option<Identity>, PiError> {
    let file = match fsops::open_at_no_follow(dir, NAME, libc::O_RDONLY | libc::O_NONBLOCK) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    let identity = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    validate(&identity)?;
    Ok(Some(identity))
}

/// Read-only lookup; an absent marker retains legacy behavior.
pub fn read(workspace: &Path) -> Result<Option<Identity>, PiError> {
    let root = match fsops::open_dir_no_follow(workspace) {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let dir = match fsops::open_dir_at_no_follow(&root, ".otto") {
        Ok(dir) => dir,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    read_at(&dir)
}

fn uuid() -> Result<String, PiError> {
    let mut bytes = crate::auth::random_bytes::<16>().map_err(io_error)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

fn publish(
    dir: &fsops::Dir,
    identity: &Identity,
    publish_ops: &dyn fsops::PublishOps,
) -> Result<(), PiError> {
    validate(identity)?;
    let bytes = serde_json::to_vec_pretty(identity).map_err(|_| invalid())?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(invalid());
    }
    let temp = format!(
        ".workspace-{}",
        crate::auth::random_hex::<8>().map_err(io_error)?
    );
    fsops::publish_file(
        dir,
        &fsops::Publish {
            name: NAME,
            temp: &temp,
            permissions: 0o600,
            mode: fsops::PublishMode::Replace,
        },
        &bytes,
        publish_ops,
    )
    .map_err(|error| io_error(error.source))
}

/// Initialize on startup, adopting existing state and retaining move history.
/// A configured memory override is adopted once; later conflicts fail closed.
pub fn initialize(workspace: &Path, memory_override: &str) -> Result<Identity, PiError> {
    update(workspace, memory_override, false)
}

/// Explicitly make a copied workspace independent; old state remains untouched.
/// Stop processes using this workspace before calling this operation.
pub fn fork(workspace: &Path) -> Result<Identity, PiError> {
    update(workspace, "", true)
}

fn update(workspace: &Path, memory_override: &str, fork: bool) -> Result<Identity, PiError> {
    let canonical = std::fs::canonicalize(workspace).map_err(io_error)?;
    let path = canonical.to_str().ok_or_else(invalid)?.to_owned();
    let root = fsops::open_dir_no_follow(&canonical).map_err(io_error)?;
    match fsops::mkdir_at(&root, ".otto", 0o700) {
        Ok(()) => fsops::open_at_no_follow(&root, ".", libc::O_RDONLY | libc::O_DIRECTORY)
            .and_then(|file| file.sync_all())
            .map_err(io_error)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(io_error(error)),
    }
    let dir = fsops::open_dir_at_no_follow(&root, ".otto").map_err(io_error)?;
    let _lock = fsops::lock_directory(&dir)?;
    let previous = read_at(&dir)?;
    let mut identity = if !fork && let Some(identity) = previous.as_ref() {
        if !memory_override.is_empty() && memory_override != identity.memory_scope {
            return Err(invalid());
        }
        identity.clone()
    } else {
        let id = uuid()?;
        let session_key = if fork {
            id.replace('-', "")
        } else {
            legacy_session_key(&path)
        };
        let memory_scope = if fork {
            id.clone()
        } else if memory_override.is_empty() {
            format!("sha256:{:x}", Sha256::digest(path.as_bytes()))
        } else {
            memory_override.to_owned()
        };
        Identity {
            version: 1,
            id,
            session_key,
            memory_scope,
            paths: vec![path.clone()],
        }
    };
    if !identity.paths.contains(&path) {
        identity.paths.push(path);
    }
    if previous.as_ref() != Some(&identity) {
        publish(&dir, &identity, &fsops::PosixPublish)?;
    }
    Ok(identity)
}

fn legacy_session_key(canonical: &str) -> String {
    format!("{:x}", Sha256::digest(canonical.as_bytes()))[..16].to_owned()
}

/// Stable directory key with a read-only legacy fallback.
pub(crate) fn session_key(canonical: &str) -> Result<String, PiError> {
    Ok(read(Path::new(canonical))?
        .map(|i| i.session_key)
        .unwrap_or_else(|| legacy_session_key(canonical)))
}

// Resolve surviving ancestors when an old directory no longer exists. This
// preserves aliases such as macOS /var -> /private/var without following a
// replacement workspace's marker or treating path history as access authority.
fn historical_path(recorded: &str) -> Result<String, PiError> {
    let absolute = std::path::absolute(recorded).map_err(io_error)?;
    let mut ancestor = absolute.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut canonical) => {
                for component in missing.iter().rev() {
                    canonical.push(component);
                }
                return Ok(fsops::clean_go_path(&canonical.to_string_lossy()));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name().ok_or_else(invalid)?);
                ancestor = ancestor.parent().ok_or_else(invalid)?;
            }
            Err(error) => return Err(io_error(error)),
        }
    }
}

/// Historical path membership is checked only for session ownership.
pub(crate) fn owns_session(workspace: &str, recorded: &str) -> Result<bool, PiError> {
    let canonical = fsops::canonical_workspace(Path::new(workspace))?;
    let recorded = historical_path(recorded)?;
    if canonical == recorded {
        return Ok(true);
    }
    Ok(read(Path::new(&canonical))?.is_some_and(|i| i.paths.contains(&recorded)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::faults::publish::FaultyPublishOps;
    use crate::faults::{Fault, FaultError, FaultPlan, Rule, Trigger};
    use crate::fsops::PublishStep;
    use std::fs;
    use std::os::unix::fs::symlink;

    /// Publishes a changed identity over an initialized workspace's marker
    /// with `step` failing, and returns the result with the `.otto` directory,
    /// the previous marker bytes and the changed identity.
    fn publish_failing_at(
        step: PublishStep,
    ) -> (tempfile::TempDir, Result<(), PiError>, Vec<u8>, Identity) {
        let dir = tempfile::tempdir().unwrap();
        let mut identity = initialize(dir.path(), "").unwrap();
        let marker = dir.path().join(MARKER);
        let previous = fs::read(&marker).unwrap();
        identity.paths.push("/elsewhere".to_owned());
        let plan = FaultPlan::new(vec![Rule::new(
            step,
            Trigger::Always,
            Fault::Before(FaultError::Os(libc::EIO)),
        )]);
        let otto = fsops::open_dir_no_follow(&dir.path().join(".otto")).unwrap();
        let result = publish(&otto, &identity, &FaultyPublishOps::new(plan.clone()));
        plan.assert_all_fired();
        (dir, result, previous, identity)
    }

    #[test]
    fn a_rename_failure_keeps_the_previous_identity_and_leaves_no_temporary_file() {
        let (dir, result, previous, _) = publish_failing_at(PublishStep::Rename);
        assert!(result.is_err());
        assert_eq!(fs::read(dir.path().join(MARKER)).unwrap(), previous);
        let names: Vec<_> = fs::read_dir(dir.path().join(".otto"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .filter(|name| name.starts_with(".workspace-"))
            .collect();
        assert!(names.is_empty(), "{names:?}");
    }

    #[test]
    fn a_directory_sync_failure_is_an_error_after_the_identity_is_in_place() {
        let (dir, result, previous, identity) = publish_failing_at(PublishStep::DirSync);
        assert!(result.is_err());
        let stored = fs::read(dir.path().join(MARKER)).unwrap();
        assert_ne!(stored, previous);
        assert_eq!(
            serde_json::from_slice::<Identity>(&stored).unwrap(),
            identity
        );
    }

    #[test]
    fn initialization_is_stable_adopts_legacy_keys_and_rejects_override_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        let path = path.to_str().unwrap();
        let legacy = legacy_session_key(path);
        let identity = initialize(Path::new(path), "configured-memory").unwrap();
        assert_eq!(identity.session_key, legacy);
        assert_eq!(identity.memory_scope, "configured-memory");
        assert_eq!(
            initialize(Path::new(path), "configured-memory").unwrap(),
            identity
        );
        assert_eq!(initialize(Path::new(path), "").unwrap(), identity);
        assert!(initialize(Path::new(path), "different-memory").is_err());
        assert_eq!(read(Path::new(path)).unwrap().unwrap(), identity);
    }

    #[test]
    fn moves_keep_identity_copies_keep_identity_and_forks_and_new_directories_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("original");
        fs::create_dir(&original).unwrap();
        let identity = initialize(&original, "").unwrap();
        let moved = dir.path().join("moved");
        fs::rename(&original, &moved).unwrap();
        let after = initialize(&moved, "").unwrap();
        assert_eq!(after.id, identity.id);
        assert_eq!(after.session_key, identity.session_key);
        assert_eq!(after.memory_scope, identity.memory_scope);
        assert!(owns_session(moved.to_str().unwrap(), &identity.paths[0]).unwrap());
        let copy = dir.path().join("copy");
        fs::create_dir_all(copy.join(".otto")).unwrap();
        fs::copy(moved.join(MARKER), copy.join(MARKER)).unwrap();
        assert_eq!(initialize(&copy, "").unwrap().id, identity.id);
        let forked = fork(&copy).unwrap();
        assert_ne!(forked.id, identity.id);
        assert_ne!(forked.session_key, identity.session_key);
        assert_ne!(forked.memory_scope, identity.memory_scope);
        assert!(!owns_session(copy.to_str().unwrap(), &identity.paths[0]).unwrap());
        let independent = dir.path().join("worktree");
        fs::create_dir(&independent).unwrap();
        assert_ne!(initialize(&independent, "").unwrap().id, identity.id);
    }

    #[test]
    fn concurrent_startups_publish_one_complete_identity() {
        let dir = tempfile::tempdir().unwrap();
        let identities = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| initialize(dir.path(), "").unwrap()))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(identities.iter().all(|identity| identity == &identities[0]));
        assert_eq!(read(dir.path()).unwrap().unwrap(), identities[0]);
    }

    #[test]
    fn malformed_and_symlinked_markers_fail_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join(".otto")).unwrap();
        let marker = dir.path().join(MARKER);
        fs::write(&marker, "bad-json").unwrap();
        assert!(initialize(dir.path(), "").is_err());
        assert!(fork(dir.path()).is_err());
        assert_eq!(fs::read_to_string(&marker).unwrap(), "bad-json");
        fs::remove_file(&marker).unwrap();
        let target = dir.path().join("target");
        fs::write(&target, "untouched").unwrap();
        symlink(&target, &marker).unwrap();
        assert!(read(dir.path()).is_err());
        assert!(initialize(dir.path(), "").is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
        fs::remove_file(&marker).unwrap();
        fs::remove_dir(dir.path().join(".otto")).unwrap();
        symlink(dir.path(), dir.path().join(".otto")).unwrap();
        assert!(initialize(dir.path(), "").is_err());
    }
}
