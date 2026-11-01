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

fn make_file(path: &str, mode: u32) -> String {
    std::fs::create_dir_all(profile::dir(path)).expect("create parent");
    std::fs::write(path, b"fixture").expect("write fixture");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("set file mode");
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
        text(&state.directories.temp),
        text(&state.directories.cache),
        state.profiles.clone(),
    ] {
        assert_canonical(&path);
        assert_entry(&path, true, 0o700);
    }
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
                    if event.kind == EventKind::DirectoryCreated && event.name == "tmp" {
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

/// The child may leave directories it made read-only: the Go module cache
/// and the plugin store both do, and an entry can only be removed through a
/// parent the owner may write and search. Cleanup restores the owner's
/// access through the descriptor it already validated and removes the tree.
#[test]
fn close_removes_a_tree_the_child_made_read_only() {
    let (_base, base) = temp_base();
    let workspace = make_directory(&profile::join(&base, "workspace"), 0o700);
    let cache = make_directory(&profile::join(&base, "user-cache"), 0o700);
    let state = create(&workspace, &cache).expect("create state");

    let module = profile::join(&text(&state.directories.cache), "go-mod/example@v1.0.0");
    let package = profile::join(&module, "example");
    std::fs::create_dir_all(&package).expect("create module");
    std::fs::write(profile::join(&module, "go.mod"), b"module example").expect("write go.mod");
    std::fs::write(profile::join(&package, "example.go"), b"package example")
        .expect("write package");
    chmod(&profile::join(&module, "go.mod"), 0o444);
    chmod(&profile::join(&package, "example.go"), 0o444);
    chmod(&package, 0o555);
    chmod(&module, 0o555);

    state.close().expect("close state");
    assert_no_leaves(&cache);
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
    let temp = text(&state.directories.temp);
    std::fs::remove_dir(&temp).expect("remove temp");
    symlink(&outside, &temp);
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
    for relative in ["tmp", "cache", "profiles", "profiles/profile.sb"] {
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
                let temp = profile::join(root, "tmp");
                std::fs::rename(&temp, profile::join(base, "created-tmp")).expect("rename temp");
                std::fs::create_dir_all(profile::join(&temp, "nested")).expect("create substitute");
                std::fs::write(profile::join(&temp, "nested/keep"), b"keep").expect("write marker");
                "tmp/nested/keep".to_string()
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

/// Fails one descriptor close after proving the number has already been
/// handed to an unrelated open file.
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

    /// Rust's test harness runs tests on parallel threads of one process,
    /// so closing the descriptor and reopening a sentinel would not
    /// reliably get the number back. `dup2` onto the same number gives the
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

/// A look-alike tree with fresh inodes.
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
