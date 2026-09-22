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
//! Security: every read goes through [`crate::tool::root::Root`], so an
//! `AGENT.md` symbolic link pointing outside its definition directory is
//! skipped rather than followed. A definition is untrusted text; its `tools`
//! list can only narrow the child tool set the runner already built, never
//! widen it.

use std::collections::BTreeMap;
use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::skill::frontmatter::{self, Fields};
use crate::tool::root::{self, Root};

/// The largest `AGENT.md` this loader will read.
pub const MAX_AGENT_FILE_BYTES: u64 = 64 << 20;
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
                let data = match read_agent_file(&directory_fs, Path::new("AGENT.md")) {
                    Ok(Some(data)) => data,
                    // A directory without a readable regular AGENT.md is
                    // silently ignored, as is one reached by an escaping link.
                    Ok(None) => continue,
                    Err(error) => {
                        warnings.push(format!("{}: {error}", agent_path.display()));
                        continue;
                    }
                };
                match load_candidate(
                    &data,
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

/// The directories agent definitions are discovered in, in precedence order:
/// the user's home directory first, then the workspace.
pub fn roots(home: Option<&Path>, workspace: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(2);
    if let Some(home) = home {
        roots.push(home.join(".otto").join("agents"));
    }
    roots.push(workspace.join(".otto").join("agents"));
    roots
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
    let mut data = Vec::new();
    file.take(MAX_AGENT_FILE_BYTES + 1).read_to_end(&mut data)?;
    if data.len() as u64 > MAX_AGENT_FILE_BYTES {
        return Err(io::Error::other(format!(
            "file is too large; maximum is {MAX_AGENT_FILE_BYTES} bytes"
        )));
    }
    Ok(Some(data))
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
mod tests {
    use super::*;

    fn write_agent(root: &Path, name: &str, frontmatter_extra: &str, body: &str) -> PathBuf {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).expect("the agent directory is creatable");
        let content = format!(
            "---\nname: {name}\ndescription: desc for {name}\n{frontmatter_extra}---\n{body}"
        );
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
        std::fs::create_dir_all(root.path().join("notanagent"))
            .expect("the directory is creatable");
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

    /// The discovery roots, in the precedence order the runner relies on.
    #[test]
    fn roots_put_the_home_directory_before_the_workspace() {
        assert_eq!(
            roots(Some(Path::new("/home/u")), Path::new("/w")),
            vec![
                PathBuf::from("/home/u/.otto/agents"),
                PathBuf::from("/w/.otto/agents"),
            ]
        );
        assert_eq!(
            roots(None, Path::new("/w")),
            vec![PathBuf::from("/w/.otto/agents")]
        );
    }
}

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
pub fn from_skill(skill: &crate::skill::Skill, body: String) -> Option<Definition> {
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
mod skill_definition_tests {
    use super::*;
    use crate::skill::{Contract, Skill};

    fn contracted(name: &str) -> Skill {
        Skill {
            name: name.into(),
            description: "Normalizes scanner output.".into(),
            contract: Some(Contract {
                input: "a scanner JSON report path".into(),
                output: "a JSON list of records".into(),
            }),
            directory: format!("/skills/{name}").into(),
            path: format!("/skills/{name}/SKILL.md").into(),
        }
    }

    #[test]
    fn a_contracted_skill_becomes_a_fresh_context_definition() {
        let definition = from_skill(&contracted("normalize"), "step one\nstep two".into())
            .expect("a definition");

        assert_eq!(definition.name, "normalize");
        assert_eq!(
            definition.context, "fresh",
            "inherit would reintroduce the context this exists to avoid"
        );
        assert!(
            definition.tools.is_none(),
            "a skill must not widen the child tool set"
        );
        assert_eq!(definition.write_policy, WritePolicy::default());
        assert!(definition.model.is_empty());
        assert!(definition.body.contains("step one"), "{}", definition.body);
    }

    #[test]
    fn the_description_carries_both_halves_of_the_contract() {
        let definition = from_skill(&contracted("normalize"), "body".into()).expect("a definition");

        assert!(
            definition
                .description
                .contains("a scanner JSON report path"),
            "{}",
            definition.description
        );
        assert!(
            definition.description.contains("a JSON list of records"),
            "{}",
            definition.description
        );
        assert!(
            definition
                .description
                .contains("Normalizes scanner output."),
            "{}",
            definition.description
        );
    }

    #[test]
    fn the_body_tells_the_child_how_to_reach_the_packages_own_files() {
        let definition = from_skill(&contracted("normalize"), "body".into()).expect("a definition");

        assert!(
            definition.body.contains("skill"),
            "the child must be told which tool reads the package: {}",
            definition.body
        );
        assert!(
            definition.body.contains("normalize"),
            "the note must name the skill: {}",
            definition.body
        );
    }

    #[test]
    fn a_skill_without_a_contract_gets_no_definition() {
        let mut plain = contracted("plain");
        plain.contract = None;

        assert!(from_skill(&plain, "body".into()).is_none());
    }
}
