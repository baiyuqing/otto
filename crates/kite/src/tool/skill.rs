//! The `skill` tool.
//!
//! The tool answers with a skill's Markdown body, headed by its name, location
//! and file listing, or with one file from inside the same skill directory.
//! Discovery, frontmatter parsing and the prompt section live in
//! [`crate::skill`]; this module only renders them for the model.
//!
//! Security: a sibling file is read through a [`Workspace`] rooted at the skill
//! directory, so it obeys the same canonical-path and symlink rules as the file
//! tools and a path that escapes the skill directory is rejected.

use std::io::Read;
use std::path::Path;

use kite_core::model::ToolDefinition;
use kite_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::read::MAX_READ_FILE_BYTES;
use super::result::capped_text_result;
use super::result::decode_strict_json;
use super::workspace::Workspace;
use super::{CONTEXT_CANCELED, Tool, definition, error_result};
use crate::skill::{Catalog, Skill, list_files, load, too_large};

/// The number of skill files named in a loaded skill's header.
const MAX_SKILL_FILE_LISTING: usize = 50;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillArgs {
    #[serde(default)]
    name: String,
    #[serde(default)]
    file: String,
}

/// Loads a skill's instructions, or one file inside its directory.
pub struct SkillTool {
    catalog: Catalog,
    max_output_bytes: usize,
}

impl SkillTool {
    pub fn new(catalog: Catalog, max_output_bytes: usize) -> Self {
        Self {
            catalog,
            max_output_bytes,
        }
    }

    fn load_skill(&self, skill: &Skill) -> ToolResult {
        let body = match load(skill) {
            Ok(body) => body,
            Err(error) => return error_result(error),
        };
        let (files, total) = match list_files(&skill.directory, MAX_SKILL_FILE_LISTING) {
            Ok(listing) => listing,
            Err(error) => return error_result(error),
        };
        let content = format!(
            "skill: {}\nlocation: {}\nfiles: {}\n\n{body}",
            skill.name,
            skill.directory.display(),
            format_skill_file_listing(&files, total),
        );
        capped_text_result(&content, self.max_output_bytes)
    }

    fn read_skill_file(&self, skill: &Skill, file: &str) -> ToolResult {
        let describe = |error: String| error_result(format!("skill {}: {error}", skill.name));
        let workspace = match Workspace::new(&skill.directory) {
            Ok(workspace) => workspace,
            Err(error) => return describe(error.to_string()),
        };
        let mut opened = match workspace.open(Path::new(file)) {
            Ok(opened) => opened,
            Err(error) => return describe(error.to_string()),
        };
        let metadata = match opened.metadata() {
            Ok(metadata) => metadata,
            Err(error) => return describe(error.to_string()),
        };
        if !metadata.is_file() {
            return error_result(format!("not a regular file: {file}"));
        }
        if metadata.len() > MAX_READ_FILE_BYTES {
            return error_result(too_large(metadata.len(), MAX_READ_FILE_BYTES));
        }
        let mut data = Vec::new();
        if let Err(error) = (&mut opened)
            .take(MAX_READ_FILE_BYTES + 1)
            .read_to_end(&mut data)
        {
            return describe(error.to_string());
        }
        if data.len() as u64 > MAX_READ_FILE_BYTES {
            return error_result(too_large(data.len() as u64, MAX_READ_FILE_BYTES));
        }
        capped_text_result(&String::from_utf8_lossy(&data), self.max_output_bytes)
    }
}

fn format_skill_file_listing(files: &[String], total: usize) -> String {
    if total == 0 {
        return "none".to_string();
    }
    let mut listing = files.join(", ");
    if total > files.len() {
        listing.push_str(&format!(", ... ({total} files)"));
    }
    listing
}

/// The schema advertised for `skill`.
pub fn skill_definition() -> ToolDefinition {
    definition(
        "skill",
        "Load a skill's instructions by name, or read a file inside that skill's directory. Call it before starting a task that matches a listed skill.",
        json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Skill name from the available_skills listing"
                },
                "file": {
                    "type": "string",
                    "description": "Optional relative path of a file inside the skill directory"
                }
            },
            "required": ["name"]
        }),
    )
}

#[async_trait::async_trait]
impl Tool for SkillTool {
    fn definition(&self) -> ToolDefinition {
        skill_definition()
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        let args: SkillArgs = match decode_strict_json(arguments.get(), &["name"]) {
            Ok(args) => args,
            Err(message) => return error_result(message),
        };
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let Some(skill) = self.catalog.lookup(&args.name) else {
            return error_result(format!("unknown skill: {}", args.name));
        };
        if args.file.is_empty() {
            self.load_skill(skill)
        } else {
            self.read_skill_file(skill, &args.file)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::{MAX_OUTPUT_BYTES, run, run_cancelled};
    use std::path::PathBuf;

    /// Writes a minimal valid skill directory and returns its path.
    fn write_test_skill(root: &Path, name: &str, body: &str) -> PathBuf {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).unwrap();
        let content = format!("---\nname: {name}\ndescription: desc for {name}\n---\n{body}");
        std::fs::write(directory.join("SKILL.md"), content).unwrap();
        directory
    }

    fn catalog(root: &Path) -> Catalog {
        let (catalog, warnings) = Catalog::discover(&[root.to_path_buf()]);
        assert!(warnings.is_empty(), "unexpected warnings: {warnings:?}");
        catalog
    }

    #[test]
    fn the_definition_requires_a_name() {
        let definition = SkillTool::new(Catalog::default(), MAX_OUTPUT_BYTES).definition();
        assert_eq!(definition.name, "skill");
        let parameters: serde_json::Value =
            serde_json::from_str(definition.parameters.as_ref().unwrap().get()).unwrap();
        assert_eq!(parameters["required"], serde_json::json!(["name"]));
    }

    #[tokio::test]
    async fn loading_a_skill_prepends_a_header_and_a_file_list() {
        let root = tempfile::tempdir().unwrap();
        let directory = write_test_skill(root.path(), "pdf", "# PDF handling\nBody text\n");
        std::fs::create_dir_all(directory.join("scripts")).unwrap();
        std::fs::write(directory.join("scripts/extract.py"), "x").unwrap();
        std::fs::write(directory.join("references.md"), "x").unwrap();

        let tool = SkillTool::new(catalog(root.path()), MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"name":"pdf"}"#).await;
        assert!(!result.is_error, "{result:?}");
        assert!(result.content.contains("skill: pdf\n"), "{result:?}");
        assert!(
            result
                .content
                .contains(&format!("location: {}\n", directory.display())),
            "{result:?}"
        );
        assert!(
            result
                .content
                .contains("files: references.md, scripts/extract.py\n"),
            "{result:?}"
        );
        assert!(
            result.content.contains("# PDF handling\nBody text\n"),
            "{result:?}"
        );
        assert!(
            !result.content.contains("---"),
            "frontmatter leaked: {result:?}"
        );
        assert!(
            result.persisted_content.is_none(),
            "the body must persist through content: {result:?}"
        );
    }

    #[tokio::test]
    async fn a_skill_without_extra_files_lists_none() {
        let root = tempfile::tempdir().unwrap();
        write_test_skill(root.path(), "empty", "body\n");
        let tool = SkillTool::new(catalog(root.path()), MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"name":"empty"}"#).await;
        assert!(
            !result.is_error && result.content.contains("files: none\n"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn more_files_than_the_listing_limit_report_the_total() {
        let root = tempfile::tempdir().unwrap();
        let directory = write_test_skill(root.path(), "many", "body\n");
        for index in 0..55 {
            std::fs::write(directory.join(format!("f{index:02}.txt")), "x").unwrap();
        }
        let tool = SkillTool::new(catalog(root.path()), MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"name":"many"}"#).await;
        assert!(
            !result.is_error && result.content.contains("... (55 files)"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn unknown_names_missing_names_and_unknown_fields_are_rejected() {
        let root = tempfile::tempdir().unwrap();
        write_test_skill(root.path(), "pdf", "body\n");
        let tool = SkillTool::new(catalog(root.path()), MAX_OUTPUT_BYTES);

        let unknown = run(&tool, r#"{"name":"nope"}"#).await;
        assert!(
            unknown.is_error && unknown.content.contains("unknown skill: nope"),
            "{unknown:?}"
        );
        let missing = run(&SkillTool::new(Catalog::default(), MAX_OUTPUT_BYTES), "{}").await;
        assert!(
            missing.is_error && missing.content.contains("name"),
            "{missing:?}"
        );
        let extra = run(&tool, r#"{"name":"pdf","extra":true}"#).await;
        assert!(
            extra.is_error && extra.content.contains("unknown field"),
            "{extra:?}"
        );
    }

    #[tokio::test]
    async fn an_oversized_body_is_truncated() {
        let root = tempfile::tempdir().unwrap();
        write_test_skill(root.path(), "pdf", "abcdefghijklmnop\n");
        let result = run(
            &SkillTool::new(catalog(root.path()), 10),
            r#"{"name":"pdf"}"#,
        )
        .await;
        assert!(
            !result.is_error && result.content.contains("[truncated:"),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn a_file_in_a_subdirectory_is_readable() {
        let root = tempfile::tempdir().unwrap();
        let directory = write_test_skill(root.path(), "pdf", "body\n");
        std::fs::create_dir_all(directory.join("references")).unwrap();
        std::fs::write(directory.join("references/api.md"), "api docs\n").unwrap();
        let tool = SkillTool::new(catalog(root.path()), MAX_OUTPUT_BYTES);
        let result = run(&tool, r#"{"name":"pdf","file":"references/api.md"}"#).await;
        assert!(
            !result.is_error && result.content == "api docs\n",
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn every_escape_out_of_the_skill_directory_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let directory = write_test_skill(root.path(), "pdf", "body\n");
        std::fs::write(root.path().join("escape.txt"), "nope").unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let outside = elsewhere.path().join("outside.txt");
        std::fs::write(&outside, "nope").unwrap();
        std::os::unix::fs::symlink(&outside, directory.join("link.txt")).unwrap();
        let tool = SkillTool::new(catalog(root.path()), MAX_OUTPUT_BYTES);

        for file in [
            "../escape.txt".to_string(),
            outside.to_string_lossy().into_owned(),
            "link.txt".to_string(),
        ] {
            let arguments = serde_json::json!({"name": "pdf", "file": file}).to_string();
            let result = run(&tool, &arguments).await;
            assert!(
                result.is_error && result.content.contains("escapes workspace"),
                "skill file {file}: {result:?}"
            );
        }

        std::fs::create_dir_all(directory.join("scripts")).unwrap();
        let a_directory = run(&tool, r#"{"name":"pdf","file":"scripts"}"#).await;
        assert!(
            a_directory.is_error && a_directory.content.contains("not a regular file"),
            "{a_directory:?}"
        );
    }

    #[tokio::test]
    async fn a_cancelled_token_is_reported() {
        let root = tempfile::tempdir().unwrap();
        write_test_skill(root.path(), "pdf", "body\n");
        let tool = SkillTool::new(catalog(root.path()), MAX_OUTPUT_BYTES);
        let result = run_cancelled(&tool, r#"{"name":"pdf"}"#).await;
        assert!(result.is_error, "{result:?}");
    }
}
