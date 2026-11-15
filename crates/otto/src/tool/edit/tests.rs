use super::*;
use crate::tool::testutil::{run, workspace};
use otto_core::model::{EffectCertainty, OperationDisposition};
use otto_faults::publish::FaultyPublishOps;
use otto_faults::{Fault, FaultError, FaultPlan, Rule, Trigger};
use otto_platform::fsops::PublishStep;

mod diff;
mod matching;
mod random;
mod replacement;

fn publish_fault(step: PublishStep) -> std::sync::Arc<FaultPlan<PublishStep>> {
    FaultPlan::new(vec![Rule::new(
        step,
        Trigger::Always,
        Fault::Before(FaultError::Os(libc::EIO)),
    )])
}

fn sample(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.txt");
    std::fs::write(&path, contents).unwrap();
    (root, path)
}

#[tokio::test]
async fn git_config_is_refused_through_a_symlink() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    std::fs::write(root.path().join(".git/config"), "old\n").unwrap();
    std::os::unix::fs::symlink(".git/config", root.path().join("link")).unwrap();
    let workspace = workspace(root.path());
    let tool = EditTool::new(Arc::clone(&workspace));

    for path in [".git/config", ".GIT/config", "link"] {
        let result = run(
            &tool,
            &format!(r#"{{"path":"{path}","old_text":"old","new_text":"new"}}"#),
        )
        .await;
        assert!(
            result.is_error && result.content.contains("read-only"),
            "{path}: {result:?}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join(".git/config")).unwrap(),
        "old\n"
    );
}

#[tokio::test]
async fn unknown_fields_and_missing_arguments_are_rejected() {
    let (root, _) = sample("same\n");
    let workspace = workspace(root.path());
    let tool = EditTool::new(Arc::clone(&workspace));

    // Malformed JSON and trailing tokens are rejected by the stream
    // decoder before a `RawValue` exists; see `result::tests`.
    let unknown = run(
        &tool,
        r#"{"path":"sample.txt","old_text":"same","new_text":"new","extra":true}"#,
    )
    .await;
    assert!(
        unknown.is_error && unknown.content.contains("unknown field"),
        "{unknown:?}"
    );

    let missing_path = run(&tool, r#"{"old_text":"same","new_text":"new"}"#).await;
    assert!(
        missing_path.is_error && missing_path.content.contains("path"),
        "{missing_path:?}"
    );

    let missing_old = run(&tool, r#"{"path":"sample.txt","new_text":"new"}"#).await;
    assert!(
        missing_old.is_error && missing_old.content.contains("old_text"),
        "{missing_old:?}"
    );
    assert_eq!(
        missing_old.outcome_override.unwrap().effect_certainty,
        EffectCertainty::NotStarted
    );
}

#[tokio::test]
async fn directory_sync_failure_after_edit_rename_is_unknown() {
    let (root, path) = sample("before\n");
    let workspace = workspace(root.path());
    let plan = publish_fault(PublishStep::DirSync);
    let ops = FaultyPublishOps::new(plan.clone());
    let tool = EditTool::with_publish_ops(Arc::clone(&workspace), &ops);

    let result = run(
        &tool,
        r#"{"path":"sample.txt","old_text":"before","new_text":"after"}"#,
    )
    .await;
    plan.assert_all_fired();

    assert!(result.is_error, "{result:?}");
    let outcome = result
        .outcome_override
        .expect("post-rename failure override");
    assert_eq!(outcome.disposition, OperationDisposition::Error);
    assert_eq!(outcome.effect_certainty, EffectCertainty::Unknown);
    assert_eq!(std::fs::read_to_string(path).unwrap(), "after\n");
}

#[tokio::test]
async fn rename_failure_keeps_the_file_and_leaves_no_temporary_file() {
    let (root, path) = sample("before\n");
    let workspace = workspace(root.path());
    let plan = publish_fault(PublishStep::Rename);
    let ops = FaultyPublishOps::new(plan.clone());
    let tool = EditTool::with_publish_ops(Arc::clone(&workspace), &ops);

    let result = run(
        &tool,
        r#"{"path":"sample.txt","old_text":"before","new_text":"after"}"#,
    )
    .await;

    plan.assert_all_fired();
    assert!(result.is_error, "{result:?}");
    // A plain error result: only a post-rename failure overrides the
    // outcome to `Unknown`.
    assert!(result.outcome_override.is_none(), "{result:?}");
    assert_eq!(std::fs::read_to_string(path).unwrap(), "before\n");
    let names: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(names, ["sample.txt"]);
}

#[tokio::test]
async fn traversal_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let nested = root.path().join("inner");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(root.path().join("escape.txt"), "x").unwrap();
    let workspace = workspace(&nested);
    let result = run(
        &EditTool::new(Arc::clone(&workspace)),
        r#"{"path":"../escape.txt","old_text":"x","new_text":"y"}"#,
    )
    .await;
    assert!(
        result.is_error && result.content.contains("escapes workspace"),
        "{result:?}"
    );
}

#[tokio::test]
async fn an_internal_final_symlink_is_edited_through() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target.txt");
    std::fs::write(&target, "write").unwrap();
    std::os::unix::fs::symlink("target.txt", root.path().join("relative-link")).unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("absolute-link")).unwrap();
    let workspace = workspace(root.path());
    let tool = EditTool::new(Arc::clone(&workspace));

    for (link, old, new) in [
        ("relative-link", "write", "edit"),
        ("absolute-link", "edit", "edited"),
    ] {
        let result = run(
            &tool,
            &format!(r#"{{"path":"{link}","old_text":"{old}","new_text":"{new}"}}"#),
        )
        .await;
        assert!(!result.is_error, "edit({link}) = {result:?}");
        assert!(
            std::fs::symlink_metadata(root.path().join(link))
                .unwrap()
                .file_type()
                .is_symlink(),
            "link {link} was replaced"
        );
    }
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "edited");
}
