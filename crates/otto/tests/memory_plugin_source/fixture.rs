//! A trusted shell-script package whose `memory` command logs each request and
//! answers with reply files that the test serializes from `otto_core::memory`.

use otto_core::memory::*;
use serde_json::{Value, json};
use std::path::PathBuf;

pub const RECALL_MARKER: &str = "plugin-recall-marker-7f3a";
pub const PACKAGE: &str = "memfix";

pub struct Fixture {
    pub root: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Installed package snapshots are read-only; the temporary directory
        // cannot remove them otherwise.
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.root.path())
            .status();
    }
}

fn capability(operation: Operation, effects: Vec<Effect>) -> Capability {
    let mut capability = Capability::new(operation);
    capability.effects = effects;
    capability.supports_delegated_learning = !capability.effects.is_empty();
    capability
}

fn description() -> ServiceDescription {
    let mut description = ServiceDescription::new("plugin-fixture".into());
    let mut forget = capability(Operation::Forget, vec![Effect::ChangeMemory]);
    forget.forget_targets = vec![ForgetTargetKind::Item];
    forget.forget_levels = vec![ForgetLevel::Suppress];
    description.capabilities = vec![
        capability(Operation::Recall, vec![]),
        capability(Operation::Inspect, vec![]),
        capability(Operation::Remember, vec![Effect::ChangeMemory]),
        forget,
        capability(Operation::Observe, vec![Effect::ChangeMemory]),
    ];
    description
}

fn receipt() -> Receipt {
    Receipt {
        request_id: "__REQUEST_ID__".into(),
        operation_ref: None,
        admission: Admission::Acknowledged,
        completion: Completion::Complete,
        visibility: Visibility::Unknown,
        items: vec![ItemOutcome {
            reference: None,
            state: ItemState::Applied,
            reason: None,
        }],
        forget_coverage: None,
    }
}

fn recall_result() -> RecallResult {
    RecallResult {
        items: vec![RecallItem {
            reference: None,
            content: Content::Text(RECALL_MARKER.into()),
            sources: vec![],
            effective_at: None,
            metadata: Default::default(),
        }],
        used_tokens: Some(8),
        coverage: Coverage::Complete,
    }
}

impl Fixture {
    pub fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }
    pub fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }
    pub fn workspace(&self) -> PathBuf {
        self.root.path().join("workspace")
    }
    pub fn log(&self) -> PathBuf {
        self.root.path().join("memory.log")
    }
    /// The file store directory Otto must not create for a plugin source.
    pub fn file_store(&self) -> PathBuf {
        self.home().join(".otto/memory")
    }
    /// Installs package `PACKAGE`; `recall_exits` makes the command fail for `recall`.
    pub fn install(&self, trust: bool, recall_exits: bool) {
        let replies = self.root.path().join("replies");
        std::fs::create_dir_all(&replies).unwrap();
        let write = |name: &str, value: Value| {
            std::fs::write(replies.join(format!("{name}.json")), value.to_string()).unwrap()
        };
        write("describe", json!({"result": description()}));
        write("remember", json!({"result": receipt()}));
        write("forget", json!({"result": receipt()}));
        write("observe", json!({"result": receipt()}));
        if !recall_exits {
            write("recall", json!({"result": recall_result()}));
        }
        // serde_json orders keys alphabetically, so `operation` is the first key; a missing reply
        // file makes the command exit 1.
        let body = format!(
            "read -r line; printf '%s\\n' \"$line\" >> {log}\n\
             op=$(printf '%s' \"$line\" | sed -n 's/^{{\"operation\":\"\\([a-z_]*\\)\".*/\\1/p')\n\
             id=$(printf '%s' \"$line\" | sed -n 's/.*\"request_id\":\"\\([^\"]*\\)\".*/\\1/p')\n\
             sed \"s/__REQUEST_ID__/$id/\" {replies}/$op.json",
            log = self.log().display(),
            replies = replies.display()
        );
        let source = self.root.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("plugin.json"),
            json!({
                "$schema": "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json",
                "name": PACKAGE,
                "extensions": {"dev.otto": {
                    "version": 1,
                    "memory": {"command": ["/bin/sh", "-c", body], "timeout": "10s"}
                }}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::create_dir_all(self.home()).unwrap();
        let service = otto::plugin::Service::new(&self.home());
        service.install(source.to_str().unwrap(), None).unwrap();
        if trust {
            service.change(PACKAGE, "trust").unwrap();
        }
    }
    /// The requests the memory command received, in order.
    pub fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    pub fn operations(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|request| request["operation"].as_str().unwrap().to_string())
            .collect()
    }
    /// An embed builder over `home/.config/otto/config.toml` with `memory` appended.
    pub fn builder(&self, url: &str, memory: &str) -> otto::embed::OttoBuilder {
        let home = self.home();
        let workspace = self.workspace();
        std::fs::create_dir_all(home.join(".config/otto")).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            home.join(".config/otto/config.toml"),
            format!(
                "default_profile = \"test\"\n[profiles.test]\nprovider = \
                \"openai-compatible\"\nbase_url = \"{url}\"\nmodel = \
                \"offline-test\"\napi_key_env = \"TEST_KEY\"\n[sandbox]\ndriver = \
                \"off\"\n[server]\nworkspace_roots = [\"{}\"]\n{memory}\n",
                self.root.path().to_string_lossy()
            ),
        )
        .unwrap();
        otto::embed::Otto::builder()
            .workspace(workspace)
            .environment([
                ("HOME".to_string(), home.to_string_lossy().into_owned()),
                ("PATH".to_string(), "/usr/bin:/bin".to_string()),
                ("TEST_KEY".to_string(), "offline-placeholder".to_string()),
            ])
            .disable(otto::embed::Component::Subagents)
    }
}

pub fn plugin_memory_config() -> String {
    format!("[memory]\nbackend = \"plugin:{PACKAGE}\"")
}
