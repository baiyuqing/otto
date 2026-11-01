use super::*;
use crate::fixtures;
use std::path::{Path, PathBuf};

/// Writes `<root>/<directory>/<file>` with the standard frontmatter shape
/// and returns the definition directory.
fn write_definition(
    root: &Path,
    file: &str,
    directory: &str,
    name: &str,
    description: &str,
    body: &str,
) -> PathBuf {
    let directory = root.join(directory);
    std::fs::create_dir_all(&directory).expect("definition directory");
    let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}");
    std::fs::write(directory.join(file), content).expect("definition file");
    directory
}

/// A builder over two canonical temporary directories, with `HOME` set so
/// the default `~/.otto/...` roots resolve under the first one.
struct Fixture {
    _home: tempfile::TempDir,
    _workspace: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
    builder: Builder,
}

fn fixture() -> Fixture {
    let home_dir = tempfile::tempdir().expect("home");
    let workspace_dir = tempfile::tempdir().expect("workspace");
    let home = std::fs::canonicalize(home_dir.path()).expect("canonical home");
    let workspace = std::fs::canonicalize(workspace_dir.path()).expect("canonical workspace");
    let mut builder = fixtures::builder(&workspace, workspace_dir.path());
    builder
        .shared_mut()
        .environment
        .insert("HOME".to_string(), home.to_string_lossy().into_owned());
    Fixture {
        _home: home_dir,
        _workspace: workspace_dir,
        home,
        workspace,
        builder,
    }
}

/// `build_catalogs` with its two output channels made inspectable.
fn catalogs(builder: &Builder) -> (CatalogWiring, Vec<String>, String) {
    let mut tools: Vec<Box<dyn Tool + Send + Sync>> = Vec::new();
    let mut stderr = Vec::new();
    let wiring = builder
        .build_catalogs(&mut tools, 65536, &mut stderr)
        .expect("build_catalogs");
    let names = tools
        .iter()
        .map(|tool| tool.definition().name.clone())
        .collect();
    (
        wiring,
        names,
        String::from_utf8(stderr).expect("utf-8 warnings"),
    )
}

fn definition_names(definitions: &[ToolDefinition]) -> Vec<String> {
    definitions
        .iter()
        .map(|definition| definition.name.clone())
        .collect()
}

#[test]
fn a_user_level_skill_is_listed_and_registers_the_skill_tool() {
    let fixture = fixture();
    let directory = write_definition(
        &fixture.home.join(".otto/skills"),
        "SKILL.md",
        "pdf",
        "pdf",
        "Extract text and tables from PDF files.",
        "# PDF handling\nExtract pdfs from files.\n",
    );

    let (wiring, tools, warnings) = catalogs(&fixture.builder);
    assert!(
        wiring.skill_section.contains("\n<skills>\n"),
        "{:?}",
        wiring.skill_section
    );
    assert!(
        wiring.skill_section.contains(&format!(
            "<skill name=\"pdf\" location=\"{}\"",
            directory.display()
        )),
        "{:?}",
        wiring.skill_section
    );
    assert_eq!(tools, vec!["skill".to_string()]);
    assert_eq!(warnings, "");
}

#[test]
fn a_workspace_skill_overrides_a_user_level_skill() {
    let fixture = fixture();
    let user = write_definition(
        &fixture.home.join(".otto/skills"),
        "SKILL.md",
        "repo-notes",
        "repo-notes",
        "User-level notes skill.",
        "user body\n",
    );
    let workspace = write_definition(
        &fixture.workspace.join(".otto/skills"),
        "SKILL.md",
        "repo-notes",
        "repo-notes",
        "Workspace-level notes skill.",
        "workspace body\n",
    );

    let (wiring, _, _) = catalogs(&fixture.builder);
    assert!(
        wiring.skill_section.contains(&format!(
            "<skill name=\"repo-notes\" location=\"{}\"",
            workspace.display()
        )),
        "{:?}",
        wiring.skill_section
    );
    assert!(
        !wiring.skill_section.contains(&user.display().to_string()),
        "the overridden user skill must not be listed: {:?}",
        wiring.skill_section
    );
}

#[test]
fn no_skills_omit_the_section_and_the_tool() {
    let fixture = fixture();
    let (wiring, tools, warnings) = catalogs(&fixture.builder);
    assert_eq!(wiring.skill_section, "");
    assert!(tools.is_empty(), "{tools:?}");
    assert_eq!(warnings, "");
}

#[test]
fn disabled_skills_omit_the_section_the_tool_and_the_boundary_definition() {
    let mut fixture = fixture();
    write_definition(
        &fixture.home.join(".otto/skills"),
        "SKILL.md",
        "pdf",
        "pdf",
        "Extract pdfs.",
        "body\n",
    );
    fixture.builder.shared_mut().config.skills.enabled = Some(false);

    let (wiring, tools, _) = catalogs(&fixture.builder);
    assert_eq!(wiring.skill_section, "");
    assert!(wiring.discovered_skills.lookup("pdf").is_some());
    assert!(wiring.skills.is_empty());
    assert!(!tools.contains(&"skill".to_string()), "{tools:?}");
    let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
    assert!(!names.contains(&"skill".to_string()), "{names:?}");
}

#[test]
fn an_invalid_skill_warns_and_is_skipped_while_the_valid_one_is_listed() {
    let fixture = fixture();
    let root = fixture.home.join(".otto/skills");
    let pdf = write_definition(&root, "SKILL.md", "pdf", "pdf", "Extract pdfs.", "body\n");
    write_definition(
        &root,
        "SKILL.md",
        "bad",
        "other",
        "Mismatched name.",
        "body\n",
    );

    let (wiring, _, warnings) = catalogs(&fixture.builder);
    assert!(warnings.starts_with("warning: skill "), "{warnings:?}");
    assert!(
        warnings.contains("does not match directory \"bad\""),
        "{warnings:?}"
    );
    assert!(
        wiring.skill_section.contains(&format!(
            "<skill name=\"pdf\" location=\"{}\"",
            pdf.display()
        )),
        "{:?}",
        wiring.skill_section
    );
}

#[test]
fn a_workspace_agent_is_listed_in_the_agents_section() {
    let fixture = fixture();
    write_definition(
        &fixture.workspace.join(".otto/agents"),
        "AGENT.md",
        "reviewer",
        "reviewer",
        "Reviews code for style and correctness.",
        "# Reviewer\nFocus on style.\n",
    );

    let (wiring, _, warnings) = catalogs(&fixture.builder);
    assert!(wiring.agents.enabled);
    assert!(
        wiring.agent_section.contains("\n<agents>\n"),
        "{:?}",
        wiring.agent_section
    );
    assert!(
        wiring.agent_section.contains("<agent name=\"reviewer\">"),
        "{:?}",
        wiring.agent_section
    );
    assert_eq!(warnings, "");
    let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
    for want in [
        "agent",
        "agent_status",
        "agent_send",
        "agent_cancel",
        "remind",
        "remind_status",
        "remind_cancel",
    ] {
        assert!(
            names.contains(&want.to_string()),
            "{names:?} is missing {want}"
        );
    }
}

#[test]
fn disabled_agents_omit_the_section_and_the_boundary_definitions() {
    let mut fixture = fixture();
    write_definition(
        &fixture.workspace.join(".otto/agents"),
        "AGENT.md",
        "reviewer",
        "reviewer",
        "Reviews code for style and correctness.",
        "body\n",
    );
    fixture.builder.shared_mut().config.agents.enabled = Some(false);

    let (wiring, _, _) = catalogs(&fixture.builder);
    assert!(!wiring.agents.enabled);
    assert_eq!(wiring.agent_section, "");
    let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, true));
    for unwanted in [
        "agent",
        "agent_status",
        "agent_send",
        "agent_cancel",
        "remind",
        "remind_status",
        "remind_cancel",
    ] {
        assert!(
            !names.contains(&unwanted.to_string()),
            "{names:?} still lists {unwanted}"
        );
    }
}

/// The caller passes the dynamic-content decision directly rather than
/// reaching it through a provider secret that exhausts the redaction
/// marker.
#[test]
fn a_closed_redaction_boundary_drops_the_agent_definitions() {
    let fixture = fixture();
    let names = definition_names(&fixture.builder.boundary_catalog_definitions(65536, false));
    assert_eq!(names, vec!["skill".to_string()]);
}

#[test]
fn an_out_of_range_max_parallel_fails_the_build() {
    let mut fixture = fixture();
    fixture.builder.shared_mut().config.agents.max_parallel = Some(17);
    let mut tools: Vec<Box<dyn Tool + Send + Sync>> = Vec::new();
    let error = match fixture
        .builder
        .build_catalogs(&mut tools, 65536, &mut Vec::new())
    {
        Err(error) => error,
        Ok(_) => panic!("max_parallel = 17 must fail the build"),
    };
    assert_eq!(
        error,
        "[agents].max_parallel must be between 1 and 16, got 17"
    );
}

/// Skill and agent sandbox read paths, folded into one table because both
/// roots travel the same code path.
#[test]
fn sandbox_read_paths_pick_up_only_existing_enabled_roots() {
    for (name, make_directories, disabled, want) in [
        ("both roots exist", true, false, true),
        ("roots absent", false, false, false),
        ("disabled with roots present", true, true, false),
    ] {
        let fixture = fixture();
        let skills = fixture.workspace.join(".otto/skills");
        let agents = fixture.workspace.join(".otto/agents");
        if make_directories {
            std::fs::create_dir_all(&skills).expect("skills root");
            std::fs::create_dir_all(&agents).expect("agents root");
        }
        let mut config = fixture.builder.config.clone();
        if disabled {
            config.skills.enabled = Some(false);
            config.agents.enabled = Some(false);
        }
        let settings = crate::runtime::startup::resolve_sandbox_settings(
            &config,
            &fixture.builder.environment,
            &fixture.workspace.to_string_lossy(),
            None,
        )
        .expect("sandbox settings");
        for root in [&skills, &agents] {
            let root = root.to_string_lossy().into_owned();
            assert_eq!(
                settings.read_paths.contains(&root),
                want,
                "{name}: {:?} against {root}",
                settings.read_paths
            );
        }
    }
}
