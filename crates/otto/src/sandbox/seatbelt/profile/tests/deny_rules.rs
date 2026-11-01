//! Tests for the default-closed write and deny rules.

use super::*;

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
        "(allow file-write*)",
        "(allow network-outbound)",
        "(allow network-inbound)",
    ] {
        assert!(
            !profile.contains(unfiltered),
            "profile contains unfiltered grant {unfiltered}"
        );
    }

    let write_section = section(&profile, "WRITE");
    for path in [
        fixture.options.workspace.clone(),
        fixture.directory(&fixture.options.directories.temp),
        fixture.directory(&fixture.options.directories.cache),
    ] {
        assert!(
            write_section.contains(&filter("subpath", &path)),
            "writable path {} is not writable",
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

    let root = fixture.directory(&fixture.options.directories.root);
    assert!(
        !profile.contains(&filter("subpath", &root)),
        "private state parent received a subpath grant"
    );
    assert!(
        !profile.contains(&fixture.state().profiles)
            && !profile.contains(&fixture.state().profile_path),
        "profiles directory or generated profile leaked into child policy"
    );
}

fn modules_pattern_filter(workspace: &str, pattern: &str) -> String {
    pattern_filter(&format!("{workspace}/.git/modules"), pattern)
}

fn worktrees_pattern_filter(workspace: &str, pattern: &str) -> String {
    pattern_filter(&format!("{workspace}/.git/worktrees"), pattern)
}

fn pattern_filter(root: &str, pattern: &str) -> String {
    format!(
        "(require-all {} (regex #\"{pattern}\"))",
        filter("subpath", root)
    )
}

#[test]
fn workspace_git_metadata_and_identity_are_denied_after_every_write_allow() {
    let fixture = new_fixture();
    let workspace = fixture.options.workspace.clone();
    let profile = fixture.render();
    let tag = tag_message(&fixture);
    let deny = format!(
        "(deny file-write*\n  (literal {})\n  (literal {})\n  (literal {})\n  (literal {})\n  (subpath {})\n  {}\n  {}\n  {}\n  (literal {})\n  (literal {})\n  {tag}\n)\n",
        quote(&format!("{workspace}/.git")),
        quote(&format!("{workspace}/.git/config")),
        quote(&format!("{workspace}/.git/config.worktree")),
        quote(&format!("{workspace}/.git/commondir")),
        quote(&format!("{workspace}/.git/hooks")),
        modules_pattern_filter(&workspace, r"/(config|config\.worktree|commondir)$"),
        modules_pattern_filter(&workspace, r"/hooks(/|$)"),
        worktrees_pattern_filter(&workspace, r"/config\.worktree$"),
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
    for (prefix, pattern) in GIT_DIR_PROGRAM_SETTINGS {
        assert!(
            write.contains(&pattern_filter(&join(&linked, prefix), pattern)),
            "linked worktree lacks the {prefix} filter for {pattern}"
        );
    }
}

#[test]
fn git_directory_patterns_are_constant_and_never_hold_workspace_text() {
    for (prefix, pattern) in GIT_DIR_PROGRAM_SETTINGS {
        assert!(prefix.starts_with(".git/") && !prefix.contains('"'));
        assert!(
            !pattern.contains(['"', '\n']),
            "pattern {pattern} would end its Seatbelt literal"
        );
    }
    let mut fixture = new_fixture();
    let linked = make_directory(&join(&fixture.base, "linked-worktree"));
    fixture.options.workspaces.push(linked.clone());
    let profile = fixture.render();
    let write = section(&profile, "WRITE");
    let mut patterns = 0;
    for (at, _) in write.match_indices("(regex #\"") {
        let rest = &write[at + "(regex #\"".len()..];
        let pattern = &rest[..rest.find("\")").expect("regex literal end")];
        assert!(
            GIT_DIR_PROGRAM_SETTINGS
                .iter()
                .any(|(_, constant)| *constant == pattern),
            "regex {pattern} is not one of the constant patterns"
        );
        for path in [&fixture.options.workspace, &linked] {
            assert!(
                !pattern.contains(path.as_str()),
                "regex holds workspace text"
            );
        }
        patterns += 1;
    }
    assert_eq!(patterns, 2 * GIT_DIR_PROGRAM_SETTINGS.len());
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
    assert_eq!(
        forms, 7,
        "expected five template denies, the read deny and the git deny"
    );
}

#[test]
fn rejects_every_read_path_overlapping_private_state() {
    type Mutate = fn(&mut Fixture);
    let cases: [(&str, Mutate); 4] = [
        ("explicit ancestor", |fixture| {
            fixture.options.read_paths = vec![fixture.state().root_parent.clone()];
        }),
        ("explicit profiles child", |fixture| {
            fixture.options.read_paths = vec![fixture.state().profiles.clone()];
        }),
        ("another private root", |fixture| {
            let other = make_directory(&join(&fixture.state().root_parent, "otto-sandbox-other"));
            fixture.options.read_paths = vec![make_directory(&join(&other, "tmp"))];
        }),
        ("own tmp directory", |fixture| {
            fixture.options.read_paths = vec![fixture.directory(&fixture.options.directories.temp)];
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
