//! The workspace boundary shared by every file tool.
//!
//! A [`Workspace`] holds one directory tree and turns a caller-supplied path
//! into a name that is guaranteed to stay inside it. The resolution order and
//! the rejection rules are the security contract:
//!
//! - the root is canonicalized once at construction and kept open as a
//!   directory handle, so renaming or replacing the root directory afterwards
//!   cannot redirect a tool to another tree;
//! - relative paths are resolved through that handle ([`super::root::Root`]),
//!   which refuses `..` that leaves the root and refuses absolute symlink
//!   targets;
//! - absolute paths, paths containing `..`, and paths crossing a symlink are
//!   additionally canonicalized against the real filesystem and checked to be
//!   inside the root;
//! - a path that resolves outside the root fails with `path escapes workspace:
//!   {path}`.
//!
//! Ownership: the workspace owns the root directory handle and closes it on
//! drop. Callers own returned files and paths.
//!
//! Concurrency: every method takes `&self`; a `Workspace` may be shared across
//! threads. Resolution is not atomic with the operation the caller performs
//! afterwards, so a tool must act through the returned handle or through the
//! workspace's own root-relative operations rather than re-opening by absolute
//! path.
//!
//! Errors: all failures are `std::io::Error`; escapes use
//! [`std::io::ErrorKind::InvalidInput`] with the message text above.

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

use super::gopath::{base, bytes, clean, dir, has_parent_traversal, is_abs, join, path_from, rel};
use super::root::{Root, is_symlink};

/// Maximum number of symbolic links followed while resolving a write target.
const MAX_WRITE_SYMLINKS: usize = 40;

/// A directory tree that file tools may not leave.
#[derive(Debug)]
pub struct Workspace {
    root: PathBuf,
    lexical_root: PathBuf,
    root_fs: Root,
    /// One mutex per file so that `write` and `edit`, which subagents share
    /// with the parent agent, never interleave a read-modify-write on the same
    /// file.
    mutations: Mutex<HashMap<MutationKey, Arc<tokio::sync::Mutex<()>>>>,
}

/// The identity a mutation lock is keyed by. Spellings of one file that differ
/// in `.`, repeated slashes, or a symbolic-linked directory name the same
/// parent directory, so they share an [`MutationKey::Entry`]. A path whose
/// parent does not exist yet falls back to its cleaned spelling.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum MutationKey {
    Entry {
        device: u64,
        inode: u64,
        name: Vec<u8>,
    },
    Path(Vec<u8>),
}

impl Workspace {
    /// Opens `root` as a workspace. The path is made absolute, cleaned, and
    /// resolved through symbolic links; the resolved directory is opened and
    /// held for the lifetime of the workspace.
    pub fn new(root: &Path) -> io::Result<Self> {
        let absolute = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()?.join(root)
        };
        let lexical_root = path_from(clean(bytes(&absolute)));
        let resolved = std::fs::canonicalize(&lexical_root)?;
        let root_fs = Root::open(&resolved)?;
        Ok(Self {
            root: resolved,
            lexical_root,
            root_fs,
            mutations: Mutex::new(HashMap::new()),
        })
    }

    /// Waits until no other `write` or `edit` holds the file `key` names, then
    /// returns the guard that releases it. `key` is the root-relative name
    /// returned by [`Workspace::write_relative`], which already resolves a
    /// final symbolic link; the lock is keyed by the parent directory's
    /// identity and the final name, so every spelling of one file shares it.
    // ponytail: entries live for the workspace lifetime; add ref-counted
    // cleanup if path churn matters.
    pub(crate) async fn lock_path(&self, key: &Path) -> tokio::sync::OwnedMutexGuard<()> {
        let key = self.mutation_key(key);
        let lock = {
            let mut mutations = self
                .mutations
                .lock()
                .expect("the lock table is never poisoned");
            Arc::clone(mutations.entry(key).or_default())
        };
        lock.lock_owned().await
    }

    fn mutation_key(&self, path: &Path) -> MutationKey {
        let cleaned = clean(bytes(path));
        let parent = self.root_fs.open_file(
            &path_from(dir(&cleaned)),
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NONBLOCK,
            Mode::empty(),
        );
        match parent.and_then(|parent| parent.metadata()) {
            Ok(metadata) => MutationKey::Entry {
                device: std::os::unix::fs::MetadataExt::dev(&metadata),
                inode: std::os::unix::fs::MetadataExt::ino(&metadata),
                name: base(&cleaned),
            },
            Err(_) => MutationKey::Path(cleaned),
        }
    }

    /// The canonical absolute root directory. Every resolved path is inside it.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The cleaned absolute root as the caller spelled it, before symbolic
    /// links were resolved. `bash` uses it to validate its working directory.
    pub fn lexical_root(&self) -> &Path {
        &self.lexical_root
    }

    /// The root directory handle, for tools that walk or write by
    /// root-relative name.
    pub(crate) fn root_fs(&self) -> &Root {
        &self.root_fs
    }

    /// Opens an existing workspace file for reading. Opening never blocks on a
    /// FIFO or device: the file is opened non-blocking and the caller is
    /// expected to reject anything that is not a regular file.
    pub fn open(&self, path: &Path) -> io::Result<File> {
        let relative = self.existing_relative(path)?;
        self.open_relative(&relative)
    }

    pub(crate) fn open_relative(&self, path: &Path) -> io::Result<File> {
        self.root_fs
            .open_file(path, OFlag::O_RDONLY | OFlag::O_NONBLOCK, Mode::empty())
    }

    /// Resolves an existing path to its canonical absolute location inside the
    /// workspace, following symbolic links.
    pub fn resolve_existing(&self, path: &Path) -> io::Result<PathBuf> {
        self.canonical_existing(path)
    }

    /// Resolves a write target to its canonical absolute location inside the
    /// workspace. The final element need not exist; missing parents are
    /// allowed, and a symbolic link that stays inside the workspace resolves to
    /// its target.
    pub fn resolve_for_write(&self, path: &Path) -> io::Result<PathBuf> {
        self.canonical_for_write(path)
    }

    /// Resolves an existing path to a root-relative name usable with
    /// [`Workspace::root_fs`].
    pub(crate) fn existing_relative(&self, path: &Path) -> io::Result<PathBuf> {
        let relative = self.direct_relative(path)?;
        match self.open_relative(&relative) {
            Ok(file) => {
                drop(file);
                if has_parent_traversal(bytes(&relative)) {
                    return self.canonical_existing_relative(path);
                }
                if self.path_has_symlink(&relative)? {
                    return self.canonical_existing_relative(path);
                }
                Ok(path_from(clean(bytes(&relative))))
            }
            Err(open_error) => {
                if is_abs(bytes(path)) {
                    return Err(open_error);
                }
                // The root handle rejects absolute symlink targets, including
                // ones pointing back into the workspace. Convert a verified
                // internal target to a root-relative name.
                let canonical = self.canonical_existing_relative(path)?;
                if canonical != relative {
                    let file = self.open_relative(&canonical)?;
                    drop(file);
                    return Ok(canonical);
                }
                Err(open_error)
            }
        }
    }

    /// Resolves a write target to a root-relative name without resolving the
    /// final element through ordinary filesystem I/O, so writing through a
    /// symbolic link updates the link target instead of replacing the link.
    pub(crate) fn write_relative(&self, path: &Path) -> io::Result<PathBuf> {
        if is_abs(bytes(path)) || has_parent_traversal(bytes(path)) {
            let canonical = self.canonical_for_write(path)?;
            return Ok(path_from(rel(bytes(&self.root), bytes(&canonical))?));
        }
        let relative = self.direct_relative(path)?;
        let mut ancestor = dir(bytes(&relative));
        while ancestor != b"." {
            match self.open_relative(&path_from(ancestor.clone())) {
                Ok(file) => {
                    drop(file);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    ancestor = dir(&ancestor);
                }
                Err(_) => {
                    let canonical = self.canonical_for_write(path)?;
                    return Ok(path_from(rel(bytes(&self.root), bytes(&canonical))?));
                }
            }
        }
        self.final_write_relative(relative, path)
    }

    fn direct_relative(&self, path: &Path) -> io::Result<PathBuf> {
        let raw = bytes(path);
        if raw.is_empty() || raw == b"." {
            return Ok(PathBuf::from("."));
        }
        if is_abs(raw) {
            return self.canonical_existing_relative(path);
        }
        Ok(path.to_path_buf())
    }

    fn path_has_symlink(&self, path: &Path) -> io::Result<bool> {
        let mut current: Vec<u8> = Vec::new();
        for part in bytes(path).split(|byte| *byte == b'/') {
            if part.is_empty() || part == b"." {
                continue;
            }
            current = join(&[&current, part]);
            let stat = self.root_fs.lstat(&path_from(current.clone()))?;
            if is_symlink(&stat) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn final_write_relative(&self, start: PathBuf, original: &Path) -> io::Result<PathBuf> {
        let mut current = start;
        for _ in 0..MAX_WRITE_SYMLINKS {
            let stat = match self.root_fs.lstat(&current) {
                Ok(stat) => stat,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(current),
                Err(error) => return Err(error),
            };
            if !is_symlink(&stat) {
                return Ok(current);
            }
            let target = self.root_fs.read_link(&current)?;
            if is_abs(bytes(&target)) {
                let canonical = self.canonical_for_write(original)?;
                return Ok(path_from(rel(bytes(&self.root), bytes(&canonical))?));
            }
            let parent = dir(bytes(&current));
            current = if parent == b"." {
                target
            } else {
                // Deliberately unjoined: the next iteration must see the
                // uncleaned name, so that a `..` inside the link target is
                // resolved by the root handle rather than lexically. inside the
                // link target is resolved by the root handle rather than
                // lexically.
                let mut spliced = parent;
                spliced.push(b'/');
                spliced.extend_from_slice(bytes(&target));
                path_from(spliced)
            };
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("too many symbolic links: {}", original.display()),
        ))
    }

    fn canonical_existing_relative(&self, path: &Path) -> io::Result<PathBuf> {
        let resolved = self.canonical_existing(path)?;
        Ok(path_from(rel(bytes(&self.root), bytes(&resolved))?))
    }

    fn canonical_existing(&self, path: &Path) -> io::Result<PathBuf> {
        let candidate = self.candidate_path(path);
        let resolved = std::fs::canonicalize(&candidate)?;
        self.ensure_inside(&resolved)?;
        Ok(resolved)
    }

    fn canonical_for_write(&self, path: &Path) -> io::Result<PathBuf> {
        let candidate = self.candidate_path(path);
        let (ancestor, suffix) = find_existing_ancestor(&candidate)?;
        let resolved_ancestor = std::fs::canonicalize(&ancestor)?;
        self.ensure_inside(&resolved_ancestor)?;
        if suffix.is_empty() {
            return Ok(resolved_ancestor);
        }
        if has_parent_traversal(&suffix) {
            return Err(escapes_workspace(bytes(path)));
        }
        Ok(path_from(join(&[bytes(&resolved_ancestor), &suffix])))
    }

    /// Builds the absolute path a relative request refers to. The result is
    /// deliberately not cleaned: `link/../note.txt` must stay unresolved so the
    /// symbolic link is followed before `..` is applied.
    pub(crate) fn candidate_path(&self, path: &Path) -> PathBuf {
        let raw = bytes(path);
        if raw.is_empty() {
            return self.root.clone();
        }
        if is_abs(raw) {
            return path.to_path_buf();
        }
        let mut candidate = bytes(&self.root).to_vec();
        candidate.push(b'/');
        candidate.extend_from_slice(raw);
        path_from(candidate)
    }

    /// Fails when `candidate` is not the root or below it.
    pub(crate) fn ensure_inside(&self, candidate: &Path) -> io::Result<()> {
        let relative = rel(bytes(&self.root), bytes(candidate))?;
        if relative == b".." || relative.starts_with(b"../") {
            return Err(escapes_workspace(bytes(candidate)));
        }
        Ok(())
    }
}

/// Walks up from `candidate` to the first element that exists, returning that
/// ancestor and the remaining suffix. `os.Lstat` is used, so a dangling
/// symbolic link counts as existing.
fn find_existing_ancestor(candidate: &Path) -> io::Result<(PathBuf, Vec<u8>)> {
    let raw = bytes(candidate).to_vec();
    let mut ancestor = raw.clone();
    loop {
        match std::fs::symlink_metadata(path_from(ancestor.clone())) {
            Ok(_) => {
                let mut suffix = raw[ancestor.len().min(raw.len())..].to_vec();
                if !raw.starts_with(&ancestor) {
                    suffix = raw.clone();
                }
                if suffix.first() == Some(&b'/') {
                    suffix.remove(0);
                }
                return Ok((path_from(ancestor), suffix));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let parent = dir(&ancestor);
        if parent == ancestor {
            return Err(escapes_workspace(&raw));
        }
        ancestor = parent;
    }
}

fn escapes_workspace(path: &[u8]) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("path escapes workspace: {}", String::from_utf8_lossy(path)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(root: &Path) -> Workspace {
        Workspace::new(root).expect("workspace should open")
    }

    fn canonical(path: &Path) -> PathBuf {
        std::fs::canonicalize(path).expect("path should canonicalize")
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let error = workspace(root.path())
            .resolve_for_write(Path::new("../escape.txt"))
            .expect_err("traversal should be rejected");
        assert!(error.to_string().contains("escapes workspace"), "{error}");
    }

    #[test]
    fn a_symlink_leaving_the_workspace_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let error = workspace(root.path())
            .resolve_for_write(Path::new("link/new.txt"))
            .expect_err("symlink escape should be rejected");
        assert!(error.to_string().contains("escapes workspace"), "{error}");
    }

    #[test]
    fn a_missing_nested_relative_path_resolves_under_the_root() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = canonical(root.path());
        let got = workspace(root.path())
            .resolve_for_write(Path::new("nested/dir/file.txt"))
            .unwrap();
        assert_eq!(got, canonical_root.join("nested/dir/file.txt"));
    }

    #[test]
    fn an_absolute_path_inside_the_workspace_resolves_to_itself() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = canonical(root.path());
        let want = canonical_root.join("nested/file.txt");
        let got = workspace(root.path()).resolve_for_write(&want).unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn an_absolute_path_outside_the_workspace_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let error = workspace(root.path())
            .resolve_for_write(&outside.path().join("file.txt"))
            .expect_err("outside path should be rejected");
        assert!(error.to_string().contains("escapes workspace"), "{error}");
    }

    #[test]
    fn a_symlink_that_stays_inside_resolves_to_its_target() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = canonical(root.path());
        std::fs::create_dir_all(root.path().join("inside")).unwrap();
        std::os::unix::fs::symlink(root.path().join("inside"), root.path().join("link")).unwrap();
        let got = workspace(root.path())
            .resolve_for_write(Path::new("link/new.txt"))
            .unwrap();
        assert_eq!(got, canonical_root.join("inside/new.txt"));
    }

    #[test]
    fn the_root_itself_resolves_to_the_canonical_root() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = canonical(root.path());
        let got = workspace(root.path())
            .resolve_for_write(root.path())
            .unwrap();
        assert_eq!(got, canonical_root);
    }

    #[test]
    fn resolve_existing_follows_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let canonical_root = canonical(root.path());
        std::fs::create_dir_all(root.path().join("inside")).unwrap();
        let want = canonical_root.join("inside/file.txt");
        std::fs::write(&want, "ok").unwrap();
        std::os::unix::fs::symlink(root.path().join("inside"), root.path().join("link")).unwrap();
        let got = workspace(root.path())
            .resolve_existing(Path::new("link/file.txt"))
            .unwrap();
        assert_eq!(got, want);
    }

    #[test]
    fn parent_traversal_through_an_escaping_symlink_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let error = workspace(root.path())
            .write_relative(Path::new("link/../note.txt"))
            .expect_err("traversal through an escaping symlink should be rejected");
        assert!(error.to_string().contains("escapes workspace"), "{error}");
    }

    #[test]
    fn write_relative_keeps_a_final_symlink_in_place() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("target.txt"), "old").unwrap();
        std::os::unix::fs::symlink("target.txt", root.path().join("relative-link")).unwrap();
        std::os::unix::fs::symlink(
            root.path().join("target.txt"),
            root.path().join("absolute-link"),
        )
        .unwrap();
        let workspace = workspace(root.path());
        for link in ["relative-link", "absolute-link"] {
            let got = workspace.write_relative(Path::new(link)).unwrap();
            assert_eq!(got, PathBuf::from("target.txt"), "write_relative({link})");
        }
    }

    #[tokio::test]
    async fn equivalent_spellings_share_one_mutation_lock() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        std::fs::write(root.path().join("real/b.txt"), "x").unwrap();
        std::os::unix::fs::symlink("real", root.path().join("alias")).unwrap();
        let workspace = workspace(root.path());
        for (canonical, spellings) in [
            ("real/b.txt", ["./real/b.txt", "real//b.txt", "alias/b.txt"]),
            (
                "real/new.txt",
                ["./real/new.txt", "real//new.txt", "alias/new.txt"],
            ),
        ] {
            let key = workspace.write_relative(Path::new(canonical)).unwrap();
            let guard = workspace.lock_path(&key).await;
            for spelling in spellings {
                let key = workspace.write_relative(Path::new(spelling)).unwrap();
                let blocked = tokio::time::timeout(
                    std::time::Duration::from_millis(20),
                    workspace.lock_path(&key),
                )
                .await;
                assert!(
                    blocked.is_err(),
                    "{spelling} did not share the lock of {canonical}"
                );
            }
            drop(guard);
        }
    }

    #[test]
    fn the_workspace_stays_bound_after_its_directory_is_replaced() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("note.txt"), "inside").unwrap();
        let workspace = workspace(&root);

        std::fs::rename(&root, parent.path().join("moved")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("note.txt"), "outside secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();

        let relative = workspace
            .existing_relative(Path::new("note.txt"))
            .expect("the original file is still reachable");
        assert_eq!(relative, PathBuf::from("note.txt"));
        let mut file = workspace.open(Path::new("note.txt")).unwrap();
        let mut content = String::new();
        std::io::Read::read_to_string(&mut file, &mut content).unwrap();
        assert_eq!(content, "inside");
    }

    #[test]
    fn an_empty_path_names_the_root() {
        let root = tempfile::tempdir().unwrap();
        let workspace = workspace(root.path());
        assert_eq!(workspace.candidate_path(Path::new("")), workspace.root());
        assert_eq!(
            workspace.existing_relative(Path::new("")).unwrap(),
            PathBuf::from(".")
        );
    }
}
