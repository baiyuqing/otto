//! A child's private workspace-local research directory. Its pinned handles
//! survive path swaps; drop removes only this generated tree, never siblings.
use crate::tool::session_directory::{create_session_directory, session_directory_name};
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use otto_platform::fsops::root::Root;
use std::io::{self, Write};
use std::path::Path;

pub(super) struct Scratch {
    parent: Root,
    directory: Root,
    name: String,
    path: String,
}

impl Scratch {
    /// Creates `.otto/tool-output/<session_id>/research-<hex>/`.
    pub(super) fn new(workspace: &Root, session_id: &str) -> io::Result<Self> {
        let session_directory_name = session_directory_name(session_id)?;
        let otto = workspace.create_child_directory(".otto", Mode::S_IRWXU)?;
        let output = otto.create_child_directory("tool-output", Mode::S_IRWXU)?;
        match output.open_file(
            Path::new(".gitignore"),
            OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL,
            Mode::S_IRUSR | Mode::S_IWUSR,
        ) {
            Ok(mut file) => file.write_all(b"*\n")?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let parent = create_session_directory(&output, session_directory_name)?;
        let id = otto_platform::random::random_hex::<16>().map_err(io::Error::other)?;
        let name = format!("research-{id}");
        let directory = parent.create_new_child_directory(&name, Mode::S_IRWXU)?;
        Ok(Self {
            parent,
            directory,
            path: format!(".otto/tool-output/{session_directory_name}/{name}"),
            name,
        })
    }

    pub(super) fn path(&self) -> String {
        self.path.clone()
    }
}

fn clear(directory: &Root) -> io::Result<()> {
    for entry in directory.read_dir(Path::new("."))? {
        let name = Path::new(&entry.name);
        if entry.is_dir && !entry.is_symlink {
            let child = Root::from_directory(directory.open_file(
                name,
                OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NONBLOCK,
                Mode::empty(),
            )?)?;
            clear(&child)?;
            directory.remove_directory(name)?;
        } else {
            directory.remove(name)?;
        }
    }
    Ok(())
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if clear(&self.directory).is_ok() {
            let _ = self.parent.remove_directory(Path::new(&self.name));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn exit_removes_nested_research_but_not_siblings_or_symlink_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let scratch = Scratch::new(&root, "session-x").unwrap();
        let path = dir.path().join(scratch.path());
        std::fs::create_dir(path.join("source")).unwrap();
        std::fs::write(path.join("source/code.rs"), "source").unwrap();
        let sibling = dir.path().join(".otto/tool-output/session-x/other.log");
        std::fs::write(&sibling, "keep").unwrap();
        symlink(dir.path(), path.join("outside")).unwrap();
        drop(scratch);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(sibling).unwrap(), "keep");
    }

    #[tokio::test]
    async fn cancellation_drops_the_temporary_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let scratch = Scratch::new(&root, "session-x").unwrap();
        let path = dir.path().join(scratch.path());
        let (ready, started) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _scratch = scratch;
            ready.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!path.exists());
    }

    #[test]
    fn escaping_directory_is_rejected_before_creating_research_files() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join(".otto")).unwrap();
        let root = Root::open(dir.path()).unwrap();
        assert!(Scratch::new(&root, "session-x").is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn scratch_path_names_the_owning_session_and_rejects_invalid_ids() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let scratch = Scratch::new(&root, "session-x").unwrap();
        assert!(
            scratch
                .path()
                .starts_with(".otto/tool-output/session-x/research-")
        );
        assert!(dir.path().join(scratch.path()).is_dir());
        for id in ["..", "a/b", ""] {
            assert!(Scratch::new(&root, id).is_err(), "{id:?}");
        }
        assert_eq!(
            std::fs::read_dir(dir.path().join(".otto/tool-output"))
                .unwrap()
                .count(),
            2
        );
    }
}
