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

/// Starts `otto acp` with `backend = "remote"` at `url`. `memory` is added
/// to `[memory]`; `token`, when set, is the `OTTO_MEM_TOKEN` value.
async fn start_remote(
    url: &str,
    memory: &str,
    token: Option<&str>,
    extension: Option<super::super::wiring::MemoryExtension>,
) -> (Result<Started, String>, String, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("tempdir");
    let home = directory.path().join("home");
    let workspace = directory.path().join("workspace");
    std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    std::fs::write(
        home.join(".config/otto/config.toml"),
        format!(
            r#"
default_profile = "alpha"

[profiles.alpha]
provider = "openai-compatible"
base_url = "https://example.com/v1"
model = "gpt-test"
api_key_env = "ALPHA_KEY"

[memory]
enabled = true
backend = "remote"
{memory}

[memory.remote]
organization = "acme"
url = "{url}"
user = "u1"

[sandbox]
driver = "off"
"#
        ),
    )
    .expect("write config");
    let mut host_entries = vec![
        format!("HOME={}", home.to_string_lossy()).into_bytes(),
        b"ALPHA_KEY=sk-alpha".to_vec(),
    ];
    if let Some(token) = token {
        host_entries.push(format!("OTTO_MEM_TOKEN={token}").into_bytes());
    }
    let mut stderr = Vec::new();
    let started = start(
        StartOptions {
            memory_extension: extension,
            host_entries,
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
        &CancellationToken::new(),
        &mut stderr,
    )
    .await;
    (
        started,
        String::from_utf8_lossy(&stderr).into_owned(),
        directory,
    )
}

#[tokio::test]
async fn remote_memory_without_a_token_runs_without_memory() {
    let (started, stderr, directory) =
        start_remote("https://mem.example.test", "", None, None).await;
    let started = started.expect("start");
    assert!(
        stderr.contains("OTTO_MEM_TOKEN is unset or empty; continuing without memory"),
        "{stderr}"
    );
    assert!(started.builder.memory.extension.is_none());
    assert!(started.builder.memory.client.is_none());
    assert!(!started.builder.memory.usable);
    assert!(
        !directory.path().join("home/.otto/memory").exists(),
        "the files store is not opened"
    );
    started.sandbox.control.close().await.expect("close");
}

#[tokio::test]
async fn remote_memory_without_a_token_fails_when_required() {
    let (started, _, _directory) =
        start_remote("https://mem.example.test", "required = true", None, None).await;
    let message = started.err().expect("startup fails");
    assert!(message.contains("OTTO_MEM_TOKEN is unset"), "{message}");
}

#[tokio::test]
async fn remote_memory_rejects_an_injected_extension() {
    let service = crate::memory::remote::RemoteService::new(crate::memory::remote::Config {
        url: "https://other.example.test".into(),
        token: "t".into(),
        recall_timeout: std::time::Duration::from_secs(1),
        logger: Arc::new(crate::log::Logger::discard()),
    })
    .expect("service");
    let extension = super::super::wiring::MemoryExtension::new(
        Arc::new(service),
        otto_core::memory::Scope {
            namespace: "user".into(),
            id: "u2".into(),
        },
    );
    let (started, _, _directory) = start_remote(
        "https://mem.example.test",
        "",
        Some("mem-token"),
        Some(extension),
    )
    .await;
    let message = started.err().expect("startup fails");
    assert!(message.contains("injected memory extension"), "{message}");
}

/// The token is a secret like a provider key: it is redacted and its
/// variable is kept out of sandboxed commands. A workspace `otto serve`
/// loads later gets a grant for its own scope, so the client does not
/// refuse its writes.
#[tokio::test(flavor = "multi_thread")]
async fn remote_memory_with_a_token_selects_the_service() {
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let url = format!("http://{}", closed.local_addr().expect("addr"));
    drop(closed);
    let (started, _, directory) = start_remote(&url, "", Some("mem-secret-token"), None).await;
    let started = started.expect("start");
    let builder = &started.builder;
    assert!(builder.memory.usable);
    let client = builder.memory.client.as_ref().expect("client");
    assert_eq!(client.describe().identity, "remote");
    assert_eq!(
        client.mode(),
        otto_core::memory::AuthorizationMode::DelegatedLearning
    );
    let secrets = builder.secret_values(Some(&started.runtime));
    assert!(secrets.iter().any(|value| value == "mem-secret-token"));
    assert!(secrets.iter().any(|value| value == "OTTO_MEM_TOKEN"));
    assert!(
        sandbox_provider_environment_names(&builder.config, "")
            .contains(&"OTTO_MEM_TOKEN".to_string())
    );

    let second = directory.path().join("second");
    std::fs::create_dir_all(&second).expect("second workspace");
    let second = second.canonicalize().expect("canonical");
    let mut stderr = Vec::new();
    let (loaded, sandbox) = load_workspace(
        Arc::clone(&builder.shared),
        &second,
        &Disabled::default(),
        &CancellationToken::new(),
        &mut stderr,
    )
    .await
    .expect("load workspace");
    assert_ne!(loaded.workspace_scope, builder.workspace_scope);
    let request = otto_core::memory::RememberRequest {
        key: None,
        context: loaded.memory_context(otto_core::memory::Origin::Model, None),
        request_id: crate::memory::new_id().expect("id"),
        content: otto_core::memory::Content::Text("fact".into()),
        target: None,
        metadata: Default::default(),
    };
    let error = loaded
        .memory
        .client
        .as_ref()
        .expect("client")
        .remember(&request, &CancellationToken::new())
        .await
        .expect_err("the port is closed");
    assert_ne!(error.kind, otto_core::memory::ErrorKind::Unauthorized);
    sandbox.control.close().await.expect("close");
    started.sandbox.control.close().await.expect("close");
}
