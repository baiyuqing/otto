//! `AGENT.md` discovery.
//!
//! A named sub-agent is a directory holding an `AGENT.md` file with the same
//! YAML frontmatter dialect skills use. Definitions are discovered under
//! `~/.otto/agents` and `<workspace>/.otto/agents`; the workspace root wins on
//! a name collision.
//!
//! Ownership: a [`Catalog`] owns plain data and performs no I/O after
//! [`Catalog::discover`] returns. Concurrency: nothing here holds shared
//! mutable state.
//!
//! Security: every read goes through `crate::fsops::root::Root`, so an
//! `AGENT.md` symbolic link pointing outside its definition directory is
//! skipped rather than followed. A definition is untrusted text; its `tools`
//! list can only narrow the child tool set the runner already built, never
//! widen it.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::fsops::root::{self, Root};
use crate::skill::frontmatter::{self, Fields};

/// The largest `AGENT.md` this loader will read.
pub(crate) const MAX_AGENT_FILE_BYTES: u64 = 64 << 20;
const MAX_AGENT_NAME_LENGTH: usize = 64;
const MAX_AGENT_DESCRIPTION_CHARS: usize = 1024;

/// One discovered named sub-agent definition.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Definition {
    /// The frontmatter name, equal to the directory base name.
    pub name: String,
    /// The frontmatter description, trimmed.
    pub description: String,
    /// The frontmatter tools allowlist, or `None` when the frontmatter has no
    /// `tools` key, which means "every child tool".
    pub tools: Option<Vec<String>>,
    /// The frontmatter model id, or empty to use the caller's or session model.
    pub model: String,
    /// `"fresh"` (the default) or `"inherit"`.
    pub context: String,
    /// Workflow write coordination policy. The default is `single_writer` for
    /// backward compatibility; `read_only` and `propose_only` cannot use
    /// workspace mutation tools.
    pub write_policy: WritePolicy,
    /// Workspace-relative write ownership globs for `owned_paths` agents.
    pub write_paths: Vec<String>,
    /// The Markdown after the frontmatter, trimmed. May be empty.
    pub body: String,
    /// The absolute directory holding `AGENT.md`.
    pub directory: PathBuf,
    /// The absolute path of `AGENT.md`.
    pub path: PathBuf,
    /// Set only by `from_skill`: this definition came from a skill's
    /// `SKILL.md`, not a real `AGENT.md`. The automatic contract check
    /// (`crate::skill::check`) triggers only for these.
    pub is_skill_derived: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WritePolicy {
    ReadOnly,
    ProposeOnly,
    #[default]
    SingleWriter,
    OwnedPaths,
}

impl WritePolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::ProposeOnly => "propose_only",
            Self::SingleWriter => "single_writer",
            Self::OwnedPaths => "owned_paths",
        }
    }
}

impl std::str::FromStr for WritePolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "read_only" => Ok(Self::ReadOnly),
            "propose_only" => Ok(Self::ProposeOnly),
            "single_writer" => Ok(Self::SingleWriter),
            "owned_paths" => Ok(Self::OwnedPaths),
            _ => Err(
                r#"write_policy must be "read_only", "propose_only", "single_writer", or "owned_paths""#
                    .to_string(),
            ),
        }
    }
}

/// The set of agent definitions discovered for one runner, sorted by name.
/// The default value is empty and usable.
#[derive(Clone, Debug, Default)]
pub struct Catalog {
    definitions: Vec<Definition>,
}

impl Catalog {
    /// Scans `roots` in order; a later root overrides an earlier one on the
    /// same name. Roots are absolute directories. A missing root is skipped
    /// silently. An unreadable root, or a directory whose `AGENT.md` fails to
    /// parse or validate, produces one warning string and is skipped.
    /// Discovery never fails.
    pub fn discover(roots: &[PathBuf]) -> (Self, Vec<String>) {
        let mut warnings = Vec::new();
        let mut by_name: BTreeMap<String, Definition> = BTreeMap::new();

        for root_path in roots {
            let root_fs = match Root::open(root_path) {
                Ok(root_fs) => root_fs,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    warnings.push(format!("agents root {}: {error}", root_path.display()));
                    continue;
                }
            };
            let entries = match root_fs.read_dir(Path::new(".")) {
                Ok(entries) => entries,
                Err(error) => {
                    warnings.push(format!("agents root {}: {error}", root_path.display()));
                    continue;
                }
            };
            for entry in entries {
                let directory = root_path.join(&entry.name);
                // Opening the candidate as a root follows a symbolic link to a
                // directory and fails on anything else.
                let Ok(directory_fs) = Root::open(&directory) else {
                    continue;
                };
                let agent_path = directory.join("AGENT.md");
                let agent_file_bytes = match read_agent_file(&directory_fs, Path::new("AGENT.md")) {
                    Ok(Some(agent_file_bytes)) => agent_file_bytes,
                    // A directory without a readable regular AGENT.md is
                    // silently ignored, as is one reached by an escaping link.
                    Ok(None) => continue,
                    Err(error) => {
                        warnings.push(format!("{}: {error}", agent_path.display()));
                        continue;
                    }
                };
                match load_candidate(
                    &agent_file_bytes,
                    &directory,
                    &agent_path,
                    &entry.name.to_string_lossy(),
                ) {
                    Ok(definition) => {
                        by_name.insert(definition.name.clone(), definition);
                    }
                    Err(warning) => warnings.push(warning),
                }
            }
        }

        (
            Self {
                definitions: by_name.into_values().collect(),
            },
            warnings,
        )
    }

    /// Adds a sub-agent definition for every skill that declares a contract,
    /// and reports the names that became callable.
    ///
    /// An explicit `AGENT.md` of the same name wins: it is the definition its
    /// author wrote on purpose, and a skill silently replacing it would change
    /// what an existing `agent` call does. The clash is reported rather than
    /// resolved quietly.
    ///
    /// A skill whose body cannot be read is reported and registered nowhere,
    /// so the listing never marks a skill the `agent` tool could not run.
    pub(crate) fn extend_from_skills(
        &mut self,
        skills: &crate::skill::Catalog,
    ) -> (Vec<String>, BTreeSet<String>) {
        let mut warnings = Vec::new();
        let mut registered = BTreeSet::new();
        for skill in skills.skills() {
            if skill.contract.is_none() {
                continue;
            }
            if self
                .definitions
                .iter()
                .any(|definition| definition.name == skill.name)
            {
                warnings.push(format!(
                    "skill {} is not callable as a sub-agent: an agent definition already has that name",
                    skill.name
                ));
                continue;
            }
            let body = match crate::skill::load(skill) {
                Ok(body) => body,
                Err(error) => {
                    warnings.push(format!("skill {}: {error}", skill.name));
                    continue;
                }
            };
            let Some(definition) = from_skill(skill, body) else {
                continue;
            };
            registered.insert(definition.name.clone());
            self.definitions.push(definition);
        }
        self.definitions
            .sort_by(|left, right| left.name.cmp(&right.name));
        (warnings, registered)
    }

    /// The catalog's definitions, sorted by name.
    pub fn definitions(&self) -> &[Definition] {
        &self.definitions
    }

    /// A catalog holding exactly `definitions`. Tests build one directly.
    #[cfg(test)]
    pub(crate) fn from_definitions(definitions: Vec<Definition>) -> Self {
        Self { definitions }
    }

    /// The definition with this name, if any.
    pub fn lookup(&self, name: &str) -> Option<&Definition> {
        self.definitions
            .iter()
            .find(|definition| definition.name == name)
    }

    /// The number of definitions in the catalog.
    pub fn len(&self) -> usize {
        self.definitions.len()
    }

    /// Whether the catalog holds no definitions.
    pub fn is_empty(&self) -> bool {
        self.definitions.is_empty()
    }
}

/// Reads `AGENT.md` below `root_fs`.
///
/// `Ok(None)` means "not a definition": the file is missing, is not regular,
/// or is a symbolic link that leaves the definition directory. `Err` is a
/// reportable failure, such as an oversized file.
fn read_agent_file(root_fs: &Root, name: &Path) -> io::Result<Option<Vec<u8>>> {
    let file = match root_fs.open_file(
        name,
        nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_NONBLOCK,
        nix::sys::stat::Mode::empty(),
    ) {
        Ok(file) => file,
        Err(error) => {
            // An external symbolic link leaves this definition directory and
            // is ignored just like a missing or non-regular AGENT.md.
            if is_symlink(root_fs, name) || error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(error);
        }
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(None);
    }
    if metadata.len() > MAX_AGENT_FILE_BYTES {
        return Err(io::Error::other(format!(
            "file is too large ({} bytes); maximum is {MAX_AGENT_FILE_BYTES} bytes",
            metadata.len()
        )));
    }
    let mut agent_file_bytes = Vec::new();
    file.take(MAX_AGENT_FILE_BYTES + 1)
        .read_to_end(&mut agent_file_bytes)?;
    if agent_file_bytes.len() as u64 > MAX_AGENT_FILE_BYTES {
        return Err(io::Error::other(format!(
            "file is too large; maximum is {MAX_AGENT_FILE_BYTES} bytes"
        )));
    }
    Ok(Some(agent_file_bytes))
}

fn is_symlink(root_fs: &Root, name: &Path) -> bool {
    root_fs.lstat(name).as_ref().is_ok_and(root::is_symlink)
}

/// Parses and validates one agent directory, returning a warning string when it
/// is not a usable definition.
fn load_candidate(
    data: &[u8],
    directory: &Path,
    agent_path: &Path,
    directory_name: &str,
) -> Result<Definition, String> {
    let describe = |error: String| format!("{}: {error}", agent_path.display());
    let (fields, body) = frontmatter::parse(data).map_err(describe)?;
    let name = validate_agent_name(&fields, directory_name).map_err(describe)?;
    let description = validate_agent_description(&fields).map_err(describe)?;
    let tools = parse_agent_tools(&fields).map_err(describe)?;
    let context = validate_agent_context(&fields).map_err(describe)?;
    let write_policy = validate_write_policy(&fields).map_err(describe)?;
    let write_paths = parse_write_paths(&fields, write_policy).map_err(describe)?;
    Ok(Definition {
        name,
        description,
        tools,
        model: fields
            .get("model")
            .map_or("", String::as_str)
            .trim()
            .to_string(),
        context,
        write_policy,
        write_paths,
        body: body.trim().to_string(),
        directory: directory.to_path_buf(),
        path: agent_path.to_path_buf(),
        is_skill_derived: false,
    })
}

fn validate_agent_name(fields: &Fields, directory_name: &str) -> Result<String, String> {
    let raw = fields.get("name").map_or("", String::as_str);
    if raw.is_empty() {
        return Err("missing name".to_string());
    }
    if raw.len() > MAX_AGENT_NAME_LENGTH || !is_valid_agent_name(raw) {
        return Err(format!("name {raw:?} is invalid"));
    }
    if raw != directory_name {
        return Err(format!(
            "name {raw:?} does not match directory {directory_name:?}"
        ));
    }
    Ok(raw.to_string())
}

/// `^[a-z0-9]+(-[a-z0-9]+)*$`, spelled out to avoid a regex for one rule.
fn is_valid_agent_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
}

fn validate_agent_description(fields: &Fields) -> Result<String, String> {
    let trimmed = fields.get("description").map_or("", String::as_str).trim();
    if trimmed.is_empty() {
        return Err("missing description".to_string());
    }
    if trimmed.chars().count() > MAX_AGENT_DESCRIPTION_CHARS {
        return Err(format!(
            "description exceeds {MAX_AGENT_DESCRIPTION_CHARS} characters"
        ));
    }
    Ok(trimmed.to_string())
}

/// Parses the optional `tools` frontmatter key. An absent key means every
/// child tool. A key that yields no item after splitting, or any item that is
/// not `^[a-z0-9_]+$`, is an error.
fn parse_agent_tools(fields: &Fields) -> Result<Option<Vec<String>>, String> {
    let Some(raw) = fields.get("tools") else {
        return Ok(None);
    };
    let mut tools = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if !item
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(format!("tools item {item:?} is invalid"));
        }
        tools.push(item.to_string());
    }
    if tools.is_empty() {
        return Err("tools must be a comma-separated list".to_string());
    }
    Ok(Some(tools))
}

fn validate_agent_context(fields: &Fields) -> Result<String, String> {
    let trimmed = fields.get("context").map_or("", String::as_str).trim();
    if trimmed.is_empty() {
        return Ok("fresh".to_string());
    }
    if trimmed != "fresh" && trimmed != "inherit" {
        return Err(r#"context must be "fresh" or "inherit""#.to_string());
    }
    Ok(trimmed.to_string())
}

fn validate_write_policy(fields: &Fields) -> Result<WritePolicy, String> {
    let trimmed = fields.get("write_policy").map_or("", String::as_str).trim();
    if trimmed.is_empty() {
        return Ok(WritePolicy::SingleWriter);
    }
    trimmed.parse()
}

fn parse_write_paths(fields: &Fields, write_policy: WritePolicy) -> Result<Vec<String>, String> {
    let Some(raw) = fields.get("write_paths") else {
        if write_policy == WritePolicy::OwnedPaths {
            return Err("write_paths is required when write_policy is owned_paths".to_string());
        }
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    for item in raw.split(',') {
        let item = item.trim();
        if item.is_empty() {
            continue;
        }
        if item.starts_with('/')
            || item.contains("..")
            || item.contains('\\')
            || item.bytes().any(|byte| byte == 0)
        {
            return Err(format!("write_paths item {item:?} is invalid"));
        }
        paths.push(item.to_string());
    }
    if paths.is_empty() {
        return Err("write_paths must be a comma-separated list".to_string());
    }
    if write_policy != WritePolicy::OwnedPaths {
        return Err("write_paths requires write_policy owned_paths".to_string());
    }
    Ok(paths)
}

#[cfg(test)]
mod tests;

/// Builds the definition that runs `skill` as a sub-agent, given its already
/// loaded Markdown body.
///
/// `None` for a skill with no declared contract: without one the delegating
/// agent cannot know what to send or what comes back, which is the shape the
/// [design](../../../docs/specs/2026-09-22-skill-subagent-execution.md)
/// identifies as making sub-agent execution fail.
///
/// The definition takes no execution knobs from the skill. `context` is
/// always `fresh`, because `inherit` would copy in exactly the context this
/// mechanism exists to keep out, and `tools` stays `None`, which means "every
/// child tool" and so cannot widen the set the runner already built.
pub(crate) fn from_skill(skill: &crate::skill::Skill, body: String) -> Option<Definition> {
    let contract = skill.contract.as_ref()?;
    Some(Definition {
        name: skill.name.clone(),
        description: format!(
            "{}\nExpected input: {}\nOutput: {}",
            skill.description, contract.input, contract.output
        ),
        tools: None,
        model: String::new(),
        context: "fresh".to_string(),
        write_policy: WritePolicy::default(),
        write_paths: Vec::new(),
        body: format!("{body}\n\n{}", resource_note(&skill.name)),
        directory: skill.directory.clone(),
        path: skill.path.clone(),
        is_skill_derived: true,
    })
}

/// The trailer appended to a skill-derived body.
///
/// A skill package's own `scripts/` and `references/` are reached with the
/// `skill` tool, not the file tools: a user-level skill lives outside the
/// workspace, so the file tools reject it by design. Without this the child
/// follows instructions that reference files it cannot open.
fn resource_note(name: &str) -> String {
    format!(
        "## This skill's own files\n\
         Read any file this skill package ships with the `skill` tool, as\n\
         `skill(name: {name:?}, file: \"<path inside the package>\")`. The package\n\
         may sit outside the workspace, where the file tools cannot reach it.\n\
         Report your result in your final message, as the contract above states."
    )
}

#[cfg(test)]
mod skill_definition_tests;

#[cfg(test)]
mod skill_merge_tests;
