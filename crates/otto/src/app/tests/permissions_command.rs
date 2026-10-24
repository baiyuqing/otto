//! `/permissions` argument parsing and report text.

use super::*;
use crate::app::{
    PERMISSIONS_USAGE, PermissionsChange, format_permissions, parse_permissions_command,
};
use otto_core::approval::{ApprovalPolicy, ApprovalReviewer, ApprovalSettings};

#[test]
fn parser_accepts_presets_legacy_synonyms_policy_and_reviewer() {
    let current = ApprovalSettings::default();
    assert_eq!(parse_permissions_command("", current), Ok(None));
    for (args, mode) in [
        ("manual", ApprovalMode::Manual),
        ("off", ApprovalMode::Manual),
        ("auto", ApprovalMode::Auto),
        ("on", ApprovalMode::Auto),
        (" yolo ", ApprovalMode::Yolo),
    ] {
        assert_eq!(
            parse_permissions_command(args, current),
            Ok(Some(PermissionsChange::Preset(mode))),
            "{args}"
        );
    }
    assert_eq!(
        parse_permissions_command("policy never", current),
        Ok(Some(PermissionsChange::Settings(
            ApprovalPolicy::Never,
            current.reviewer
        )))
    );
    assert_eq!(
        parse_permissions_command("reviewer auto_review", current),
        Ok(Some(PermissionsChange::Settings(
            current.policy,
            ApprovalReviewer::AutoReview
        )))
    );
}

#[test]
fn parser_rejects_unknown_arguments_with_the_permissions_usage() {
    let current = ApprovalSettings::default();
    for args in [
        "bogus",
        "yolo now",
        "policy",
        "reviewer bypass",
        "policy auto",
    ] {
        assert_eq!(
            parse_permissions_command(args, current),
            Err(PERMISSIONS_USAGE.to_string()),
            "{args}"
        );
    }
    assert_eq!(
        PERMISSIONS_USAGE,
        "usage: /permissions [manual|auto|yolo | policy on_request|never | reviewer user|auto_review]"
    );
}

#[test]
fn report_starts_with_the_mode_line_and_labels_never() {
    let mut settings = ApprovalSettings {
        policy: ApprovalPolicy::OnRequest,
        reviewer: ApprovalReviewer::User,
        ..ApprovalSettings::default()
    };
    assert!(
        format_permissions(settings)
            .starts_with("Approval mode: MANUAL\nApproval policy: on_request\nReviewer: user\n")
    );
    settings.policy = ApprovalPolicy::Never;
    assert!(
        format_permissions(settings)
            .starts_with("Approval mode: NEVER (reviewer disabled)\nApproval policy: never\n")
    );
}

#[tokio::test]
async fn permissions_yolo_sets_yolo_and_the_auto_alias_text_is_the_same_command() {
    let workspace = tempfile::tempdir().unwrap();
    let sessions = tempfile::tempdir().unwrap();
    let controller = controller(workspace.path(), sessions.path()).await;
    let report = controller.permissions_command("yolo").unwrap();
    assert!(report.starts_with("Approval mode: YOLO\n"), "{report}");
    assert_eq!(controller.info().approval_mode, ApprovalMode::Yolo);
    let report = controller.permissions_command("manual").unwrap();
    assert!(report.starts_with("Approval mode: MANUAL\n"), "{report}");
    assert_eq!(
        controller.permissions_command("bogus").unwrap_err(),
        PERMISSIONS_USAGE
    );
}
