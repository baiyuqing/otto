use super::ClaudeHook;
use super::manifest::{Discovered, discover, parse};
use crate::tool::root::Root;
use serde_json::{Value, json};
use std::fs;

fn handler(command: &str) -> Value {
    json!({"type": "command", "command": command})
}

fn package(files: &[(&str, Value)]) -> (tempfile::TempDir, Root) {
    let directory = tempfile::tempdir().unwrap();
    for (name, content) in files {
        let path = directory.path().join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content.to_string()).unwrap();
    }
    let root = Root::open(directory.path()).unwrap();
    (directory, root)
}

fn commands(found: &Discovered) -> Vec<String> {
    found
        .hooks
        .iter()
        .map(|hook| format!("{}:{}", hook.event, hook.command))
        .collect()
}

#[test]
fn hooks_json_wrapper_gives_ordered_hooks_with_timeouts() {
    let wrapped = json!({"description": "d", "hooks": {
        "SessionStart": [{"hooks": [
            {"type": "command", "command": "a", "timeout": 15},
            {"type": "command", "command": "b", "timeout": 1.5},
            {"type": "command", "command": "c", "timeout": 900},
            {"type": "command", "command": "d"}
        ]}],
        "UserPromptSubmit": [{"hooks": [handler("e")]}],
        "SessionEnd": [{"hooks": [handler("f")]}]
    }});
    let (_directory, root) = package(&[("hooks/hooks.json", wrapped)]);
    let found = discover(&root, &json!({}));
    assert!(found.diagnostics.is_empty(), "{:?}", found.diagnostics);
    assert_eq!(
        commands(&found),
        [
            "SessionStart:a",
            "SessionStart:b",
            "SessionStart:c",
            "SessionStart:d",
            "UserPromptSubmit:e",
            "SessionEnd:f"
        ]
    );
    let timeouts: Vec<u64> = found
        .hooks
        .iter()
        .map(|hook| hook.timeout_seconds)
        .collect();
    assert_eq!(timeouts, [15, 2, 300, 60, 30, 10]);
}

#[test]
fn manifest_field_accepts_inline_object_paths_and_arrays() {
    let custom = json!({"hooks": {"Stop": [{"hooks": [handler("from-file")]}]}});
    let default_file = json!({"hooks": {"SessionStart": [{"hooks": [handler("from-default")]}]}});
    let (_directory, root) = package(&[
        ("hooks/hooks.json", default_file),
        ("hooks/custom.json", custom),
    ]);
    let inline = json!({"PreToolUse": [{"matcher": "Bash", "hooks": [handler("inline")]}]});

    let single = discover(&root, &json!({"hooks": inline}));
    assert_eq!(
        commands(&single),
        ["SessionStart:from-default", "PreToolUse:inline"]
    );
    assert_eq!(single.hooks[1].matcher, "Bash");

    for path in [
        "./hooks/custom.json",
        "${CLAUDE_PLUGIN_ROOT}/hooks/custom.json",
        "hooks/custom.json",
    ] {
        let found = discover(&root, &json!({"hooks": path}));
        assert!(
            found.diagnostics.is_empty(),
            "{path}: {:?}",
            found.diagnostics
        );
        assert_eq!(
            commands(&found),
            ["SessionStart:from-default", "Stop:from-file"],
            "{path}"
        );
    }

    let wrapped_inline = discover(
        &root,
        &json!({"hooks": {"description": "x", "hooks": inline}}),
    );
    assert_eq!(
        commands(&wrapped_inline),
        ["SessionStart:from-default", "PreToolUse:inline"]
    );

    let array = json!({"hooks": ["./hooks/custom.json", inline, "${CLAUDE_PLUGIN_ROOT}/hooks/custom.json"]});
    let merged = discover(&root, &array);
    assert!(merged.diagnostics.is_empty(), "{:?}", merged.diagnostics);
    assert_eq!(
        commands(&merged),
        [
            "SessionStart:from-default",
            "Stop:from-file",
            "PreToolUse:inline"
        ]
    );
}

#[test]
fn manifest_path_to_hooks_json_does_not_duplicate() {
    let wrapped = json!({"hooks": {"Stop": [{"hooks": [handler("once")]}]}});
    let (_directory, root) = package(&[("hooks/hooks.json", wrapped)]);
    let found = discover(&root, &json!({"hooks": "./hooks/hooks.json"}));
    assert_eq!(commands(&found), ["Stop:once"]);
    assert!(found.diagnostics.is_empty());
}

#[test]
fn hooks_json_problems_are_diagnostics() {
    let cases: [(&str, &str); 2] = [
        (
            "{\"x\": 1}",
            "hooks/hooks.json: invalid hooks/hooks.json: missing `hooks` object",
        ),
        (
            "[]",
            "hooks/hooks.json: invalid hooks/hooks.json: top level must be an object",
        ),
    ];
    for (text, expected) in cases {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join("hooks")).unwrap();
        fs::write(directory.path().join("hooks/hooks.json"), text).unwrap();
        let root = Root::open(directory.path()).unwrap();
        let found = discover(&root, &json!({}));
        assert_eq!(found.diagnostics, [expected]);
        assert!(found.hooks.is_empty());
    }
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("hooks")).unwrap();
    fs::write(directory.path().join("hooks/hooks.json"), "{not json").unwrap();
    let root = Root::open(directory.path()).unwrap();
    let found = discover(&root, &json!({}));
    assert_eq!(found.diagnostics.len(), 1);
    assert!(found.diagnostics[0].starts_with("hooks/hooks.json: invalid hooks/hooks.json: "));
}

#[test]
fn manifest_field_problems_are_diagnostics() {
    let (_directory, root) = package(&[("hooks/bad.json", json!([1]))]);
    let diagnostics = |manifest: Value| discover(&root, &manifest).diagnostics;
    assert_eq!(
        diagnostics(json!({"hooks": 5})),
        ["manifest hooks: invalid manifest hooks field"]
    );
    assert_eq!(
        diagnostics(json!({"hooks": "../x.json"})),
        ["manifest hooks: invalid hooks path ../x.json"]
    );
    assert_eq!(
        diagnostics(json!({"hooks": "/etc/x.json"})),
        ["manifest hooks: invalid hooks path /etc/x.json"]
    );
    assert_eq!(
        diagnostics(json!({"hooks": "./"})),
        ["manifest hooks: invalid hooks path ./"]
    );
    assert_eq!(
        diagnostics(json!({"hooks": "a/../b.json"})),
        ["manifest hooks: invalid hooks path a/../b.json"]
    );
    let missing = diagnostics(json!({"hooks": "./missing.json"}));
    assert_eq!(missing.len(), 1);
    assert!(
        missing[0].starts_with("manifest hooks: cannot read hooks file ./missing.json: "),
        "{missing:?}"
    );
    let not_wrapped = diagnostics(json!({"hooks": "./hooks/bad.json"}));
    assert_eq!(
        not_wrapped,
        ["manifest hooks: invalid hooks file ./hooks/bad.json: top level must be an object"]
    );

    let (_other, other_root) = {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("broken.json"), "{").unwrap();
        let root = Root::open(directory.path()).unwrap();
        (directory, root)
    };
    let broken = discover(&other_root, &json!({"hooks": "broken.json"})).diagnostics;
    assert_eq!(broken.len(), 1);
    assert!(
        broken[0].starts_with("manifest hooks: invalid hooks file broken.json: "),
        "{broken:?}"
    );
}

#[test]
fn every_skip_reason_has_one_exact_diagnostic() {
    let config = json!({
        "Notification": [{"hooks": [handler("n")]}],
        "Stop": "nope",
        "PreCompact": [{"matcher": 3, "hooks": [handler("m")]}],
        "PostCompact": [{"hooks": "nope"}],
        "SessionStart": [{"hooks": [
            5,
            {"command": "x"},
            {"type": "prompt", "command": "x"},
            {"type": "command", "command": "x", "if": "Bash(*)"},
            {"type": "command", "command": "x", "shell": "zsh"},
            {"type": "command"},
            {"type": "command", "command": "   "},
            {"type": "command", "command": "a\u{0}b"},
            {"type": "command", "command": "x", "args": "no"},
            {"type": "command", "command": "x", "args": ["ok", 1]},
            {"type": "command", "command": "x", "args": ["a\u{0}"]},
            {"type": "command", "command": "x", "timeout": 0},
            {"type": "command", "command": "x", "timeout": -3},
            {"type": "command", "command": "x", "timeout": "5"}
        ]}],
        "SessionEnd": [{"matcher": "(", "hooks": [handler("x")]}]
    });
    let found = parse(&config, "src");
    assert!(found.hooks.is_empty());
    let mut expected = vec![
        "src: hook event Notification never fires in Otto",
        "src: invalid hook groups for Stop",
        "src: invalid hook matcher for PreCompact",
        "src: invalid hook group for PostCompact",
        "src: invalid hook handler for SessionStart",
        "src: missing hook type for SessionStart",
        "src: unsupported hook type prompt for SessionStart",
        "src: unsupported hook condition for SessionStart",
        "src: unsupported hook shell zsh for SessionStart",
        "src: invalid hook command for SessionStart",
        "src: invalid hook command for SessionStart",
        "src: invalid hook command for SessionStart",
        "src: invalid hook args for SessionStart",
        "src: invalid hook args for SessionStart",
        "src: invalid hook args for SessionStart",
        "src: invalid hook timeout for SessionStart",
        "src: invalid hook timeout for SessionStart",
        "src: invalid hook timeout for SessionStart",
        "src: invalid hook matcher ( for SessionEnd",
    ];
    let mut actual = found.diagnostics.clone();
    expected.sort();
    actual.sort();
    assert_eq!(actual, expected);
    assert_eq!(
        parse(&json!([]), "src").diagnostics,
        ["src: invalid hook configuration"]
    );
}

#[test]
fn async_handler_is_kept_with_a_diagnostic_and_unknown_keys_are_ignored() {
    let config = json!({"Stop": [{"hooks": [
        {"type": "command", "command": "x", "async": true, "once": true, "onFailure": "x", "statusMessage": "s", "other": 1}
    ]}]});
    let found = parse(&config, "src");
    assert_eq!(found.hooks.len(), 1);
    assert_eq!(
        found.diagnostics,
        ["src: async hook for Stop runs synchronously"]
    );
    let quiet = parse(
        &json!({"Stop": [{"hooks": [{"type": "command", "command": "x", "async": false}]}]}),
        "src",
    );
    assert!(quiet.diagnostics.is_empty() && quiet.hooks.len() == 1);
}

#[test]
fn matcher_star_is_stored_empty_and_args_and_shell_are_kept() {
    let config = json!({"PreToolUse": [
        {"matcher": "*", "hooks": [{"type": "command", "command": "/bin/tool", "args": ["--a", "b"], "shell": "bash"}]},
        {"matcher": "Edit|Write", "hooks": [handler("same")]},
        {"matcher": "Edit|Write", "hooks": [handler("same")]},
        {"matcher": "Bash", "hooks": [handler("same")]}
    ]});
    let found = parse(&config, "src");
    assert!(found.diagnostics.is_empty());
    assert_eq!(found.hooks.len(), 3);
    assert_eq!(found.hooks[0].matcher, "");
    assert_eq!(
        found.hooks[0].args.as_deref(),
        Some(&["--a".to_string(), "b".to_string()][..])
    );
    assert_eq!(found.hooks[0].shell, "bash");
    assert_eq!(found.hooks[1].matcher, "Edit|Write");
    assert_eq!(found.hooks[2].matcher, "Bash");
}

#[test]
fn token_economizer_configuration_parses_without_diagnostics() {
    let command = "\"${CLAUDE_PLUGIN_ROOT}/scripts/te-hook\"";
    let entry = |timeout: u64| json!([{"hooks": [{"type": "command", "command": command, "timeout": timeout}]}]);
    let config = json!({
        "SessionStart": entry(15),
        "UserPromptSubmit": entry(20),
        "PostToolUse": entry(60),
        "PostToolUseFailure": entry(60),
        "Stop": entry(10),
        "SessionEnd": entry(10)
    });
    let (_directory, root) = package(&[("hooks/hooks.json", json!({"hooks": config}))]);
    let found = discover(&root, &json!({"name": "te"}));
    assert!(found.diagnostics.is_empty(), "{:?}", found.diagnostics);
    assert_eq!(found.hooks.len(), 6);
    assert!(
        found
            .hooks
            .iter()
            .all(|hook| hook.command == command && hook.matcher.is_empty())
    );
    let timeouts: Vec<u64> = found
        .hooks
        .iter()
        .map(|hook| hook.timeout_seconds)
        .collect();
    assert_eq!(timeouts, [15, 20, 60, 60, 10, 10]);
}

#[test]
fn hook_serialization_omits_defaults_and_round_trips() {
    let hook = ClaudeHook {
        event: "Stop".into(),
        matcher: String::new(),
        command: "run".into(),
        args: None,
        shell: "sh".into(),
        timeout_seconds: 20,
    };
    let value = serde_json::to_value(&hook).unwrap();
    assert_eq!(
        value,
        json!({"event": "Stop", "command": "run", "timeout_seconds": 20})
    );
    assert_eq!(serde_json::from_value::<ClaudeHook>(value).unwrap(), hook);
}
