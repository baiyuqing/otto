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
//! Security: every read goes through [`crate::tool::root::Root`], which pins an
//! open directory file descriptor and resolves each component beneath it, so a
//! path that escapes the skill directory is rejected after canonical-path and
//! symlink validation rather than followed. Skill content is untrusted text: it
//! cannot override the system prompt, the user's requests or the sandbox
//! policy, and nothing here executes it.
//!
//! Scope: `allowed-tools` is parsed but never turned into an enforcement
//! decision; see the README for the features that are deliberately absent.

pub mod frontmatter;
pub mod prompt;

use std::io;
use std::io::Read;
use std::path::{Path, PathBuf};

pub use frontmatter::{Fields, parse as parse_frontmatter};
pub use prompt::{MAX_LISTING_BYTES, prompt_section};

use crate::tool::root::{self, Root};

/// The largest `SKILL.md`, or sibling skill file, this loader will read.
pub const MAX_SKILL_FILE_BYTES: u64 = 64 << 20;
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

/// Parses and validates one skill directory, returning a warning string when
/// the directory is not a usable skill.
fn load_candidate(
    directory: &Path,
    skill_path: &Path,
    directory_name: &str,
    directory_fs: &Root,
) -> Result<Skill, String> {
    let describe = |error: String| format!("skill {}: {error}", skill_path.display());
    let data = read_root_file(directory_fs, Path::new("SKILL.md"))
        .map_err(|error| describe(error.to_string()))?;
    let (fields, _) = frontmatter::parse(&data).map_err(describe)?;
    let name = validate_skill_name(&fields, directory_name).map_err(describe)?;
    let description = validate_skill_description(&fields).map_err(describe)?;
    Ok(Skill {
        name,
        description,
        directory: directory.to_path_buf(),
        path: skill_path.to_path_buf(),
    })
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
fn is_valid_skill_name(name: &str) -> bool {
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
    let (_, body) = frontmatter::parse(&data)?;
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
pub fn read_root_file(root_fs: &Root, name: &Path) -> io::Result<Vec<u8>> {
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

pub(crate) fn too_large(size: u64, maximum: u64) -> String {
    format!("file is too large ({size} bytes); maximum readable size is {maximum} bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, frontmatter_extra: &str, body: &str) -> PathBuf {
        let directory = root.join(name);
        std::fs::create_dir_all(&directory).expect("the skill directory is creatable");
        let content = format!(
            "---\nname: {name}\ndescription: desc for {name}\n{frontmatter_extra}---\n{body}"
        );
        std::fs::write(directory.join("SKILL.md"), content).expect("SKILL.md is writable");
        directory
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
}
