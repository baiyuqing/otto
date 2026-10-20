//! Tests for home expansion, path canonicalization and literal escaping.

use super::*;

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
