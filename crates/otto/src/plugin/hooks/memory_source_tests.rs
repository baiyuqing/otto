use super::*;
use crate::plugin::{Service, Snapshot, package};
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy, direct::DirectDriver};
use otto_core::{
    config::{File, plugins::MemoryCommand},
    memory::{
        Content, Coverage, ErrorKind, MemoryContext, MemoryService, Origin, RecallRequest,
        RememberRequest, Scope,
    },
};
use std::{collections::HashMap, fs};

const DESCRIBE_REPLY: &str = r#"{"result":{"identity":"fixture","version":1,"capabilities":[]}}"#;
const RECALL_REPLY: &str = r#"{"result":{"items":[],"used_tokens":3,"coverage":"complete"}}"#;

struct MemoryFixture {
    home: tempfile::TempDir,
    log: PathBuf,
}
impl Drop for MemoryFixture {
    fn drop(&mut self) {
        let _ = crate::plugin::thaw(self.home.path());
    }
}
impl MemoryFixture {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let log = home.path().join("memory.log");
        Self { home, log }
    }
    fn service(&self) -> Service {
        Service::new(self.home.path())
    }
    /// Installs package `name` with `components` as its `dev.otto` extension.
    fn install(&self, name: &str, components: Value, trust: bool) {
        let source = self.home.path().join(format!("source-{name}"));
        fs::create_dir_all(&source).unwrap();
        let manifest = json!({
            "$schema": package::PLUGIN_SCHEMA,
            "name": name,
            "extensions": {"dev.otto": components}
        });
        fs::write(source.join("plugin.json"), manifest.to_string()).unwrap();
        self.service()
            .install(source.to_str().unwrap(), None)
            .unwrap();
        if trust {
            self.service().change(name, "trust").unwrap();
        }
    }
    /// Installs and trusts a memory package whose command is the shell `body`, with a 1s timeout.
    fn install_memory(&self, name: &str, body: &str) {
        self.install(
            name,
            json!({"version":1,"memory":{"command":["/bin/sh","-c",body],"timeout":"1s"}}),
            true,
        );
    }
    /// A shell body that appends each stdin line to the log and answers by operation.
    fn script(&self, recall: &str, remember: &str) -> String {
        format!(
            "read line; printf '%s\\n' \"$line\" >> {log}\n\
             case \"$line\" in\n\
             *'\"operation\":\"describe\"'*) printf '%s' '{DESCRIBE_REPLY}';;\n\
             *'\"operation\":\"recall\"'*) {recall};;\n\
             *'\"operation\":\"remember\"'*) {remember};;\n\
             esac",
            log = self.log.display()
        )
    }
    fn log_lines(&self) -> Vec<Value> {
        fs::read_to_string(&self.log)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
    fn snapshot(&self) -> Snapshot {
        let env = HashMap::from([
            (
                "HOME".into(),
                self.home.path().to_string_lossy().into_owned(),
            ),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]);
        Snapshot::load(self.home.path(), &File::default(), &env)
    }
    fn runtime(&self, snapshot: &Snapshot) -> Arc<Runtime> {
        let workspace = fs::canonicalize(self.home.path()).unwrap();
        let executor = Executor::new(
            Arc::new(DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            &workspace,
        )
        .unwrap();
        Runtime::new(
            snapshot,
            self.home.path(),
            &Default::default(),
            Execution {
                executor: Some(Arc::new(executor)),
                environment: vec!["PATH=/usr/bin:/bin".into()],
                package_environment: Default::default(),
                workspace,
                guard: None,
                secrets: Vec::new(),
                logger: Arc::new(crate::log::Logger::discard()),
            },
        )
        .unwrap()
    }
    async fn source(&self, name: &str) -> Result<PluginMemoryService, String> {
        let snapshot = self.snapshot();
        self.runtime(&snapshot)
            .memory_source(&snapshot, name, &CancellationToken::new())
            .await
    }
}

fn memory_context() -> MemoryContext {
    MemoryContext {
        organization_id: None,
        principal: "user/default".into(),
        scopes: vec![Scope {
            namespace: "user".into(),
            id: "default".into(),
        }],
        write_scope: None,
        workspace_id: None,
        session_id: None,
        task_id: None,
        origin: Origin::Human,
    }
}
fn recall_request() -> RecallRequest {
    RecallRequest {
        scope_filter: None,
        context: memory_context(),
        query: "database".into(),
        limit: 5,
        token_budget: 100,
    }
}
fn remember_request() -> RememberRequest {
    RememberRequest {
        key: None,
        context: memory_context(),
        request_id: "r1".into(),
        content: Content::Text("note".into()),
        target: None,
        metadata: Default::default(),
    }
}
fn kind<T: std::fmt::Debug>(result: otto_core::memory::Result<T>) -> ErrorKind {
    result.unwrap_err().kind
}

#[tokio::test]
async fn describe_then_recall_exchange_one_json_line_each() {
    let f = MemoryFixture::new();
    f.install_memory(
        "mem",
        &f.script(&format!("printf '%s' '{RECALL_REPLY}'"), "exit 1"),
    );
    let service = f.source("mem").await.unwrap();
    assert_eq!(service.describe().identity, "fixture");
    let request = recall_request();
    let result = service
        .recall(&request, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(result.coverage, Coverage::Complete);
    assert_eq!(result.used_tokens, Some(3));
    let lines = f.log_lines();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], json!({"version":1,"operation":"describe"}));
    assert_eq!(
        lines[1],
        json!({"version":1,"operation":"recall","request":serde_json::to_value(&request).unwrap()})
    );
}

#[tokio::test]
async fn plugin_error_kind_is_kept_and_its_text_is_discarded() {
    let f = MemoryFixture::new();
    let reply = r#"{"error":{"kind":"rate_limited"}}"#;
    f.install_memory(
        "mem",
        &f.script(&format!("printf '%s' '{reply}'"), "exit 1"),
    );
    let service = f.source("mem").await.unwrap();
    let error = service
        .recall(&recall_request(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error, otto_core::memory::Error::new(ErrorKind::RateLimited));

    // An error body with extra text, or a reply holding both keys, is not accepted.
    for reply in [
        r#"{"error":{"kind":"rate_limited","message":"secret"}}"#,
        r#"{"result":{"items":[],"used_tokens":null,"coverage":"complete"},"error":{"kind":"closed"}}"#,
        "not json",
    ] {
        let f = MemoryFixture::new();
        f.install_memory(
            "mem",
            &f.script(&format!("printf '%s' '{reply}'"), "exit 1"),
        );
        let service = f.source("mem").await.unwrap();
        let result = service
            .recall(&recall_request(), &CancellationToken::new())
            .await;
        assert_eq!(kind(result), ErrorKind::Unavailable, "{reply}");
    }
}

#[tokio::test]
async fn failing_and_mistyped_commands_are_unavailable() {
    let f = MemoryFixture::new();
    f.install_memory(
        "mem",
        &f.script("exit 1", r#"printf '%s' '{"result":{"wrong":1}}'"#),
    );
    let service = f.source("mem").await.unwrap();
    let token = CancellationToken::new();
    assert_eq!(
        kind(service.recall(&recall_request(), &token).await),
        ErrorKind::Unavailable
    );
    assert_eq!(
        kind(service.remember(&remember_request(), &token).await),
        ErrorKind::Unavailable
    );
}

#[tokio::test]
async fn a_timeout_is_unknown_for_effects_and_unavailable_for_reads() {
    let f = MemoryFixture::new();
    f.install_memory("mem", &f.script("sleep 5", "sleep 5"));
    let service = f.source("mem").await.unwrap();
    let token = CancellationToken::new();
    assert_eq!(
        kind(service.remember(&remember_request(), &token).await),
        ErrorKind::OutcomeUnknown
    );
    assert_eq!(
        kind(service.recall(&recall_request(), &token).await),
        ErrorKind::Unavailable
    );
}

#[tokio::test]
async fn unusable_packages_give_distinct_reasons() {
    let f = MemoryFixture::new();
    f.install(
        "untrusted",
        json!({"version":1,"memory":{"command":["/bin/true"]}}),
        false,
    );
    f.install(
        "hooks-only",
        json!({"version":1,"tools":[{
            "name":"t","description":"d","input_schema":{"type":"object"},"command":["/bin/true"]
        }]}),
        true,
    );
    let fixture = &f;
    let reason = |name: &'static str| async move { fixture.source(name).await.err().unwrap() };
    assert_eq!(
        reason("untrusted").await,
        "memory plugin \"untrusted\" is not trusted; run otto plugin trust untrusted"
    );
    assert_eq!(
        reason("missing").await,
        "memory plugin \"missing\" is not installed or is disabled"
    );
    assert_eq!(
        reason("hooks-only").await,
        "memory plugin \"hooks-only\" declares no memory component"
    );
}

#[tokio::test]
async fn a_failing_describe_is_reported_with_the_package_name() {
    let f = MemoryFixture::new();
    f.install_memory("mem", "exit 1");
    let reason = f.source("mem").await.err().unwrap();
    assert_eq!(
        reason,
        "memory plugin \"mem\" describe failed: memory service unavailable"
    );
}

#[tokio::test]
async fn revoking_trust_makes_the_next_call_unavailable() {
    let f = MemoryFixture::new();
    f.install_memory(
        "mem",
        &f.script(&format!("printf '%s' '{RECALL_REPLY}'"), "exit 1"),
    );
    let service = f.source("mem").await.unwrap();
    let token = CancellationToken::new();
    service.recall(&recall_request(), &token).await.unwrap();
    f.service().change("mem", "revoke").unwrap();
    assert_eq!(
        kind(service.recall(&recall_request(), &token).await),
        ErrorKind::Unavailable
    );
    assert_eq!(f.log_lines().len(), 2);
}

#[test]
fn memory_component_validation_requires_a_runnable_command() {
    let memory = |command: Vec<String>, timeout: &str| Components {
        version: 1,
        memory: Some(MemoryCommand {
            command,
            timeout: timeout.into(),
        }),
        ..Default::default()
    };
    assert!(validate_components("m", &memory(vec![], "")).is_err());
    assert!(validate_components("m", &memory(vec!["run".into()], "301s")).is_err());
    assert!(validate_components("m", &memory(vec!["run".into()], "5s")).is_ok());
}
