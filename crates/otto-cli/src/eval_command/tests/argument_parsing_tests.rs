use crate::eval_command::{Action, parse};

fn arguments(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[test]
fn build_defaults() {
    let invocation = parse(&arguments(&["view-tree", "build", "--session", "abc"])).unwrap();
    let options = invocation.options;
    assert_eq!(invocation.action, Action::Build);
    assert_eq!(options.session.as_deref(), Some("abc"));
    assert_eq!(
        (
            options.view_low_kb,
            options.view_high_kb,
            options.concurrency
        ),
        (64, 128, 8)
    );
    assert!(!options.dry_run);
    assert_eq!(options.cwd, ".");
    assert_eq!(options.prices.cache_read, 0.1);
    assert_eq!(options.prices.bytes_per_token, 4.0);
}

#[test]
fn probes_and_run_defaults() {
    let probes = parse(&arguments(&["view-tree", "probes", "--out", "dir"])).unwrap();
    assert_eq!(probes.action, Action::Probes);
    assert_eq!(probes.options.probe_count, 20);
    let run = parse(&arguments(&["view-tree", "run", "--out", "dir"])).unwrap();
    assert_eq!((run.action, run.options.max_steps), (Action::Run, 12));
    assert!(run.options.probes_file.is_none());
}

#[test]
fn all_accepts_build_and_run_flags() {
    let invocation = parse(&arguments(&[
        "view-tree",
        "all",
        "--session",
        "s",
        "--out",
        "o",
        "--model",
        "m",
        "--view-low",
        "1",
        "--view-high",
        "2",
        "--concurrency",
        "3",
        "--count",
        "4",
        "--max-steps",
        "5",
        "--cache-read",
        "0.2",
        "--cwd",
        "/w",
        "--config",
        "/c",
        "--dry-run",
    ]))
    .unwrap();
    let options = invocation.options.clone();
    assert_eq!(invocation.action, Action::All);
    assert_eq!(
        (
            options.view_low_kb,
            options.view_high_kb,
            options.concurrency
        ),
        (1, 2, 3)
    );
    assert_eq!(
        (
            options.probe_count,
            options.max_steps,
            options.prices.cache_read
        ),
        (4, 5, 0.2)
    );
    assert!(options.dry_run);
    assert_eq!(
        invocation.startup_flags(),
        ["--cwd", "/w", "--config", "/c"]
    );
}

#[test]
fn errors_name_the_problem() {
    let error = |values: &[&str]| parse(&arguments(values)).unwrap_err();
    assert!(error(&[]).contains("requires a target"));
    assert!(error(&["other"]).contains("unknown eval target"));
    assert!(error(&["view-tree"]).contains("requires an action"));
    assert!(error(&["view-tree", "fly"]).contains("unknown eval view-tree action"));
    assert!(error(&["view-tree", "build"]).contains("requires --session"));
    assert!(error(&["view-tree", "run"]).contains("requires --out"));
    assert!(
        error(&["view-tree", "build", "--session", "s", "--bogus", "1"])
            .contains("unknown eval flag")
    );
    assert!(
        error(&["view-tree", "run", "--out", "o", "--concurrency", "2"]).contains("does not apply")
    );
    assert!(error(&["view-tree", "build", "--session"]).contains("requires a value"));
    assert!(
        error(&["view-tree", "build", "--session", "s", "--concurrency", "0"])
            .contains("at least 1")
    );
    assert!(
        error(&[
            "view-tree",
            "build",
            "--session",
            "s",
            "--view-low",
            "9",
            "--view-high",
            "3"
        ])
        .contains("must not exceed")
    );
    assert!(
        error(&[
            "view-tree",
            "build",
            "--session",
            "s",
            "--bytes-per-token",
            "-1"
        ])
        .contains("above 0")
    );
}
