
use super::*;
use crate::sandbox::seatbelt::state;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// [`Dependencies`] borrows its callbacks, so the fixture owns them and
/// hands out a borrowing view.
/// The fixture's owned resolver, matching [`Dependencies::resolve`].
type ResolveFn = dyn Fn(&str) -> Result<ResolvedPath, ResolveError>;

struct Deps {
    resolve: Box<ResolveFn>,
    fixed_paths: Vec<AutomaticPath>,
    developer_root: Box<dyn Fn() -> Result<Option<String>, Rejected>>,
}

impl Deps {
    fn new() -> Self {
        Self {
            resolve: Box::new(resolve_path),
            fixed_paths: Vec::new(),
            developer_root: Box::new(|| Ok(None)),
        }
    }

    fn view(&self) -> Dependencies<'_> {
        Dependencies {
            resolve: self.resolve.as_ref(),
            fixed_paths: self.fixed_paths.clone(),
            developer_root: self.developer_root.as_ref(),
        }
    }

    /// Layers one lookup over the current resolver. `None` falls through to
    /// the resolver that was installed before.
    fn override_resolve<F>(&mut self, lookup: F)
    where
        F: Fn(&str) -> Option<Result<ResolvedPath, ResolveError>> + 'static,
    {
        let base = std::mem::replace(&mut self.resolve, Box::new(resolve_path));
        self.resolve = Box::new(move |path| lookup(path).unwrap_or_else(|| base(path)));
    }

    fn with_synthetic_directories(&mut self, paths: &[String]) {
        let mut resolved: HashMap<String, ResolvedPath> = HashMap::new();
        for path in paths {
            resolved.insert(
                path.clone(),
                ResolvedPath {
                    path: path.clone(),
                    kind: PathKind::Directory,
                    executable: false,
                },
            );
        }
        self.override_resolve(move |path| resolved.get(path).cloned().map(Ok));
    }

    fn developer_root_is(&mut self, root: String) {
        self.developer_root = Box::new(move || Ok(Some(root.clone())));
    }
}

/// Dropping it closes the private state before the temporary tree is
/// removed, which is what `t.Cleanup` does there.
struct Fixture {
    temp: Option<tempfile::TempDir>,
    base: String,
    state: Option<state::State>,
    options: Options,
    deps: Deps,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let result = self.state.take().expect("state").close();
        drop(self.temp.take());
        if !std::thread::panicking() {
            assert!(result.is_ok(), "state close failed: {result:?}");
        }
    }
}

impl Fixture {
    fn state(&self) -> &state::State {
        self.state.as_ref().expect("state")
    }

    fn render(&self) -> String {
        generate_with(&self.options, &self.deps.view()).expect("generate profile")
    }

    fn generate(&self) -> Result<String, Rejected> {
        generate_with(&self.options, &self.deps.view())
    }

    fn assert_rejected(&self, what: &str) {
        match self.generate() {
            Err(Rejected) => {}
            Ok(profile) => panic!("{what} generated {} bytes", profile.len()),
        }
    }

    fn directory(&self, path: &std::path::Path) -> String {
        path.to_str().expect("utf-8 directory").to_string()
    }
}

fn new_fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("temp dir");
    let base = canonical(temp.path().to_str().expect("utf-8 temp dir"));
    let workspace = make_directory(&join(&base, "workspace"));
    let cache_base = make_directory(&join(&base, "user-cache"));
    let private_state = state::create(&workspace, &cache_base).expect("create private state");
    let home = make_directory(&join(&base, "resolved-home"));
    let shell = make_file(&join(&join(&base, "shells"), "shell"), 0o700);
    let options = Options {
        workspace: workspace.clone(),
        workspaces: vec![workspace],
        directories: private_state.directories.clone(),
        shell,
        home,
        host_entries: vec!["PATH=".to_string()],
        read_paths: Vec::new(),
        network: Some(NetworkMode::Deny),
    };
    Fixture {
        temp: Some(temp),
        base,
        state: Some(private_state),
        options,
        deps: Deps::new(),
    }
}

fn canonical(path: &str) -> String {
    std::fs::canonicalize(path)
        .expect("canonicalize")
        .to_str()
        .expect("utf-8 path")
        .to_string()
}

fn make_directory(path: &str) -> String {
    std::fs::create_dir_all(path).expect("create directory");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .expect("chmod directory");
    canonical(path)
}

fn make_file(path: &str, mode: u32) -> String {
    std::fs::create_dir_all(dir(path)).expect("create parent");
    std::fs::write(path, b"fixture").expect("write file");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod file");
    canonical(path)
}

fn symlink(target: &str, link: &str) {
    std::os::unix::fs::symlink(target, link).expect("symlink");
}

fn section<'a>(profile: &'a str, name: &str) -> &'a str {
    let begin = format!("; OTTO-DYNAMIC-{name}-BEGIN");
    let end = format!("; OTTO-DYNAMIC-{name}-END");
    let start = profile
        .find(&begin)
        .unwrap_or_else(|| panic!("profile section {name} not found"));
    let finish = profile
        .find(&end)
        .unwrap_or_else(|| panic!("profile section {name} not found"));
    assert!(finish >= start, "profile section {name} is inverted");
    &profile[start..finish + end.len()]
}

fn quote(path: &str) -> String {
    format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
}

fn filter(kind: &str, path: &str) -> String {
    format!("({kind} {})", quote(path))
}

fn count(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

fn base_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

#[test]
fn profile_is_closed_by_default_and_limits_private_writes() {
    let mut fixture = new_fixture();
    let external_directory = make_directory(&join(&join(&fixture.base, "external"), "tools"));
    let external_file = make_file(
        &join(&join(&fixture.base, "external"), "runtime.conf"),
        0o600,
    );
    fixture.options.read_paths = vec![external_directory.clone(), external_file.clone()];

    let profile = fixture.render();
    assert!(
        profile.starts_with("(version 1)\n(deny default (with message "),
        "profile does not begin closed by default:\n{profile}"
    );
    for unfiltered in [
        "(allow file-read*)",
        "(allow file-write*)",
        "(allow network-outbound)",
        "(allow network-inbound)",
    ] {
        assert!(
            !profile.contains(unfiltered),
            "profile contains unfiltered grant {unfiltered}"
        );
    }

    let read_section = section(&profile, "READ");
    let write_section = section(&profile, "WRITE");
    for path in [
        fixture.options.workspace.clone(),
        fixture.directory(&fixture.options.directories.home),
        fixture.directory(&fixture.options.directories.temp),
        fixture.directory(&fixture.options.directories.cache),
    ] {
        let grant = filter("subpath", &path);
        assert!(
            read_section.contains(&grant) && write_section.contains(&grant),
            "writable path {} is not read/write",
            base_name(&path)
        );
    }
    for path in [&external_directory, &external_file] {
        assert!(
            !write_section.contains(&quote(path)),
            "external path {} appears in write rules",
            base_name(path)
        );
    }
    assert!(
        read_section.contains(&filter("subpath", &external_directory))
            && read_section.contains(&filter("literal", &external_file)),
        "external directory/file did not become read-only subpath/literal grants"
    );

    let root = fixture.directory(&fixture.options.directories.root);
    assert!(
        !read_section.contains(&filter("subpath", &root))
            && !write_section.contains(&filter("subpath", &root)),
        "private state parent received a subpath grant"
    );
    assert!(
        !profile.contains(&fixture.state().profiles)
            && !profile.contains(&fixture.state().profile_path),
        "profiles directory or generated profile leaked into child policy"
    );
}

#[test]
fn workspace_git_metadata_and_identity_are_denied_after_every_write_allow() {
    let fixture = new_fixture();
    let workspace = fixture.options.workspace.clone();
    let profile = fixture.render();
    let tag = tag_message(&fixture);
    let deny = format!(
        "(deny file-write*\n  (literal {})\n  (literal {})\n  (literal {})\n  (literal {})\n  (subpath {})\n  (literal {})\n  (literal {})\n  {tag}\n)\n",
        quote(&format!("{workspace}/.git")),
        quote(&format!("{workspace}/.git/config")),
        quote(&format!("{workspace}/.git/config.worktree")),
        quote(&format!("{workspace}/.git/commondir")),
        quote(&format!("{workspace}/.git/hooks")),
        quote(&format!("{workspace}/.otto")),
        quote(&format!("{workspace}/.otto/workspace.json")),
    );
    assert_eq!(count(&profile, &deny), 1, "git deny missing:\n{profile}");
    // Seatbelt applies the last matching rule.
    let deny_at = profile.find(&deny).unwrap();
    let last_allow = profile.rfind("(allow file-write*").unwrap();
    assert!(
        last_allow < deny_at,
        "git deny precedes an allow file-write* rule"
    );
}

#[test]
fn managed_worktrees_are_writable_but_keep_their_git_metadata_read_only() {
    let mut fixture = new_fixture();
    let linked = make_directory(&join(&fixture.base, "linked-worktree"));
    fixture.options.workspaces.push(linked.clone());

    let profile = fixture.render();
    let write = section(&profile, "WRITE");
    assert!(write.contains(&filter("subpath", &linked)));
    assert!(write.contains(&quote(&join(&linked, ".git"))));
    assert!(write.contains(&quote(&join(&linked, crate::workspace::MARKER))));
}

fn tag_message(fixture: &Fixture) -> String {
    let root = std::path::Path::new(&fixture.options.directories.root);
    let tag = root.file_name().and_then(|name| name.to_str()).unwrap();
    assert!(tag.starts_with(state::LEAF_PREFIX));
    format!("(with message {})", quote(tag))
}

#[test]
fn every_deny_form_carries_the_message_tag() {
    let fixture = new_fixture();
    let message = tag_message(&fixture);
    let profile = fixture.render();
    let mut forms = 0;
    for (at, _) in profile.match_indices("(deny ") {
        // A deny form ends at its balanced closing parenthesis.
        let mut depth = 0;
        let mut end = at;
        for (offset, character) in profile[at..].char_indices() {
            match character {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                end = at + offset;
                break;
            }
        }
        let form = &profile[at..=end];
        assert!(
            form.strip_suffix(')')
                .unwrap()
                .trim_end()
                .ends_with(&message),
            "deny form lacks the message tag as its last element:\n{form}"
        );
        forms += 1;
    }
    assert_eq!(forms, 6, "expected five template denies and the git deny");
}

#[test]
fn rejects_every_external_root_overlapping_private_state() {
    type Mutate = fn(&mut Fixture);
    let cases: [(&str, Mutate); 5] = [
        ("explicit ancestor", |fixture| {
            fixture.options.read_paths = vec![fixture.state().root_parent.clone()];
        }),
        ("explicit profiles child", |fixture| {
            fixture.options.read_paths = vec![fixture.state().profiles.clone()];
        }),
        ("fixed automatic ancestor", |fixture| {
            fixture.deps.fixed_paths = vec![AutomaticPath {
                path: fixture.state().root_parent.clone(),
                kind: PathKind::Directory,
            }];
        }),
        ("PATH automatic ancestor", |fixture| {
            fixture.options.host_entries = vec![format!("PATH={}", fixture.state().root_parent)];
        }),
        ("developer root ancestor", |fixture| {
            let root = fixture.state().root_parent.clone();
            fixture.deps.developer_root_is(root);
        }),
    ];
    for (name, mutate) in cases {
        let mut fixture = new_fixture();
        mutate(&mut fixture);
        fixture.assert_rejected(name);
    }
}

#[test]
fn rejects_case_alias_containing_private_state() {
    let mut fixture = new_fixture();
    let parent = fixture.state().root_parent.clone();
    let alias = join(&dir(&parent), &base_name(&parent).to_uppercase());
    if std::fs::metadata(&alias).is_err() {
        println!("skipped: the test volume is case-sensitive");
        return;
    }
    fixture.options.read_paths = vec![alias];
    fixture.assert_rejected("a differently-cased state ancestor");
}

#[test]
fn network_rules_are_ip_only() {
    let mut fixture = new_fixture();
    fixture.options.network = Some(NetworkMode::Allow);
    let allow = fixture.render();
    assert!(
        allow.contains("(remote ip)") && allow.contains("(local ip)"),
        "NetworkAllow is missing filtered remote/local IP grants"
    );
    const MDNS_LOOKUP: &str = "(allow mach-lookup\n  (global-name \"com.apple.mDNSResponder\"))";
    let allow_network = section(&allow, "NETWORK");
    assert!(
        count(&allow, MDNS_LOOKUP) == 1 && allow_network.contains(MDNS_LOOKUP),
        "NetworkAllow does not contain exactly one marker-scoped mDNS broker lookup"
    );
    const RESOLVER_SOCKET: &str = "(remote unix-socket (path \"/private/var/run/mDNSResponder\"))";
    assert!(
        count(&allow, RESOLVER_SOCKET) == 1 && allow_network.contains(RESOLVER_SOCKET),
        "NetworkAllow does not contain exactly one marker-scoped resolver socket grant"
    );
    for forbidden in [
        "(allow network-outbound)",
        "(allow network-inbound)",
        "(allow network-bind)",
        "(remote unix-socket)",
        "(local unix-socket)",
    ] {
        assert!(
            !allow.contains(forbidden),
            "NetworkAllow contains unfiltered or Unix-socket grant {forbidden}"
        );
    }

    fixture.options.network = Some(NetworkMode::Deny);
    let deny = fixture.render();
    assert!(
        !deny.contains("(remote ip)")
            && !deny.contains("(local ip)")
            && !deny.contains("allow network-")
            && !deny.contains("com.apple.mDNSResponder"),
        "NetworkDeny emitted an IP or mDNS broker grant"
    );

    // [`NetworkMode`] has exactly two variants, so the only unset network
    // state is `None`, which is what this rejection guards.
    fixture.options.network = None;
    fixture.assert_rejected("an unset network mode");
}

#[test]
fn grants_the_exact_page_size_compat_sysctl() {
    let profile = new_fixture().render();
    assert!(
        profile.contains("(sysctl-name \"hw.pagesize_compat\")"),
        "profile lacks the exact Darwin page-size compatibility sysctl"
    );
    assert!(
        !profile.contains("sysctl-name-prefix") && !profile.contains("(allow sysctl-read)"),
        "runtime compatibility broadened the exact sysctl allowlist"
    );
}

#[test]
fn denies_system_volumes_read_aliases() {
    let profile = new_fixture().render();
    const CARVE_OUT: &str = "(deny file-read*\n  (subpath \"/System/Volumes\")\n  (with message ";
    assert_eq!(
        count(&profile, CARVE_OUT),
        1,
        "profile does not contain exactly one /System/Volumes read carve-out"
    );
    assert!(
        !profile.contains("(allow file-read*\n  (subpath \"/System/Volumes\"))")
            && !profile.contains("(allow file-read-data\n  (subpath \"/System/Volumes\"))"),
        "profile grants the /System/Volumes alias subtree"
    );
}

#[test]
fn grants_only_the_root_inode_for_process_cwd_resolution() {
    let profile = new_fixture().render();
    assert!(
        profile.contains("(allow file-read-data\n  (literal \"/\"))"),
        "profile lacks the exact root-inode data operation required by process cwd resolution"
    );
    assert!(
        !profile.contains("(allow file-read*\n  (literal \"/\"))")
            && !profile.contains("(subpath \"/\")"),
        "process cwd compatibility broadened the exact root operation or path"
    );
}

#[test]
fn denies_exact_escape_broker_executables() {
    let profile = new_fixture().render();
    for executable in [
        "/usr/bin/open",
        "/usr/bin/osascript",
        "/usr/bin/sandbox-exec",
    ] {
        assert!(
            profile.contains(&filter("literal", executable)),
            "profile lacks exact process-exec denial for {executable}"
        );
    }
    assert!(
        profile.contains("(deny process-exec")
            && !profile.contains("(deny process-exec\n  (subpath \"/usr/bin\"))"),
        "escape-broker process denial is absent or broadened to /usr/bin"
    );
}

#[test]
fn explicitly_denies_apple_events_and_launch_services() {
    let profile = new_fixture().render();
    for rule in [
        "(deny appleevent-send (with message ",
        "(deny lsopen (with message ",
    ] {
        assert!(
            profile.contains(rule),
            "profile lacks explicit escape-broker denial {rule}"
        );
    }
    for rule in ["(allow appleevent-send", "(allow lsopen"] {
        assert!(
            !profile.contains(rule),
            "profile grants escape-broker operation {rule}"
        );
    }
}

#[test]
fn template_has_only_reviewed_ipc_and_process_rules() {
    let profile = new_fixture().render();
    for required in [
        "(allow process-fork)",
        "(allow process-exec)",
        "(allow signal (target same-sandbox))",
        "(sysctl-name ",
        "com.apple.system.opendirectoryd.libinfo",
    ] {
        assert!(
            profile.contains(required),
            "profile is missing reviewed rule {required}"
        );
    }
    let lowered = profile.to_lowercase();
    for forbidden in [
        "(allow appleevent",
        "(allow lsopen",
        "launchservices",
        "com.apple.lsd",
        "com.apple.coreservices",
        "com.apple.securityd",
        "com.apple.tccd",
        "com.apple.pboard",
        "process-info",
        "(target self)",
        "mach-task",
        "ipc-posix",
        "sysctl-name-prefix",
        "(with no-sandbox)",
    ] {
        assert!(
            !lowered.contains(forbidden),
            "profile contains forbidden IPC/process rule {forbidden}"
        );
    }
}

#[test]
fn automatic_path_entries_skip_broad_and_sensitive_anchors() {
    let mut fixture = new_fixture();
    let broad: Vec<String> = [
        "/",
        "/Users",
        fixture.options.home.as_str(),
        "/Applications",
        "/Library",
        "/Network",
        "/Volumes",
        "/dev",
        "/private",
        "/private/etc",
        "/private/tmp",
        "/private/var",
        "/usr",
        "/opt",
        "/opt/homebrew",
        "/usr/local",
        "/opt/homebrew/etc",
        "/opt/homebrew/etc/private",
        "/opt/homebrew/var",
        "/usr/local/etc",
        "/usr/local/var/cache",
    ]
    .iter()
    .map(|path| (*path).to_string())
    .collect();
    let narrow: Vec<String> = vec![
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
        "/Applications/Xcode.app/Contents/Developer/usr/bin".to_string(),
        "/Library/Developer/CommandLineTools/usr/bin".to_string(),
        join(&join(&fixture.options.home, ".local"), "bin"),
    ];
    let mut all = broad.clone();
    all.extend(narrow.iter().cloned());
    fixture.deps.with_synthetic_directories(&all);
    fixture.options.host_entries = vec!["LANG=C".to_string(), format!("PATH={}", all.join(":"))];

    let profile = fixture.render();
    let read_section = section(&profile, "READ").to_string();
    let write_section = section(&profile, "WRITE").to_string();
    for path in &broad {
        assert!(
            !read_section.contains(&filter("subpath", path)),
            "broad/sensitive PATH entry {path} was granted"
        );
    }
    for path in &narrow {
        assert!(
            read_section.contains(&filter("subpath", path)),
            "narrow PATH entry {path} was not granted"
        );
        assert!(
            !write_section.contains(&quote(path)),
            "narrow PATH entry {path} was writable"
        );
    }
}

#[test]
fn automatic_fixed_roots_require_declared_types() {
    let fixture = new_fixture();
    let regular_target = make_file(
        &join(&join(&fixture.base, "automatic-types"), "regular"),
        0o600,
    );
    let directory_target =
        make_directory(&join(&join(&fixture.base, "automatic-types"), "directory"));
    drop(fixture);

    for (name, source, expected, resolved) in [
        (
            "fixed directory resolved to regular file",
            "/usr/bin",
            PathKind::Directory,
            ResolvedPath {
                path: regular_target,
                kind: PathKind::Regular,
                executable: false,
            },
        ),
        (
            "exact runtime file resolved to directory",
            "/private/etc/hosts",
            PathKind::Regular,
            ResolvedPath {
                path: directory_target,
                kind: PathKind::Directory,
                executable: false,
            },
        ),
    ] {
        let mut fixture = new_fixture();
        fixture.deps.fixed_paths = vec![AutomaticPath {
            path: source.to_string(),
            kind: expected,
        }];
        let source = source.to_string();
        fixture
            .deps
            .override_resolve(move |path| (path == source).then(|| Ok(resolved.clone())));
        fixture.assert_rejected(name);
    }
}

#[test]
fn canonical_automatic_targets_cannot_grant_forbidden_anchors() {
    for class in ["fixed", "developer", "PATH"] {
        let mut fixture = new_fixture();
        let forbidden_target = if class == "PATH" {
            fixture.options.home.clone()
        } else {
            make_directory(&join(&join(&fixture.options.home, "sensitive"), class))
        };
        let alias = join(&fixture.base, &format!("automatic-alias-{class}"));
        symlink(&forbidden_target, &alias);
        match class {
            "fixed" => {
                fixture.deps.fixed_paths = vec![AutomaticPath {
                    path: alias.clone(),
                    kind: PathKind::Directory,
                }];
            }
            "developer" => fixture.deps.developer_root_is(alias.clone()),
            _ => fixture.options.host_entries = vec![format!("PATH={alias}")],
        }

        let profile = fixture.render();
        let read_data = section(&profile, "READ-DATA");
        assert!(
            !read_data.contains(&quote(&forbidden_target)) && !read_data.contains(&quote(&alias)),
            "canonical automatic {class} target inside the user home was granted"
        );
    }
}

#[test]
fn canonical_automatic_symlinks_to_broad_anchor_are_skipped() {
    for class in ["fixed", "developer", "PATH"] {
        let mut fixture = new_fixture();
        let alias = join(&fixture.base, &format!("broad-root-alias-{class}"));
        symlink("/", &alias);
        match class {
            "fixed" => {
                fixture.deps.fixed_paths = vec![AutomaticPath {
                    path: alias.clone(),
                    kind: PathKind::Directory,
                }];
            }
            "developer" => fixture.deps.developer_root_is(alias.clone()),
            _ => fixture.options.host_entries = vec![format!("PATH={alias}")],
        }

        let profile = fixture.render();
        assert!(
            !section(&profile, "READ-DATA").contains(&filter("subpath", "/")),
            "canonical automatic {class} symlink target granted the filesystem root"
        );
    }
}

#[test]
fn configured_shell_is_one_canonical_executable_literal() {
    let mut fixture = new_fixture();
    let shell_target = make_file(&join(&join(&fixture.base, "shells"), "real shell"), 0o700);
    let shell_alias = join(&fixture.base, "shell alias");
    symlink(&shell_target, &shell_alias);
    fixture.options.shell = shell_alias.clone();

    let profile = fixture.render();
    let shell_section = section(&profile, "SHELL");
    assert_eq!(
        count(shell_section, &filter("literal", &shell_target)),
        1,
        "canonical shell is not exactly one literal grant:\n{shell_section}"
    );
    assert!(
        !shell_section.contains(&shell_alias)
            && !shell_section.contains(&filter("subpath", &dir(&shell_target))),
        "shell alias or shell parent received a grant"
    );
}

#[test]
fn rejects_configured_shell_overlapping_private_state() {
    for name in ["state root executable", "profiles executable"] {
        let mut fixture = new_fixture();
        let parent = if name == "state root executable" {
            fixture.directory(&fixture.options.directories.root)
        } else {
            fixture.state().profiles.clone()
        };
        fixture.options.shell = make_file(&join(&parent, "private-shell"), 0o700);
        fixture.assert_rejected(name);
    }
}

#[test]
fn rejects_invalid_configured_shell() {
    let fixture = new_fixture();
    let non_executable = make_file(
        &join(&join(&fixture.base, "shells"), "not-executable"),
        0o600,
    );
    let directory = make_directory(&join(&join(&fixture.base, "shells"), "directory"));
    let missing = join(&fixture.base, "missing");
    drop(fixture);

    // `String` cannot hold invalid UTF-8, so a non-UTF-8 path is
    // unrepresentable rather than rejected. Rust's `String` cannot hold
    // invalid UTF-8, so that input is unrepresentable and the rejection it
    // exercises is enforced by the type instead.
    for shell in [
        "relative-shell".to_string(),
        non_executable,
        directory,
        missing,
        "bad\nname".to_string(),
    ] {
        let mut fixture = new_fixture();
        fixture.options.shell = shell.clone();
        fixture.assert_rejected(&format!("invalid shell {shell:?}"));
    }

    let mut fixture = new_fixture();
    fixture.options.shell = "/injected/special".to_string();
    fixture.deps.override_resolve(|path| {
        (path == "/injected/special").then(|| {
            Ok(ResolvedPath {
                path: "/injected/special".to_string(),
                kind: PathKind::Special,
                executable: true,
            })
        })
    });
    fixture.assert_rejected("a special-file shell");
}

#[test]
fn expands_only_resolved_home_tilde_prefix() {
    let mut fixture = new_fixture();
    let tool_directory = make_directory(&join(&join(&fixture.options.home, "tool roots"), "bin"));
    fixture.options.read_paths = vec!["~/tool roots/bin".to_string()];
    let profile = fixture.render();
    assert!(
        section(&profile, "READ").contains(&filter("subpath", &tool_directory)),
        "~/ was not expanded from the resolved home"
    );

    for syntax in [
        "$OTTO_ROOT",
        "${OTTO_ROOT}",
        "$(whoami)",
        "`whoami`",
        "*.tools",
    ] {
        let mut fixture = new_fixture();
        let literal = join(&fixture.base, syntax);
        fixture.options.read_paths = vec![literal.clone()];
        let seen = Arc::new(Mutex::new(String::new()));
        let recorder = seen.clone();
        let target = literal.clone();
        fixture.deps.override_resolve(move |path| {
            if path == target {
                *recorder.lock().expect("seen") = path.to_string();
                return Some(Err(ResolveError::NotFound));
            }
            None
        });
        let profile = fixture.render();
        assert!(
            !section(&profile, "READ").contains(&literal),
            "an unavailable legacy path was granted"
        );
        assert_eq!(
            *seen.lock().expect("seen"),
            literal,
            "shell syntax was transformed instead of checked literally"
        );
    }
}

#[test]
fn canonicalizes_read_paths_and_rejects_special_entries() {
    let mut fixture = new_fixture();
    let directory = make_directory(&join(&join(&fixture.base, "canonical"), "directory"));
    let file = make_file(&join(&join(&fixture.base, "canonical"), "file"), 0o600);
    let directory_alias = join(&fixture.base, "directory-alias");
    let file_alias = join(&fixture.base, "file-alias");
    symlink(&directory, &directory_alias);
    symlink(&file, &file_alias);
    fixture.options.read_paths = vec![directory_alias.clone(), file_alias.clone()];
    let profile = fixture.render();
    let read_section = section(&profile, "READ");
    assert!(
        read_section.contains(&filter("subpath", &directory))
            && read_section.contains(&filter("literal", &file))
            && !read_section.contains(&directory_alias)
            && !read_section.contains(&file_alias),
        "read path aliases were not canonicalized to typed grants"
    );

    drop(fixture);
    let cases: Vec<(&str, String, Result<ResolvedPath, ResolveError>)> = vec![
        (
            "relative",
            "relative/path".to_string(),
            Err(ResolveError::Rejected),
        ),
        (
            "special",
            "/injected/special".to_string(),
            Ok(ResolvedPath {
                path: "/injected/special".to_string(),
                kind: PathKind::Special,
                executable: false,
            }),
        ),
    ];
    for (name, path, resolved) in cases {
        let mut fixture = new_fixture();
        fixture.options.read_paths = vec![path.clone()];
        let target = path.clone();
        fixture
            .deps
            .override_resolve(move |candidate| (candidate == target).then(|| resolved.clone()));
        fixture.assert_rejected(&format!("invalid read path {name}"));
    }

    let mut fixture = new_fixture();
    let missing = join(&fixture.base, "missing");
    fixture.options.read_paths = vec![missing.clone()];
    let profile = fixture.render();
    assert!(
        !section(&profile, "READ").contains(&missing),
        "a missing legacy grant must not reach the profile"
    );
}

#[test]
fn escapes_dynamic_literals_without_injection() {
    let mut fixture = new_fixture();
    let name = "tools 空格 \\\" ) (allow network-outbound) ; #";
    let path = make_directory(&join(&fixture.base, name));
    fixture.options.read_paths = vec![path.clone()];

    let profile = fixture.render();
    assert!(
        profile.contains(&filter("subpath", &path)),
        "spaces, Unicode, quotes, backslashes, or profile punctuation were not safely represented"
    );
    assert!(
        count(&profile, "(deny default") == 1 && !profile.contains("(allow network-outbound)\n"),
        "dynamic path injected an SBPL form"
    );

    // `String` cannot hold invalid UTF-8, so a non-UTF-8 value is
    // unrepresentable rather than rejected. Rust's `String` cannot hold
    // invalid UTF-8, so that input is unrepresentable.
    let base = fixture.base.clone();
    drop(fixture);
    let invalid = [
        join(&base, "line\nbreak"),
        join(&base, "tab\tcontrol"),
        join(&base, "line\u{2028}separator"),
        join(&base, "paragraph\u{2029}separator"),
        join(&base, "nul\0x"),
    ];
    for path in invalid {
        let mut fixture = new_fixture();
        fixture.options.read_paths = vec![path.clone()];
        fixture
            .deps
            .with_synthetic_directories(std::slice::from_ref(&path));
        let error = match fixture.generate() {
            Err(error) => error,
            Ok(profile) => panic!("control/invalid path generated {} bytes", profile.len()),
        };
        let text = error.to_string();
        assert!(
            text.len() <= 128 && !text.contains(&fixture.base) && !text.contains("allow"),
            "profile error is not bounded and source-free: {text:?}"
        );
    }
}

#[test]
fn marker_like_path_text_renders_without_template_reprocessing() {
    let mut fixture = new_fixture();
    let markers = [READ_MARKER, WRITE_MARKER, NETWORK_MARKER, SHELL_MARKER];
    let marker_name = markers.join("-");
    let read_directory = make_directory(&join(&join(&fixture.base, "marker-paths"), &marker_name));
    let shell = make_file(
        &join(&join(&fixture.base, "marker-shells"), &marker_name),
        0o700,
    );
    fixture.options.read_paths = vec![read_directory.clone()];
    fixture.options.shell = shell.clone();

    let first = fixture.render();
    let second = fixture.render();
    assert_eq!(
        first, second,
        "marker-like path text rendered nondeterministically"
    );
    assert!(
        section(&first, "READ-DATA").contains(&filter("subpath", &read_directory))
            && section(&first, "SHELL").contains(&filter("literal", &shell)),
        "marker-like filenames were not rendered as escaped literal path text"
    );
    for marker in markers {
        assert_eq!(
            count(&first, marker),
            2,
            "marker-like text {marker} does not appear exactly twice"
        );
    }
    for name in ["READ", "WRITE", "NETWORK", "SHELL"] {
        assert!(
            count(&first, &format!("; OTTO-DYNAMIC-{name}-BEGIN")) == 1
                && count(&first, &format!("; OTTO-DYNAMIC-{name}-END")) == 1,
            "marker-like text changed the {name} section structure"
        );
    }
    assert!(
        count(&first, "(version 1)") == 1
            && count(&first, "(deny default") == 1
            && !first.contains("(allow network-outbound)\n"),
        "marker-like path text injected or changed fixed profile forms"
    );
}

#[test]
fn ancestor_metadata_does_not_grant_ancestor_contents() {
    let mut fixture = new_fixture();
    let ancestor = make_directory(&join(&fixture.base, "metadata-only"));
    let approved = make_directory(&join(&join(&ancestor, "approved"), "bin"));
    fixture.options.read_paths = vec![approved.clone()];
    let profile = fixture.render();

    let metadata = section(&profile, "METADATA");
    let read_data = section(&profile, "READ-DATA");
    assert!(
        metadata.contains(&filter("literal", &ancestor)),
        "approved root ancestor lacks traversal metadata"
    );
    assert!(
        !read_data.contains(&quote(&ancestor)) && read_data.contains(&filter("subpath", &approved)),
        "ancestor content was granted or approved root content was omitted"
    );
}

#[test]
fn dynamic_root_limits_apply_after_deterministic_collapse() {
    let mut fixture = new_fixture();
    let paths: Vec<String> = (0..129)
        .map(|index| format!("/dynamic/tool-{index:03}"))
        .collect();
    fixture.options.read_paths = paths.clone();
    fixture.deps.with_synthetic_directories(&paths);
    fixture.assert_rejected("129 effective roots");
    drop(fixture);

    let mut fixture = new_fixture();
    let paths: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|letter| format!("/dynamic/{letter}-{}", letter.repeat(11_000)))
        .collect();
    fixture.options.read_paths = paths.clone();
    fixture.deps.with_synthetic_directories(&paths);
    fixture.assert_rejected("oversized roots");
    drop(fixture);

    let mut fixture = new_fixture();
    let parent = "/dynamic/tools".to_string();
    let mut paths = vec![parent.clone(), parent.clone()];
    for index in 0..129 {
        paths.push(format!("{parent}/tool-{index:03}/bin"));
    }
    fixture.options.read_paths = paths.clone();
    fixture.deps.with_synthetic_directories(&paths);
    let profile = fixture.render();
    let read_data = section(&profile, "READ-DATA");
    assert!(
        count(read_data, &filter("subpath", &parent)) == 1 && !read_data.contains("tool-000"),
        "nested/duplicate roots were not collapsed to their parent"
    );
    drop(fixture);

    let mut fixture = new_fixture();
    let paths: Vec<String> = (0..128)
        .map(|index| format!("/dynamic-{index:03}/bin"))
        .collect();
    fixture.options.read_paths = paths.clone();
    fixture.deps.with_synthetic_directories(&paths);
    let _ = fixture.render();
}

#[test]
fn equivalent_semantic_inputs_render_byte_identically() {
    let fixture = new_fixture();
    let parent = make_directory(&join(&join(&fixture.base, "deterministic"), "tools"));
    let child = make_directory(&join(&join(&parent, "nested"), "bin"));
    let alias = join(&fixture.base, "deterministic-alias");
    symlink(&parent, &alias);
    let path_one = make_directory(&join(&fixture.base, "path-one"));
    let path_two = make_directory(&join(&fixture.base, "path-two"));

    let mut first_options = fixture.options.clone();
    first_options.read_paths = vec![child.clone(), alias, parent.clone(), child];
    first_options.host_entries = vec![format!("PATH={path_one}:{path_two}")];
    let first_deps = Deps {
        resolve: Box::new(resolve_path),
        fixed_paths: vec![
            AutomaticPath {
                path: path_two.clone(),
                kind: PathKind::Directory,
            },
            AutomaticPath {
                path: path_one.clone(),
                kind: PathKind::Directory,
            },
        ],
        developer_root: Box::new(|| Ok(None)),
    };

    let mut second_options = fixture.options.clone();
    second_options.read_paths = vec![parent];
    second_options.host_entries = vec!["LANG=C".to_string(), format!("PATH={path_two}:{path_one}")];
    let second_deps = Deps {
        resolve: Box::new(resolve_path),
        fixed_paths: vec![
            AutomaticPath {
                path: path_one.clone(),
                kind: PathKind::Directory,
            },
            AutomaticPath {
                path: path_two,
                kind: PathKind::Directory,
            },
            AutomaticPath {
                path: path_one,
                kind: PathKind::Directory,
            },
        ],
        developer_root: Box::new(|| Ok(None)),
    };

    let first = generate_with(&first_options, &first_deps.view()).expect("first profile");
    let second = generate_with(&second_options, &second_deps.view()).expect("second profile");
    assert_eq!(
        first, second,
        "semantically equal roots rendered different profile bytes"
    );
}

#[test]
fn grants_etc_symlink_traversal_metadata() {
    let profile = new_fixture().render();
    const EXACT_METADATA_RULE: &str = "(allow file-read-metadata\n  (literal \"/etc\"))";
    assert!(
        profile.contains(EXACT_METADATA_RULE),
        "profile lacks the exact metadata-only /etc symlink traversal grant"
    );
    for parent in ["/etc", "/private/etc"] {
        assert!(
            !profile.contains(&filter("subpath", parent)),
            "/etc traversal compatibility granted sensitive parent subtree {parent}"
        );
    }
    assert!(
        !profile.contains(&format!(
            "(allow file-read*\n  {})",
            filter("literal", "/etc")
        )),
        "/etc symlink traversal grant is not metadata-only"
    );
}

#[test]
fn network_allow_grants_certificate_trust_broker() {
    let mut fixture = new_fixture();
    fixture.options.network = Some(NetworkMode::Allow);
    let allow = fixture.render();
    const TRUST_LOOKUP: &str = "(allow mach-lookup\n  (global-name \"com.apple.trustd\")\n  (global-name \"com.apple.trustd.agent\"))";
    let allow_network = section(&allow, "NETWORK");
    assert!(
        count(&allow, TRUST_LOOKUP) == 1 && allow_network.contains(TRUST_LOOKUP),
        "NetworkAllow does not contain exactly one marker-scoped trustd broker lookup"
    );

    fixture.options.network = Some(NetworkMode::Deny);
    let deny = fixture.render();
    assert!(
        !deny.contains("com.apple.trustd"),
        "NetworkDeny emitted a trustd broker grant"
    );
}

#[test]
fn xcode_selector_uses_only_exact_read_and_traversal_metadata() {
    let profile = new_fixture().render();
    const EXACT_METADATA_RULE: &str = "(allow file-read-metadata\n  (literal \"/var\")\n  (literal \"/var/select\")\n  (literal \"/private/var/select\")\n  (literal \"/var/select/developer_dir\")\n  (literal \"/private/var/select/developer_dir\"))";
    assert!(
        profile.contains(EXACT_METADATA_RULE),
        "Git/xcrun compatibility lacks the reviewed metadata-only selector rule"
    );
    for selector in [
        "/var/select/developer_dir",
        "/private/var/select/developer_dir",
    ] {
        assert_eq!(
            count(&profile, &filter("literal", selector)),
            1,
            "Git/xcrun compatibility lacks one exact selector grant for {selector}"
        );
    }
    for ancestor in ["/var", "/var/select", "/private/var/select"] {
        assert!(
            profile.contains(&filter("literal", ancestor)),
            "developer selector lacks exact traversal metadata for {ancestor}"
        );
    }
    for parent in ["/var", "/var/select", "/private/var", "/private/var/select"] {
        assert!(
            !profile.contains(&filter("subpath", parent)),
            "developer selector compatibility granted sensitive parent subtree {parent}"
        );
    }
}

#[test]
fn bin_sh_selector_is_one_exact_read_only_runtime_file() {
    let profile = new_fixture().render();
    const SELECTOR: &str = "/private/var/select/sh";
    assert!(
        count(&profile, &filter("literal", SELECTOR)) == 1
            && profile.contains(&format!(
                "(allow file-read-metadata\n  {}",
                filter("literal", SELECTOR)
            )),
        "/bin/sh compatibility lacks one exact metadata-only shell-selector grant"
    );
    assert!(
        !profile.contains(&format!(
            "(allow file-read*\n  {}",
            filter("literal", SELECTOR)
        )) && !profile.contains(&filter("subpath", "/private/var/select"))
            && !profile.contains(&filter("subpath", "/private/var")),
        "/bin/sh selector compatibility granted a writable/sensitive parent subtree"
    );
}

#[test]
fn reviewed_automatic_roots_are_narrow() {
    for required in [
        "/bin",
        "/sbin",
        "/usr/bin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/libexec",
        "/usr/share",
        "/usr/include",
        "/System",
        "/Library/Apple",
        "/Library/Developer",
        "/opt/homebrew/bin",
        "/opt/homebrew/lib",
        "/opt/homebrew/Cellar",
        "/opt/homebrew/opt",
        "/usr/local/bin",
        "/usr/local/lib",
        "/usr/local/Homebrew",
    ] {
        assert!(
            REVIEWED_AUTOMATIC_PATHS.contains(&required),
            "reviewed automatic roots omit {required}"
        );
    }
    for forbidden in [
        "/",
        "/Users",
        "/Applications",
        "/Library",
        "/Network",
        "/Volumes",
        "/dev",
        "/private",
        "/private/etc",
        "/private/tmp",
        "/private/var",
        "/usr",
        "/opt",
        "/opt/homebrew",
        "/opt/homebrew/etc",
        "/opt/homebrew/var",
        "/usr/local",
        "/usr/local/etc",
        "/usr/local/var",
    ] {
        assert!(
            !REVIEWED_AUTOMATIC_PATHS.contains(&forbidden),
            "reviewed automatic roots contain broad/sensitive {forbidden}"
        );
    }
    for path in REVIEWED_RUNTIME_FILES {
        assert!(
            path.starts_with("/private/etc/") && *path != "/private/etc",
            "runtime compatibility path is not one exact /private/etc file: {path}"
        );
    }
}

#[test]
fn embedded_template_has_four_unique_markers() {
    for marker in [READ_MARKER, WRITE_MARKER, NETWORK_MARKER, SHELL_MARKER] {
        assert_eq!(
            count(super::super::TEMPLATE, marker),
            1,
            "template marker {marker} does not appear exactly once"
        );
    }
}

#[test]
fn discovery_errors_remain_bounded() {
    let mut fixture = new_fixture();
    let private_diagnostic = join(&fixture.base, "private-discovery-value");
    fixture.deps.developer_root = Box::new(|| Err(Rejected));
    let error = match fixture.generate() {
        Err(error) => error,
        Ok(profile) => panic!("discovery failure generated {} bytes", profile.len()),
    };
    let text = error.to_string();
    assert!(
        text.len() <= 128 && !text.contains(&private_diagnostic) && !text.contains(&fixture.base),
        "profile error leaks discovery details: {text:?}"
    );
}
