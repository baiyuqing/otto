//! Skill discovery and loading.
//!
//! A skill is a directory holding a `SKILL.md` file with YAML frontmatter,
//! following the Agent Skills format. Skills are discovered under
//! `~/.otto/skills` and `<workspace>/.otto/skills`; the workspace root wins on
//! a name collision.
//!
//! Ownership: a [`Catalog`] owns plain data, is cheap to clone and performs no
//! I/O after [`Catalog::discover`] returns, so it is safe to share across
//! tasks. Concurrency: every function here is free of shared mutable state.
//!
//! Security: every read goes through `crate::fsops::root::Root`, which pins an
//! open directory file descriptor and resolves each component beneath it, so a
//! path that escapes the skill directory is rejected after canonical-path and
//! symlink validation rather than followed. Skill content is untrusted text: it
//! cannot override the system prompt, the user's requests or the sandbox
//! policy, and nothing here executes it.
//!
//! Scope: `allowed-tools` is parsed but never turned into an enforcement
//! decision; see the README for the features that are deliberately absent.

pub mod check;
pub mod check_repository;
pub mod frontmatter;
pub mod prompt;
pub mod sqlite;

use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

pub use frontmatter::{Fields, parse as parse_frontmatter};
pub(crate) use prompt::prompt_section;

use crate::fsops::root::{self, Root};

/// The largest `SKILL.md`, or sibling skill file, this loader will read.
pub(crate) const MAX_SKILL_FILE_BYTES: u64 = 64 << 20;
pub(crate) const MAX_SKILL_NAME_LENGTH: usize = 64;
pub(crate) const MAX_SKILL_DESCRIPTION_CHARS: usize = 1024;
/// The bound on each half of a declared contract.
pub(crate) const MAX_SKILL_CONTRACT_CHARS: usize = 1024;

/// What a skill promises to be given and to return.
///
/// Both halves are required together: a skill that declares one alone is the
/// case that makes sub-agent execution fail, so it is reported and treated as
/// declaring neither. See the
/// [sub-agent execution design](../../../docs/specs/2026-09-22-skill-subagent-execution.md).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Contract {
    pub input: String,
    pub output: String,
}

/// One discovered skill.
///
/// `name` is the catalog ID (qualified for plugins). Local frontmatter names
/// must equal the directory base name.
/// `directory` is absolute; `path` is `directory/SKILL.md`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The declared input/output contract, absent unless the frontmatter
    /// declares both halves. Its presence is what will make a skill eligible
    /// for sub-agent execution; nothing reads it for that purpose yet.
    pub contract: Option<Contract>,
    pub directory: PathBuf,
    pub path: PathBuf,
}

/// The set of skills discovered for one runner, sorted by name.
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
                    Ok((skill, warning)) => {
                        warnings.extend(warning);
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

    /// Adds package Skills without changing their local name validation.
    pub(crate) fn extend_plugin(
        &mut self,
        plugin: &str,
        package: &Path,
        directories: &[PathBuf],
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        let package_root = match Root::open(package) {
            Ok(root) => root,
            Err(_) => return vec![format!("plugin {plugin}: cannot open skill package")],
        };
        for relative in directories {
            if package_root.stat(relative).is_err() {
                warnings.push(format!(
                    "plugin {plugin}: invalid skill path {}",
                    relative.display()
                ));
                continue;
            }
            let directory = package.join(relative);
            let Some(local_name) = relative.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let result = package_root
                .open_file(
                    relative,
                    nix::fcntl::OFlag::O_RDONLY | nix::fcntl::OFlag::O_DIRECTORY,
                    nix::sys::stat::Mode::empty(),
                )
                .and_then(Root::from_directory)
                .map_err(|e| e.to_string())
                .and_then(|root| {
                    load_candidate(&directory, &directory.join("SKILL.md"), local_name, &root)
                });
            match result {
                Ok((mut skill, warning)) => {
                    if let Some(warning) = warning {
                        warnings.push(warning);
                    }
                    skill.name = format!("{plugin}:{}", skill.name);
                    if self.lookup(&skill.name).is_some() {
                        warnings.push(format!("duplicate plugin skill {}", skill.name));
                    } else {
                        self.skills.push(skill);
                    }
                }
                Err(error) => warnings.push(error),
            }
        }
        self.skills.sort_by(|a, b| a.name.cmp(&b.name));
        warnings
    }

    /// Removes skills whose names are in `names`, preserving name order.
    pub(crate) fn without_names(&self, names: &std::collections::BTreeSet<String>) -> Self {
        Self {
            skills: self
                .skills
                .iter()
                .filter(|skill| !names.contains(&skill.name))
                .cloned()
                .collect(),
        }
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

/// The directories skills are discovered in, in precedence order: the user's
/// home directory first, then the workspace, so a workspace skill overrides a
/// user skill of the same name.
pub fn roots(home: Option<&Path>, workspace: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::with_capacity(2);
    if let Some(home) = home {
        roots.push(home.join(".otto").join("skills"));
    }
    roots.push(workspace.join(".otto").join("skills"));
    roots
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

/// Parses and validates one skill directory.
///
/// `Err` means the directory is not a usable skill. `Ok` may still carry a
/// warning: a malformed contract degrades the skill to one without a
/// contract rather than discarding a skill that otherwise works.
fn load_candidate(
    directory: &Path,
    skill_path: &Path,
    directory_name: &str,
    directory_fs: &Root,
) -> Result<(Skill, Option<String>), String> {
    let describe = |error: String| format!("skill {}: {error}", skill_path.display());
    let skill_file_bytes = read_root_file(directory_fs, Path::new("SKILL.md"))
        .map_err(|error| describe(error.to_string()))?;
    let (fields, _) = frontmatter::parse(&skill_file_bytes).map_err(describe)?;
    let name = validate_skill_name(&fields, directory_name).map_err(describe)?;
    let description = validate_skill_description(&fields).map_err(describe)?;
    let (contract, warning) = validate_skill_contract(&fields);
    Ok((
        Skill {
            name,
            description,
            contract,
            directory: directory.to_path_buf(),
            path: skill_path.to_path_buf(),
        },
        warning.map(describe),
    ))
}

fn validate_skill_name(fields: &Fields, directory_name: &str) -> Result<String, String> {
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

/// `^[a-z0-9]+(-[a-z0-9]+)*$`, spelled out to avoid a regex for one rule.
pub(crate) fn is_valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

fn validate_skill_description(fields: &Fields) -> Result<String, String> {
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

/// Reads the optional `input`/`output` contract.
///
/// Returns the contract and, when the declaration is unusable, one warning.
/// Declaring neither half is the silent default: every skill written before
/// this key pair existed takes that path, and nothing about it changes.
fn validate_skill_contract(fields: &Fields) -> (Option<Contract>, Option<String>) {
    let half = |key: &str| {
        fields
            .get(key)
            .map_or("", String::as_str)
            .trim()
            .to_string()
    };
    let input = half("input");
    let output = half("output");

    match (input.is_empty(), output.is_empty()) {
        (true, true) => (None, None),
        (false, false) => {
            for (key, value) in [("input", &input), ("output", &output)] {
                if value.chars().count() > MAX_SKILL_CONTRACT_CHARS {
                    return (
                        None,
                        Some(format!(
                            "{key} exceeds {MAX_SKILL_CONTRACT_CHARS} characters; \
                             the input/output contract is ignored"
                        )),
                    );
                }
            }
            (Some(Contract { input, output }), None)
        }
        // Half a contract is the shape that makes sub-agent execution fail, so
        // it is never treated as good enough.
        _ => (
            None,
            Some(
                "input and output must be declared together; \
                 the input/output contract is ignored"
                    .to_string(),
            ),
        ),
    }
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
    let skill_file_bytes = read_root_file(&root_fs, name).map_err(|error| error.to_string())?;
    let (_, body) = frontmatter::parse(&skill_file_bytes)?;
    Ok(body)
}

/// Relative slash-separated paths of the regular files under `directory`,
/// recursively: `SKILL.md` at the top level is excluded, hidden names are
/// skipped, symbolic links are neither listed nor descended, the result is
/// sorted and truncated to `limit`. `total` is the count before truncation.
pub(crate) fn list_files(directory: &Path, limit: usize) -> io::Result<(Vec<String>, usize)> {
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
pub(crate) fn read_root_file(root_fs: &Root, name: &Path) -> io::Result<Vec<u8>> {
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
    let mut skill_file_bytes = Vec::new();
    file.take(MAX_SKILL_FILE_BYTES + 1)
        .read_to_end(&mut skill_file_bytes)?;
    if skill_file_bytes.len() as u64 > MAX_SKILL_FILE_BYTES {
        return Err(io::Error::other(too_large(
            skill_file_bytes.len() as u64,
            MAX_SKILL_FILE_BYTES,
        )));
    }
    Ok(skill_file_bytes)
}

pub(crate) fn too_large(size: u64, maximum: u64) -> String {
    format!("file is too large ({size} bytes); maximum readable size is {maximum} bytes")
}

#[cfg(test)]
mod tests;
