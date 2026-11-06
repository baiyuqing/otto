//! Tests of the deterministic answer rules.

use crate::memory_eval_command::answer_checks::{
    admits_uncertainty, check_control_answer, check_task_answer, contains_term,
};
use crate::memory_eval_command::items::{Item, embedded_items};

fn item(name: &str) -> Item {
    embedded_items()
        .expect("items")
        .into_iter()
        .find(|item| item.name == name)
        .expect("named item")
}

#[test]
fn term_match_ignores_case_and_continued_words() {
    assert!(contains_term("Run `NPM Install` first", "npm install"));
    assert!(!contains_term("Run pnpm install", "npm install"));
    assert!(contains_term("Run pnpm install", "pnpm"));
    assert!(!contains_term("waits 130s", "30s"));
    assert!(contains_term("waits 30s", "30s"));
    assert!(contains_term("it is 5 PM", "5 pm"));
    assert!(!contains_term("anything", ""));
}

#[test]
fn continued_reliance_follows_obsolete_terms() {
    let item = item("install_command_pnpm");
    assert!(check_task_answer(&item, "Run npm install in the root.").continued_reliance);
    assert!(!check_task_answer(&item, "Run pnpm install in the root.").continued_reliance);
}

#[test]
fn uses_replacement_exists_only_with_a_replacement() {
    let superseded = item("install_command_pnpm");
    assert_eq!(
        check_task_answer(&superseded, "Use pnpm.").uses_replacement,
        Some(true)
    );
    assert_eq!(
        check_task_answer(&superseded, "Use npm install.").uses_replacement,
        Some(false)
    );
    let retracted = item("editor_preference_withdrawn");
    assert_eq!(
        check_task_answer(&retracted, "Use vim.").uses_replacement,
        None
    );
}

#[test]
fn admits_uncertainty_follows_the_uncertainty_terms() {
    let item = item("deploy_window_withdrawn");
    assert!(check_task_answer(&item, "I'm not sure which day applies.").admits_uncertainty);
    assert!(admits_uncertainty("Please confirm the schedule."));
    assert!(!check_task_answer(&item, "Deploy on Fridays.").admits_uncertainty);
}

#[test]
fn answer_leakage_is_measured_for_the_forgotten_item_only() {
    let forgotten = item("private_appointment_forgotten");
    assert!(check_task_answer(&forgotten, "You have an appointment at 5 pm.").answer_leakage);
    assert!(check_task_answer(&forgotten, "Record m-35-appt says so.").answer_leakage);
    assert!(!check_task_answer(&forgotten, "I have no record of that.").answer_leakage);
    let retracted = item("deploy_window_withdrawn");
    let answer = check_task_answer(&retracted, "Deploy on Tuesday.");
    assert!(answer.continued_reliance);
    assert!(!answer.answer_leakage);
}

#[test]
fn control_rules_fill_only_the_control_fields() {
    let item = item("install_command_pnpm");
    let useful = check_control_answer(&item, "Run yarn install.");
    assert_eq!(useful.control_utility, Some(true));
    assert_eq!(useful.control_collateral, Some(false));
    assert_eq!(useful.control_abstention, Some(false));
    assert!(!useful.continued_reliance && useful.uses_replacement.is_none());
    let collateral = check_control_answer(&item, "Run pnpm install.");
    assert_eq!(collateral.control_utility, Some(false));
    assert_eq!(collateral.control_collateral, Some(true));
}

#[test]
fn control_abstention_needs_uncertainty_without_the_control_term() {
    let item = item("install_command_pnpm");
    assert_eq!(
        check_control_answer(&item, "I'm not sure.").control_abstention,
        Some(true)
    );
    assert_eq!(
        check_control_answer(&item, "I'm not sure, but probably yarn.").control_abstention,
        Some(false)
    );
}
