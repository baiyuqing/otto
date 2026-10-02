//! Source guard for the reflection write boundary.
//!
//! Reflection is model-originated, so it may only queue memory *candidates*
//! for a human to review (`Service::propose`, with `Origin::Extractor`). It
//! must never write a record directly, forget one, decide a review, or claim
//! human or migration authority. See
//! `docs/specs/2026-10-02-session-reflection.md` ("Output contract" and
//! "Safety").
//!
//! Skills have their own boundary: only `skillwrite.rs` touches a skill root
//! on disk, it accepts only a `Vetted` skill, and `Vetted` is built only in
//! `guard.rs`, so no code path can write a skill without passing the vetting
//! pipeline.
//!
//! The scan is a substring match over the non-test source of
//! `src/reflection/`. A legitimate new use needs an entry in `EXEMPT` with the
//! reason, not a silenced assertion. Fix a failure by routing the write
//! through `propose`.

use std::path::{Path, PathBuf};

/// Tokens reflection code must not contain, with why.
const FORBIDDEN: &[(&str, &str)] = &[
    (
        ".remember(",
        "writes a record directly; use Service::propose",
    ),
    (
        "RememberRequest",
        "writes a record directly; use ProposeRequest",
    ),
    (
        ".forget(",
        "deletes a record directly; propose a Forget candidate",
    ),
    (
        "ForgetRequest",
        "deletes a record directly; propose a Forget candidate",
    ),
    (".review(", "decides a candidate; only a human reviews"),
    ("ReviewRequest", "decides a candidate; only a human reviews"),
    ("Origin::Human", "claims human authority"),
    ("Origin::Migration", "claims migration authority"),
];

/// Filesystem mutations, allowed only in `skillwrite.rs` (skill files and
/// history). The stores create their own database files through SQLite and
/// `create_dir_all`/`set_permissions`, which are not in this list.
const FILESYSTEM_WRITES: &[&str] = &[
    "fs::write(",
    "fs::rename(",
    "fs::remove_file(",
    "fs::remove_dir(",
    "fs::remove_dir_all(",
    "fs::copy(",
    "OpenOptions",
    "File::create(",
];
const SKILL_WRITER: &str = "skillwrite.rs";
const VETTING: &str = "guard.rs";

/// Allowed uses, as `file:token`.
const EXEMPT: &[&str] = &[];

fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).expect("the reflection directory is readable") {
        let path = entry.expect("a readable entry").path();
        if path.is_dir() {
            found.extend(sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}

/// The source before its test module, so test fixtures may use the store
/// directly. The cut is the `#[cfg(test)]` that opens `mod tests`, not the
/// first `#[cfg(test)]` in the file: a test-only helper earlier in a file must
/// not hide the production code after it from these scans.
fn non_test(source: &str) -> &str {
    source
        .find("#[cfg(test)]\nmod tests")
        .map_or(source, |index| &source[..index])
}

#[test]
fn reflection_only_queues_candidates() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/reflection");
    let files = sources(&dir);
    assert!(!files.is_empty(), "no reflection sources found in {dir:?}");

    let mut offenders = Vec::new();
    for path in &files {
        let source = std::fs::read_to_string(path).expect("a readable source");
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        for line in non_test(&source).lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") {
                continue;
            }
            for (token, why) in FORBIDDEN {
                if line.contains(token) && !EXEMPT.contains(&format!("{file}:{token}").as_str()) {
                    offenders.push(format!("{file}: `{token}` ({why}): {}", line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "reflection may only queue candidates through Service::propose with Origin::Extractor \
         (docs/specs/2026-10-02-session-reflection.md, \"Safety\"); fix the code, or add a \
         justified entry to EXEMPT:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn the_guard_sees_the_propose_path() {
    let source = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/reflection/mod.rs"),
    )
    .expect("a readable source");
    let production = non_test(&source);
    assert!(production.contains(".propose("), "the propose call moved");
    assert!(production.contains("Origin::Extractor"), "the origin moved");
}

#[test]
fn only_skillwrite_mutates_the_filesystem() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/reflection");
    let mut offenders = Vec::new();
    for path in sources(&dir) {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        if file == SKILL_WRITER {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("a readable source");
        for line in non_test(&source).lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for token in FILESYSTEM_WRITES {
                if line.contains(token) {
                    offenders.push(format!("{file}: `{token}`: {}", line.trim()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "only reflection/{SKILL_WRITER} may write, rename, or delete files; route skill writes \
         through it:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn a_vetted_skill_is_built_only_by_the_vetting_module() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/reflection");
    let mut offenders = Vec::new();
    for path in sources(&dir) {
        let file = path.file_name().unwrap().to_string_lossy().into_owned();
        if file == VETTING {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("a readable source");
        for line in non_test(&source).lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains("Vetted {") && !line.contains("struct Vetted") {
                offenders.push(format!("{file}: {}", line.trim()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "`Vetted` may be constructed only in reflection/{VETTING} (through `approve`), so every \
         skill write has passed the checks:\n{}",
        offenders.join("\n")
    );

    let writer = std::fs::read_to_string(dir.join(SKILL_WRITER)).expect("a readable source");
    let production = non_test(&writer);
    assert!(
        production.contains("vetted: &Vetted"),
        "skillwrite::apply must take a Vetted skill"
    );
    let guard = std::fs::read_to_string(dir.join(VETTING)).expect("a readable source");
    let pipeline = non_test(&guard);
    assert!(
        pipeline.contains("pub fn approve(") && pipeline.contains("pub fn check("),
        "the vetting entry points moved"
    );
}
