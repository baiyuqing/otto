//! Writing, versioning and reverting the skills reflection generates.
//!
//! This is the only module that writes into a skill root, and it accepts only
//! a [`Vetted`] skill, so nothing reaches disk without passing the checks in
//! `guard` (and the model review in `review`).
//!
//! Ownership: reflection owns a skill only while the file's content hash
//! equals the hash of the last content reflection wrote
//! (`reflection.db`, `generated_skills`). A skill a human wrote, or a
//! generated one a human has since edited, is human-owned: reflection never
//! revises or removes it.
//!
//! Layout: a skill is written to `<write_root>/<name>/SKILL.md`
//! (`~/.otto/skills`). Every content reflection writes is also kept at
//! `<history_root>/<name>/<hash>.md` (`~/.otto/skill-history`), outside every
//! skill root so history is never discovered or loaded. `revert` restores the
//! previous version from there.
//!
//! Concurrency: the write is atomic (temp file in the skill directory, then
//! rename), so a reader sees the old or the new file. Two Otto processes
//! reflecting at once can race on the same name; the loser's content may be
//! overwritten, and the ownership check then reports it as human-owned. That
//! is the safe direction.
//!
//! Errors: a refused write is a drop reason (`&'static str`) the caller
//! counts; revert errors are text for the user.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::guard::Vetted;
use super::output::SkillAction;
use super::store::{SkillWrite, Store};

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
    store: &Store,
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
            let current = fs::read_to_string(skill_file(&roots.write_root, name))
                .map_err(|_| "skill_not_owned")?;
            if hash(&current) != owned.hash {
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
    store: &Store,
    roots: &Roots,
    vetted: &Vetted,
    run_id: &str,
    session_id: &str,
    at: &str,
    maximum_generated: usize,
) -> Result<Written, &'static str> {
    let name = vetted.name();
    let target = skill_file(&roots.write_root, name);
    let content = render(vetted);
    let new_hash = hash(&content);

    let replaced = precheck(store, roots, vetted.action(), name, maximum_generated)?;
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

    save_history(roots, name, &new_hash, &content).map_err(|_| "skill_write_error")?;
    write_atomically(&target, &content).map_err(|_| "skill_write_error")?;
    store
        .record_skill_write(&SkillWrite {
            name: name.to_owned(),
            hash: new_hash,
            run_id: run_id.to_owned(),
            session_id: session_id.to_owned(),
            reason: vetted.reason().to_owned(),
            at: at.to_owned(),
        })
        .map_err(|_| "skill_store_error")?;
    Ok(written)
}

/// Restores the previous version of a reflection-owned skill, or removes it
/// when reflection created it.
pub fn revert(store: &Store, roots: &Roots, name: &str, at: &str) -> Result<Reverted, String> {
    if !crate::skill::is_valid_skill_name(name) {
        return Err(format!("{name:?} is not a valid skill name"));
    }
    let owned = store
        .generated(name)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("{name} was not generated by reflection"))?;
    let target = skill_file(&roots.write_root, name);
    let current = fs::read_to_string(&target)
        .map_err(|_| format!("{name}: {} is missing or unreadable", target.display()))?;
    if hash(&current) != owned.hash {
        return Err(format!(
            "{name} has been edited since reflection wrote it, so reflection no longer owns it; \
             edit or remove {} yourself",
            target.display()
        ));
    }
    refuse_symlink(&roots.write_root.join(name))
        .map_err(|_| format!("{name}: refusing to touch a symbolic link"))?;

    let versions = store.skill_versions(name).map_err(|e| e.to_string())?;
    if let [.., previous, _latest] = versions.as_slice() {
        let kept = fs::read_to_string(history_file(roots, name, previous))
            .map_err(|_| format!("{name}: the previous version is missing from the history"))?;
        if &hash(&kept) != previous {
            return Err(format!(
                "{name}: the history copy of the previous version is corrupt"
            ));
        }
        write_atomically(&target, &kept).map_err(|error| format!("{name}: {error}"))?;
        store
            .pop_skill_version(name, at)
            .map_err(|error| error.to_string())?;
        Ok(Reverted::Restored)
    } else {
        fs::remove_file(&target).map_err(|error| format!("{name}: {error}"))?;
        // Leaves the directory if anything else is in it.
        let _ = fs::remove_dir(roots.write_root.join(name));
        store
            .pop_skill_version(name, at)
            .map_err(|error| error.to_string())?;
        Ok(Reverted::Removed)
    }
}

fn refuse_symlink(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(std::io::Error::other("skill directory is a symbolic link"))
        }
        _ => Ok(()),
    }
}

fn save_history(roots: &Roots, name: &str, hash: &str, content: &str) -> std::io::Result<()> {
    let directory = roots.history_root.join(name);
    fs::create_dir_all(&directory)?;
    fs::set_permissions(&roots.history_root, fs::Permissions::from_mode(0o700))?;
    let path = history_file(roots, name, hash);
    if path.exists() {
        return Ok(());
    }
    write_atomically(&path, content)
}

/// Writes `content` to `path` through a temp file in the same directory, then
/// renames it into place.
fn write_atomically(path: &Path, content: &str) -> std::io::Result<()> {
    let directory = path
        .parent()
        .ok_or_else(|| std::io::Error::other("path has no parent"))?;
    fs::create_dir_all(directory)?;
    let temporary = directory.join(format!(
        ".{}.tmp-{}",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
        let error = revert(&fixture.store, &fixture.roots, "nothing-here", "t").unwrap_err();
        assert!(error.contains("not generated by reflection"), "{error}");
        let error = revert(&fixture.store, &fixture.roots, "../escape", "t").unwrap_err();
        assert!(error.contains("not a valid skill name"), "{error}");

        create(&fixture, "rust-lint", BODY).expect("create");
        let path = skill_file(&fixture.roots.write_root, "rust-lint");
        fs::write(&path, "edited by hand").expect("edit");
        let error = revert(&fixture.store, &fixture.roots, "rust-lint", "t").unwrap_err();
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
