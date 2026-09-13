//! Shared scaffolding for the `cli` unit tests: a two-profile configuration
//! that resolves offline, and a [`Controller`] built on temporary
//! directories. Used by `controller.rs` and `repl.rs`, which both need a
//! working composition root but never reach the provider.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use otto_core::config::resolve::{Overrides, Runtime};
use otto_core::config::{File, Profile};
use otto_core::model::{Block, BlockType, Message, Role};

use super::controller::Controller;
use super::info::{SandboxInfo, SandboxMode, SandboxNetwork, SandboxReason};
use super::runtime_builder::{Builder, leaked_workspace, resolve_initial_runtime};

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
    Builder {
        config_path: workspace_root.join("config.toml"),
        config: config(),
        environment: environment(),
        workspace: leaked_workspace(workspace_root).expect("workspace"),
        workspace_path: workspace_root.to_string_lossy().into_owned(),
        session_root: session_root.to_path_buf(),
        shell: "/bin/sh".to_string(),
        no_session: false,
        overrides: Overrides {
            shell_timeout: Duration::from_secs(30),
            max_output_bytes: 65536,
            ..Overrides::default()
        },
        command_executor: None,
        sandbox_environment: None,
        sandbox_info: SandboxInfo {
            mode: SandboxMode::Off,
            network: SandboxNetwork::Unconfined,
            bash_available: false,
            reason: SandboxReason::None,
        },
        sandbox_secrets: Vec::new(),
        sandbox_secrets_complete: true,
        auth_path: String::new(),
        auth_credentials: crate::auth::Credentials::default(),
        auth_credentials_loaded: false,
    }
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
