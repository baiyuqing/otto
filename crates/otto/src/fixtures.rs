//! Test support shared by this crate's unit tests and by the tests of the
//! crates that build on it: a two-provider configuration that resolves
//! offline, a [`Controller`] built on temporary directories, and
//! [`scripted_provider`], a loopback OpenAI-compatible origin server.
//!
//! Compiled under `cfg(test)` or the `test-support` feature; never part of a
//! shipped build. `test-support` implies `fault-injection`, because the
//! scripted provider runs on `crate::faults::http`.

pub mod embed_harness;
pub mod plugin_package;
pub mod scripted_provider;
pub mod subagent_doubles;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use otto_core::config::File;
use otto_core::config::memory::MemoryRuntime;
use otto_core::config::providers::{ModelEntry, ProviderEntry};
use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::model::{Block, BlockType, Message, Role};

use crate::app::Controller;
use crate::runtime::builder::{Builder, Shared, resolve_initial_runtime};
use crate::runtime::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};

pub fn config() -> File {
    let mut providers = std::collections::BTreeMap::new();
    for (name, model, key_env) in [
        ("alpha", "gpt-alpha", "ALPHA_KEY"),
        ("beta", "gpt-beta", "BETA_KEY"),
    ] {
        providers.insert(
            name.to_string(),
            ProviderEntry {
                type_: "openai-compatible".to_string(),
                base_url: "https://gw.example.com/v1".to_string(),
                api_key_env: key_env.to_string(),
                models: std::collections::BTreeMap::from([(
                    model.to_string(),
                    ModelEntry::default(),
                )]),
                ..ProviderEntry::default()
            },
        );
    }
    File {
        default_model: "alpha/gpt-alpha".to_string(),
        providers,
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
        storage: Arc::new(Default::default()),
        approvals: Arc::new(crate::approval::Service::in_memory().expect("approval store")),
        config_path: workspace_root.join("config.toml"),
        config: config(),
        environment: environment(),
        home: workspace_root.to_string_lossy().into_owned(),
        session_root: session_root.to_path_buf(),
        shell: "/bin/sh".to_string(),
        no_session: false,
        headless: false,
        host_entries: Vec::new(),
        overrides: Overrides {
            shell_timeout: Duration::from_secs(30),
            max_output_bytes: 65536,
            ..Overrides::default()
        },
        sandbox_secrets_baseline: Vec::new(),
        sandbox_secrets_baseline_complete: true,
        sandbox_driver_override: None,
        explicit_config: false,
        auth_path: String::new(),
        auth_credentials: crate::auth::Credentials::default(),
        auth_credentials_loaded: false,
        usage: None,
        task_recorder: None,
        schedule: None,
        skill_checker: None,
        memory: Default::default(),
        logger: Arc::new(crate::log::Logger::discard()),
        parent_tool_factory: crate::runtime::builder::ParentToolSlot::default(),
    });
    let mut builder = Builder::for_workspace(
        shared,
        std::sync::Arc::new(
            crate::tool::workspace::Workspace::new(workspace_root).expect("workspace"),
        ),
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
        ..Default::default()
    };
    builder
}

/// A [`Shared`] with an offline (`driver = "off"`) sandbox, for tests that
/// load a workspace at runtime (`runtime::startup::load_workspace`,
/// `serve::Workspaces::load`) without a Seatbelt subprocess or a dependence
/// on the host running the tests.
pub fn shared_with_offline_sandbox(root: &Path) -> Arc<Shared> {
    let mut config = config();
    config.sandbox.driver = Some("off".to_string());
    let mut environment = environment();
    environment.insert("HOME".to_string(), root.to_string_lossy().into_owned());
    Arc::new(Shared {
        storage: Arc::new(Default::default()),
        approvals: Arc::new(crate::approval::Service::in_memory().expect("approval store")),
        config_path: root.join("config.toml"),
        config,
        environment,
        home: root.to_string_lossy().into_owned(),
        session_root: root.join("sessions"),
        shell: "/bin/sh".to_string(),
        no_session: true,
        headless: false,
        overrides: Overrides::default(),
        host_entries: vec![b"HOME=/tmp".to_vec()],
        sandbox_secrets_baseline: Vec::new(),
        sandbox_secrets_baseline_complete: true,
        sandbox_driver_override: None,
        explicit_config: false,
        auth_path: String::new(),
        auth_credentials: crate::auth::Credentials::default(),
        auth_credentials_loaded: false,
        usage: None,
        task_recorder: None,
        schedule: None,
        skill_checker: None,
        memory: Default::default(),
        logger: Arc::new(crate::log::Logger::discard()),
        parent_tool_factory: crate::runtime::builder::ParentToolSlot::default(),
    })
}

/// A [`Shared`] whose config leaves `[sandbox].driver` unset but carries a
/// `--sandbox` override, for the test that a workspace loaded at runtime
/// honors the same override startup applies to its own workspace.
pub fn shared_with_sandbox_driver_override(root: &Path, driver: &str) -> Arc<Shared> {
    let config = config();
    let mut environment = environment();
    environment.insert("HOME".to_string(), root.to_string_lossy().into_owned());
    Arc::new(Shared {
        storage: Arc::new(Default::default()),
        approvals: Arc::new(crate::approval::Service::in_memory().expect("approval store")),
        config_path: root.join("config.toml"),
        config,
        environment,
        home: root.to_string_lossy().into_owned(),
        session_root: root.join("sessions"),
        shell: "/bin/sh".to_string(),
        no_session: true,
        headless: false,
        overrides: Overrides::default(),
        host_entries: vec![b"HOME=/tmp".to_vec()],
        sandbox_secrets_baseline: Vec::new(),
        sandbox_secrets_baseline_complete: true,
        sandbox_driver_override: Some(driver.to_string()),
        explicit_config: false,
        auth_path: String::new(),
        auth_credentials: crate::auth::Credentials::default(),
        auth_credentials_loaded: false,
        usage: None,
        task_recorder: None,
        schedule: None,
        skill_checker: None,
        memory: Default::default(),
        logger: Arc::new(crate::log::Logger::discard()),
        parent_tool_factory: crate::runtime::builder::ParentToolSlot::default(),
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

/// A controller whose builder carries a usable file-backed memory service.
/// Shared with `tui::app`'s tests, which need the same fixture to exercise
/// `/memory`/`/remember` dispatch; `cli/repl_commands`'s own memory tests
/// keep their private copy since it predates this one.
pub async fn controller_with_memory(
    workspace: &Path,
    session_root: &Path,
    store_path: &Path,
) -> Controller {
    let mut builder = builder(workspace, session_root);
    let runtime = MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: store_path.to_string_lossy().into_owned(),
        ..MemoryRuntime::default()
    };
    let (service, user_scope, usable) =
        crate::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new())
            .expect("open memory service");
    assert!(usable, "the test store must be usable");
    builder.shared_mut().memory = crate::runtime::wiring::MemoryWiring {
        client: None,
        extension: None,
        plugin: None,
        service,
        usable,
        user_scope,
        recall_limit: 8,
        recall_token_budget: 1000,
        advice_audit: None,
    };
    builder.workspace_scope =
        crate::runtime::wiring::workspace_memory_scope(&runtime, &workspace.to_string_lossy())
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

/// Writes three user-scope records of about 600 tokens each straight into the
/// store directory `store_path`, past the 2000-token standing budget, as data written
/// before the budget existed would be.
pub fn seed_over_budget_standing(store_path: &Path) {
    use crate::memory::files::FileStore;
    use crate::memory::repository::MemoryOptions;
    let runtime = MemoryRuntime {
        enabled: true,
        backend: "files".into(),
        dir: store_path.to_string_lossy().into_owned(),
        ..MemoryRuntime::default()
    };
    let (service, user_scope, _) =
        crate::runtime::wiring::open_memory_service(&runtime, &[], &mut Vec::new()).expect("open");
    service.close().expect("close");
    let store = FileStore::open(store_path, MemoryOptions::default()).expect("open store");
    for id in ["a", "b", "c"] {
        let now = chrono::Utc::now();
        store
            .upsert(&crate::memory::UpsertRequest {
                record: crate::memory::Record {
                    id: id.into(),
                    scope: user_scope.clone(),
                    kind: "preference".into(),
                    key: id.into(),
                    text: id.repeat(2400),
                    source: crate::memory::Provenance {
                        origin: Some(crate::memory::Origin::Human),
                        ..crate::memory::Provenance::default()
                    },
                    confidence: 0.5,
                    created_at: now,
                    updated_at: now,
                    ..crate::memory::Record::default()
                },
                expected_revision: None,
            })
            .expect("legacy write");
    }
    store.close().expect("close store");
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
    /// Returned by `read_denylist`; `None` models a sandbox without one.
    pub read_denylist: Option<crate::sandbox::read_denylist::ReadDenylist>,
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
            read_denylist: None,
        });
        (control, calls)
    }

    /// Like [`FakeSandbox::new`] with a read denylist that grants are checked against.
    pub fn with_read_denylist(
        info: SandboxInfo,
        read_denylist: crate::sandbox::read_denylist::ReadDenylist,
    ) -> (Arc<Self>, Arc<Mutex<usize>>) {
        let calls = Arc::new(Mutex::new(0));
        let control = Arc::new(Self {
            reloaded: info.clone(),
            info,
            failure: None,
            calls: Arc::clone(&calls),
            read_denylist: Some(read_denylist),
        });
        (control, calls)
    }
}

#[async_trait::async_trait]
impl crate::app::SandboxControl for FakeSandbox {
    fn info(&self) -> SandboxInfo {
        self.info.clone()
    }

    async fn reload(&self) -> Result<SandboxInfo, String> {
        *self.calls.lock().expect("calls") += 1;
        match &self.failure {
            Some(message) => Err(message.clone()),
            None => Ok(self.reloaded.clone()),
        }
    }

    fn read_denylist(&self) -> Option<crate::sandbox::read_denylist::ReadDenylist> {
        self.read_denylist.clone()
    }
}

/// A Seatbelt state with `network`.
pub fn seatbelt_info(network: SandboxNetwork) -> SandboxInfo {
    SandboxInfo {
        mode: SandboxMode::Seatbelt,
        network,
        bash_available: true,
        reason: SandboxReason::None,
        ..Default::default()
    }
}
