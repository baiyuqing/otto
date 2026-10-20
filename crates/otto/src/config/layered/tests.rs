use super::*;
fn write(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}
#[test]
fn local_root_beats_shared_leaf_and_forbidden_fields_are_explained() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write(&root.path().join(".git/HEAD"), "ref: refs/heads/main");
    let leaf = root.path().join("sub");
    std::fs::create_dir(&leaf).unwrap();
    write(
        &root.path().join(".otto/config.local.toml"),
        "[ui]\nmode='repl'",
    );
    write(
        &leaf.join(".otto/config.toml"),
        "[ui]\nmode='tui'\n[sandbox]\ndriver='off'",
    );
    let loaded = load_with_policy(
        home.path(),
        &home.path().join("config.toml"),
        false,
        &leaf,
        None,
    )
    .unwrap();
    assert_eq!(loaded.file.ui.mode, "repl");
    assert!(loaded.file.sandbox.driver.is_none());
    assert!(!loaded.merged.diagnostics.is_empty());
}
#[test]
fn executable_changes_invalidate_trust_but_preferences_do_not() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let p = root.path().join(".otto/config.toml");
    write(&p, "[ui]\nmode='repl'\n[mcp.servers.a]\ncommand='first'");
    set_trust(home.path(), root.path(), true).unwrap();
    assert!(approved(home.path(), root.path()).unwrap());
    write(&p, "[ui]\nmode='tui'\n[mcp.servers.a]\ncommand='first'");
    assert!(approved(home.path(), root.path()).unwrap());
    write(&p, "[mcp.servers.a]\ncommand='other'");
    assert!(!approved(home.path(), root.path()).unwrap());
    set_trust(home.path(), root.path(), true).unwrap();
    set_trust(home.path(), root.path(), false).unwrap();
    assert!(!approved(home.path(), root.path()).unwrap());
}
#[test]
fn instructions_follow_chain_and_agents_wins_over_claude() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write(&root.path().join(".git/HEAD"), "ref: refs/heads/main");
    write(&root.path().join("AGENTS.md"), "root");
    write(&root.path().join("CLAUDE.md"), "ignored");
    write(&root.path().join("sub/CLAUDE.md"), "leaf");
    write(&home.path().join(".config/otto/AGENTS.md"), "global");
    let files = instruction_files(home.path(), &root.path().join("sub")).unwrap();
    assert_eq!(
        files.into_iter().map(|(_, t)| t).collect::<Vec<_>>(),
        ["global", "root", "leaf"]
    );
}
#[test]
fn nested_checkout_and_symlink_escape_are_bounded() {
    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    write(&root.path().join(".git/HEAD"), "ref: refs/heads/main");
    write(&root.path().join("sub/.git/HEAD"), "ref: refs/heads/main");
    // `directories` returns canonical paths; macOS temporary directories
    // sit under the `/var` -> `/private/var` symlink.
    assert_eq!(
        directories(&root.path().join("sub")).unwrap(),
        [std::fs::canonicalize(root.path().join("sub")).unwrap()]
    );
    write(&outside.path().join("config.toml"), "[ui]\nmode='tui'");
    std::os::unix::fs::symlink(outside.path(), root.path().join("sub/.otto")).unwrap();
    assert!(
        load_with_policy(
            home.path(),
            &home.path().join("none"),
            false,
            &root.path().join("sub"),
            None
        )
        .is_err()
    );
}
#[test]
fn custom_project_catalogs_are_granted_and_changes_revoke_existing_authority() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let user = home.path().join("custom.toml");
    write(
        &user,
        "[skills]\npaths=['custom-skills']\n[agents]\npaths=[]",
    );
    write(&project.path().join("custom-skills/a/SKILL.md"), "first");
    let loaded = load_with_policy(home.path(), &user, true, project.path(), None).unwrap();
    assert!(!loaded.trusted);
    assert!(
        catalog_roots(
            &loaded.file,
            home.path(),
            project.path(),
            &[project.path().join("custom-skills").display().to_string()],
            "skills"
        )
        .unwrap()
        .is_empty()
    );
    set_trust_for(home.path(), &user, project.path(), true).unwrap();
    let loaded = load_with_policy(home.path(), &user, true, project.path(), None).unwrap();
    assert!(loaded.trusted);
    assert_eq!(
        catalog_roots(
            &loaded.file,
            home.path(),
            project.path(),
            &[project.path().join("custom-skills").display().to_string()],
            "skills"
        )
        .unwrap()
        .len(),
        1
    );
    assert!(
        catalog_roots(&loaded.file, home.path(), project.path(), &[], "agents")
            .unwrap()
            .is_empty()
    );
    write(&project.path().join("custom-skills/a/SKILL.md"), "changed");
    assert!(!approved_for(home.path(), &user, project.path()).unwrap());
}
#[test]
fn executable_resources_can_be_binary_and_symlink_replacements_fail_closed() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let asset = project.path().join(".otto/skills/a/resource.bin");
    std::fs::create_dir_all(asset.parent().unwrap()).unwrap();
    std::fs::write(&asset, [0xff, 0]).unwrap();
    set_trust(home.path(), project.path(), true).unwrap();
    assert!(approved(home.path(), project.path()).unwrap());
    std::fs::remove_file(&asset).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", &asset).unwrap();
    assert!(approved(home.path(), project.path()).is_err());
}
#[test]
fn per_workspace_loads_do_not_inherit_the_startup_project_overlay() {
    let home = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let user = home.path().join("config.toml");
    write(
        &user,
        "default_profile='a'\n[profiles.a]\nprovider='chatgpt'\nmodel='global'",
    );
    write(
        &first.path().join(".otto/config.toml"),
        "[profiles.a]\nmodel='first'",
    );
    write(
        &second.path().join(".otto/config.toml"),
        "[profiles.a]\nmodel='second'",
    );
    let base = load_with_policy(home.path(), &user, true, first.path(), None).unwrap();
    let other = for_workspace(home.path(), &user, &base.file, second.path()).unwrap();
    assert_eq!(base.file.profiles["a"].model, "first");
    assert_eq!(other.file.profiles["a"].model, "second");
    assert_eq!(other.file.profiles["a"].provider, "chatgpt");
    assert_eq!(
        other.merged.origins["profiles.a.provider"].layer,
        Layer::User
    );
    assert_eq!(
        other.merged.origins["profiles.a.model"].source,
        std::fs::canonicalize(second.path())
            .unwrap()
            .join(".otto/config.toml")
            .display()
            .to_string()
    );
}
#[test]
fn managed_project_prohibition_overrides_existing_grants() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let user = home.path().join("config.toml");
    write(
        &project.path().join(".otto/config.toml"),
        "[mcp.servers.a]\ntransport='stdio'\ncommand='tool'",
    );
    set_trust(home.path(), project.path(), true).unwrap();
    let policy = ManagedPolicy {
        allow_project_executables: Some(false),
        ..Default::default()
    };
    let loaded = load_with_policy(home.path(), &user, false, project.path(), Some(policy)).unwrap();
    assert!(!loaded.trusted);
    assert!(loaded.file.mcp.servers.is_empty());
}
#[test]
fn malformed_managed_policy_and_user_writable_policy_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    assert!(policy_at(&path).unwrap().is_none());
    write(&path, "unknown='value'");
    assert!(policy_at(&path).is_err());
    assert!(toml::from_str::<ManagedPolicy>("unknown='value'").is_err());
}
#[test]
fn instructions_enforce_both_file_and_total_budgets() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(&project.path().join("AGENTS.md"), &"x".repeat(16385));
    assert!(instruction_files(home.path(), project.path()).is_err());
    write(&project.path().join(".git"), "gitdir: /gitdir");
    let mut leaf = project.path().to_path_buf();
    for _ in 0..5 {
        write(&leaf.join("AGENTS.md"), &"x".repeat(15000));
        leaf = leaf.join("child");
    }
    std::fs::create_dir_all(&leaf).unwrap();
    assert!(instruction_files(home.path(), &leaf).is_err());
}
#[test]
fn revocation_works_after_project_sources_become_invalid() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    set_trust(home.path(), project.path(), true).unwrap();
    write(
        &project.path().join(".otto/config.toml"),
        "this is not TOML",
    );
    assert!(approved(home.path(), project.path()).is_err());
    set_trust(home.path(), project.path(), false).unwrap();
    assert!(!approved(home.path(), project.path()).unwrap());
}
#[test]
fn default_catalogs_follow_root_to_leaf_and_explicit_empty_paths_clear_them() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    write(&project.path().join(".git/HEAD"), "ref: refs/heads/main");
    let leaf = project.path().join("sub");
    std::fs::create_dir(&leaf).unwrap();
    let mut file = File {
        project_executables: Some(true),
        ..Default::default()
    };
    let env = std::collections::HashMap::from([("HOME".into(), home.path().display().to_string())]);
    let configured = otto_core::config::resolve_skills(&file, &env, leaf.to_str().unwrap());
    let roots = catalog_roots(&file, home.path(), &leaf, &configured.roots, "skills").unwrap();
    assert_eq!(
        roots,
        vec![
            home.path().join(".otto/skills").display().to_string(),
            std::fs::canonicalize(project.path())
                .unwrap()
                .join(".otto/skills")
                .display()
                .to_string(),
            std::fs::canonicalize(&leaf)
                .unwrap()
                .join(".otto/skills")
                .display()
                .to_string()
        ]
    );
    file.skills.paths = Some(Vec::new());
    assert!(
        catalog_roots(&file, home.path(), &leaf, &[], "skills")
            .unwrap()
            .is_empty()
    );
}
#[test]
fn unrelated_project_grants_do_not_activate_or_scan_unsafe_catalogs() {
    let home = tempfile::tempdir().unwrap();
    let outer = tempfile::tempdir().unwrap();
    std::fs::create_dir(outer.path().join(".git")).unwrap();
    let first = tempfile::tempdir_in(outer.path()).unwrap();
    let other = tempfile::tempdir_in(outer.path()).unwrap();
    set_trust(home.path(), first.path(), true).unwrap();
    std::fs::create_dir(other.path().join(".otto")).unwrap();
    std::os::unix::fs::symlink("/etc", other.path().join(".otto/skills")).unwrap();
    assert!(!approved(home.path(), other.path()).unwrap());
    assert!(
        !load(
            home.path(),
            &home.path().join(".config/otto/config.toml"),
            false,
            other.path()
        )
        .unwrap()
        .trusted
    );
}
#[test]
fn grants_use_canonical_workspace_paths_independently_of_alias_spelling() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let holder = tempfile::tempdir().unwrap();
    write(&project.path().join(".otto/skills/a/SKILL.md"), "reviewed");
    let alias = holder.path().join("alias");
    std::os::unix::fs::symlink(project.path(), &alias).unwrap();
    set_trust(home.path(), &alias, true).unwrap();
    assert!(approved(home.path(), project.path()).unwrap());
    assert!(approved(home.path(), &alias).unwrap());
}
#[test]
fn executable_state_backups_do_not_mix_with_user_config_backups() {
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    set_trust(home.path(), project.path(), true).unwrap();
    set_trust(home.path(), project.path(), false).unwrap();
    assert!(home.path().join(".config/otto/trust/backups").exists());
    assert!(!home.path().join(".config/otto/backups").exists());
}
