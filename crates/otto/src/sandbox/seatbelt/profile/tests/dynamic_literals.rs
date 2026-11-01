//! Tests for home expansion, path canonicalization and literal escaping.

use super::*;

/// A directory that `deny_read_paths` denies, so `read_paths` entries below it
/// render as exceptions.
fn denied_directory(fixture: &mut Fixture, name: &str) -> String {
    let denied = make_directory(&join(&fixture.base, name));
    fixture.options.deny_read_paths = vec![denied.clone()];
    denied
}

#[test]
fn expands_only_resolved_home_tilde_prefix() {
    let mut fixture = new_fixture();
    let denied = make_directory(&join(&fixture.options.home, "tool roots"));
    fixture.options.deny_read_paths = vec!["~/tool roots".to_string()];
    let tool_directory = make_directory(&join(&denied, "bin"));
    fixture.options.read_paths = vec!["~/tool roots/bin".to_string()];
    let profile = fixture.render();
    assert!(
        section(&profile, "READ-DENY").contains(&filter("subpath", &denied)),
        "~/ in deny_read_paths was not expanded from the resolved home"
    );
    assert!(
        section(&profile, "READ-ALLOW").contains(&filter("subpath", &tool_directory)),
        "~/ in read_paths was not expanded from the resolved home"
    );

    for syntax in [
        "$OTTO_ROOT",
        "${OTTO_ROOT}",
        "$(whoami)",
        "`whoami`",
        "*.tools",
    ] {
        let mut fixture = new_fixture();
        let denied = denied_directory(&mut fixture, "denied");
        let literal = join(&denied, syntax);
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
            !section(&profile, "READ-ALLOW").contains(&literal),
            "an unavailable path was granted"
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
    let denied = denied_directory(&mut fixture, "denied");
    let directory = make_directory(&join(&join(&denied, "canonical"), "directory"));
    let file = make_file(&join(&join(&denied, "canonical"), "file"), 0o600);
    let directory_alias = join(&fixture.base, "directory-alias");
    let file_alias = join(&fixture.base, "file-alias");
    symlink(&directory, &directory_alias);
    symlink(&file, &file_alias);
    fixture.options.read_paths = vec![directory_alias.clone(), file_alias.clone()];
    let profile = fixture.render();
    let allow_section = section(&profile, "READ-ALLOW");
    assert!(
        allow_section.contains(&filter("subpath", &directory))
            && allow_section.contains(&filter("literal", &file))
            && !allow_section.contains(&directory_alias)
            && !allow_section.contains(&file_alias),
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
    let denied = denied_directory(&mut fixture, "denied");
    let missing = join(&denied, "missing");
    fixture.options.read_paths = vec![missing.clone()];
    let generated = fixture.generated();
    assert!(
        !generated.text.contains(&missing) && generated.inert_read_paths.is_empty(),
        "a missing read path must be skipped without a trace in the profile or the inert list"
    );
}

#[test]
fn escapes_dynamic_literals_without_injection() {
    let mut fixture = new_fixture();
    let denied = denied_directory(&mut fixture, "denied");
    let name = "tools 空格 \\\" ) (allow network-outbound) ; #";
    let path = make_directory(&join(&denied, name));
    fixture.options.read_paths = vec![path.clone()];
    let denied_name = "secrets 空格 \\\" ) (allow network-outbound) ; #";
    let denied_path = make_directory(&join(&fixture.base, denied_name));
    fixture.options.deny_read_paths = vec![denied.clone(), denied_path.clone()];

    let profile = fixture.render();
    assert!(
        section(&profile, "READ-ALLOW").contains(&filter("subpath", &path))
            && section(&profile, "READ-DENY").contains(&filter("subpath", &denied_path)),
        "spaces, Unicode, quotes, backslashes, or profile punctuation were not safely represented"
    );
    assert!(
        count(&profile, "(deny default") == 1 && !profile.contains("(allow network-outbound)\n"),
        "dynamic path injected an SBPL form"
    );

    // `String` cannot hold invalid UTF-8, so a non-UTF-8 value is
    // unrepresentable rather than rejected.
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
        for as_deny_entry in [false, true] {
            let mut fixture = new_fixture();
            if as_deny_entry {
                fixture.options.deny_read_paths = vec![path.clone()];
            } else {
                fixture.options.read_paths = vec![path.clone()];
            }
            fixture
                .deps
                .with_synthetic_directories(std::slice::from_ref(&path));
            let error = match fixture.generate() {
                Err(error) => error,
                Ok(profile) => panic!(
                    "control/invalid path generated {} bytes",
                    profile.text.len()
                ),
            };
            let text = error.to_string();
            assert!(
                text.len() <= 128 && !text.contains(&fixture.base) && !text.contains("allow"),
                "profile error is not bounded and source-free: {text:?}"
            );
        }
    }
}

#[test]
fn marker_like_path_text_renders_without_template_reprocessing() {
    let mut fixture = new_fixture();
    let markers = [READ_MARKER, WRITE_MARKER, NETWORK_MARKER, SHELL_MARKER];
    let marker_name = markers.join("-");
    let denied = denied_directory(&mut fixture, &marker_name);
    let read_directory = make_directory(&join(&denied, &marker_name));
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
        section(&first, "READ-DENY").contains(&filter("subpath", &denied))
            && section(&first, "READ-ALLOW").contains(&filter("subpath", &read_directory))
            && section(&first, "SHELL").contains(&filter("literal", &shell)),
        "marker-like filenames were not rendered as escaped literal path text"
    );
    for marker in markers {
        // Once in the denied directory and once in each path below it, the
        // shell included.
        assert!(
            count(&first, marker) >= 2,
            "marker-like text {marker} is missing from the rendered paths"
        );
    }
    for name in ["READ-DENY", "READ-ALLOW", "WRITE", "NETWORK", "SHELL"] {
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
fn dynamic_read_rules_are_bounded_by_total_path_bytes() {
    let mut fixture = new_fixture();
    let paths: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|letter| format!("/dynamic/{letter}-{}", letter.repeat(11_000)))
        .collect();
    fixture.options.deny_read_paths = paths.clone();
    fixture.deps.with_synthetic_directories(&paths);
    fixture.assert_rejected("oversized deny entries");
    drop(fixture);

    let mut fixture = new_fixture();
    let denied = "/dynamic/denied".to_string();
    let exceptions: Vec<String> = ["a", "b", "c"]
        .iter()
        .map(|letter| format!("{denied}/{letter}-{}", letter.repeat(11_000)))
        .collect();
    fixture.options.deny_read_paths = vec![denied.clone()];
    fixture.options.read_paths = exceptions.clone();
    let mut synthetic = exceptions;
    synthetic.push(denied);
    fixture.deps.with_synthetic_directories(&synthetic);
    fixture.assert_rejected("oversized exceptions");
    drop(fixture);

    let mut fixture = new_fixture();
    let paths: Vec<String> = (0..300)
        .map(|index| format!("/dynamic-{index:03}/secrets"))
        .collect();
    fixture.options.deny_read_paths = paths.clone();
    fixture.deps.with_synthetic_directories(&paths);
    let profile = fixture.render();
    assert_eq!(
        count(section(&profile, "READ-DENY"), "(subpath \"/dynamic-"),
        300,
        "many short deny entries within the byte bound must all render"
    );
}

#[test]
fn equivalent_semantic_inputs_render_byte_identically() {
    let fixture = new_fixture();
    let denied = make_directory(&join(&fixture.base, "deterministic"));
    let parent = make_directory(&join(&denied, "tools"));
    let child = make_directory(&join(&join(&parent, "nested"), "bin"));
    let alias = join(&fixture.base, "deterministic-alias");
    symlink(&parent, &alias);

    let mut first_options = fixture.options.clone();
    first_options.deny_read_paths = vec![denied.clone(), denied.clone()];
    first_options.read_paths = vec![child.clone(), alias, parent.clone(), child.clone()];
    let mut second_options = fixture.options.clone();
    second_options.deny_read_paths = vec![denied];
    second_options.read_paths = vec![parent.clone(), child.clone(), parent];

    let deps = Deps::new();
    let first = generate_with(&first_options, &deps.view()).expect("first profile");
    let second = generate_with(&second_options, &deps.view()).expect("second profile");
    assert_eq!(
        first.text, second.text,
        "semantically equal inputs rendered different profile bytes"
    );
}
