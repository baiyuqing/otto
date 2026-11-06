//! Scaffolding for tests that drive a whole [`Otto`] composition offline: a
//! temporary home with one `openai-compatible` provider, a scripted provider
//! that runners built under that home use, and a diagnostics capture.
//!
//! The `embed` tests and the `otto-cli` parity tests share it.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use otto_core::model::{Block, BlockType, FinishReason, Message, Role};
use otto_core::operation::OperationControl;
use otto_core::provider::{Provider, ProviderSettlement, Request, Response, StreamSink};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use crate::embed::{ApprovalDecision, ApprovalRequest, Input, Otto, OttoBuilder, Session, Stop};
use crate::runtime::builder::TEST_PROVIDERS;

/// A temporary root holding a home directory and an empty workspace.
pub struct Fixture {
    _directory: tempfile::TempDir,
    /// The canonical temporary root; `home` and `workspace` are under it.
    pub root: PathBuf,
    /// The `HOME` the composition reads its configuration from.
    pub home: PathBuf,
    /// The workspace the composition runs in.
    pub workspace: PathBuf,
}

/// The user configuration of every fixture: one `openai-compatible` provider
/// whose key comes from `ALPHA_KEY`, with an explicitly trusted test endpoint.
pub const PROFILE: &str = r#"
default_model = "alpha/gpt-6-luna"

[providers.alpha]
type = "openai-compatible"
base_url = "https://example.com/v1"
api_key_env = "ALPHA_KEY"
trusted_origins = ["https://example.com"]
"#;

/// A temp home with a user config of [`PROFILE`], `extra` and
/// `[sandbox] driver = "off"`, and an empty workspace. `{ROOT}` in `extra`
/// is replaced with the fixture root.
pub fn fixture(extra: &str) -> Fixture {
    let directory = tempfile::tempdir().expect("tempdir");
    let root = directory.path().canonicalize().expect("canonical root");
    let home = root.join("home");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(home.join(".config/otto")).expect("config dir");
    std::fs::create_dir_all(&workspace).expect("workspace dir");
    let extra = extra.replace("{ROOT}", &root.to_string_lossy());
    std::fs::write(
        home.join(".config/otto/config.toml"),
        format!("{PROFILE}\n{extra}\n[sandbox]\ndriver = \"off\"\n"),
    )
    .expect("write config");
    Fixture {
        _directory: directory,
        root,
        home,
        workspace,
    }
}

/// The process environment a composition under `fixture` sees: its `HOME`
/// and the profile's API key.
pub fn environment(fixture: &Fixture) -> Vec<(String, String)> {
    vec![
        (
            "HOME".to_string(),
            fixture.home.to_string_lossy().into_owned(),
        ),
        ("ALPHA_KEY".to_string(), "sk-alpha".to_string()),
    ]
}

/// An [`OttoBuilder`] on the fixture's workspace and environment.
pub fn builder(fixture: &Fixture) -> OttoBuilder {
    Otto::builder()
        .workspace(&fixture.workspace)
        .environment(environment(fixture))
}

/// A diagnostics writer the test can read back.
#[derive(Clone, Default)]
pub struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    /// Everything written so far, lossily decoded.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Replays `replies` in order and records every request. Once the replies
/// are used up it answers `done`.
#[derive(Default)]
pub struct Script {
    pub replies: Mutex<VecDeque<Message>>,
    pub requests: Mutex<Vec<Request>>,
}

#[async_trait::async_trait]
impl Provider for Script {
    async fn complete(
        &self,
        request: &Request,
        _: StreamSink<'_>,
        _: &dyn OperationControl,
    ) -> ProviderSettlement {
        self.requests.lock().unwrap().push(request.clone());
        let message = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| reply("done"));
        ProviderSettlement::succeeded(
            Response {
                message,
                ..Response::default()
            },
            1,
        )
    }
}

/// An assistant text message that ends the turn.
pub fn reply(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block::text(text)],
        finish_reason: Some(FinishReason::Stop),
        ..Message::default()
    }
}

/// An assistant message holding one call of `name` with the JSON
/// `arguments`.
pub fn tool_call(name: &str, arguments: &str) -> Message {
    Message {
        role: Role::Assistant,
        blocks: vec![Block {
            block_type: BlockType::ToolCall,
            tool_call_id: "call-1".into(),
            tool_name: name.into(),
            arguments: Some(RawValue::from_string(arguments.into()).unwrap()),
            ..Block::default()
        }],
        finish_reason: Some(FinishReason::ToolCalls),
        ..Message::default()
    }
}

/// A scripted provider that runners built under a served fixture's home
/// use until dropped.
pub struct Installed(Arc<Script>);

impl Installed {
    /// A provider replaying `replies`, with a handle to its script.
    pub fn new(replies: Vec<Message>) -> (Self, Arc<Script>) {
        let script = Arc::new(Script {
            replies: Mutex::new(replies.into()),
            ..Script::default()
        });
        (Self(script.clone()), script)
    }

    /// Makes runners built under `fixture`'s home use this script.
    pub fn serve(&self, fixture: &Fixture) {
        TEST_PROVIDERS
            .lock()
            .unwrap()
            .push((fixture.home.to_string_lossy().into_owned(), self.0.clone()));
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        TEST_PROVIDERS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(_, provider)| {
                !std::ptr::addr_eq(Arc::as_ptr(provider), Arc::as_ptr(&self.0))
            });
    }
}

/// An approval callback that denies every request.
pub async fn deny(_: ApprovalRequest) -> ApprovalDecision {
    ApprovalDecision::Deny
}

/// One prompt of `text` on `session`, with every approval denied.
pub async fn say(session: &Session, text: &str) -> Stop {
    session
        .prompt(
            Input::Text(text),
            &mut |_| {},
            &CancellationToken::new(),
            deny,
        )
        .await
        .expect("prompt")
}

/// Every file under `root`, in directory-walk order.
pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path);
            }
        }
    }
    found
}
