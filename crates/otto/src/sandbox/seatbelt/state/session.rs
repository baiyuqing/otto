//! Profile writes and close on a created state tree.

use super::*;

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

    pub(super) fn descriptors<'a>(
        &self,
        inner: &'a Inner,
    ) -> Result<(&'a OwnedFd, &'a OwnedFd), Error> {
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
