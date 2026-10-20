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
use std::io::Read as _;
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
use crate::fsops::{self, PosixPublish, Publish, PublishMode, PublishOps};
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

/// Writes `content` to `path` through [`fsops::publish_file`]: a temp file in
/// the same directory, renamed into place, then the directory is `fsync`ed.
fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    write_atomically_with(path, content, &PosixPublish)
}

fn write_atomically_with(
    path: &Path,
    content: &str,
    publish: &dyn PublishOps,
) -> std::io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent"))?;
    fs::create_dir_all(directory)?;
    let (parent, name) = fsops::open_parent(path)?;
    let temp = format!(".reflection-file-{}", crate::auth::random_hex::<8>()?);
    fsops::publish_file(
        &parent,
        &Publish {
            name,
            temp: &temp,
            permissions: 0o644,
            mode: PublishMode::Replace,
        },
        content.as_bytes(),
        publish,
    )
    .map_err(|error| error.source)
}

#[cfg(test)]
mod tests;
