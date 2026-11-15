use super::*;

/// `start` composes the `otto acp` runtime without a frontend: `[mcp]
/// enabled = false` reaches the builder and the usage and task stores open.
#[tokio::test]
async fn start_composes_the_runtime_from_the_user_config() {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(
        home.join(".config/otto/config.toml"),
        r#"
default_model = "alpha/gpt-test"

[providers.alpha]
type = "claude"
base_url = "https://api.anthropic.com"
api_key_env = "ALPHA_KEY"
models."gpt-test" = {}

[memory]
enabled = false

[mcp]
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
        },
        &cancel,
        &mut stderr,
    )
    .await
    .unwrap_or_else(|message| panic!("start: {message}"));

    assert!(!started.builder.mcp.enabled);
    assert!(home.join(".otto/usage.db").exists());
    assert!(home.join(".otto/tasks.db").exists());
    started
        .sandbox
        .control
        .close()
        .await
        .expect("close sandbox");
}
