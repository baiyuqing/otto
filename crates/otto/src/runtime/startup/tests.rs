use super::*;

/// `start` composes the `otto acp` runtime without a frontend, and a
/// disabled component stays off: MCP is switched off in the resolved
/// configuration and the usage store is never opened.
#[tokio::test]
async fn start_honors_the_disabled_components() {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(
        home.join(".config/otto/config.toml"),
        r#"
default_profile = "alpha"

[profiles.alpha]
provider = "openai-compatible"
base_url = "https://example.com/v1"
model = "gpt-test"
api_key_env = "ALPHA_KEY"

[memory]
enabled = false

[sandbox]
driver = "off"
"#,
    )
    .expect("write config");
    let cancel = CancellationToken::new();
    let mut stderr = Vec::new();

    let started = start(
        StartOptions {
            memory_extension: None,
            host_entries: vec![
                format!("HOME={}", home.to_string_lossy()).into_bytes(),
                b"ALPHA_KEY=sk-alpha".to_vec(),
            ],
            config_path: String::new(),
            explicit_config: false,
            cwd: workspace.to_string_lossy().into_owned(),
            overrides: Overrides::default(),
            logger: Arc::new(crate::log::Logger::discard()),
            disabled: Disabled {
                mcp: true,
                usage: true,
                ..Disabled::default()
            },
        },
        &cancel,
        &mut stderr,
    )
    .await
    .unwrap_or_else(|message| panic!("start: {message}"));

    assert!(!started.builder.mcp.enabled);
    assert!(!home.join(".otto/usage.db").exists());
    assert!(
        home.join(".otto/tasks.db").exists(),
        "an enabled store still opens"
    );
    started
        .sandbox
        .control
        .close()
        .await
        .expect("close sandbox");
}

/// The component switches write `enabled = false` for exactly the
/// disabled components.
#[test]
fn disabled_applies_only_the_chosen_switches() {
    let mut file = File::default();
    Disabled {
        skills: true,
        subagents: true,
        reflection: true,
        ..Disabled::default()
    }
    .apply(&mut file);
    assert_eq!(file.skills.enabled, Some(false));
    assert_eq!(file.agents.enabled, Some(false));
    assert!(!file.reflection.enabled);
    assert_eq!(file.mcp.enabled, None);
    assert_eq!(file.memory.enabled, None);
    assert_eq!(file.plugins.enabled, None);
}
