use super::*;

fn write_skill(root: &Path, name: &str, contract: bool) {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).expect("the skill directory is creatable");
    let halves = if contract {
        "input: a path\noutput: a list\n"
    } else {
        ""
    };
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: about {name}\n{halves}---\nthe {name} body\n"),
    )
    .expect("SKILL.md is writable");
}

fn write_agent(root: &Path, name: &str) {
    let directory = root.join(name);
    std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
    std::fs::write(
        directory.join("AGENT.md"),
        format!("---\nname: {name}\ndescription: the real agent\n---\nagent body\n"),
    )
    .expect("AGENT.md is writable");
}

fn skills(root: &Path) -> crate::skill::Catalog {
    let (catalog, warnings) = crate::skill::Catalog::discover(&[root.to_path_buf()]);
    assert!(warnings.is_empty(), "{warnings:?}");
    catalog
}

#[test]
fn a_contracted_skill_joins_the_catalog_and_a_bare_one_does_not() {
    let root = tempfile::tempdir().expect("a temporary directory");
    write_skill(root.path(), "contracted", true);
    write_skill(root.path(), "bare", false);
    let mut catalog = Catalog::default();

    let (warnings, registered) = catalog.extend_from_skills(&skills(root.path()));

    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(
        registered.iter().map(String::as_str).collect::<Vec<_>>(),
        ["contracted"]
    );
    let definition = catalog.lookup("contracted").expect("the definition");
    assert!(definition.body.contains("the contracted body"));
    assert!(catalog.lookup("bare").is_none());
}

#[test]
fn a_real_definition_wins_the_name_and_the_clash_is_reported() {
    let agents = tempfile::tempdir().expect("a temporary directory");
    write_agent(agents.path(), "shared");
    let (mut catalog, discover_warnings) = Catalog::discover(&[agents.path().to_path_buf()]);
    assert!(discover_warnings.is_empty(), "{discover_warnings:?}");

    let root = tempfile::tempdir().expect("a temporary directory");
    write_skill(root.path(), "shared", true);

    let (warnings, registered) = catalog.extend_from_skills(&skills(root.path()));

    assert_eq!(
        catalog
            .lookup("shared")
            .expect("the definition")
            .body
            .trim(),
        "agent body",
        "an explicit AGENT.md must not be displaced by a skill"
    );
    assert!(registered.is_empty(), "{registered:?}");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("shared"), "{warnings:?}");
}

#[test]
fn the_catalog_stays_sorted_by_name() {
    let root = tempfile::tempdir().expect("a temporary directory");
    for name in ["zulu", "alpha", "mike"] {
        write_skill(root.path(), name, true);
    }
    let mut catalog = Catalog::default();

    catalog.extend_from_skills(&skills(root.path()));

    let names: Vec<&str> = catalog
        .definitions()
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    assert_eq!(names, ["alpha", "mike", "zulu"]);
}

#[test]
fn an_unreadable_body_warns_and_registers_nothing() {
    let root = tempfile::tempdir().expect("a temporary directory");
    write_skill(root.path(), "vanishing", true);
    let catalog_of_skills = skills(root.path());
    std::fs::remove_file(root.path().join("vanishing/SKILL.md")).expect("removable");
    let mut catalog = Catalog::default();

    let (warnings, registered) = catalog.extend_from_skills(&catalog_of_skills);

    assert!(registered.is_empty(), "{registered:?}");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("vanishing"), "{warnings:?}");
    assert!(catalog.lookup("vanishing").is_none());
}
