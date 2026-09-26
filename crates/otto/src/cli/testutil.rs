//! Shared scaffolding for the `cli` unit tests: a two-profile configuration
//! that resolves offline, and a [`Controller`] built on temporary
//! directories. Used by `controller.rs` and `repl.rs`, which both need a
//! working composition root but never reach the provider.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use otto_core::config::memory::MemoryRuntime;
use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::config::{File, Profile};
use otto_core::model::{Block, BlockType, Message, Role};

use super::controller::Controller;
use super::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};
use super::runtime_builder::{Builder, Shared, leaked_workspace, resolve_initial_runtime};

pub fn config() -> File {
    let mut profiles = HashMap::new();
    profiles.insert(
        "alpha".to_string(),
        Profile {
            provider: "openai-compatible".to_string(),
            model: "gpt-alpha".to_string(),
            base_url: "https://gw.example.com/v1".to_string(),
            api_key_env: "ALPHA_KEY".to_string(),
            ..Profile::default()
        },
    );
    profiles.insert(
        "beta".to_string(),
        Profile {
            provider: "openai-compatible".to_string(),
            model: "gpt-beta".to_string(),
            base_url: "https://gw.example.com/v1".to_string(),
            api_key_env: "BETA_KEY".to_string(),
            ..Profile::default()
        },
    );
    File {
        default_profile: "alpha".to_string(),
        profiles,
        ..File::default()
    }
}

pub fn environment() -> HashMap<String, String> {
    HashMap::from([
        ("ALPHA_KEY".to_string(), "sk-alpha-secret".to_string()),
        ("BETA_KEY".to_string(), "sk-beta-secret".to_string()),
    ])
}

pub fn builder(workspace_root: &Path, session_root: &Path) -> Builder {
    let shared = Arc::new(Shared {
        config_path: workspace_root.join("config.toml"),
        config: config(),
        environment: environment(),
        home: workspace_root.to_string_lossy().into_owned(),
        session_root: session_root.to_path_buf(),
        shell: "/bin/sh".to_string(),
        no_session: false,
        host_entries: Vec::new(),
        overrides: Overrides {
            shell_timeout: Duration::from_secs(30),
            max_output_bytes: 65536,
            ..Overrides::default()
        },
        sandbox_secrets_baseline: Vec::new(),
        sandbox_secrets_baseline_complete: true,
        auth_path: String::new(),
        auth_credentials: crate::auth::Credentials::default(),
        auth_credentials_loaded: false,
        usage: None,
        task_recorder: None,
        skill_checker: None,
        memory: Default::default(),
    });
    let mut builder = Builder::for_workspace(
        shared,
        leaked_workspace(workspace_root).expect("workspace"),
        workspace_root.to_string_lossy().into_owned(),
        otto_core::config::McpRuntime {
            enabled: false,
            call_timeout_secs: 60,
            connect_timeout_secs: 20,
            servers: Vec::new(),
        },
    );
    builder.sandbox_info = SandboxInfo {
        mode: SandboxMode::Off,
        network: SandboxNetwork::Unconfined,
        bash_available: false,
        reason: SandboxReason::None,
    };
    builder
}

/// A [`Shared`] with an offline (`driver = "off"`) sandbox, for tests that
/// load a workspace at runtime (`run::load_workspace`,
/// `serve::Workspaces::load`) without a Seatbelt subprocess or a dependence
/// on the host running the tests.
pub fn shared_with_offline_sandbox(root: &Path) -> Arc<Shared> {
    let mut config = config();
    config.sandbox.driver = Some("off".to_string());
    let mut environment = environment();
    environment.insert("HOME".to_string(), root.to_string_lossy().into_owned());
    Arc::new(Shared {
        config_path: root.join("config.toml"),
        config,
        environment,
        home: root.to_string_lossy().into_owned(),
        session_root: root.join("sessions"),
        shell: "/bin/sh".to_string(),
        no_session: true,
        overrides: Overrides::default(),
        host_entries: vec![b"HOME=/tmp".to_vec()],
        sandbox_secrets_baseline: Vec::new(),
        sandbox_secrets_baseline_complete: true,
        auth_path: String::new(),
        auth_credentials: crate::auth::Credentials::default(),
        auth_credentials_loaded: false,
        usage: None,
        task_recorder: None,
        skill_checker: None,
        memory: Default::default(),
    })
}

/// The startup resolution path: no stored session metadata.
pub fn initial_runtime(builder: &Builder) -> Runtime {
    resolve_initial_runtime(
        &builder.config,
        &builder.environment,
        None,
        &builder.overrides,
    )
    .expect("resolve")
}

pub async fn controller(workspace_root: &Path, session_root: &Path) -> Controller {
    let builder = builder(workspace_root, session_root);
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    Controller::new(builder, true, session, runner, info)
}

/// A controller whose builder carries a usable SQLite memory service.
/// Shared with `tui::app`'s tests, which need the same fixture to exercise
/// `/memory`/`/remember` dispatch; `repl_commands.rs`'s own memory tests
/// keep their private copy since it predates this one.
pub async fn controller_with_memory(
    workspace: &Path,
    session_root: &Path,
    store_path: &Path,
) -> Controller {
    let mut builder = builder(workspace, session_root);
    let runtime = MemoryRuntime {
        enabled: true,
        backend: "sqlite".into(),
        sqlite_path: store_path.to_string_lossy().into_owned(),
        ..MemoryRuntime::default()
    };
    let (service, user_scope, usable) =
        super::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the test store must be usable");
    builder.shared_mut().memory = super::wiring::MemoryWiring {
        service,
        usable,
        user_scope,
        recall_limit: 8,
        recall_token_budget: 1000,
    };
    builder.workspace_scope =
        super::wiring::workspace_memory_scope(&runtime, &workspace.to_string_lossy())
            .expect("workspace scope");
    let runtime = initial_runtime(&builder);
    let session = builder.create_session(&runtime).expect("session");
    let runner = builder
        .build_runner(&session, &runtime)
        .await
        .expect("runner");
    let info = builder.runtime_info(&runtime);
    Controller::new(builder, true, session, runner, info)
}

pub fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let directory = root.join(".otto").join("skills").join(name);
    std::fs::create_dir_all(&directory).expect("skill directory");
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .expect("write skill");
}

pub fn user(text: &str) -> Message {
    Message {
        role: Role::User,
        blocks: vec![Block {
            block_type: BlockType::Text,
            text: text.to_string(),
            ..Block::default()
        }],
        created_at: chrono::Utc::now(),
        ..Message::default()
    }
}

/// The sandbox-reload capability without a sandbox: it counts the reloads and
/// answers with whatever the test asked for.
pub struct FakeSandbox {
    pub info: SandboxInfo,
    pub reloaded: SandboxInfo,
    pub failure: Option<String>,
    pub calls: Arc<Mutex<usize>>,
}

impl FakeSandbox {
    /// A control that reports `info` and returns `reloaded`, or `failure`.
    pub fn new(
        info: SandboxInfo,
        reloaded: SandboxInfo,
        failure: Option<&str>,
    ) -> (Arc<Self>, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
        let control = Arc::new(Self {
            info,
            reloaded,
            failure: failure.map(str::to_string),
            calls: Arc::clone(&calls),
        });
        (control, calls)
    }
}

#[async_trait::async_trait]
impl crate::app::SandboxControl for FakeSandbox {
    fn info(&self) -> SandboxInfo {
        self.info
    }

    async fn reload(&self) -> Result<SandboxInfo, String> {
        *self.calls.lock().expect("calls") += 1;
        match &self.failure {
            Some(message) => Err(message.clone()),
            None => Ok(self.reloaded),
        }
    }
}

/// A Seatbelt state with `network`.
pub fn seatbelt_info(network: SandboxNetwork) -> SandboxInfo {
    SandboxInfo {
        mode: SandboxMode::Seatbelt,
        network,
        bash_available: true,
        reason: SandboxReason::None,
    }
}
