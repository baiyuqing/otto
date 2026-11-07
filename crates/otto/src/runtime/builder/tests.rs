use super::runner::{log_runtime_event, turn_limit};
use super::*;
use crate::sandbox::direct::DirectDriver;
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy};
use otto_core::model::{EffectCertainty, OperationDisposition};
use otto_core::operation::OperationControl;
use otto_core::provider::ProviderSettlement;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

mod authorization;
mod child_recovery;
mod memory_proposals;
mod provider_turns;
mod runtime_info;
mod session_identity;
mod session_resources_sharing;
mod session_shutdown;
mod tool_sets;
mod turn_limits;
mod workflows;

fn shared(root: &Path) -> Arc<Shared> {
    shared_with_no_session(root, true)
}

fn shared_with_no_session(root: &Path, no_session: bool) -> Arc<Shared> {
    Arc::new(Shared {
        storage: Arc::new(Default::default()),
        approvals: Arc::new(crate::approval::Service::in_memory().expect("approval store")),
        config_path: root.join("config.toml"),
        config: File::default(),
        environment: HashMap::new(),
        home: root.to_string_lossy().into_owned(),
        session_root: root.join("sessions"),
        shell: "/bin/sh".to_string(),
        no_session,
        headless: false,
        host_entries: Vec::new(),
        overrides: Overrides::default(),
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
        parent_tool_factory: ParentToolSlot::default(),
    })
}

fn builder_for(shared: Arc<Shared>, root: &Path) -> Builder {
    let workspace = Arc::new(Workspace::new(root).expect("workspace"));
    let workspace_path = workspace.root().to_string_lossy().into_owned();
    let mut builder = Builder::for_workspace(
        shared,
        workspace,
        workspace_path,
        McpRuntime {
            enabled: false,
            call_timeout_secs: 60,
            connect_timeout_secs: 20,
            servers: Vec::new(),
        },
    );
    builder.sandbox_info = SandboxInfo::unavailable(SandboxReason::SeatbeltMissing);
    builder
}

fn builder(root: &Path) -> Builder {
    builder_for(shared(root), root)
}

/// Two `Builder`s built from one `Shared` (`Builder::for_workspace`) stay
/// independent: each keeps its own workspace path, its own file-tool
/// root, and its own sandbox executor, none of which leak from one
/// `Builder` into the other.
#[test]
fn two_builders_from_one_shared_stay_independent_per_workspace() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace_a = tempfile::tempdir().expect("workspace a");
    let workspace_b = tempfile::tempdir().expect("workspace b");
    let shared = shared(shared_root.path());

    let mut a = builder_for(Arc::clone(&shared), workspace_a.path());
    let b = builder_for(Arc::clone(&shared), workspace_b.path());

    assert_ne!(a.workspace_path, b.workspace_path);
    assert_ne!(a.workspace.root(), b.workspace.root());
    assert_eq!(a.workspace_path, a.workspace.root().to_string_lossy());
    assert_eq!(b.workspace_path, b.workspace.root().to_string_lossy());

    with_bash(&mut a, workspace_a.path());
    assert!(a.command_executor.is_some());
    assert!(b.command_executor.is_none());

    // Both still share the one process-wide `Shared`.
    assert!(Arc::ptr_eq(&a.shared, &b.shared));
}

/// Loading a second workspace must not open a second memory service: the
/// service lives on `Shared`, so every `Builder` built from one `Shared`
/// sees the identical `Arc`.
#[test]
fn two_builders_from_one_shared_use_the_same_memory_service() {
    let shared_root = tempfile::tempdir().expect("shared root");
    let workspace_a = tempfile::tempdir().expect("workspace a");
    let workspace_b = tempfile::tempdir().expect("workspace b");
    let shared = shared(shared_root.path());

    let a = builder_for(Arc::clone(&shared), workspace_a.path());
    let b = builder_for(Arc::clone(&shared), workspace_b.path());

    assert!(Arc::ptr_eq(&a.memory.service, &b.memory.service));
}

fn with_bash(builder: &mut Builder, root: &Path) {
    let executor = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        root,
    )
    .expect("executor");
    builder.command_executor = Some(Arc::new(executor));
    builder.sandbox_environment = Some(vec!["PATH=/usr/bin:/bin".to_string()]);
    builder.sandbox_info = SandboxInfo {
        mode: SandboxMode::Off,
        network: SandboxNetwork::Unconfined,
        bash_available: true,
        reason: SandboxReason::None,
        ..Default::default()
    };
}

fn user_message(text: &str) -> Message {
    Message {
        role: otto_core::model::Role::User,
        blocks: vec![Block::text(text)],
        created_at: Utc::now(),
        ..Message::default()
    }
}

fn runtime() -> Runtime {
    Runtime {
        model_selector: otto_core::config::ModelSelector {
            provider: "default".to_string(),
            model_id: "gpt-test".to_string(),
        },
        provider_name: "default".to_string(),
        provider_type: "openai-compatible".to_string(),
        base_url: "https://gw.example.com/v1".to_string(),
        endpoint: Some(
            otto_core::config::endpoint::TrustedEndpoint::authorize(
                "https://gw.example.com/v1",
                "openai-compatible",
                &["https://gw.example.com".into()],
                &[],
            )
            .unwrap(),
        ),
        model: "gpt-test".to_string(),
        api_key: "sk-secret-value".to_string(),
        api_key_env: "OTTO_API_KEY".to_string(),
        shell_timeout: Duration::from_secs(30),
        max_output_bytes: 65536,
        ..Runtime::default()
    }
}

fn tool_names(runner: &Runner) -> Vec<String> {
    runner
        .definitions()
        .into_iter()
        .map(|definition| definition.name)
        .collect()
}

struct CountingParentToolFactory {
    calls: Arc<AtomicUsize>,
    contexts: Mutex<Vec<ParentToolContext>>,
}

impl ParentToolFactory for CountingParentToolFactory {
    fn tools(
        &self,
        context: &ParentToolContext,
    ) -> Result<Vec<Box<dyn Tool + Send + Sync>>, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.lock().unwrap().push(context.clone());
        Ok(vec![
            crate::fixtures::subagent_doubles::StubTool::new(
                "parent_factory_tool",
                otto_core::tool::ToolResult::default(),
            )
            .boxed(),
        ])
    }
}

/// A provider `Runner::scripted` needs a value for but this test never
/// calls: `close_resources`/`close` never send a turn.
struct UnusedProvider;

#[async_trait::async_trait]
impl Provider for UnusedProvider {
    async fn complete(
        &self,
        _request: &Request,
        _emit: StreamSink<'_>,
        _control: &dyn OperationControl,
    ) -> ProviderSettlement {
        unreachable!("close-ordering test never sends a turn")
    }
}
