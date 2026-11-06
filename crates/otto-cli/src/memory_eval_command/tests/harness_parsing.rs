//! Tests of the `otto eval memory-retraction` argument grammar.

use std::path::PathBuf;

use crate::eval_command::{EvalCommand, parse_command};
use crate::memory_eval_command::argument_parsing::{
    Condition, DEFAULT_MAX_CALLS, Layer, Options, Variant, parse,
};

fn arguments(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn parse_error(values: &[&str]) -> String {
    parse(&arguments(values)).expect_err("the arguments must be rejected")
}

#[test]
fn defaults_run_every_layer_condition_and_variant() {
    let invocation = parse(&[]).unwrap();
    assert_eq!(invocation.options, Options::default());
    assert_eq!(invocation.options.layer, Layer::All);
    assert_eq!(invocation.options.conditions, Condition::ALL.to_vec());
    assert_eq!(invocation.options.variants, Variant::ALL.to_vec());
    assert_eq!(invocation.options.repeat, 1);
    assert_eq!(invocation.options.max_calls, DEFAULT_MAX_CALLS);
    assert!(invocation.options.judge);
    assert!(!invocation.is_dry_run());
}

#[test]
fn every_flag_sets_its_option() {
    let invocation = parse(&arguments(&[
        "--layer",
        "3",
        "--conditions",
        "a,D",
        "--variants",
        "tool_result,native_recall",
        "--items",
        "install_command_pnpm,second",
        "--repeat",
        "20",
        "--out",
        "/tmp/out",
        "--max-calls",
        "100000",
        "--no-judge",
        "--dry-run",
        "--cwd",
        "/work",
        "--config",
        "/work/otto.toml",
    ]))
    .unwrap();
    let options = invocation.options;
    assert_eq!(options.layer, Layer::Three);
    assert_eq!(options.conditions, vec![Condition::A, Condition::D]);
    assert_eq!(
        options.variants,
        vec![Variant::ToolResult, Variant::NativeRecall]
    );
    assert_eq!(
        options.items,
        Some(vec!["install_command_pnpm".to_owned(), "second".to_owned()])
    );
    assert_eq!(options.repeat, 20);
    assert_eq!(options.out, Some(PathBuf::from("/tmp/out")));
    assert_eq!(options.max_calls, 100_000);
    assert!(!options.judge);
    assert!(options.dry_run);
    assert_eq!(options.cwd, "/work");
    assert_eq!(options.config.as_deref(), Some("/work/otto.toml"));
}

#[test]
fn layer_values_two_and_all_parse() {
    assert_eq!(
        parse(&arguments(&["--layer", "2"])).unwrap().options.layer,
        Layer::Two
    );
    assert_eq!(
        parse(&arguments(&["--layer", "all"]))
            .unwrap()
            .options
            .layer,
        Layer::All
    );
}

#[test]
fn startup_flags_carry_cwd_and_config() {
    let invocation = parse(&arguments(&["--cwd", "/work", "--config", "/c.toml"])).unwrap();
    assert_eq!(
        invocation.startup_flags(),
        vec!["--cwd", "/work", "--config", "/c.toml"]
    );
    let defaults = parse(&[]).unwrap();
    assert_eq!(defaults.startup_flags(), vec!["--cwd", "."]);
}

#[test]
fn errors_start_with_the_command_prefix_and_name_the_flag() {
    let prefix = "otto: eval memory-retraction:";
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec!["--bogus"], "--bogus"),
        (vec!["--layer"], "--layer"),
        (vec!["--layer", "4"], "--layer"),
        (vec!["--conditions", "A,E"], "--conditions"),
        (vec!["--conditions", "A,a"], "--conditions"),
        (vec!["--conditions", "A,,B"], "--conditions"),
        (vec!["--variants", "native_recall,nope"], "--variants"),
        (vec!["--variants", "tool_result,tool_result"], "--variants"),
        (vec!["--items", ""], "--items"),
        (vec!["--repeat", "0"], "--repeat"),
        (vec!["--repeat", "21"], "--repeat"),
        (vec!["--repeat", "x"], "--repeat"),
        (vec!["--max-calls", "0"], "--max-calls"),
        (vec!["--max-calls", "100001"], "--max-calls"),
        (vec!["--out", ""], "--out"),
        (vec!["--layer", "2", "--layer", "3"], "--layer"),
        (vec!["--no-judge", "--no-judge"], "--no-judge"),
        (vec!["--dry-run", "--dry-run"], "--dry-run"),
    ];
    for (values, flag) in cases {
        let message = parse_error(&values);
        assert!(message.starts_with(prefix), "{values:?}: {message}");
        assert!(message.contains(flag), "{values:?}: {message}");
    }
}

#[test]
fn a_flag_without_a_value_is_rejected() {
    let message = parse_error(&["--repeat"]);
    assert!(message.contains("requires a value"), "{message}");
}

#[test]
fn eval_routes_each_target_to_its_parser() {
    let memory = parse_command(&arguments(&["memory-retraction", "--layer", "2"])).unwrap();
    match memory {
        EvalCommand::MemoryRetraction(invocation) => {
            assert_eq!(invocation.options.layer, Layer::Two);
        }
        other => panic!("expected the memory-retraction target, got {other:?}"),
    }
    let view_tree = parse_command(&arguments(&["view-tree", "build", "--session", "s"])).unwrap();
    assert!(matches!(view_tree, EvalCommand::ViewTree(_)));
}

#[test]
fn eval_errors_name_both_targets() {
    for values in [vec![], vec!["other"]] {
        let message = parse_command(&arguments(&values)).expect_err("rejected");
        assert!(message.contains("view-tree"), "{message}");
        assert!(message.contains("memory-retraction"), "{message}");
    }
}
