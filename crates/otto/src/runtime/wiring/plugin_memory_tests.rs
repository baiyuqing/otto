use super::*;
use crate::app::management::{Reply, Request};
use otto_core::memory as port;
use serde_json::{Value, json};

const PACKAGE: &str = "memfix";

struct PluginFixture {
    root: tempfile::TempDir,
    workspace: PathBuf,
    sessions: PathBuf,
}

impl Drop for PluginFixture {
    fn drop(&mut self) {
        // Installed package snapshots are read-only.
        let _ = std::process::Command::new("chmod")
            .args(["-R", "u+w"])
            .arg(self.root.path())
            .status();
    }
}

impl PluginFixture {
    /// Installs package `memfix`, whose memory command logs each request and
    /// answers `describe` and `remember`.
    fn new(trust: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let sessions = root.path().join("sessions");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&sessions).unwrap();
        let mut description = port::ServiceDescription::new("plugin-fixture".into());
        let mut recall = port::Capability::new(port::Operation::Recall);
        recall.supports_delegated_learning = false;
        let mut remember = port::Capability::new(port::Operation::Remember);
        remember.effects = vec![port::Effect::ChangeMemory];
        remember.supports_delegated_learning = true;
        description.capabilities = vec![recall, remember];
        let receipt = |id: &str| {
            json!({"result": {"request_id": id, "operation_ref": null,
                "admission": "acknowledged", "completion": "complete",
                "visibility": "unknown", "forget_coverage": null,
                "items": [{"reference": null, "state": "applied", "reason": null}]}})
        };
        let receipt_template = receipt("__REQUEST_ID__").to_string();
        let body = format!(
            "read -r line; printf '%s\\n' \"$line\" >> {log}\n\
             case \"$line\" in\n\
             *'\"operation\":\"describe\"'*) printf '%s' '{describe}';;\n\
             *'\"operation\":\"remember\"'*)\n\
             id=$(printf '%s' \"$line\" | sed -n 's/.*\"request_id\":\"\\([^\"]*\\)\".*/\\1/p')\n\
             printf '%s' '{receipt_template}' | sed \"s/__REQUEST_ID__/$id/\";;\n\
             *) exit 1;;\n\
             esac",
            log = root.path().join("memory.log").display(),
            describe = json!({"result": description}),
        );
        let source = root.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("plugin.json"),
            json!({
                "$schema": crate::plugin::package::PLUGIN_SCHEMA,
                "name": PACKAGE,
                "extensions": {"dev.otto": {"version": 1,
                    "memory": {"command": ["/bin/sh", "-c", body], "timeout": "10s"}}}
            })
            .to_string(),
        )
        .unwrap();
        let service = crate::plugin::Service::new(&workspace);
        service.install(source.to_str().unwrap(), None).unwrap();
        if trust {
            service.change(PACKAGE, "trust").unwrap();
        }
        Self {
            root,
            workspace,
            sessions,
        }
    }

    /// A builder whose memory source is the package; its plugin commands run unconfined.
    fn builder(&self, required: bool) -> Builder {
        let mut builder = crate::cli::fixture_tests::builder(&self.workspace, &self.sessions);
        let executor = crate::sandbox::Executor::new(
            Arc::new(crate::sandbox::direct::DirectDriver::new()),
            crate::sandbox::Policy {
                filesystem: crate::sandbox::FilesystemMode::Unconfined,
                network: crate::sandbox::NetworkMode::Allow,
            },
            &self.workspace,
        )
        .unwrap();
        builder.command_executor = Some(Arc::new(executor));
        builder.sandbox_environment = Some(vec!["PATH=/usr/bin:/bin".into()]);
        builder.workspace_scope = crate::memory::Scope::new("workspace", "w1");
        builder.shared_mut().memory = MemoryWiring {
            plugin: Some(PluginMemorySelection {
                package: PACKAGE.into(),
                required,
                require_encryption: false,
            }),
            user_scope: crate::memory::Scope::new("user", "default"),
            usable: true,
            ..Default::default()
        };
        builder
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.root.path().join("memory.log"))
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

async fn runner_memory_client(
    builder: &Builder,
    warnings: &mut Vec<u8>,
) -> Result<Option<Arc<crate::memory::client::Client>>, String> {
    let runtime = crate::cli::fixture_tests::initial_runtime(builder);
    let plugins = crate::plugin::Snapshot::load(
        Path::new(&builder.home),
        &builder.config,
        &builder.environment,
    );
    let extensions = builder.plugin_runtime(&plugins, &[]).unwrap();
    builder
        .runner_memory_client(&plugins, &extensions, &runtime, warnings)
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn trusted_package_gives_a_delegated_learning_client() {
    let fixture = PluginFixture::new(true);
    let builder = fixture.builder(false);
    let mut warnings = Vec::new();
    let client = runner_memory_client(&builder, &mut warnings).await.unwrap();
    let client = client.unwrap_or_else(|| {
        panic!(
            "no client: {} {:?}",
            String::from_utf8_lossy(&warnings),
            fixture.requests()
        )
    });
    assert!(warnings.is_empty());
    assert_eq!(client.mode(), port::AuthorizationMode::DelegatedLearning);
    assert_eq!(client.describe().identity, "plugin-fixture");
}

#[tokio::test(flavor = "multi_thread")]
async fn untrusted_package_warns_and_continues_without_memory_unless_required() {
    let fixture = PluginFixture::new(false);
    let mut warnings = Vec::new();
    let client = runner_memory_client(&fixture.builder(false), &mut warnings).await;
    assert!(client.unwrap().is_none());
    let text = String::from_utf8(warnings).unwrap();
    assert_eq!(
        text,
        "warning: memory plugin \"memfix\" is not trusted; run otto plugin trust memfix; \
         continuing without memory\n"
    );
    let error = runner_memory_client(&fixture.builder(true), &mut Vec::new())
        .await
        .err()
        .expect("required source fails");
    assert!(error.contains("not trusted"), "{error}");
    assert!(fixture.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn remember_command_sends_a_human_remember_and_opens_no_file_store() {
    let fixture = PluginFixture::new(true);
    let builder = fixture.builder(false);
    let runtime = crate::cli::fixture_tests::initial_runtime(&builder);
    let session = builder.create_session(&runtime).unwrap();
    let runner = builder.build_runner(&session, &runtime).await.unwrap();
    let info = builder.runtime_info(&runtime);
    let controller = crate::app::Controller::new(builder, true, session, runner, info);
    let reply = controller
        .management(Request::parse("remember", "--scope user prefers tabs").unwrap())
        .map_err(|error| error.to_string());
    assert!(
        matches!(&reply, Ok(Reply::Notice(line)) if line.contains("state=applied")),
        "{:?}",
        reply.map(|_| ())
    );
    let requests = fixture.requests();
    let remember: Vec<_> = requests
        .iter()
        .filter(|request| request["operation"] == "remember")
        .collect();
    assert_eq!(remember.len(), 1, "{requests:?}");
    assert_eq!(remember[0]["request"]["context"]["origin"], "human");
    assert_eq!(
        remember[0]["request"]["context"]["write_scope"],
        json!({"namespace": "user", "id": "default"})
    );
    assert!(!fixture.workspace.join(".otto/memory").exists());
    let refusal = controller
        .management(Request::MemoryReviewList)
        .unwrap_err();
    assert_eq!(
        refusal,
        "memory source plugin:memfix does not support this command; the plugin manages its records"
    );
}

#[test]
fn boundary_check_sees_every_memory_tool_of_a_plugin_source() {
    let fixture = PluginFixture::new(false);
    let names: Vec<_> = fixture
        .builder(false)
        .boundary_memory_definitions(1024)
        .into_iter()
        .map(|definition| definition.name)
        .collect();
    assert_eq!(names, ["memory_search", "remember", "forget"]);
}
