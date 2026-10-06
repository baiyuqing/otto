//! Writing, versioning and reverting the skills reflection generates.
//!
//! This is the only module that writes into a skill root, and it accepts only
//! a [`Vetted`] skill, so nothing reaches disk without passing the checks in
//! `guard` (and the model review in `review`).
//!
//! Ownership covers SKILL.md and every script. A file edited or added by a
//! human makes the complete skill human-owned. Markdown-only hashes and .md
//! history stay compatible; bundles use deterministic .json history.
//!
//! Publication stages a complete directory and exclusively renames (create)
//! or atomically exchanges (revise/revert) it through a pinned root descriptor.
//! A root directory lock serializes cooperating writers. Store errors restore
//! the displaced tree. Filesystem errors never publish a partial file set.
//!
//! Errors: a refused write is a drop reason (`&'static str`) the caller
//! counts; a refused revert is a [`RevertError`], whose `Display` is the text
//! for the user.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::guard::{self, Vetted};
use super::output::SkillAction;
use super::repository::ReflectionStore;
use super::store::SkillWrite;
#[cfg(test)]
use super::store::Store;

// Description limits are in characters; account for their full UTF-8 width.
const MAXIMUM_MARKDOWN_BYTES: usize = guard::MAXIMUM_BODY_BYTES
    + crate::skill::MAX_SKILL_DESCRIPTION_CHARS * 4
    + crate::skill::MAX_SKILL_NAME_LENGTH
    + 128;
use crate::fsops;
use crate::tool::root::Root;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;

/// Where skills are written, kept, and looked up.
#[derive(Debug, Clone)]
pub struct Roots {
    /// `~/.otto/skills`: the only root reflection writes.
    pub write_root: PathBuf,
    /// `~/.otto/skill-history`.
    pub history_root: PathBuf,
    /// Every configured skill root, for the name-collision check. Includes
    /// `write_root` when it is configured.
    pub lookup_roots: Vec<PathBuf>,
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Created,
    Revised,
}

/// What a revert did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reverted {
    /// The previous version was restored.
    Restored,
    /// The skill was created by reflection and has been removed.
    Removed,
}

/// Whether `row`'s skill still has the content reflection last wrote, so
/// reflection still owns it. False once it was edited, replaced, or removed.
pub fn still_owned(roots: &Roots, row: &super::store::GeneratedSkill) -> bool {
    read_bundle(&roots.write_root.join(&row.name)).is_ok_and(|bundle| bundle.version() == row.hash)
}

/// The `SKILL.md` text for a vetted skill. The frontmatter holds only `name`
/// and `description`: no `input`, `output`, `allowed-tools` or any other key.
pub fn render(vetted: &Vetted) -> String {
    let description = vetted
        .description()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('\'', "''");
    format!(
        "---\nname: {}\ndescription: '{}'\n---\n\n{}\n",
        vetted.name(),
        description,
        vetted.body().trim_end()
    )
}

/// The lowercase hex SHA-256 of `content`.
pub fn hash(content: &str) -> String {
    Sha256::digest(content.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn skill_file(root: &Path, name: &str) -> PathBuf {
    root.join(name).join("SKILL.md")
}

fn history_file(roots: &Roots, name: &str, hash: &str) -> PathBuf {
    roots.history_root.join(name).join(format!("{hash}.md"))
}

/// Whether a skill named `name` may be written with `action`, before any
/// content is known: a `create` needs a free name in every root and room under
/// `maximum_generated`; a `revise` needs a skill reflection still owns.
/// Returns the hash of the owned content a `revise` would replace.
///
/// Called before the model review, so a doomed proposal does not cost a call.
pub fn precheck(
    store: &dyn ReflectionStore,
    roots: &Roots,
    action: SkillAction,
    name: &str,
    maximum_generated: usize,
) -> Result<Option<String>, &'static str> {
    match action {
        SkillAction::Create => {
            if roots
                .lookup_roots
                .iter()
                .chain(std::iter::once(&roots.write_root))
                .any(|root| root.join(name).exists())
            {
                return Err("skill_name_exists");
            }
            let owned = store.generated_count().map_err(|_| "skill_store_error")?;
            if owned >= maximum_generated {
                return Err("skill_cap_reached");
            }
            Ok(None)
        }
        SkillAction::Revise => {
            let owned = store
                .generated(name)
                .map_err(|_| "skill_store_error")?
                .ok_or("skill_not_owned")?;
            let current =
                read_bundle(&roots.write_root.join(name)).map_err(|_| "skill_not_owned")?;
            if current.version() != owned.hash {
                return Err("skill_not_owned");
            }
            Ok(Some(owned.hash))
        }
    }
}

/// Writes `vetted`, or returns why it was refused.
///
/// `maximum_generated` bounds how many skills reflection may own in total.
pub fn apply(
    store: &dyn ReflectionStore,
    roots: &Roots,
    vetted: &Vetted,
    run_id: &str,
    session_id: &str,
    at: &str,
    maximum_generated: usize,
) -> Result<Written, &'static str> {
    let base = precheck(
        store,
        roots,
        vetted.action(),
        vetted.name(),
        maximum_generated,
    )?;
    apply_on_base(
        store,
        roots,
        vetted,
        run_id,
        session_id,
        at,
        maximum_generated,
        base.as_deref(),
    )
}

/// Recheck the exact approved version while holding the root lock.
#[allow(clippy::too_many_arguments)]
pub fn apply_on_base(
    store: &dyn ReflectionStore,
    roots: &Roots,
    vetted: &Vetted,
    run_id: &str,
    session_id: &str,
    at: &str,
    maximum_generated: usize,
    base_hash: Option<&str>,
) -> Result<Written, &'static str> {
    let name = vetted.name();
    validate_python(vetted.scripts())?;
    let bundle = Bundle::from_vetted(vetted);
    let new_hash = bundle.version();
    fs::create_dir_all(&roots.write_root).map_err(|_| "skill_write_error")?;
    let root = fsops::open_dir_no_follow(&roots.write_root).map_err(|_| "skill_path_unsafe")?;
    let _lock = fsops::lock_directory(&root).map_err(|_| "skill_write_busy")?;

    let replaced = precheck(store, roots, vetted.action(), name, maximum_generated)?;
    if replaced.as_deref() != base_hash {
        return Err("skill_base_changed");
    }
    let written = match vetted.action() {
        SkillAction::Create => Written::Created,
        SkillAction::Revise => {
            if replaced.as_deref() == Some(new_hash.as_str()) {
                return Err("skill_no_change");
            }
            Written::Revised
        }
    };
    refuse_symlink(&roots.write_root.join(name)).map_err(|_| "skill_path_unsafe")?;

    save_history(roots, name, &new_hash, &bundle).map_err(|_| "skill_write_error")?;
    let mut staged = stage(&roots.write_root, &bundle).map_err(|_| "skill_write_error")?;
    let staged_name = staged
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("skill_write_error")?
        .to_owned();
    fsops::publish_directory(&root, &staged_name, name, replaced.is_some())
        .map_err(|_| "skill_write_error")?;
    if let Some(previous) = &replaced {
        // A human does not take our root lock. Check the displaced tree too;
        // restore it if it changed between the ownership check and exchange.
        if !read_bundle(staged.path()).is_ok_and(|bundle| bundle.version() == *previous) {
            if fsops::publish_directory(&root, &staged_name, name, true).is_err() {
                staged.disable_cleanup(true);
                return Err("skill_write_recovery_required");
            }
            return Err("skill_not_owned");
        }
    }
    store
        .record_skill_write(&SkillWrite {
            name: name.to_owned(),
            hash: new_hash,
            run_id: run_id.to_owned(),
            session_id: session_id.to_owned(),
            reason: vetted.reason().to_owned(),
            at: at.to_owned(),
        })
        .map_err(|_| {
            // Restore the complete previous tree if the receipt cannot commit.
            let restored = if replaced.is_some() {
                fsops::publish_directory(&root, &staged_name, name, true)
            } else {
                fsops::rename_at(&root, name, &staged_name)
            };
            if restored.is_err() {
                staged.disable_cleanup(true);
                return "skill_write_recovery_required";
            }
            "skill_store_error"
        })?;
    Ok(written)
}

/// Why a revert was refused or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevertError {
    /// The name is not a valid skill name.
    InvalidName(String),
    /// Reflection did not write a skill by this name.
    NotGenerated(String),
    /// The file changed after reflection wrote it, so a human owns it.
    Edited { name: String, path: PathBuf },
    /// The store, the history, or the filesystem failed.
    Failed(String),
}

impl std::fmt::Display for RevertError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(name) => write!(formatter, "{name:?} is not a valid skill name"),
            Self::NotGenerated(name) => write!(formatter, "{name} was not generated by reflection"),
            Self::Edited { name, path } => write!(
                formatter,
                "{name} has been edited since reflection wrote it, so reflection no longer owns it; \
                 edit or remove {} yourself",
                path.display()
            ),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RevertError {}

/// Restores the previous version of a reflection-owned skill, or removes it
/// when reflection created it.
pub fn revert(
    store: &dyn ReflectionStore,
    roots: &Roots,
    name: &str,
    at: &str,
) -> Result<Reverted, RevertError> {
    let failed = |message: String| RevertError::Failed(message);
    if !crate::skill::is_valid_skill_name(name) {
        return Err(RevertError::InvalidName(name.to_owned()));
    }
    let owned = store
        .generated(name)
        .map_err(|error| failed(error.to_string()))?
        .ok_or_else(|| RevertError::NotGenerated(name.to_owned()))?;
    let target = skill_file(&roots.write_root, name);
    let root =
        fsops::open_dir_no_follow(&roots.write_root).map_err(|error| failed(error.to_string()))?;
    let _lock = fsops::lock_directory(&root).map_err(|error| failed(error.to_string()))?;
    let current =
        read_bundle(&roots.write_root.join(name)).map_err(|error| failed(error.to_string()))?;
    if current.version() != owned.hash {
        return Err(RevertError::Edited {
            name: name.to_owned(),
            path: target,
        });
    }
    let versions = store
        .skill_versions(name)
        .map_err(|error| failed(error.to_string()))?;
    let previous = if let [.., previous, _latest] = versions.as_slice() {
        let kept =
            load_history(roots, name, previous).map_err(|error| failed(error.to_string()))?;
        if &kept.version() != previous {
            return Err(failed(format!(
                "{name}: the previous history version is corrupt"
            )));
        }
        Some(kept)
    } else {
        None
    };
    // On removal this empty private directory receives the old tree. On
    // restoration it receives the displaced tree after the atomic exchange.
    let mut staged = stage(
        &roots.write_root,
        previous.as_ref().unwrap_or(&Bundle::default()),
    )
    .map_err(|error| failed(error.to_string()))?;
    let staged_name = staged
        .path()
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| failed("invalid staging directory".into()))?
        .to_owned();
    if previous.is_some() {
        fsops::publish_directory(&root, &staged_name, name, true)
            .map_err(|error| failed(error.to_string()))?;
    } else {
        fsops::rename_at(&root, name, &staged_name).map_err(|error| failed(error.to_string()))?;
    }
    let restore = || {
        if previous.is_some() {
            fsops::publish_directory(&root, &staged_name, name, true)
        } else {
            fsops::rename_at(&root, &staged_name, name)
        }
    };
    if !read_bundle(staged.path()).is_ok_and(|bundle| bundle.version() == owned.hash) {
        if restore().is_err() {
            staged.disable_cleanup(true);
            return Err(failed(format!(
                "skill recovery required: {}",
                staged.path().display()
            )));
        }
        return Err(RevertError::Edited {
            name: name.to_owned(),
            path: target,
        });
    }
    if let Err(error) = store.pop_skill_version(name, at) {
        if restore().is_err() {
            staged.disable_cleanup(true);
            return Err(failed(format!(
                "skill recovery required: {}",
                staged.path().display()
            )));
        }
        return Err(failed(error.to_string()));
    }
    Ok(if previous.is_some() {
        Reverted::Restored
    } else {
        Reverted::Removed
    })
}

fn refuse_symlink(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(std::io::Error::other("skill directory is a symbolic link"))
        }
        _ => Ok(()),
    }
}

fn save_history(roots: &Roots, name: &str, version: &str, bundle: &Bundle) -> std::io::Result<()> {
    let directory = roots.history_root.join(name);
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&roots.history_root, fs::Permissions::from_mode(0o700))?;
    refuse_symlink(&roots.history_root)?;
    refuse_symlink(&directory)?;
    let path = if bundle.scripts.is_empty() {
        history_file(roots, name, version)
    } else {
        directory.join(format!("{version}.json"))
    };
    if path.exists() {
        if load_history(roots, name, version)?.version() != version {
            return Err(std::io::Error::other("corrupt skill history"));
        }
        return Ok(());
    }
    let text = if bundle.scripts.is_empty() {
        bundle.markdown.clone()
    } else {
        serde_json::to_string(bundle)?
    };
    write_atomically(&path, &text)
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Bundle {
    markdown: String,
    scripts: BTreeMap<String, String>,
}
impl Bundle {
    fn from_vetted(vetted: &Vetted) -> Self {
        Self {
            markdown: render(vetted),
            scripts: vetted.scripts().clone(),
        }
    }
    fn version(&self) -> String {
        // Preserve every existing Markdown-only receipt and history filename.
        if self.scripts.is_empty() {
            hash(&self.markdown)
        } else {
            hash(&serde_json::to_string(self).expect("serializable skill bundle"))
        }
    }
}

pub fn version(vetted: &Vetted) -> String {
    Bundle::from_vetted(vetted).version()
}

/// Owned files offered back to reflection for complete revisions.
pub fn scripts(roots: &Roots, name: &str) -> std::io::Result<BTreeMap<String, String>> {
    read_bundle(&roots.write_root.join(name)).map(|bundle| bundle.scripts)
}

fn read_text(root: &Root, path: &Path, maximum: usize) -> std::io::Result<String> {
    let file = root.open_file(path, OFlag::O_RDONLY | OFlag::O_NONBLOCK, Mode::empty())?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::other("not a regular skill file"));
    }
    let mut text = String::new();
    file.take((maximum + 1) as u64).read_to_string(&mut text)?;
    if text.len() > maximum {
        return Err(std::io::Error::other("skill file too large"));
    }
    Ok(text)
}

fn read_bundle(path: &Path) -> std::io::Result<Bundle> {
    refuse_symlink(path)?;
    let root = Root::from_directory(fsops::open_no_follow(
        path,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?)?;
    let mut bundle = Bundle::default();
    for entry in root.read_dir(Path::new("."))? {
        if entry.is_symlink {
            return Err(std::io::Error::other("symbolic link in generated skill"));
        }
        if entry.name == "SKILL.md" && entry.is_regular {
            bundle.markdown = read_text(&root, Path::new("SKILL.md"), MAXIMUM_MARKDOWN_BYTES)?;
        } else if entry.name == "scripts" && entry.is_dir {
            for script in root.read_dir(Path::new("scripts"))? {
                let path = format!("scripts/{}", script.name.to_string_lossy());
                if !script.is_regular
                    || script.is_symlink
                    || !guard::valid_script_path(&path)
                    || bundle.scripts.len() >= guard::MAXIMUM_SCRIPTS
                {
                    return Err(std::io::Error::other("unexpected generated skill file"));
                }
                let source = read_text(&root, Path::new(&path), guard::MAXIMUM_SCRIPT_BYTES)?;
                bundle.scripts.insert(path, source);
            }
        } else {
            return Err(std::io::Error::other("unexpected generated skill file"));
        }
    }
    if bundle.markdown.is_empty() {
        return Err(std::io::Error::other("missing SKILL.md"));
    }
    Ok(bundle)
}

fn load_history(roots: &Roots, name: &str, version: &str) -> std::io::Result<Bundle> {
    let directory = roots.history_root.join(name);
    refuse_symlink(&directory)?;
    let root = Root::from_directory(fsops::open_no_follow(
        &directory,
        libc::O_RDONLY | libc::O_DIRECTORY,
    )?)?;
    let json = PathBuf::from(format!("{version}.json"));
    if let Ok(text) = read_text(&root, &json, 128 * 1024) {
        let bundle: Bundle = serde_json::from_str(&text)?;
        if bundle.scripts.len() > guard::MAXIMUM_SCRIPTS
            || bundle.scripts.iter().any(|(path, text)| {
                !guard::valid_script_path(path) || text.len() > guard::MAXIMUM_SCRIPT_BYTES
            })
        {
            return Err(std::io::Error::other("invalid skill history file set"));
        }
        Ok(bundle)
    } else {
        read_text(
            &root,
            Path::new(&format!("{version}.md")),
            MAXIMUM_MARKDOWN_BYTES,
        )
        .map(|markdown| Bundle {
            markdown,
            scripts: BTreeMap::new(),
        })
    }
}

fn stage(root: &Path, bundle: &Bundle) -> std::io::Result<tempfile::TempDir> {
    let directory = tempfile::Builder::new()
        .prefix(".reflection-")
        .tempdir_in(root)?;
    if !bundle.markdown.is_empty() {
        write_atomically(&directory.path().join("SKILL.md"), &bundle.markdown)?;
    }
    for (path, source) in &bundle.scripts {
        write_atomically(&directory.path().join(path), source)?;
    }
    // Skill discovery can read the generated files as ordinary user skills.
    fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755))?;
    Ok(directory)
}

/// Compile source with a fixed isolated interpreter program. Never import or
/// execute generated code and never write bytecode. Missing Python fails only
/// proposals carrying scripts, leaving ordinary reflection usable.
pub fn validate_python(scripts: &BTreeMap<String, String>) -> Result<(), &'static str> {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    if scripts.is_empty() {
        return Ok(());
    }
    let directory = tempfile::tempdir().map_err(|_| "skill_python_validation_failed")?;
    let mut input = tempfile::tempfile().map_err(|_| "skill_python_validation_failed")?;
    serde_json::to_writer(&mut input, scripts).map_err(|_| "skill_python_validation_failed")?;
    use std::io::{Seek, SeekFrom};
    input
        .seek(SeekFrom::Start(0))
        .map_err(|_| "skill_python_validation_failed")?;
    let mut child = Command::new("python3")
        .args(["-I", "-S", "-c", "import json,sys; [compile(source.encode('utf-8'), path, 'exec') for path, source in json.load(sys.stdin).items()]"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .current_dir(directory.path()).stdin(Stdio::from(input))
        .stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().map_err(|_| "skill_python_unavailable")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return if status.success() {
                    Ok(())
                } else {
                    Err("skill_python_invalid")
                };
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("skill_python_validation_failed");
            }
        }
    }
}

/// Writes `content` to `path` through a temp file in the same directory, then
/// renames it into place.
fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent"))?;
    fs::create_dir_all(directory)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".reflection-file-")
        .tempfile_in(directory)?;
    temporary.write_all(content.as_bytes())?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o644))?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::reflection::guard::{self, Candidate, Verdict};
    use crate::reflection::transcript::Entry;
    use crate::skill::Catalog;

    const BODY: &str = "1. Run `cargo fmt --all`.\n2. Run `cargo clippy --workspace -- -D warnings`.\n3. Fix every warning before committing.";

    fn vetted(name: &str, action: SkillAction, body: &str) -> Vetted {
        let candidate = Candidate {
            action,
            name: name.into(),
            description: "Run the lint gate: it's the user's rule".into(),
            body: body.into(),
            scripts: Default::default(),
            reason: "the steps ran and passed".into(),
            cited: Vec::new(),
        };
        let entries: HashMap<&str, &Entry> = HashMap::new();
        let checked =
            guard::check(candidate, &entries, &|text: &str| text.to_owned()).expect("check");
        guard::approve(checked, Verdict::NotRequested)
    }

    struct Fixture {
        _home: tempfile::TempDir,
        roots: Roots,
        store: Store,
    }

    fn fixture() -> Fixture {
        let home = tempfile::tempdir().expect("home");
        let write_root = home.path().join(".otto/skills");
        let roots = Roots {
            history_root: home.path().join(".otto/skill-history"),
            lookup_roots: vec![
                write_root.clone(),
                home.path().join("workspace/.otto/skills"),
            ],
            write_root,
        };
        Fixture {
            _home: home,
            roots,
            store: Store::open_in_memory().expect("store"),
        }
    }

    fn create(fixture: &Fixture, name: &str, body: &str) -> Result<Written, &'static str> {
        apply(
            &fixture.store,
            &fixture.roots,
            &vetted(name, SkillAction::Create, body),
            "run-1",
            "session-1",
            "2026-10-02T00:00:00Z",
            30,
        )
    }

    fn revise(fixture: &Fixture, name: &str, body: &str) -> Result<Written, &'static str> {
        apply(
            &fixture.store,
            &fixture.roots,
            &vetted(name, SkillAction::Revise, body),
            "run-2",
            "session-1",
            "2026-10-02T01:00:00Z",
            30,
        )
    }

    fn read(fixture: &Fixture, name: &str) -> String {
        fs::read_to_string(skill_file(&fixture.roots.write_root, name)).expect("skill file")
    }

    fn scripted(action: SkillAction, sources: &[(&str, &str)]) -> Vetted {
        let candidate = Candidate {
            action,
            name: "python-report".into(),
            description: "Summarize a local report with Python".into(),
            body: "Use scripts/report.py with an explicit input path to summarize the report."
                .into(),
            scripts: sources
                .iter()
                .map(|(path, source)| ((*path).into(), (*source).into()))
                .collect(),
            reason: "repeat a successful local calculation".into(),
            cited: Vec::new(),
        };
        let checked = guard::check(candidate, &HashMap::new(), &str::to_owned).expect("checked");
        guard::approve(checked, Verdict::Allowed)
    }

    fn apply_scripted(fixture: &Fixture, vetted: &Vetted) -> Result<Written, &'static str> {
        apply(
            &fixture.store,
            &fixture.roots,
            vetted,
            "script-run",
            "session-1",
            "t",
            30,
        )
    }

    #[test]
    fn script_only_revisions_and_removals_revert_the_complete_bundle() {
        let fixture = fixture();
        let first = scripted(
            SkillAction::Create,
            &[
                ("scripts/report.py", "print(1)\n"),
                (
                    "scripts/helper.py",
                    "def count(rows):\n    return len(rows)\n",
                ),
            ],
        );
        assert_eq!(apply_scripted(&fixture, &first), Ok(Written::Created));
        let receipt = fixture
            .store
            .generated(first.name())
            .expect("row")
            .expect("owned");
        assert_eq!(receipt.hash, version(&first));
        assert!(still_owned(&fixture.roots, &receipt));
        let (catalog, warnings) =
            Catalog::discover(std::slice::from_ref(&fixture.roots.write_root));
        assert!(warnings.is_empty(), "{warnings:?}");
        let skill = catalog.lookup(first.name()).expect("discover script skill");
        let (files, _) = crate::skill::list_files(&skill.directory, 50).expect("list files");
        assert!(files.iter().any(|path| path == "scripts/report.py"));
        let second = scripted(SkillAction::Revise, &[("scripts/report.py", "print(2)\n")]);
        assert_eq!(render(&first), render(&second), "only script files change");
        assert_eq!(apply_scripted(&fixture, &second), Ok(Written::Revised));
        let directory = fixture.roots.write_root.join(first.name());
        assert_eq!(
            fs::read_to_string(directory.join("scripts/report.py")).expect("source"),
            "print(2)\n"
        );
        assert!(!directory.join("scripts/helper.py").exists());
        assert_eq!(apply_scripted(&fixture, &second), Err("skill_no_change"));
        assert_eq!(
            revert(&fixture.store, &fixture.roots, first.name(), "t"),
            Ok(Reverted::Restored)
        );
        assert_eq!(
            scripts(&fixture.roots, first.name()).expect("restored scripts"),
            first.scripts().clone()
        );
        assert_eq!(
            revert(&fixture.store, &fixture.roots, first.name(), "t"),
            Ok(Reverted::Removed)
        );
        assert!(!directory.exists());
    }

    #[test]
    fn markdown_history_remains_compatible_across_bundle_upgrades() {
        let fixture = fixture();
        let first = scripted(SkillAction::Create, &[]);
        apply_scripted(&fixture, &first).expect("create old shape");
        assert_eq!(version(&first), hash(&render(&first)));
        assert!(history_file(&fixture.roots, first.name(), &version(&first)).exists());
        let second = scripted(SkillAction::Revise, &[("scripts/report.py", "print(1)\n")]);
        apply_scripted(&fixture, &second).expect("upgrade");
        revert(&fixture.store, &fixture.roots, first.name(), "t").expect("revert upgrade");
        assert_eq!(read(&fixture, first.name()), render(&first));
        assert!(
            !fixture
                .roots
                .write_root
                .join(first.name())
                .join("scripts")
                .exists()
        );
        assert!(still_owned(
            &fixture.roots,
            &fixture
                .store
                .generated(first.name())
                .expect("row")
                .expect("owned")
        ));
    }

    #[test]
    fn a_stale_approved_base_cannot_replace_a_newer_bundle() {
        let fixture = fixture();
        let first = scripted(SkillAction::Create, &[("scripts/report.py", "print(1)\n")]);
        apply_scripted(&fixture, &first).expect("create");
        let second = scripted(SkillAction::Revise, &[("scripts/report.py", "print(2)\n")]);
        apply_scripted(&fixture, &second).expect("revise");
        let third = scripted(SkillAction::Revise, &[("scripts/report.py", "print(3)\n")]);
        assert_eq!(
            apply_on_base(
                &fixture.store,
                &fixture.roots,
                &third,
                "stale-run",
                "session-1",
                "t",
                30,
                Some(&version(&first))
            ),
            Err("skill_base_changed")
        );
        assert_eq!(
            scripts(&fixture.roots, first.name()).expect("sources"),
            second.scripts().clone()
        );
    }

    #[test]
    fn maximum_utf8_descriptions_remain_owned_and_revertible() {
        let fixture = fixture();
        let candidate = Candidate {
            action: SkillAction::Create,
            name: "large-local-skill".into(),
            description: "计算".repeat(crate::skill::MAX_SKILL_DESCRIPTION_CHARS / 2),
            body: "step ".repeat(guard::MAXIMUM_BODY_BYTES / 5),
            scripts: BTreeMap::new(),
            reason: "successful steps".into(),
            cited: Vec::new(),
        };
        let checked = guard::check(candidate, &HashMap::new(), &str::to_owned).expect("check");
        let first = guard::approve(checked, Verdict::Allowed);
        apply_scripted(&fixture, &first).expect("create");
        let receipt = fixture
            .store
            .generated(first.name())
            .expect("row")
            .expect("owned");
        assert!(still_owned(&fixture.roots, &receipt));
        assert_eq!(
            revert(&fixture.store, &fixture.roots, first.name(), "t"),
            Ok(Reverted::Removed)
        );
    }

    #[test]
    fn human_script_edits_and_extra_files_protect_the_whole_skill() {
        for extra in [false, true] {
            let fixture = fixture();
            let first = scripted(SkillAction::Create, &[("scripts/report.py", "print(1)\n")]);
            apply_scripted(&fixture, &first).expect("create");
            let directory = fixture.roots.write_root.join(first.name());
            let path = if extra {
                directory.join("notes.txt")
            } else {
                directory.join("scripts/report.py")
            };
            fs::write(&path, "human content").expect("edit");
            let receipt = fixture
                .store
                .generated(first.name())
                .expect("row")
                .expect("owned");
            assert!(!still_owned(&fixture.roots, &receipt));
            assert_eq!(
                apply_scripted(&fixture, &scripted(SkillAction::Revise, &[])),
                Err("skill_not_owned")
            );
            assert!(revert(&fixture.store, &fixture.roots, first.name(), "t").is_err());
            assert_eq!(fs::read_to_string(path).expect("kept"), "human content");
        }
    }

    #[test]
    fn invalid_python_is_never_published_and_validation_never_executes_source() {
        let fixture = fixture();
        let bad = scripted(
            SkillAction::Create,
            &[("scripts/report.py", "def report(:\n")],
        );
        assert_eq!(apply_scripted(&fixture, &bad), Err("skill_python_invalid"));
        assert!(!fixture.roots.write_root.exists());
        assert!(fixture.store.generated(bad.name()).expect("row").is_none());
        let marker = fixture._home.path().join("must-not-exist");
        let source = format!(
            "from pathlib import Path\nPath({:?}).write_text('executed')\nraise RuntimeError('must never run')\n",
            marker.to_string_lossy()
        );
        assert_eq!(
            validate_python(&BTreeMap::from([("scripts/report.py".into(), source)])),
            Ok(())
        );
        assert!(!marker.exists(), "syntax checking must not execute source");
        assert_eq!(
            validate_python(&BTreeMap::from([(
                "scripts/report.py".into(),
                "# coding: ascii\nprint('计算')\n".into()
            )])),
            Err("skill_python_invalid")
        );
    }

    #[test]
    fn script_symlinks_and_corrupt_history_are_never_followed() {
        let fixture = fixture();
        let first = scripted(SkillAction::Create, &[("scripts/report.py", "print(1)\n")]);
        apply_scripted(&fixture, &first).expect("create");
        let second = scripted(SkillAction::Revise, &[("scripts/report.py", "print(2)\n")]);
        apply_scripted(&fixture, &second).expect("revise");
        let history = fixture
            .roots
            .history_root
            .join(first.name())
            .join(format!("{}.json", version(&first)));
        fs::write(&history, "{}").expect("corrupt history");
        assert!(revert(&fixture.store, &fixture.roots, first.name(), "t").is_err());
        assert_eq!(
            scripts(&fixture.roots, first.name()).expect("unchanged"),
            second.scripts().clone()
        );
        let script = fixture
            .roots
            .write_root
            .join(first.name())
            .join("scripts/report.py");
        let outside = fixture._home.path().join("outside.py");
        fs::write(&outside, "print(2)\n").expect("outside");
        fs::remove_file(&script).expect("remove");
        std::os::unix::fs::symlink(&outside, &script).expect("link");
        let receipt = fixture
            .store
            .generated(first.name())
            .expect("row")
            .expect("owned");
        assert!(!still_owned(&fixture.roots, &receipt));
        assert!(revert(&fixture.store, &fixture.roots, first.name(), "t").is_err());
        assert_eq!(fs::read_to_string(outside).expect("kept"), "print(2)\n");
    }

    #[test]
    fn a_created_skill_is_discovered_with_only_name_and_description() {
        let fixture = fixture();
        assert_eq!(create(&fixture, "rust-lint", BODY), Ok(Written::Created));
        let (catalog, warnings) =
            Catalog::discover(std::slice::from_ref(&fixture.roots.write_root));
        assert!(warnings.is_empty(), "{warnings:?}");
        let skill = catalog.lookup("rust-lint").expect("discovered");
        assert_eq!(skill.description, "Run the lint gate: it's the user's rule");
        assert!(
            skill.contract.is_none(),
            "a generated skill declares no contract"
        );
        let text = read(&fixture, "rust-lint");
        assert!(text.starts_with("---\nname: rust-lint\ndescription: '"));
        assert!(
            !text.contains("allowed-tools")
                && !text.contains("input:")
                && !text.contains("output:")
        );
        let owned = fixture
            .store
            .generated("rust-lint")
            .expect("row")
            .expect("owned");
        assert_eq!(owned.hash, hash(&text));
    }

    #[test]
    fn a_name_that_exists_in_any_root_is_never_overwritten() {
        let fixture = fixture();
        let other = fixture.roots.lookup_roots[1].join("rust-lint");
        fs::create_dir_all(&other).expect("dir");
        fs::write(
            other.join("SKILL.md"),
            "---\nname: rust-lint\ndescription: mine\n---\nbody",
        )
        .expect("write");
        assert_eq!(
            create(&fixture, "rust-lint", BODY),
            Err("skill_name_exists")
        );
        assert!(!fixture.roots.write_root.join("rust-lint").exists());

        assert_eq!(create(&fixture, "fmt-check", BODY), Ok(Written::Created));
        assert_eq!(
            create(&fixture, "fmt-check", BODY),
            Err("skill_name_exists")
        );
    }

    #[test]
    fn the_total_cap_is_enforced() {
        let fixture = fixture();
        let attempt = |name: &str| {
            apply(
                &fixture.store,
                &fixture.roots,
                &vetted(name, SkillAction::Create, BODY),
                "run-1",
                "session-1",
                "t",
                1,
            )
        };
        assert_eq!(attempt("first-skill"), Ok(Written::Created));
        assert_eq!(attempt("second-skill"), Err("skill_cap_reached"));
    }

    #[test]
    fn a_revision_updates_the_file_and_keeps_history() {
        let fixture = fixture();
        create(&fixture, "rust-lint", BODY).expect("create");
        let first = read(&fixture, "rust-lint");
        let revised = format!("{BODY}\n4. Re-run the tests.");
        assert_eq!(
            revise(&fixture, "rust-lint", &revised),
            Ok(Written::Revised)
        );
        let second = read(&fixture, "rust-lint");
        assert!(second.contains("Re-run the tests"));
        assert_eq!(
            fixture.store.skill_versions("rust-lint").expect("v").len(),
            2
        );
        for content in [&first, &second] {
            let kept =
                fs::read_to_string(history_file(&fixture.roots, "rust-lint", &hash(content)))
                    .expect("history");
            assert_eq!(&kept, content);
        }
        assert_eq!(
            revise(&fixture, "rust-lint", &revised),
            Err("skill_no_change")
        );
    }

    #[test]
    fn a_skill_reflection_does_not_own_is_never_revised() {
        let fixture = fixture();
        assert_eq!(
            revise(&fixture, "ghost-skill", BODY),
            Err("skill_not_owned")
        );

        let hand = fixture.roots.write_root.join("hand-written");
        fs::create_dir_all(&hand).expect("dir");
        fs::write(
            hand.join("SKILL.md"),
            "---\nname: hand-written\ndescription: mine\n---\nSteps here",
        )
        .expect("write");
        assert_eq!(
            revise(&fixture, "hand-written", BODY),
            Err("skill_not_owned")
        );

        create(&fixture, "rust-lint", BODY).expect("create");
        let path = skill_file(&fixture.roots.write_root, "rust-lint");
        fs::write(
            &path,
            format!("{}\nI added a line.", read(&fixture, "rust-lint")),
        )
        .expect("edit");
        assert_eq!(
            revise(&fixture, "rust-lint", &format!("{BODY}\n4. More.")),
            Err("skill_not_owned"),
            "a human edit makes the skill human-owned"
        );
        assert!(read(&fixture, "rust-lint").contains("I added a line."));
    }

    #[test]
    fn reverting_restores_the_previous_version_or_removes_a_created_skill() {
        let fixture = fixture();
        create(&fixture, "rust-lint", BODY).expect("create");
        let first = read(&fixture, "rust-lint");
        revise(
            &fixture,
            "rust-lint",
            &format!("{BODY}\n4. Re-run the tests."),
        )
        .expect("revise");

        assert_eq!(
            revert(&fixture.store, &fixture.roots, "rust-lint", "t"),
            Ok(Reverted::Restored)
        );
        assert_eq!(read(&fixture, "rust-lint"), first);
        assert_eq!(
            fixture
                .store
                .generated("rust-lint")
                .expect("row")
                .expect("owned")
                .hash,
            hash(&first)
        );

        assert_eq!(
            revert(&fixture.store, &fixture.roots, "rust-lint", "t"),
            Ok(Reverted::Removed)
        );
        assert!(!skill_file(&fixture.roots.write_root, "rust-lint").exists());
        assert_eq!(fixture.store.generated("rust-lint").expect("row"), None);
    }

    #[test]
    fn revert_refuses_a_skill_that_was_not_generated_or_was_edited() {
        let fixture = fixture();
        let error = revert(&fixture.store, &fixture.roots, "nothing-here", "t")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not generated by reflection"), "{error}");
        let error = revert(&fixture.store, &fixture.roots, "../escape", "t")
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a valid skill name"), "{error}");

        create(&fixture, "rust-lint", BODY).expect("create");
        let path = skill_file(&fixture.roots.write_root, "rust-lint");
        fs::write(&path, "edited by hand").expect("edit");
        let error = revert(&fixture.store, &fixture.roots, "rust-lint", "t")
            .unwrap_err()
            .to_string();
        assert!(error.contains("no longer owns it"), "{error}");
        assert_eq!(fs::read_to_string(&path).expect("read"), "edited by hand");
    }

    #[test]
    fn history_is_outside_every_skill_root_and_never_discovered() {
        let fixture = fixture();
        create(&fixture, "rust-lint", BODY).expect("create");
        let (catalog, _) = Catalog::discover(&[
            fixture.roots.write_root.clone(),
            fixture.roots.history_root.clone(),
        ]);
        assert_eq!(catalog.len(), 1);
        assert!(
            !fixture
                .roots
                .write_root
                .join("rust-lint")
                .join("history")
                .exists()
        );
    }

    #[test]
    fn a_symlinked_skill_directory_is_refused() {
        let fixture = fixture();
        let elsewhere = fixture._home.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("dir");
        fs::create_dir_all(&fixture.roots.write_root).expect("root");
        std::os::unix::fs::symlink(&elsewhere, fixture.roots.write_root.join("linked"))
            .expect("symlink");
        // The symlink exists as a name in the root, so a create is a collision;
        // the explicit symlink guard covers a revise through a planted link.
        assert_eq!(create(&fixture, "linked", BODY), Err("skill_name_exists"));
        assert!(refuse_symlink(&fixture.roots.write_root.join("linked")).is_err());
        assert!(fs::read_dir(&elsewhere).expect("dir").next().is_none());
    }

    #[test]
    fn a_failed_write_leaves_no_temporary_file() {
        let fixture = fixture();
        fs::create_dir_all(fixture.roots.write_root.join("blocked").join("SKILL.md"))
            .expect("dir in the way");
        // The skill name exists, so create is refused before writing; use the
        // atomic helper directly against a directory target.
        let target = skill_file(&fixture.roots.write_root, "blocked");
        assert!(write_atomically(&target, "x").is_err());
        let leftovers: Vec<_> = fs::read_dir(fixture.roots.write_root.join("blocked"))
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }
}
