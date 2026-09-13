//! The `skill` tool. Port of `internal/tool/skill.go` plus the minimal slice
//! of `internal/skill` that the tool depends on.
//!
//! A skill is a directory holding a `SKILL.md` file with YAML frontmatter.
//! The tool loads that file's Markdown body, or reads another file inside the
//! same directory through a [`Workspace`] rooted at it, so every skill-file
//! read obeys the same canonical-path and symlink rules as the file tools.
//!
//! Scope: this module carries only the loader the tool needs. Skill prompt
//! rendering, `allowed-tools`, and the full frontmatter grammar stay with the
//! `internal/skill` port in a later phase; see [`parse_frontmatter`] for the
//! constructs that are rejected rather than guessed at here.

use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::read::MAX_READ_FILE_BYTES;
use super::result::{capped_text_result, decode_strict_json};
use super::root::{self, Root};
use super::workspace::Workspace;
use super::{CONTEXT_CANCELED, Tool, definition, error_result};

/// The number of skill files named in a loaded skill's header.
const MAX_SKILL_FILE_LISTING: usize = 50;
/// The largest `SKILL.md` (and sibling file) this loader will read.
const MAX_SKILL_FILE_BYTES: u64 = 64 << 20;
const MAX_SKILL_NAME_LENGTH: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 1024;

/// One discovered skill.
///
/// `name` is the frontmatter name, which must equal the directory base name.
/// `directory` is absolute; `path` is `directory/SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub directory: PathBuf,
    pub path: PathBuf,
}

/// The set of skills discovered for one runner, sorted by name.
///
/// The catalog owns plain data and is cheap to clone. It performs no I/O after
/// [`Catalog::discover`] returns, so it is safe to share across tasks.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    skills: Vec<Skill>,
}

impl Catalog {
    /// Scans `roots` in order; a later root overrides an earlier one on the
    /// same name. Roots are absolute directories. A missing root is skipped
    /// silently. An unreadable root, or a directory whose `SKILL.md` fails to
    /// parse or validate, produces one warning string and is skipped.
    /// Discovery never fails.
    pub fn discover(roots: &[PathBuf]) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut by_name: std::collections::BTreeMap<String, Skill> =
            std::collections::BTreeMap::new();

        for root_path in roots {
            let root_fs = match Root::open(root_path) {
                Ok(root_fs) => root_fs,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(format!("skills root {}: {error}", root_path.display()));
                    continue;
                }
            };
            let canonical_root = match std::fs::canonicalize(root_path) {
                Ok(canonical) => canonical,
                Err(error) => {
                    warnings.push(format!("skills root {}: {error}", root_path.display()));
                    continue;
                }
            };
            let entries = match root_fs.read_dir(Path::new(".")) {
                Ok(entries) => entries,
                Err(error) => {
                    warnings.push(format!("skills root {}: {error}", root_path.display()));
                    continue;
                }
            };
            for entry in entries {
                let directory = root_path.join(&entry.name);
                let Some(directory_fs) = open_skill_dir(&directory, &canonical_root, &entry) else {
                    continue;
                };
                let skill_path = directory.join("SKILL.md");
                match directory_fs.stat(Path::new("SKILL.md")) {
                    Ok(stat) if root::is_regular(&stat) => {}
                    Ok(_) => continue,
                    // A directory without SKILL.md is silently ignored.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        warnings.push(format!("skill {}: {error}", skill_path.display()));
                        continue;
                    }
                }
                match load_candidate(
                    &directory,
                    &skill_path,
                    &entry.name.to_string_lossy(),
                    &directory_fs,
                ) {
                    Ok(skill) => {
                        by_name.insert(skill.name.clone(), skill);
                    }
                    Err(warning) => warnings.push(warning),
                }
            }
        }

        (
            Self {
                skills: by_name.into_values().collect(),
            },
            warnings,
        )
    }

    /// The catalog's skills, sorted by name.
    pub fn skills(&self) -> &[Skill] {
        &self.skills
    }

    /// The skill with this name, if any.
    pub fn lookup(&self, name: &str) -> Option<&Skill> {
        self.skills.iter().find(|skill| skill.name == name)
    }

    /// The number of skills in the catalog.
    pub fn len(&self) -> usize {
        self.skills.len()
    }

    /// Whether the catalog holds no skills.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

/// Opens a candidate skill directory. A plain directory becomes its own root;
/// a symbolic link is resolved first and reopened from its canonical target,
/// so the link's destination becomes the root. Anything else is not a skill
/// directory. `directory` is `<skills root>/<entry name>`.
fn open_skill_dir(directory: &Path, canonical_root: &Path, entry: &root::DirEntry) -> Option<Root> {
    if entry.is_dir {
        return Root::open(directory).ok();
    }
    if !entry.is_symlink {
        return None;
    }
    let target = std::fs::canonicalize(canonical_root.join(&entry.name)).ok()?;
    let directory_fs = Root::open(&target).ok()?;
    let stat = directory_fs.stat(Path::new(".")).ok()?;
    root::is_dir(&stat).then_some(directory_fs)
}

/// Parses and validates one skill directory, returning the warning string Go
/// would have produced when the directory is not a usable skill.
fn load_candidate(
    directory: &Path,
    skill_path: &Path,
    directory_name: &str,
    directory_fs: &Root,
) -> Result<Skill, String> {
    let describe = |error: String| format!("skill {}: {error}", skill_path.display());
    let data = read_root_file(directory_fs, Path::new("SKILL.md"))
        .map_err(|error| describe(error.to_string()))?;
    let (fields, _) = parse_frontmatter(&data).map_err(describe)?;
    let name = validate_skill_name(&fields, directory_name).map_err(describe)?;
    let description = validate_skill_description(&fields).map_err(describe)?;
    Ok(Skill {
        name,
        description,
        directory: directory.to_path_buf(),
        path: skill_path.to_path_buf(),
    })
}

fn validate_skill_name(
    fields: &std::collections::BTreeMap<String, String>,
    directory_name: &str,
) -> Result<String, String> {
    let raw = fields.get("name").map_or("", String::as_str);
    if raw.is_empty() {
        return Err("missing name".to_string());
    }
    if raw.len() > MAX_SKILL_NAME_LENGTH || !is_valid_skill_name(raw) {
        return Err(format!("name {raw:?} is invalid"));
    }
    if raw != directory_name {
        return Err(format!(
            "name {raw:?} does not match directory {directory_name:?}"
        ));
    }
    Ok(raw.to_string())
}

/// Go's `^[a-z0-9]+(-[a-z0-9]+)*$`, spelled out to avoid a regex for one rule.
fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn validate_skill_description(
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<String, String> {
    let trimmed = fields.get("description").map_or("", String::as_str).trim();
    if trimmed.is_empty() {
        return Err("missing description".to_string());
    }
    if trimmed.chars().count() > MAX_SKILL_DESCRIPTION_CHARS {
        return Err(format!(
            "description exceeds {MAX_SKILL_DESCRIPTION_CHARS} characters"
        ));
    }
    Ok(trimmed.to_string())
}

/// Splits `data` into its frontmatter fields and the Markdown body.
///
/// This is the minimal subset the tool needs: `---` delimiters, `key: value`
/// lines with plain, single-quoted or double-quoted scalars, blank lines and
/// `#` comments. Block scalars (`|`, `>`), nested blocks and multi-line plain
/// continuations are reported as `unsupported frontmatter line N` rather than
/// guessed at; porting them belongs with `internal/skill/frontmatter.go`.
pub fn parse_frontmatter(
    data: &[u8],
) -> Result<(std::collections::BTreeMap<String, String>, String), String> {
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.is_empty() || !is_frontmatter_delimiter(lines[0]) {
        return Err("missing frontmatter".to_string());
    }
    let end = lines[1..]
        .iter()
        .position(|line| is_frontmatter_delimiter(line))
        .map(|offset| offset + 1)
        .ok_or_else(|| "unterminated frontmatter".to_string())?;

    let fields = parse_frontmatter_fields(&lines[1..end])?;
    let body = lines[end + 1..].join("\n");
    let body = body.strip_prefix('\n').unwrap_or(&body).to_string();
    Ok((fields, body))
}

fn is_frontmatter_delimiter(line: &str) -> bool {
    line.strip_suffix('\r').unwrap_or(line) == "---"
}

/// Parses the lines strictly between the delimiters. `lines[i]` is file line
/// `i + 2`, 1-based, which is what the error messages report.
fn parse_frontmatter_fields(
    lines: &[&str],
) -> Result<std::collections::BTreeMap<String, String>, String> {
    let mut fields = std::collections::BTreeMap::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        let line_number = index + 2;
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if line.starts_with([' ', '\t']) {
            return Err(format!("unsupported frontmatter line {line_number}"));
        }
        let (key, rest) = split_frontmatter_key(line)
            .ok_or_else(|| format!("unsupported frontmatter line {line_number}"))?;
        let value = parse_frontmatter_value(rest, line_number)?;
        fields.insert(key.to_string(), value);
    }
    Ok(fields)
}

/// Splits a `key: value` line into its key and the remainder after the
/// separator, or `None` when the line does not match the key grammar.
fn split_frontmatter_key(line: &str) -> Option<(&str, &str)> {
    let separator = line.find(':')?;
    let key = &line[..separator];
    if key.is_empty()
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return None;
    }
    let after = &line[separator + 1..];
    if !after.is_empty() && !after.starts_with(' ') {
        return None;
    }
    Some((key, after.strip_prefix(' ').unwrap_or(after)))
}

fn parse_frontmatter_value(rest: &str, line_number: usize) -> Result<String, String> {
    let unsupported = || format!("unsupported frontmatter line {line_number}");
    match rest.as_bytes().first() {
        None => Err(unsupported()),
        Some(b'|' | b'>') => Err(unsupported()),
        Some(b'"') => parse_double_quoted(rest)
            .map_err(|error| format!("frontmatter line {line_number}: {error}")),
        Some(b'\'') => parse_single_quoted(rest)
            .map_err(|error| format!("frontmatter line {line_number}: {error}")),
        Some(_) => Ok(rest.trim().to_string()),
    }
}

fn parse_double_quoted(value: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut characters = value.chars().skip(1);
    while let Some(character) = characters.next() {
        match character {
            '"' => return Ok(out),
            '\\' => match characters.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(escaped) => out.push(escaped),
                None => out.push('\\'),
            },
            _ => out.push(character),
        }
    }
    Err("unterminated double-quoted value".to_string())
}

fn parse_single_quoted(value: &str) -> Result<String, String> {
    let mut out = String::new();
    let mut characters = value.chars().skip(1).peekable();
    while let Some(character) = characters.next() {
        if character != '\'' {
            out.push(character);
            continue;
        }
        if characters.peek() == Some(&'\'') {
            characters.next();
            out.push('\'');
            continue;
        }
        return Ok(out);
    }
    Err("unterminated single-quoted value".to_string())
}

/// Reads `skill.path` and returns the Markdown body without the frontmatter.
pub fn load(skill: &Skill) -> Result<String, String> {
    let name = skill.path.strip_prefix(&skill.directory).map_err(|_| {
        format!(
            "{} is not inside {}",
            skill.path.display(),
            skill.directory.display()
        )
    })?;
    let root_fs = Root::open(&skill.directory).map_err(|error| error.to_string())?;
    let data = read_root_file(&root_fs, name).map_err(|error| error.to_string())?;
    let (_, body) = parse_frontmatter(&data)?;
    Ok(body)
}

/// Relative slash-separated paths of the regular files under `directory`,
/// recursively: `SKILL.md` at the top level is excluded, hidden names are
/// skipped, symbolic links are neither listed nor descended, the result is
/// sorted and truncated to `limit`. `total` is the count before truncation.
pub fn list_files(directory: &Path, limit: usize) -> io::Result<(Vec<String>, usize)> {
    let root_fs = Root::open(directory)?;
    let mut files = Vec::new();
    collect_skill_files(&root_fs, "", &mut files)?;
    files.sort();
    let total = files.len();
    if limit > 0 && total > limit {
        files.truncate(limit);
    }
    Ok((files, total))
}

fn collect_skill_files(root_fs: &Root, prefix: &str, files: &mut Vec<String>) -> io::Result<()> {
    let directory = if prefix.is_empty() { "." } else { prefix };
    for entry in root_fs.read_dir(Path::new(directory))? {
        let name = entry.name.to_string_lossy();
        if name.starts_with('.') || entry.is_symlink {
            continue;
        }
        let relative = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.is_dir {
            collect_skill_files(root_fs, &relative, files)?;
            continue;
        }
        if entry.is_regular && relative != "SKILL.md" {
            files.push(relative);
        }
    }
    Ok(())
}

/// Reads one regular file below `root_fs`, bounded by [`MAX_SKILL_FILE_BYTES`].
fn read_root_file(root_fs: &Root, name: &Path) -> io::Result<Vec<u8>> {
    let file = root_fs.open_file(
        name,
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_NONBLOCK,
        nix::sys::stat::Mode::empty(),
    )?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other(format!(
            "not a regular file: {}",
            name.display()
        )));
    }
    if metadata.len() > MAX_SKILL_FILE_BYTES {
        return Err(io::Error::other(too_large(
            metadata.len(),
            MAX_SKILL_FILE_BYTES,
        )));
    }
    let mut data = Vec::new();
    file.take(MAX_SKILL_FILE_BYTES + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_SKILL_FILE_BYTES {
        return Err(io::Error::other(too_large(
            data.len() as u64,
            MAX_SKILL_FILE_BYTES,
        )));
    }
    Ok(data)
}

fn too_large(size: u64, maximum: u64) -> String {
    format!("file is too large ({size} bytes); maximum readable size is {maximum} bytes")
}

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

    #[test]
    fn frontmatter_splits_fields_from_the_body() {
        let (fields, body) =
            parse_frontmatter(b"---\nname: pdf\ndescription: \"a desc\"\n---\nbody\n").unwrap();
        assert_eq!(fields["name"], "pdf");
        assert_eq!(fields["description"], "a desc");
        assert_eq!(body, "body\n");

        assert_eq!(
            parse_frontmatter(b"body\n").unwrap_err(),
            "missing frontmatter"
        );
        assert_eq!(
            parse_frontmatter(b"---\nname: pdf\n").unwrap_err(),
            "unterminated frontmatter"
        );
        // Block scalars are deferred with the rest of internal/skill.
        assert_eq!(
            parse_frontmatter(b"---\nname: |\n  pdf\n---\n").unwrap_err(),
            "unsupported frontmatter line 2"
        );
    }
}
