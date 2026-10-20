
use super::*;

fn write_skill(root: &Path, name: &str, frontmatter_extra: &str, body: &str) -> PathBuf {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).expect("the skill directory is creatable");
    let content =
        format!("---\nname: {name}\ndescription: desc for {name}\n{frontmatter_extra}---\n{body}");
    std::fs::write(directory.join("SKILL.md"), content).expect("SKILL.md is writable");
    directory
}

/// Discovers one skill written with `frontmatter_extra` and returns it
/// alongside the discovery warnings.
fn discover_one(frontmatter_extra: &str) -> (Option<Skill>, Vec<String>) {
    let root = tempfile::tempdir().expect("a temporary directory");
    write_skill(root.path(), "contracted", frontmatter_extra, "body\n");
    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
    (catalog.lookup("contracted").cloned(), warnings)
}

#[test]
fn declaring_both_halves_gives_the_skill_a_contract() {
    let (skill, warnings) = discover_one("input: a report path\noutput: a list of records\n");

    assert!(warnings.is_empty(), "{warnings:?}");
    let contract = skill.expect("the skill").contract.expect("a contract");
    assert_eq!(contract.input, "a report path");
    assert_eq!(contract.output, "a list of records");
}

#[test]
fn declaring_neither_half_is_the_silent_default() {
    let (skill, warnings) = discover_one("");

    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(skill.expect("the skill").contract.is_none());
}

#[test]
fn declaring_one_half_warns_and_leaves_the_skill_contract_free() {
    for half in ["input: a report path\n", "output: a list of records\n"] {
        let (skill, warnings) = discover_one(half);

        let skill = skill.expect("the skill is still usable");
        assert!(skill.contract.is_none(), "{half:?}");
        assert_eq!(warnings.len(), 1, "{half:?}: {warnings:?}");
        assert!(
            warnings[0].contains("input") && warnings[0].contains("output"),
            "the warning must name both halves: {warnings:?}"
        );
    }
}

#[test]
fn an_over_long_half_warns_and_drops_the_contract_but_keeps_the_skill() {
    let long = "a".repeat(MAX_SKILL_CONTRACT_CHARS + 1);
    let (skill, warnings) = discover_one(&format!("input: {long}\noutput: fine\n"));

    let skill = skill.expect("the skill is still usable");
    assert!(skill.contract.is_none());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("input"), "{warnings:?}");
}

#[test]
fn a_blank_half_counts_as_undeclared() {
    let (skill, warnings) = discover_one("input: \"   \"\noutput: a list\n");

    let skill = skill.expect("the skill");
    assert!(skill.contract.is_none());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

fn write_raw_skill(root: &Path, directory_name: &str, content: &str) -> PathBuf {
    let directory = root.join(directory_name);
    std::fs::create_dir_all(&directory).expect("the skill directory is creatable");
    std::fs::write(directory.join("SKILL.md"), content).expect("SKILL.md is writable");
    directory
}

#[test]
fn discovery_merges_two_roots_and_the_later_root_wins() {
    let user = tempfile::tempdir().expect("a temporary directory");
    let workspace = tempfile::tempdir().expect("a temporary directory");
    write_skill(user.path(), "pdf", "", "# pdf\n");
    write_skill(user.path(), "shared", "", "# user shared\n");
    write_skill(workspace.path(), "shared", "", "# workspace shared\n");

    let (catalog, warnings) =
        Catalog::discover(&[user.path().to_path_buf(), workspace.path().to_path_buf()]);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(catalog.len(), 2);
    assert_eq!(
        catalog
            .lookup("shared")
            .expect("the shared skill")
            .directory,
        workspace.path().join("shared")
    );
    let names: Vec<&str> = catalog
        .skills()
        .iter()
        .map(|skill| skill.name.as_str())
        .collect();
    assert_eq!(names, ["pdf", "shared"]);
}

#[test]
fn a_missing_root_and_a_directory_without_a_skill_file_are_silent() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let (catalog, warnings) = Catalog::discover(&[root.path().join("does-not-exist")]);
    assert!(warnings.is_empty() && catalog.is_empty(), "{warnings:?}");

    std::fs::create_dir_all(root.path().join("notaskill")).expect("creatable");
    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
    assert!(warnings.is_empty() && catalog.is_empty(), "{warnings:?}");
}

#[test]
fn an_unreadable_root_produces_one_warning() {
    if nix::unistd::geteuid().is_root() {
        // Skipped as root, which ignores the mode bits.
        return;
    }
    let root = tempfile::tempdir().expect("a temporary directory");
    let blocked = root.path().join("blocked");
    std::fs::create_dir(&blocked).expect("creatable");
    std::fs::set_permissions(
        &blocked,
        std::os::unix::fs::PermissionsExt::from_mode(0o000),
    )
    .expect("the mode is settable");

    let (_, warnings) = Catalog::discover(std::slice::from_ref(&blocked));
    let _ = std::fs::set_permissions(
        &blocked,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].starts_with("skills root "), "{warnings:?}");
    assert!(
        warnings[0].contains(&blocked.display().to_string()),
        "{warnings:?}"
    );
}

#[test]
fn an_invalid_skill_warns_and_the_others_still_load() {
    let root = tempfile::tempdir().expect("a temporary directory");
    write_skill(root.path(), "good", "", "# good\n");
    let bad = write_raw_skill(root.path(), "bad", "no frontmatter here");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
    assert_eq!(catalog.len(), 1);
    assert!(catalog.lookup("good").is_some());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains(&bad.join("SKILL.md").display().to_string()),
        "{warnings:?}"
    );
}

/// The link is followed but the reported directory stays inside the skills
/// root.
#[test]
fn a_symlinked_skill_directory_is_followed_without_rewriting_its_path() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let real = tempfile::tempdir().expect("a temporary directory");
    write_skill(real.path(), "linked", "", "# linked\n");
    std::os::unix::fs::symlink(real.path().join("linked"), root.path().join("linked"))
        .expect("the link is creatable");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        catalog
            .lookup("linked")
            .expect("the linked skill")
            .directory,
        root.path().join("linked")
    );
}

/// A `SKILL.md` that is a symbolic link out of the skill directory is
/// refused, not followed.
#[test]
fn a_skill_file_symlink_out_of_the_directory_is_rejected() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let directory = root.path().join("escaped");
    std::fs::create_dir(&directory).expect("creatable");
    let elsewhere = tempfile::tempdir().expect("a temporary directory");
    let outside = elsewhere.path().join("SKILL.md");
    std::fs::write(
        &outside,
        "---\nname: escaped\ndescription: outside\n---\nbody\n",
    )
    .expect("writable");
    std::os::unix::fs::symlink(&outside, directory.join("SKILL.md"))
        .expect("the link is creatable");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
    assert!(catalog.is_empty(), "{catalog:?}");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
}

#[test]
fn an_invalid_or_mismatched_name_is_rejected_with_a_reason() {
    let long = "a".repeat(65);
    for (directory_name, frontmatter_name, reason) in [
        ("Upper", "Upper", "invalid"),
        ("-lead", "-lead", "invalid"),
        ("trail-", "trail-", "invalid"),
        ("dou--ble", "dou--ble", "invalid"),
        (long.as_str(), long.as_str(), "invalid"),
        ("actualdir", "otherdir", "does not match directory"),
        ("noname", "", "missing name"),
    ] {
        let root = tempfile::tempdir().expect("a temporary directory");
        let name_line = if frontmatter_name.is_empty() {
            String::new()
        } else {
            format!("name: {frontmatter_name}\n")
        };
        write_raw_skill(
            root.path(),
            directory_name,
            &format!("---\n{name_line}description: d\n---\nbody\n"),
        );

        let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
        assert!(catalog.is_empty(), "{directory_name}: {catalog:?}");
        assert_eq!(warnings.len(), 1, "{directory_name}: {warnings:?}");
        assert!(
            warnings[0].contains(reason),
            "{directory_name}: {warnings:?}"
        );
    }
}

#[test]
fn a_description_must_be_present_bounded_and_trimmed() {
    for (content, reason) in [
        (
            "---\nname: x\n---\nbody\n".to_string(),
            "missing description",
        ),
        (
            format!(
                "---\nname: x\ndescription: {}\n---\nbody\n",
                "a".repeat(1025)
            ),
            "exceeds 1024",
        ),
    ] {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_raw_skill(root.path(), "x", &content);
        let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
        assert!(catalog.is_empty(), "{reason}: {catalog:?}");
        assert_eq!(warnings.len(), 1, "{reason}: {warnings:?}");
        assert!(warnings[0].contains(reason), "{reason}: {warnings:?}");
    }

    let root = tempfile::tempdir().expect("a temporary directory");
    write_raw_skill(
        root.path(),
        "x",
        "---\nname: x\ndescription: \"  padded  \"\n---\nbody\n",
    );
    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        catalog.lookup("x").expect("the skill").description,
        "padded"
    );
}

#[test]
fn loading_a_skill_returns_the_body_without_the_frontmatter() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let directory = write_skill(root.path(), "pdf", "", "# PDF handling\nBody text\n");
    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let skill = catalog.lookup("pdf").expect("the pdf skill");
    assert_eq!(skill.directory, directory);
    assert_eq!(skill.path, directory.join("SKILL.md"));
    assert_eq!(
        load(skill).expect("the body loads"),
        "# PDF handling\nBody text\n"
    );
}

/// A path outside the skill directory is refused rather than read.
#[test]
fn loading_honours_the_selected_path_and_refuses_one_outside_the_directory() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let directory = write_skill(root.path(), "sample", "", "default\n");
    let alternate = directory.join("alternate.md");
    std::fs::write(&alternate, "---\nname: sample\n---\nalternate\n").expect("writable");
    let selected = Skill {
        name: "sample".into(),
        description: "d".into(),
        contract: None,
        directory: directory.clone(),
        path: alternate,
    };
    assert_eq!(load(&selected).expect("the body loads").trim(), "alternate");

    let other_root = tempfile::tempdir().expect("a temporary directory");
    let outside = write_skill(other_root.path(), "outside", "", "external\n");
    let escaping = Skill {
        path: outside.join("SKILL.md"),
        ..selected
    };
    assert!(
        load(&escaping).is_err(),
        "a path outside the skill directory was accepted"
    );
}

/// The file is sparse, so it costs no disk space.
#[test]
fn an_oversized_skill_file_is_refused() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let directory = write_skill(root.path(), "pdf", "", "body\n");
    let path = directory.join("SKILL.md");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .expect("the file is writable")
        .set_len(MAX_SKILL_FILE_BYTES + 1)
        .expect("the file is resizable");

    let skill = Skill {
        name: "pdf".into(),
        description: "d".into(),
        contract: None,
        directory,
        path,
    };
    let error = load(&skill).expect_err("an oversized file is refused");
    assert!(error.contains("too large"), "{error:?}");
}

#[test]
fn listing_sorts_skips_hidden_and_linked_entries_and_reports_the_total() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let path = root.path();
    for (name, contents) in [("SKILL.md", "x"), ("b.txt", "x"), (".hidden", "x")] {
        std::fs::write(path.join(name), contents).expect("writable");
    }
    std::fs::create_dir_all(path.join("scripts")).expect("creatable");
    std::fs::write(path.join("scripts/a.py"), "x").expect("writable");
    std::fs::create_dir_all(path.join(".hiddendir")).expect("creatable");
    std::fs::write(path.join(".hiddendir/inner.txt"), "x").expect("writable");
    let elsewhere = tempfile::tempdir().expect("a temporary directory");
    let outside = elsewhere.path().join("outside.txt");
    std::fs::write(&outside, "x").expect("writable");
    std::os::unix::fs::symlink(&outside, path.join("link.txt")).expect("linkable");

    let (files, total) = list_files(path, 50).expect("the listing succeeds");
    assert_eq!(files, ["b.txt", "scripts/a.py"]);
    assert_eq!(total, 2);

    let limited = tempfile::tempdir().expect("a temporary directory");
    for index in 0..5u8 {
        let name = format!("f{}.txt", (b'a' + index) as char);
        std::fs::write(limited.path().join(name), "x").expect("writable");
    }
    let (files, total) = list_files(limited.path(), 3).expect("the listing succeeds");
    assert_eq!((files.len(), total), (3, 5));

    let empty = tempfile::tempdir().expect("a temporary directory");
    std::fs::write(empty.path().join("SKILL.md"), "x").expect("writable");
    let (files, total) = list_files(empty.path(), 50).expect("the listing succeeds");
    assert!(files.is_empty() && total == 0, "{files:?}");
}

/// The discovery roots, in the precedence order the runtime uses.
#[test]
fn the_workspace_root_is_searched_after_the_home_root() {
    assert_eq!(
        roots(Some(Path::new("/home/u")), Path::new("/ws")),
        [
            PathBuf::from("/home/u/.otto/skills"),
            PathBuf::from("/ws/.otto/skills")
        ]
    );
    assert_eq!(
        roots(None, Path::new("/ws")),
        [PathBuf::from("/ws/.otto/skills")]
    );
}
