//! Creation of the state tree and its construction-time checks.

use super::*;

/// Everything construction has created so far, so a failure can undo exactly
/// what it made and nothing else.
pub(super) struct Partial {
    pub(super) parent_file: Option<OwnedFd>,
    pub(super) root_name: String,
    pub(super) root_path: String,
    pub(super) root_edge: FileStat,
    pub(super) root_file: Option<OwnedFd>,
    pub(super) root_identity: Option<Identity>,
    pub(super) child_identities: HashMap<String, Identity>,
    pub(super) profile_identity: Option<Identity>,
}

impl Partial {
    /// Removes what construction created.
    ///
    /// Once the root's own identity is known the removal walks the tree and
    /// refuses to descend into anything it did not create. Before that, only
    /// the single edge whose `fstatat` result was captured at creation is
    /// removed, and only if it still names the same object.
    pub(super) fn discard(&mut self) {
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
pub(super) fn build_state(
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

    let temp_path = profile::join(&root_path, "tmp");
    let cache = profile::join(&root_path, "cache");
    let profiles = profile::join(&root_path, "profiles");
    let child_paths = [
        ("tmp", temp_path.as_str()),
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
            temp: temp_path.into(),
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

/// Canonicalises `path` and proves it is a real directory, not a symlink.
pub(super) fn canonical_state_directory(
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
pub(super) fn open_state_directory_path(path: &str) -> Result<OwnedFd, Error> {
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
pub(super) fn open_state_directory_at(
    parent: BorrowedFd<'_>,
    name: &str,
) -> Result<OwnedFd, Error> {
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
pub(super) fn open_state_directory_at_raw(
    parent: BorrowedFd<'_>,
    name: &str,
) -> Result<OwnedFd, Errno> {
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
pub(super) fn create_random_state_root(
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
