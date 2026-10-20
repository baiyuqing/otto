
use super::*;

fn write_agent(root: &Path, name: &str, frontmatter_extra: &str, body: &str) -> PathBuf {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
    let content =
        format!("---\nname: {name}\ndescription: desc for {name}\n{frontmatter_extra}---\n{body}");
    std::fs::write(directory.join("AGENT.md"), content).expect("AGENT.md is writable");
    directory
}

fn write_raw_agent(root: &Path, directory_name: &str, content: &str) -> PathBuf {
    let directory = root.join(directory_name);
    std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
    std::fs::write(directory.join("AGENT.md"), content).expect("AGENT.md is writable");
    directory
}

#[test]
fn every_frontmatter_field_reaches_the_definition() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let directory = write_agent(
        root.path(),
        "reviewer",
        "tools: read, grep ,bash\nmodel: gpt-4o-mini\ncontext: inherit\n",
        "Review the diff.\n",
    );

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(catalog.len(), 1);
    let definition = catalog.lookup("reviewer").expect("the reviewer definition");
    assert_eq!(definition.name, "reviewer");
    assert_eq!(definition.description, "desc for reviewer");
    assert_eq!(
        definition.tools.as_deref(),
        Some(["read".to_string(), "grep".to_string(), "bash".to_string()].as_slice())
    );
    assert_eq!(definition.model, "gpt-4o-mini");
    assert_eq!(definition.context, "inherit");
    assert_eq!(definition.write_policy, WritePolicy::SingleWriter);
    assert!(definition.write_paths.is_empty());
    assert_eq!(definition.body, "Review the diff.");
    assert_eq!(definition.directory, directory);
    assert_eq!(definition.path, directory.join("AGENT.md"));
}

/// A missing name, a name that disagrees with the directory, invalid name
/// characters, a missing or over-long description, an empty or malformed
/// `tools` value and an invalid `context` value, folded into one table:
/// each case is the same "skip the directory and emit one warning"
/// contract.
#[test]
fn an_invalid_definition_is_skipped_with_one_warning() {
    let long_description = "a".repeat(1025);
    let cases: &[(&str, &str, &str)] = &[
        ("noname", "---\ndescription: d\n---\nbody\n", "missing name"),
        (
            "actualdir",
            "---\nname: otherdir\ndescription: d\n---\nbody\n",
            "does not match directory",
        ),
        (
            "Upper",
            "---\nname: Upper\ndescription: d\n---\nbody\n",
            "invalid",
        ),
        ("x", "---\nname: x\n---\nbody\n", "missing description"),
        (
            "x",
            &format!("---\nname: x\ndescription: {long_description}\n---\nbody\n"),
            "exceeds 1024",
        ),
        (
            "x",
            "---\nname: x\ndescription: d\ntools: \n---\nbody\n",
            "tools must be a comma-separated list",
        ),
        (
            "x",
            "---\nname: x\ndescription: d\ntools: \" , , \"\n---\nbody\n",
            "tools must be a comma-separated list",
        ),
        (
            "x",
            "---\nname: x\ndescription: d\ntools: Read, bash\n---\nbody\n",
            "tools item \"Read\" is invalid",
        ),
        (
            "x",
            "---\nname: x\ndescription: d\ncontext: foo\n---\nbody\n",
            r#"context must be "fresh" or "inherit""#,
        ),
        (
            "x",
            "---\nname: x\ndescription: d\nwrite_policy: later\n---\nbody\n",
            "write_policy must be",
        ),
        (
            "x",
            "---\nname: x\ndescription: d\nwrite_policy: owned_paths\n---\nbody\n",
            "write_paths is required",
        ),
        (
            "x",
            "---\nname: x\ndescription: d\nwrite_policy: read_only\nwrite_paths: crates/**\n---\nbody\n",
            "write_paths requires write_policy owned_paths",
        ),
        (
            "x",
            "---\nname: x\ndescription: d\nwrite_policy: owned_paths\nwrite_paths: ../outside\n---\nbody\n",
            "write_paths item",
        ),
    ];

    for (directory_name, content, want) in cases {
        let root = tempfile::tempdir().expect("a temporary directory");
        write_raw_agent(root.path(), directory_name, content);

        let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

        assert_eq!(catalog.len(), 0, "{content}");
        assert_eq!(warnings.len(), 1, "{content}: {warnings:?}");
        assert!(warnings[0].contains(want), "{content}: {warnings:?}");
    }
}

/// The optional keys and their defaults: `tools` with spaces, `context:
/// inherit`, an absent `context` defaulting to fresh, an absent `model`,
/// and a trimmed body.
#[test]
fn optional_frontmatter_keys_take_their_defaults() {
    let root = tempfile::tempdir().expect("a temporary directory");
    write_agent(root.path(), "plain", "", "\n\n  Body text.  \n\n");
    write_agent(root.path(), "spaced", "tools: read, grep ,bash\n", "body\n");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    let plain = catalog.lookup("plain").expect("the plain definition");
    assert_eq!(plain.tools, None);
    assert_eq!(plain.model, "");
    assert_eq!(plain.context, "fresh");
    assert_eq!(plain.body, "Body text.");
    let spaced = catalog.lookup("spaced").expect("the spaced definition");
    assert_eq!(
        spaced.tools.as_deref(),
        Some(["read".to_string(), "grep".to_string(), "bash".to_string()].as_slice())
    );
}

#[test]
fn a_later_root_wins_and_a_missing_root_is_silent() {
    let user = tempfile::tempdir().expect("a temporary directory");
    let workspace = tempfile::tempdir().expect("a temporary directory");
    write_agent(user.path(), "shared", "", "from a\n");
    write_agent(workspace.path(), "shared", "", "from b\n");

    let (catalog, warnings) = Catalog::discover(&[
        user.path().to_path_buf(),
        workspace.path().to_path_buf(),
        workspace.path().join("does-not-exist"),
    ]);

    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    assert_eq!(catalog.len(), 1);
    assert_eq!(catalog.lookup("shared").expect("shared").body, "from b");
}

#[test]
fn write_policy_frontmatter_is_loaded() {
    let root = tempfile::tempdir().expect("a temporary directory");
    write_agent(
        root.path(),
        "executor",
        "write_policy: owned_paths\nwrite_paths: crates/otto/**, docs/*.md\n",
        "body\n",
    );
    write_agent(
        root.path(),
        "planner",
        "write_policy: propose_only\ntools: read, grep\n",
        "body\n",
    );

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    let executor = catalog.lookup("executor").expect("executor");
    assert_eq!(executor.write_policy, WritePolicy::OwnedPaths);
    assert_eq!(executor.write_paths, ["crates/otto/**", "docs/*.md"]);
    let planner = catalog.lookup("planner").expect("planner");
    assert_eq!(planner.write_policy, WritePolicy::ProposeOnly);
    assert!(planner.write_paths.is_empty());
}

/// An AGENT.md link pointing outside its definition directory is skipped,
/// not followed.
#[test]
fn an_agent_file_symlink_out_of_the_directory_is_skipped() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let outside_root = tempfile::tempdir().expect("a temporary directory");
    let directory = root.path().join("external");
    std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
    let outside = outside_root.path().join("AGENT.md");
    std::fs::write(
        &outside,
        "---\nname: external\ndescription: outside description\n---\noutside body\n",
    )
    .expect("the outside file is writable");
    std::os::unix::fs::symlink(&outside, directory.join("AGENT.md"))
        .expect("the symbolic link is creatable");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert_eq!(catalog.len(), 0);
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
}

/// A relative link that stays inside the definition directory is followed.
#[test]
fn an_agent_file_symlink_inside_the_directory_is_followed() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let directory = root.path().join("linked");
    std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
    std::fs::write(
        directory.join("definition.md"),
        "---\nname: linked\ndescription: internal\n---\ninternal body\n",
    )
    .expect("the target file is writable");
    std::os::unix::fs::symlink("definition.md", directory.join("AGENT.md"))
        .expect("the symbolic link is creatable");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    let definition = catalog.lookup("linked").expect("the linked definition");
    assert_eq!(definition.description, "internal");
    assert_eq!(definition.body, "internal body");
}

/// The definition directory itself may be a link, and `directory` keeps the
/// link path.
#[test]
fn a_symlinked_definition_directory_is_followed() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let real_root = tempfile::tempdir().expect("a temporary directory");
    let real_directory = write_agent(real_root.path(), "linked", "", "linked body\n");
    std::os::unix::fs::symlink(&real_directory, root.path().join("linked"))
        .expect("the symbolic link is creatable");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    let definition = catalog.lookup("linked").expect("the linked definition");
    assert_eq!(definition.body, "linked body");
    assert_eq!(definition.directory, root.path().join("linked"));
}

/// The open must use `O_NONBLOCK`, or a FIFO with no writer would hang
/// discovery.
#[test]
fn a_fifo_agent_file_is_skipped_without_blocking() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let directory = root.path().join("fifo");
    std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
    nix::unistd::mkfifo(
        &directory.join("AGENT.md"),
        nix::sys::stat::Mode::from_bits_truncate(0o644),
    )
    .expect("the FIFO is creatable");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert_eq!(catalog.len(), 0);
    assert!(warnings.is_empty(), "warnings = {warnings:?}");
}

#[test]
fn an_oversized_agent_file_is_rejected() {
    let root = tempfile::tempdir().expect("a temporary directory");
    let directory = write_raw_agent(
        root.path(),
        "large",
        "---\nname: large\ndescription: d\n---\nbody\n",
    );
    // A sparse file: the bytes are never written, only the length is set.
    std::fs::OpenOptions::new()
        .write(true)
        .open(directory.join("AGENT.md"))
        .expect("AGENT.md is writable")
        .set_len(MAX_AGENT_FILE_BYTES + 1)
        .expect("the file is extendable");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert_eq!(catalog.len(), 0);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("too large"), "{warnings:?}");
}

#[test]
fn a_directory_without_agent_md_is_ignored_and_bad_frontmatter_names_its_path() {
    let root = tempfile::tempdir().expect("a temporary directory");
    std::fs::create_dir_all(root.path().join("notanagent")).expect("the directory is creatable");
    let bad = write_raw_agent(root.path(), "bad", "no frontmatter here");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert_eq!(catalog.len(), 0);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0].contains(&bad.join("AGENT.md").display().to_string()),
        "{warnings:?}"
    );
}

#[test]
fn definitions_are_sorted_and_lookup_misses_an_unknown_name() {
    let root = tempfile::tempdir().expect("a temporary directory");
    write_agent(root.path(), "zebra", "", "body\n");
    write_agent(root.path(), "alpha", "", "body\n");
    write_agent(root.path(), "mid", "", "body\n");

    let (catalog, warnings) = Catalog::discover(&[root.path().to_path_buf()]);

    assert!(warnings.is_empty(), "warnings = {warnings:?}");
    let names: Vec<&str> = catalog
        .definitions()
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    assert_eq!(names, ["alpha", "mid", "zebra"]);
    assert!(catalog.lookup("alpha").is_some());
    assert!(catalog.lookup("missing").is_none());
}
