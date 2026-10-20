//! Tests for automatic read roots and the configured shell.

use super::*;

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
