//! Tests for the read denylist rules: the global read allow, the denied
//! subtrees, the `read_paths` exceptions and the entries left out.

use super::*;
use crate::sandbox::read_denylist::BUILT_IN_DENY_PATHS;

fn deny_section(profile: &str) -> &str {
    section(profile, "READ-DENY")
}

fn allow_section(profile: &str) -> &str {
    section(profile, "READ-ALLOW")
}

#[test]
fn global_read_allow_precedes_every_deny_and_every_exception() {
    let mut fixture = new_fixture();
    let denied = make_directory(&join(&fixture.base, "denied"));
    fixture.options.deny_read_paths = vec![denied.clone()];
    fixture.options.read_paths = vec![make_directory(&join(&denied, "allowed"))];
    let profile = fixture.render();
    let global_allow = profile
        .find("(allow file-read*)\n")
        .expect("global read allow");
    let deny_at = profile
        .find("; OTTO-DYNAMIC-READ-DENY-BEGIN")
        .expect("deny section");
    let allow_at = profile
        .find("; OTTO-DYNAMIC-READ-ALLOW-BEGIN")
        .expect("allow section");
    assert!(global_allow < deny_at && deny_at < allow_at);
    assert!(
        profile.find("(deny file-read*").expect("first read deny") > global_allow,
        "a read deny precedes the global read allow"
    );
}

#[test]
fn renders_every_built_in_entry_below_the_resolved_home() {
    let fixture = new_fixture();
    let profile = fixture.render();
    let deny = deny_section(&profile);
    for relative in BUILT_IN_DENY_PATHS {
        let expected = filter("subpath", &join(&fixture.options.home, relative));
        assert!(
            deny.contains(&expected),
            "built-in entry {relative} is not denied"
        );
    }
    assert!(fixture.generated().skipped_deny_paths.is_empty());
}

#[test]
fn deny_form_names_the_private_root_prefix_by_regex() {
    let fixture = new_fixture();
    let profile = fixture.render();
    let cache_base = dir(&fixture.directory(&fixture.options.directories.root));
    let regex = format!("(regex #\"^{}/otto-sandbox-\")", regex_escape(&cache_base));
    assert_eq!(count(deny_section(&profile), &regex), 1, "{profile}");
    assert!(deny_section(&profile).contains(&format!("(deny file-read*\n  {regex}\n")));
}

#[test]
fn regex_escape_neutralizes_every_regex_metacharacter() {
    assert_eq!(regex_escape("/a.b+c*d?e"), "/a\\.b\\+c\\*d\\?e");
    assert_eq!(
        regex_escape("(x)[y]{z}|^$"),
        "\\(x\\)\\[y\\]\\{z\\}\\|\\^\\$"
    );
    assert_eq!(regex_escape("back\\slash\"quote"), "back\\\\slash\\\"quote");
    assert_eq!(
        regex_escape("/plain/Library/Caches"),
        "/plain/Library/Caches"
    );
}

#[test]
fn own_tmp_and_cache_are_allowed_after_the_regex_deny_and_profiles_is_not() {
    let fixture = new_fixture();
    let profile = fixture.render();
    let directories = &fixture.options.directories;
    let allow = allow_section(&profile);
    assert!(allow.contains(&filter("subpath", &fixture.directory(&directories.temp))));
    assert!(allow.contains(&filter("subpath", &fixture.directory(&directories.cache))));
    assert!(!profile.contains(&fixture.state().profiles));
    assert!(
        profile.find("OTTO-DYNAMIC-READ-DENY-END").unwrap()
            < profile.find("OTTO-DYNAMIC-READ-ALLOW-BEGIN").unwrap()
    );
}

#[test]
fn configured_entries_render_in_both_spellings_when_they_differ() {
    let mut fixture = new_fixture();
    let target = make_directory(&join(&fixture.base, "secret-target"));
    let alias = join(&fixture.base, "secret-alias");
    symlink(&target, &alias);
    let missing = join(&fixture.base, "not-created-yet");
    let below_workspace = join(&fixture.options.workspace, "private-notes");
    fixture.options.deny_read_paths = vec![alias.clone(), missing.clone(), below_workspace.clone()];
    let profile = fixture.render();
    let deny = deny_section(&profile);
    for path in [&alias, &target, &missing, &below_workspace] {
        assert!(
            deny.contains(&filter("subpath", path)),
            "{path} is not denied"
        );
    }
}

#[test]
fn built_in_entry_containing_a_writable_root_is_skipped_and_reported() {
    let mut fixture = new_fixture();
    let nested_workspace = make_directory(&join(&fixture.options.home, ".config/gh/work"));
    fixture.options.workspaces.push(nested_workspace);
    let generated = fixture.generated();
    let gh = join(&fixture.options.home, ".config/gh");
    assert!(!deny_section(&generated.text).contains(&quote(&gh)));
    assert!(
        deny_section(&generated.text)
            .contains(&filter("subpath", &join(&fixture.options.home, ".ssh")))
    );
    assert_eq!(
        generated.skipped_deny_paths,
        vec!["~/.config/gh".to_string()]
    );
}

#[test]
fn configured_entry_containing_a_writable_root_is_rejected() {
    let mut fixture = new_fixture();
    let projects = make_directory(&join(&fixture.options.home, "projects"));
    let nested_workspace = make_directory(&join(&projects, "work"));
    fixture.options.workspaces.push(nested_workspace);
    fixture.options.deny_read_paths = vec![projects];
    fixture.assert_rejected("a deny entry containing a workspace");
    fixture.options.deny_read_paths = vec![fixture.options.workspace.clone()];
    fixture.assert_rejected("a deny entry equal to the workspace");
}

#[test]
fn read_path_is_active_only_at_or_below_a_deny_entry() {
    let mut fixture = new_fixture();
    let area = make_directory(&join(&fixture.base, "area"));
    let denied = make_directory(&join(&area, "denied"));
    let inside_directory = make_directory(&join(&denied, "docs"));
    let inside_file = make_file(&join(&denied, "token-notes.txt"), 0o600);
    let outside = make_directory(&join(&fixture.base, "outside"));
    let ancestor = area;
    fixture.options.deny_read_paths = vec![denied.clone()];
    fixture.options.read_paths = vec![
        inside_directory.clone(),
        inside_file.clone(),
        denied.clone(),
        outside.clone(),
        ancestor.clone(),
    ];
    let generated = fixture.generated();
    let allow = allow_section(&generated.text);
    assert!(allow.contains(&filter("subpath", &inside_directory)));
    assert!(allow.contains(&filter("literal", &inside_file)));
    assert!(
        allow.contains(&filter("subpath", &denied)),
        "an exception equal to a deny entry is active"
    );
    assert!(!allow.contains(&quote(&outside)) && !allow.contains(&quote(&ancestor)));
    assert_eq!(generated.inert_read_paths, vec![outside, ancestor]);
}

#[test]
fn read_path_below_a_built_in_entry_is_active() {
    let mut fixture = new_fixture();
    let ssh_config_directory = make_directory(&join(&fixture.options.home, ".ssh/shared"));
    fixture.options.read_paths = vec![ssh_config_directory.clone()];
    let generated = fixture.generated();
    assert!(allow_section(&generated.text).contains(&filter("subpath", &ssh_config_directory)));
    assert!(generated.inert_read_paths.is_empty());
}

#[test]
fn rejects_read_paths_at_or_above_the_cache_base_or_inside_a_private_root() {
    let cases: [fn(&Fixture) -> String; 4] = [
        |fixture| fixture.state().root_parent.clone(),
        |fixture| dir(&fixture.state().root_parent),
        |fixture| join(&fixture.state().root_parent, "otto-sandbox-other"),
        |fixture| {
            let other = make_directory(&join(&fixture.state().root_parent, "otto-sandbox-other"));
            join(&other, "cache")
        },
    ];
    for case in cases {
        let mut fixture = new_fixture();
        let path = case(&fixture);
        std::fs::create_dir_all(&path).expect("create path");
        let path = canonical(&path);
        fixture.options.read_paths = vec![path.clone()];
        fixture.assert_rejected(&path);
    }
}

#[test]
fn read_path_in_the_cache_base_outside_private_roots_is_not_rejected() {
    let mut fixture = new_fixture();
    let sibling = make_directory(&join(&fixture.state().root_parent, "unrelated-cache"));
    fixture.options.read_paths = vec![sibling];
    let generated = fixture.generated();
    assert_eq!(generated.inert_read_paths.len(), 1);
}

#[test]
fn rejection_text_stays_bounded_and_source_free() {
    let mut fixture = new_fixture();
    fixture.options.read_paths = vec![fixture.state().root_parent.clone()];
    let error = fixture.generate().expect_err("rejected");
    let text = error.to_string();
    assert!(
        text.len() <= 128 && !text.contains(&fixture.base),
        "{text:?}"
    );
}
