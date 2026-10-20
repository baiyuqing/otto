
use std::collections::HashMap;

use super::*;
use crate::faults::publish::FaultyPublishOps;
use crate::faults::{Fault, FaultError, FaultPlan, Rule, Trigger};
use crate::fsops::PublishStep;
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
    let checked = guard::check(candidate, &entries, &|text: &str| text.to_owned()).expect("check");
    guard::approve(checked, Verdict::NotRequested)
}

struct Fixture {
    _home: tempfile::TempDir,
    roots: Roots,
    store: Store,
}

fn roots_in(home: &Path) -> Roots {
    let write_root = home.join(".otto/skills");
    Roots {
        history_root: home.join(".otto/skill-history"),
        lookup_roots: vec![write_root.clone(), home.join("workspace/.otto/skills")],
        write_root,
    }
}

fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    Fixture {
        roots: roots_in(home.path()),
        _home: home,
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
        body: "Use scripts/report.py with an explicit input path to summarize the report.".into(),
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
    let (catalog, warnings) = Catalog::discover(std::slice::from_ref(&fixture.roots.write_root));
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
    let (catalog, warnings) = Catalog::discover(std::slice::from_ref(&fixture.roots.write_root));
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
        !text.contains("allowed-tools") && !text.contains("input:") && !text.contains("output:")
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
        let kept = fs::read_to_string(history_file(&fixture.roots, "rust-lint", &hash(content)))
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

/// A store that ends the process when a revert records its pop, which
/// is after the skill directory was moved aside.
struct CrashOnPop(Store);

impl ReflectionStore for CrashOnPop {
    fn watermark(&self, session_id: &str) -> super::super::store::Result<Option<String>> {
        self.0.watermark(session_id)
    }
    fn record(&self, row: &super::super::store::RunRow) -> super::super::store::Result<()> {
        self.0.record(row)
    }
    fn last_auto_started(&self, session_id: &str) -> super::super::store::Result<Option<String>> {
        self.0.last_auto_started(session_id)
    }
    fn generated(
        &self,
        name: &str,
    ) -> super::super::store::Result<Option<super::super::store::GeneratedSkill>> {
        self.0.generated(name)
    }
    fn list_generated(
        &self,
    ) -> super::super::store::Result<Vec<super::super::store::GeneratedSkill>> {
        self.0.list_generated()
    }
    fn generated_count(&self) -> super::super::store::Result<usize> {
        self.0.generated_count()
    }
    fn record_skill_write(&self, write: &SkillWrite) -> super::super::store::Result<()> {
        self.0.record_skill_write(write)
    }
    fn skill_versions(&self, name: &str) -> super::super::store::Result<Vec<String>> {
        self.0.skill_versions(name)
    }
    fn pop_skill_version(
        &self,
        _name: &str,
        _at: &str,
    ) -> super::super::store::Result<Option<String>> {
        crate::faults::crash::exit()
    }
    fn runs(
        &self,
        session_id: &str,
    ) -> super::super::store::Result<Vec<(String, super::super::store::Status)>> {
        self.0.runs(session_id)
    }
}

#[test]
fn a_crash_after_a_revert_moves_the_skill_aside_keeps_the_whole_tree_in_a_staging_directory() {
    use crate::faults::crash::{self, CRASH_EXIT, Role};
    crash::run(crash::name_of(&|| {}), |role| match role {
        Role::Child(dir) => {
            let roots = roots_in(dir);
            let store = Store::open_in_memory().expect("store");
            apply(
                &store,
                &roots,
                &vetted("rust-lint", SkillAction::Create, BODY),
                "run-1",
                "session-1",
                "2026-10-02T00:00:00Z",
                30,
            )
            .expect("create");
            let created = fs::read(skill_file(&roots.write_root, "rust-lint")).expect("skill");
            fs::write(dir.join("expected.md"), created).expect("expected");
            let _ = revert(&CrashOnPop(store), &roots, "rust-lint", "t");
        }
        Role::Parent(dir, status) => {
            assert_eq!(
                status.code(),
                Some(CRASH_EXIT),
                "child did not crash at the fault"
            );
            let roots = roots_in(dir);
            let expected = fs::read(dir.join("expected.md")).expect("expected");
            assert!(
                !roots.write_root.join("rust-lint").exists(),
                "the revert had moved the skill aside"
            );
            let staged: Vec<PathBuf> = fs::read_dir(&roots.write_root)
                .expect("skill root")
                .map(|entry| entry.expect("entry").path())
                .filter(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with(".reflection-"))
                })
                .collect();
            let [staged] = staged.try_into().expect("one staging directory");
            assert_eq!(
                fs::read(staged.join("SKILL.md")).expect("the staged tree holds the skill"),
                expected
            );
            // Moving the directory back restores the skill.
            fs::rename(&staged, roots.write_root.join("rust-lint")).expect("restore");
            assert_eq!(
                fs::read(skill_file(&roots.write_root, "rust-lint")).expect("restored skill"),
                expected
            );
        }
    });
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

fn write_failing_at(step: PublishStep) -> (tempfile::TempDir, PathBuf, std::io::Result<()>) {
    let dir = tempfile::tempdir().expect("dir");
    let path = dir.path().join("SKILL.md");
    fs::write(&path, "old").expect("previous");
    let plan = FaultPlan::new(vec![Rule::new(
        step,
        Trigger::Always,
        Fault::Before(FaultError::Os(libc::EIO)),
    )]);
    let result = write_atomically_with(&path, "new", &FaultyPublishOps::new(plan.clone()));
    plan.assert_all_fired();
    (dir, path, result)
}

#[test]
fn the_directory_is_synced_after_the_rename() {
    let (_dir, path, result) = write_failing_at(PublishStep::DirSync);
    assert_eq!(
        result.expect_err("sync fails").raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(fs::read_to_string(path).expect("read"), "new");
}

#[test]
fn a_rename_failure_keeps_the_previous_file_and_leaves_no_temporary_file() {
    let (dir, path, result) = write_failing_at(PublishStep::Rename);
    assert_eq!(
        result.expect_err("rename fails").raw_os_error(),
        Some(libc::EIO)
    );
    assert_eq!(fs::read_to_string(path).expect("read"), "old");
    let names: Vec<_> = fs::read_dir(dir.path())
        .expect("dir")
        .map(|entry| entry.expect("entry").file_name())
        .collect();
    assert_eq!(names, ["SKILL.md"]);
}
