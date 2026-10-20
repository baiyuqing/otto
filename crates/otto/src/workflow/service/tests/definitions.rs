//! Definition parsing and write-coordination validation.

use super::*;

#[test]
fn definition_accepts_a_bounded_dag() {
    let definition = parse_definition(
        "review-change",
        br#"
version = 1
description = "Review a change"

[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence"

[[steps]]
id = "review"
agent = "reviewer"
prompt = "Review it"
needs = ["research"]

[[steps]]
id = "approve"
kind = "approval"
prompt = "Ship it?"
needs = ["review"]
"#,
        &|name| matches!(name, "researcher" | "reviewer"),
    )
    .expect("definition");

    assert_eq!(definition.name, "review-change");
    assert_eq!(definition.steps[0].kind, StepKind::Agent);
    assert_eq!(definition.steps[2].kind, StepKind::Approval);
    assert_eq!(definition.steps[2].needs, ["review"]);
    assert_eq!(definition.hash.len(), 64);
}

#[test]
fn write_coordination_rejects_parallel_global_writers() {
    let mut definition = definition(
        br#"
version = 1
[[steps]]
id = "left"
agent = "left"
prompt = "left"
[[steps]]
id = "right"
agent = "right"
prompt = "right"
"#,
    );
    definition.agents = vec![
        agent_snapshot(
            "left",
            Some(&["read", "edit"]),
            SnapshotWritePolicy::SingleWriter,
            &[],
        ),
        agent_snapshot(
            "right",
            Some(&["write"]),
            SnapshotWritePolicy::SingleWriter,
            &[],
        ),
    ];

    let error = validate_write_coordination(&definition).expect_err("parallel writers fail");

    assert!(error.message().contains("may run concurrently"), "{error}");
}

#[test]
fn write_coordination_allows_readonly_parallel_and_serial_writers() {
    let mut serial = definition(
        br#"
version = 1
[[steps]]
id = "left"
agent = "left"
prompt = "left"
[[steps]]
id = "right"
agent = "right"
prompt = "right"
needs = ["left"]
[[steps]]
id = "plan"
agent = "planner"
prompt = "plan"
"#,
    );
    serial.agents = vec![
        agent_snapshot(
            "left",
            Some(&["edit"]),
            SnapshotWritePolicy::SingleWriter,
            &[],
        ),
        agent_snapshot(
            "right",
            Some(&["write"]),
            SnapshotWritePolicy::SingleWriter,
            &[],
        ),
        agent_snapshot(
            "planner",
            Some(&["read", "edit"]),
            SnapshotWritePolicy::ProposeOnly,
            &[],
        ),
    ];

    validate_write_coordination(&serial).expect("serial writers and propose-only planner are ok");
}

#[test]
fn write_coordination_allows_disjoint_owned_paths() {
    let mut definition = definition(
        br#"
version = 1
[[steps]]
id = "core"
agent = "core"
prompt = "core"
[[steps]]
id = "docs"
agent = "docs"
prompt = "docs"
"#,
    );
    definition.agents = vec![
        agent_snapshot(
            "core",
            Some(&["edit"]),
            SnapshotWritePolicy::OwnedPaths,
            &["crates/otto-core/**"],
        ),
        agent_snapshot(
            "docs",
            Some(&["write"]),
            SnapshotWritePolicy::OwnedPaths,
            &["docs/**"],
        ),
    ];

    validate_write_coordination(&definition).expect("disjoint owned paths are ok");
}

#[test]
fn write_coordination_rejects_overlapping_owned_paths() {
    let mut definition = definition(
        br#"
version = 1
[[steps]]
id = "a"
agent = "a"
prompt = "a"
[[steps]]
id = "b"
agent = "b"
prompt = "b"
"#,
    );
    definition.agents = vec![
        agent_snapshot(
            "a",
            Some(&["edit"]),
            SnapshotWritePolicy::OwnedPaths,
            &["crates/otto/**"],
        ),
        agent_snapshot(
            "b",
            Some(&["write"]),
            SnapshotWritePolicy::OwnedPaths,
            &["crates/otto/src/**"],
        ),
    ];

    let error = validate_write_coordination(&definition).expect_err("overlap fails");

    assert!(error.message().contains("may run concurrently"), "{error}");
}

#[test]
fn definition_accepts_handoff_to_a_known_agent() {
    let definition = parse_definition(
        "handoff-change",
        br#"
version = 1
[[steps]]
id = "research"
agent = "researcher"
prompt = "Collect evidence"
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over and review it"
needs = ["research"]
"#,
        &|name| matches!(name, "researcher" | "reviewer"),
    )
    .expect("definition");

    assert_eq!(definition.steps[1].kind, StepKind::Handoff);
    assert_eq!(definition.steps[1].agent, "reviewer");
    assert_eq!(definition.steps[1].needs, ["research"]);
}

#[test]
fn definition_rejects_a_root_handoff() {
    let error = parse_definition(
        "handoff",
        br#"
version = 1
[[steps]]
id = "review"
kind = "handoff"
agent = "reviewer"
prompt = "Take over"
"#,
        &|_| true,
    )
    .expect_err("root handoff");

    assert_eq!(
        error.message(),
        "workflow handoff handoff step \"review\" requires a dependency"
    );
}

#[test]
fn definition_rejects_cycles_before_execution() {
    let error = parse_definition(
        "cycle",
        br#"
version = 1
[[steps]]
id = "one"
agent = "worker"
prompt = "one"
needs = ["two"]
[[steps]]
id = "two"
agent = "worker"
prompt = "two"
needs = ["one"]
"#,
        &|_| true,
    )
    .expect_err("cycle");

    assert_eq!(
        error.message(),
        "workflow cycle contains a dependency cycle"
    );
}

#[test]
fn definition_read_refuses_a_symlink() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().expect("directory");
    let source = directory.path().join("source.toml");
    let link = directory.path().join("link.toml");
    std::fs::write(&source, "version = 1").expect("write");
    symlink(&source, &link).expect("symlink");
    assert!(read_definition_file(&link).is_err());
}
