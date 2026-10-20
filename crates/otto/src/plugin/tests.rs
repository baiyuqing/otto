use super::*;
use otto_core::{
    config::{File, McpTransport, PluginBindings},
    tool::ToolResult,
};
use serde_json::json;
use std::{collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;

struct Fixture {
    home: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
        }
    }
    fn service(&self) -> Service {
        Service::new(self.home.path())
    }
    fn package(&self, name: &str) -> PathBuf {
        let path = self.home.path().join(format!("source-{name}"));
        fs::create_dir_all(path.join("skills/review")).unwrap();
        fs::write(
            path.join("plugin.json"),
            json!({"$schema":package::PLUGIN_SCHEMA,"name":name,"version":"1.0.0"}).to_string(),
        )
        .unwrap();
        fs::write(
            path.join("skills/review/SKILL.md"),
            "---\nname: review\ndescription: Review changes\n---\nReview carefully.\n",
        )
        .unwrap();
        path
    }
    fn mcp(&self, path: &Path, servers: serde_json::Value) {
        fs::write(
            path.join("mcp.json"),
            json!({"$schema":package::MCP_SCHEMA,"mcpServers":servers}).to_string(),
        )
        .unwrap();
    }
    fn env(&self) -> HashMap<String, String> {
        HashMap::from([
            (
                "HOME".into(),
                self.home.path().to_string_lossy().into_owned(),
            ),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ])
    }
    fn install(&self, path: &Path) -> Installed {
        self.service()
            .install(path.to_str().unwrap(), None)
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = thaw(self.home.path());
    }
}

#[test]
fn installation_is_a_snapshot_and_runs_no_package_scripts() {
    let f = Fixture::new();
    let path = f.package("example");
    fs::write(
        path.join("package.json"),
        r#"{"scripts":{"postinstall":"touch SHOULD_NOT_EXIST"}}"#,
    )
    .unwrap();
    let p = f.install(&path);
    assert!(!p.trusted);
    assert!(p.enabled);
    assert!(!path.join("SHOULD_NOT_EXIST").exists());
    fs::write(path.join("skills/review/SKILL.md"), "changed").unwrap();
    let snapshot = Snapshot::load(f.home.path(), &File::default(), &f.env());
    let mut catalog = crate::skill::Catalog::default();
    assert!(snapshot.extend_skills(&mut catalog).is_empty());
    let skill = catalog.lookup("example:review").unwrap();
    assert!(
        crate::skill::load(skill)
            .unwrap()
            .contains("Review carefully")
    );
    assert_eq!(
        digest(&f.service().package_path(&p.digest)).unwrap(),
        p.digest
    );
    assert!(
        f.service()
            .install(path.to_str().unwrap(), None)
            .unwrap_err()
            .contains("already installed")
    );
}
#[test]
fn same_local_skill_and_server_names_are_independent() {
    let f = Fixture::new();
    for name in ["one", "two"] {
        let path = f.package(name);
        f.mcp(
            &path,
            json!({"tools":{"type":"stdio","command":"sh","args":["${PLUGIN_ROOT}/server.sh"]}}),
        );
        fs::write(path.join("server.sh"), "").unwrap();
        f.install(&path);
    }
    let snapshot = Snapshot::load(f.home.path(), &File::default(), &f.env());
    let mut catalog = crate::skill::Catalog::default();
    assert!(snapshot.extend_skills(&mut catalog).is_empty());
    assert_eq!(
        catalog
            .skills()
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>(),
        ["one:review", "two:review"]
    );
    assert_eq!(snapshot.servers.len(), 2);
    assert_ne!(snapshot.servers[0].name, snapshot.servers[1].name);
    assert!(
        snapshot
            .servers
            .iter()
            .all(|s| s.name.len() <= 32 && !s.enabled)
    );
}
#[test]
fn disable_remove_and_revoke_have_distinct_effects() {
    let f = Fixture::new();
    let p = f.install(&f.package("example"));
    let service = f.service();
    service.change(&p.name, "trust").unwrap();
    assert!(service.authorized(&p.name, &p.digest));
    service.change(&p.name, "disable").unwrap();
    assert!(
        Snapshot::load(f.home.path(), &File::default(), &f.env())
            .packages
            .is_empty()
    );
    assert!(
        service.authorized(&p.name, &p.digest),
        "disable applies to new runners"
    );
    service.change(&p.name, "enable").unwrap();
    service.change(&p.name, "revoke").unwrap();
    assert!(!service.authorized(&p.name, &p.digest));
    let data = service.ensure_data(&p.name).unwrap();
    fs::write(data.join("saved"), "state").unwrap();
    service.change(&p.name, "remove").unwrap();
    assert!(service.list().unwrap().is_empty());
    assert!(data.join("saved").exists());
    assert!(
        service.package_path(&p.digest).exists(),
        "active runner files are retained"
    );
}
#[test]
fn changed_content_and_executable_mode_invalidate_trust() {
    let f = Fixture::new();
    let p = f.install(&f.package("example"));
    let service = f.service();
    service.change(&p.name, "trust").unwrap();
    let file = service.package_path(&p.digest).join("plugin.json");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o555)).unwrap();
    assert!(!service.authorized(&p.name, &p.digest));
    assert!(service.change(&p.name, "trust").is_err());
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&file, "changed").unwrap();
    assert!(!service.authorized(&p.name, &p.digest));
    assert!(
        Snapshot::load(f.home.path(), &File::default(), &f.env())
            .packages
            .is_empty()
    );
}
#[test]
fn escaping_links_fail_and_safe_links_are_materialized() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let path = f.package("example");
    fs::write(f.home.path().join("outside"), "private").unwrap();
    symlink("../outside", path.join("escape")).unwrap();
    assert!(f.service().install(path.to_str().unwrap(), None).is_err());
    fs::remove_file(path.join("escape")).unwrap();
    symlink("skills/review/SKILL.md", path.join("inside")).unwrap();
    let p = f.install(&path);
    assert!(
        fs::symlink_metadata(f.service().package_path(&p.digest).join("inside"))
            .unwrap()
            .is_file()
    );
}
#[test]
fn invalid_server_does_not_hide_valid_skill_or_sibling_server() {
    let f = Fixture::new();
    let path = f.package("example");
    f.mcp(&path,json!({"bad":{"type":"sse","url":"https://example.test/events"},"good":{"type":"streamable-http","url":"https://example.test/mcp"},"malformed":{"type":"stdio","command":false}}));
    f.install(&path);
    let snapshot = Snapshot::load(f.home.path(), &File::default(), &f.env());
    assert_eq!(snapshot.servers.len(), 1);
    assert!(
        snapshot
            .diagnostics
            .iter()
            .any(|s| s.contains("unsupported MCP transport"))
    );
    assert!(snapshot.diagnostics.iter().any(|s| s.contains("malformed")));
    let mut catalog = crate::skill::Catalog::default();
    snapshot.extend_skills(&mut catalog);
    assert!(catalog.lookup("example:review").is_some());
}
#[test]
fn legacy_inline_mcp_and_custom_skill_paths_load_with_diagnostics() {
    let f = Fixture::new();
    let path = f.package("example");
    fs::remove_file(path.join("plugin.json")).unwrap();
    fs::create_dir(path.join(".claude-plugin")).unwrap();
    fs::rename(path.join("skills"), path.join("custom-skills")).unwrap();
    fs::write(
        path.join(".claude-plugin/plugin.json"),
        json!({
            "name":"example",
            "skills":"./custom-skills",
            "hooks":"./hooks.json",
            "mcpServers":{"tools":{"command":"sh","args":["${CLAUDE_PLUGIN_ROOT}/server.sh"]}}
        })
        .to_string(),
    )
    .unwrap();
    fs::write(path.join("server.sh"), "").unwrap();
    f.install(&path);
    let snapshot = Snapshot::load(f.home.path(), &File::default(), &f.env());
    assert_eq!(snapshot.servers.len(), 1);
    assert!(
        snapshot
            .diagnostics
            .iter()
            .any(|s| s.contains("unsupported component hooks"))
    );
    let mut catalog = crate::skill::Catalog::default();
    assert!(snapshot.extend_skills(&mut catalog).is_empty());
    assert!(catalog.lookup("example:review").is_some());
}
#[test]
fn portable_placeholders_do_not_expand_host_secrets_and_bindings_are_redacted() {
    let f = Fixture::new();
    let path = f.package("example");
    f.mcp(&path,json!({"tools":{"type":"stdio","command":"sh","args":["${PLUGIN_ROOT}/server.sh","${HOST_SECRET}"],"env":{"LITERAL":"${HOST_SECRET}"}}}));
    fs::write(path.join("server.sh"), "").unwrap();
    f.install(&path);
    let mut file = File::default();
    file.plugins.bindings.insert(
        "example:tools".into(),
        PluginBindings {
            env: Some(BTreeMap::from([("TOKEN".into(), "${HOST_SECRET}".into())])),
            ..Default::default()
        },
    );
    let mut env = f.env();
    env.insert("HOST_SECRET".into(), "fixture-only-value".into());
    let snapshot = Snapshot::load(f.home.path(), &file, &env);
    assert_eq!(snapshot.servers.len(), 1);
    let server = &snapshot.servers[0];
    let McpTransport::Stdio { args, env, .. } = &server.transport else {
        panic!("stdio")
    };
    assert_eq!(args[1], "${HOST_SECRET}");
    assert!(env.contains(&("LITERAL".into(), "${HOST_SECRET}".into())));
    assert!(env.contains(&("TOKEN".into(), "fixture-only-value".into())));
    assert_eq!(server.secrets, ["fixture-only-value"]);
    assert!(!snapshot.provenance().contains("fixture-only-value"));
    let registry = fs::read_to_string(f.home.path().join(".otto/plugins/registry.json")).unwrap();
    assert!(!registry.contains("fixture-only-value"));
}
#[test]
fn transport_cwd_reserved_variables_and_managed_policy_are_enforced() {
    let f = Fixture::new();
    let path = f.package("example");
    f.mcp(
        &path,
        json!({
            "cwd":{"type":"stdio","command":"sh","cwd":"${PLUGIN_ROOT}/.."},
            "reserved":{"type":"stdio","command":"sh","env":{"PLUGIN_ROOT":"evil"}},
            "remote":{"type":"streamable-http","url":"http://example.test/mcp"},
            "good":{"type":"stdio","command":"sh"}
        }),
    );
    f.install(&path);
    let snapshot = Snapshot::load(f.home.path(), &File::default(), &f.env());
    assert_eq!(snapshot.servers.len(), 1);
    let file = File {
        policy: Some(otto_core::config::layered::ManagedPolicy {
            allowed_mcp_commands: Some(vec!["other".into()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let snapshot = Snapshot::load(f.home.path(), &file, &f.env());
    assert!(snapshot.servers.is_empty());
    assert!(
        snapshot
            .diagnostics
            .iter()
            .any(|s| s.contains("prohibited by managed policy"))
    );
}
#[test]
fn git_install_pins_commit_and_rejects_floating_revisions() {
    let f = Fixture::new();
    let path = f.package("example");
    fn git(path: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
    git(&path, &["init", "-q"]);
    git(&path, &["add", "."]);
    git(
        &path,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    let commit = git(&path, &["rev-parse", "HEAD"]);
    assert!(
        f.service()
            .install(path.to_str().unwrap(), Some("main"))
            .is_err()
    );
    let installed = f
        .service()
        .install(path.to_str().unwrap(), Some(commit.trim()))
        .unwrap();
    assert_eq!(installed.commit.as_deref(), Some(commit.trim()));
    assert!(
        !f.service()
            .package_path(&installed.digest)
            .join(".git")
            .exists()
    );
}
#[test]
fn cli_management_requires_no_provider_and_reports_configuration() {
    let f = Fixture::new();
    let path = f.package("example");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let run = |args: Vec<String>, out: &mut Vec<u8>, err: &mut Vec<u8>| {
        crate::cli::plugin::run(&args, out, err, &f.env())
    };
    assert_eq!(
        run(
            vec!["install".into(), path.to_string_lossy().into_owned()],
            &mut out,
            &mut err
        ),
        0
    );
    out.clear();
    assert_eq!(
        run(vec!["inspect".into(), "example".into()], &mut out, &mut err),
        0
    );
    let report: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(report["plugin"]["name"], "example");
    assert_eq!(report["content_authorized"], false);
    assert!(err.is_empty());
}

fn result_text(result: &ToolResult) -> String {
    result.content.clone()
}

#[tokio::test]
async fn shared_runtime_routes_plugin_tools_and_revocation_blocks_parent_and_child() {
    let f = Fixture::new();
    let path = f.package("example");
    f.mcp(
        &path,
        json!({"tools":{"type":"stdio","command":"sh","args":["${PLUGIN_ROOT}/server.sh"]}}),
    );
    fs::write(path.join("server.sh"),r#": > "$PLUGIN_DATA/started"
while IFS= read -r line; do
  id=${line#*'"id":'}; id=${id%%[,\}]*}
  case "$line" in
    *'"method":"server/discover"'*) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no"}}\n' "$id";;
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-11-25"}}\n' "$id";;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"echo","inputSchema":{"type":"object"}}]}}\n' "$id";;
    *'"method":"tools/call"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"fixture response"}]}}\n' "$id";;
  esac
done
"#).unwrap();
    f.install(&path);
    let mut builder = crate::cli::testutil::builder(f.home.path(), &f.home.path().join("sessions"));
    builder.shared_mut().environment.extend(f.env());
    builder.mcp.enabled = true;
    builder.mcp.connect_timeout_secs = 2;
    let mut warnings = Vec::new();
    let (tools, _, servers) = builder.connect_mcp(4096, &mut warnings).await;
    assert!(tools.is_empty());
    assert_eq!(servers.status()[0].state, crate::mcp::ServerState::Disabled);
    assert!(!f.service().data_path("example").join("started").exists());
    f.service().change("example", "trust").unwrap();
    let (tools, connected, servers) = builder.connect_mcp(4096, &mut warnings).await;
    assert_eq!(servers.status()[0].name, "example:tools");
    assert!(
        matches!(
            servers.status()[0].state,
            crate::mcp::ServerState::Connected { tools: 1 }
        ),
        "{:?} {}",
        servers.status(),
        String::from_utf8_lossy(&warnings)
    );
    assert!(f.service().data_path("example").join("started").exists());
    let raw = serde_json::value::RawValue::from_string(
        json!({"tool":format!("mcp__{}__echo",runtime_name("example:tools")),"arguments":{}})
            .to_string(),
    )
    .unwrap();
    let call = tools
        .iter()
        .find(|t| t.definition().name == "mcp_call_tool")
        .unwrap();
    assert!(
        result_text(&call.execute(&raw, &CancellationToken::new()).await)
            .contains("fixture response")
    );
    let child = crate::runtime::wiring::mcp_child_tools(&connected, 4096);
    f.service().change("example", "revoke").unwrap();
    for call in [
        call,
        child
            .iter()
            .find(|t| t.definition().name == "mcp_call_tool")
            .unwrap(),
    ] {
        let result = call.execute(&raw, &CancellationToken::new()).await;
        assert!(result.is_error);
        assert_eq!(
            result.outcome_override.as_ref().unwrap().effect_certainty,
            otto_core::model::EffectCertainty::NotStarted
        );
        assert!(result_text(&result).contains("authorization was revoked"));
    }
    servers.close().await;
}

#[tokio::test]
async fn plugin_oauth_logout_accepts_logical_identity_without_network() {
    let f = Fixture::new();
    let path = f.package("example");
    f.mcp(
        &path,
        json!({"remote":{"type":"streamable-http","url":"https://example.test/mcp"}}),
    );
    f.install(&path);
    fs::create_dir_all(f.home.path().join(".config/otto")).unwrap();
    fs::write(
        f.home.path().join(".config/otto/config.toml"),
        "[plugins.bindings.'example:remote']\nauth='oauth'\n",
    )
    .unwrap();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = crate::cli::mcp::run(
        &["logout".into(), "example:remote".into()],
        &mut out,
        &mut err,
        &f.env(),
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert!(String::from_utf8_lossy(&out).contains(&runtime_name("example:remote")));
}

#[test]
fn plugin_bindings_participate_in_environment_capture_and_redaction() {
    let mut file = File::default();
    file.plugins.bindings.insert(
        "example:tools".into(),
        PluginBindings {
            env: Some(BTreeMap::from([(
                "TOKEN".into(),
                "${PLUGIN_CREDENTIAL}".into(),
            )])),
            ..Default::default()
        },
    );
    let lookup = HashMap::from([("PLUGIN_CREDENTIAL".into(), "fixture-only-value".into())]);
    let environment = crate::cli::run::config_environment_for(&file, &lookup);
    assert_eq!(
        environment.get("PLUGIN_CREDENTIAL"),
        lookup.get("PLUGIN_CREDENTIAL")
    );
    let root = tempfile::tempdir().unwrap();
    let mut builder = crate::cli::testutil::builder(root.path(), &root.path().join("sessions"));
    builder.shared_mut().config = file;
    builder.shared_mut().environment = environment;
    let secrets = builder.secret_values(None);
    assert!(secrets.contains(&"PLUGIN_CREDENTIAL".into()));
    assert!(secrets.contains(&"fixture-only-value".into()));
}

#[tokio::test]
async fn plugin_dispatch_runs_before_provider_configuration() {
    let f = Fixture::new();
    let path = f.package("example");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = crate::cli::run::run(
        &[
            "plugin".into(),
            "install".into(),
            path.to_string_lossy().into_owned(),
        ],
        Box::new(std::io::Cursor::new(Vec::<u8>::new())),
        &mut out,
        &mut err,
        vec![format!("HOME={}", f.home.path().display()).into_bytes()],
        false,
        &CancellationToken::new(),
        &crate::cli::terminate::Terminate::new(),
    )
    .await;
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert_eq!(f.service().list().unwrap().len(), 1);
}

#[test]
fn package_commands_and_cwd_use_filesystem_resolved_home() {
    use std::os::unix::fs::symlink;
    let f = Fixture::new();
    let path = f.package("example");
    fs::write(path.join("server.sh"), "#!/bin/sh\n").unwrap();
    fs::set_permissions(path.join("server.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    f.mcp(
        &path,
        json!({"tools":{"type":"stdio","command":"./server.sh"}}),
    );
    f.install(&path);
    let alias = f.home.path().join("home-alias");
    symlink(f.home.path(), &alias).unwrap();
    let snapshot = Snapshot::load(&alias, &File::default(), &f.env());
    assert_eq!(snapshot.servers.len(), 1, "{:?}", snapshot.diagnostics);
    let McpTransport::Stdio {
        command, cwd, env, ..
    } = &snapshot.servers[0].transport
    else {
        panic!("stdio")
    };
    let actual = fs::canonicalize(f.home.path()).unwrap();
    assert!(Path::new(command).starts_with(&actual));
    assert!(Path::new(cwd).starts_with(&actual));
    assert!(
        env.iter()
            .filter(|(k, _)| k == "PLUGIN_ROOT" || k == "PLUGIN_DATA")
            .all(|(_, v)| Path::new(v).starts_with(&actual))
    );
    fs::remove_file(alias).unwrap();
}

#[test]
fn managed_command_allowlist_uses_resolved_administrator_executable() {
    let f = Fixture::new();
    let path = f.package("example");
    f.mcp(&path, json!({"tools":{"type":"stdio","command":"sh"}}));
    f.install(&path);
    let executable =
        crate::runtime::sandbox_runtime::canonical_executable_file(Path::new("/bin/sh"))
            .unwrap()
            .to_string_lossy()
            .into_owned();
    let file = File {
        policy: Some(otto_core::config::layered::ManagedPolicy {
            allowed_mcp_commands: Some(vec![executable.clone()]),
            ..Default::default()
        }),
        ..Default::default()
    };
    let snapshot = Snapshot::load(f.home.path(), &file, &f.env());
    assert_eq!(snapshot.servers.len(), 1, "{:?}", snapshot.diagnostics);
    let McpTransport::Stdio { command, .. } = &snapshot.servers[0].transport else {
        panic!("stdio")
    };
    assert_eq!(command, &executable);
}

#[test]
fn header_templates_stay_literal_until_user_binding_overrides_them() {
    let f = Fixture::new();
    let path = f.package("example");
    f.mcp(&path,json!({"remote":{"type":"streamable-http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer ${PACKAGE_TOKEN}"}}}));
    f.install(&path);
    let snapshot = Snapshot::load(f.home.path(), &File::default(), &f.env());
    let McpTransport::Http { headers, .. } = &snapshot.servers[0].transport else {
        panic!("HTTP")
    };
    assert_eq!(headers[0].1, "Bearer ${PACKAGE_TOKEN}");
    let mut file = File::default();
    file.plugins.bindings.insert(
        "example:remote".into(),
        PluginBindings {
            headers: Some(BTreeMap::from([(
                "Authorization".into(),
                "Bearer ${HOST_TOKEN}".into(),
            )])),
            ..Default::default()
        },
    );
    let mut env = f.env();
    env.insert("HOST_TOKEN".into(), "fixture-only-value".into());
    let snapshot = Snapshot::load(f.home.path(), &file, &env);
    let McpTransport::Http { headers, .. } = &snapshot.servers[0].transport else {
        panic!("HTTP")
    };
    assert_eq!(headers[0].1, "Bearer fixture-only-value");
    assert_eq!(snapshot.servers[0].secrets, ["fixture-only-value"]);
    assert!(
        !fs::read_to_string(f.home.path().join(".otto/plugins/registry.json"))
            .unwrap()
            .contains("fixture-only-value")
    );
}

#[test]
fn literal_credentials_cannot_be_persisted_by_installation() {
    let f = Fixture::new();
    let path = f.package("example");
    f.mcp(&path,json!({"remote":{"type":"streamable-http","url":"https://example.test/mcp","headers":{"Authorization":"Bearer fixture-only-value"}}}));
    assert!(
        f.service()
            .install(path.to_str().unwrap(), None)
            .unwrap_err()
            .contains("literal credential")
    );
    assert!(f.service().list().unwrap().is_empty());
}

#[tokio::test]
async fn otto_commands_require_content_trust_and_revoke_without_reloading() {
    use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy, direct::DirectDriver};
    let f = Fixture::new();
    let path = f.package("command-example");
    let components = json!({
        "version":1,
        "hooks":{"before_tool":[{
            "matcher":["write"],
            "command":["/bin/sh","-c","read input; printf '%s' '{\"decision\":\"deny\",\"reason\":\"blocked\"}'"]
        }]},
        "tools":[{
            "name":"echo",
            "description":"Echo the value",
            "input_schema":{
                "type":"object",
                "required":["value"],
                "properties":{"value":{"type":"integer","minimum":1}},
                "additionalProperties":false
            },
            "command":["/bin/sh","-c","read input; printf '%s' '{\"content\":\"done\"}'"]
        }]
    });
    fs::write(path.join("plugin.json"),json!({"$schema":package::PLUGIN_SCHEMA,"name":"command-example","extensions":{"dev.otto":components}}).to_string()).unwrap();
    f.install(&path);
    let workspace = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let executor = Arc::new(
        Executor::new(
            Arc::new(DirectDriver::new()),
            Policy {
                filesystem: FilesystemMode::Unconfined,
                network: NetworkMode::Allow,
            },
            &root,
        )
        .unwrap(),
    );
    let execution = hooks::Execution {
        executor: Some(executor),
        workspace: root,
        environment: vec!["PATH=/usr/bin:/bin".into()],
        guard: None,
        secrets: Vec::new(),
        logger: Arc::new(crate::log::Logger::discard()),
    };
    let snapshot = Snapshot::load(f.home.path(), &File::default(), &f.env());
    let untrusted = hooks::Runtime::new(
        &snapshot,
        f.home.path(),
        &Default::default(),
        execution.clone(),
    )
    .unwrap();
    assert!(untrusted.tools().is_empty());
    assert!(!untrusted.has_hooks());
    assert!(
        snapshot
            .diagnostics
            .iter()
            .any(|s| s.contains("hooks and custom tools disabled"))
    );
    f.service().change("command-example", "trust").unwrap();
    let snapshot = Snapshot::load(f.home.path(), &File::default(), &f.env());
    let runtime = hooks::Runtime::new(
        &snapshot,
        f.home.path(),
        &Default::default(),
        execution.clone(),
    )
    .unwrap();
    let response = runtime
        .bind("s".into(), None, String::new(), "new", None)
        .run(
            "before_tool",
            json!({"tool":"write","arguments":{}}),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.decision,
        Some(otto_core::agent::hooks::Decision::Deny)
    );
    let tools = runtime.tools();
    assert_eq!(tools[0].definition().name, "plugin__command-example__echo");
    let raw = |s: &str| serde_json::value::RawValue::from_string(s.into()).unwrap();
    let bad = tools[0]
        .execute(&raw("{\"value\":0}"), &CancellationToken::new())
        .await;
    assert!(bad.is_error);
    assert_eq!(
        bad.outcome_override.unwrap().effect_certainty,
        otto_core::model::EffectCertainty::NotStarted
    );
    let good = tools[0]
        .execute(&raw("{\"value\":1}"), &CancellationToken::new())
        .await;
    assert_eq!(good.content, "done");
    assert!(!good.is_error);
    f.service().change("command-example", "revoke").unwrap();
    assert!(
        tools[0]
            .execute(&raw("{\"value\":1}"), &CancellationToken::new())
            .await
            .is_error
    );
    assert!(
        runtime
            .bind("s".into(), None, String::new(), "new", None)
            .run(
                "before_tool",
                json!({"tool":"write"}),
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    f.service().change("command-example", "disable").unwrap();
    let disabled = Snapshot::load(f.home.path(), &File::default(), &f.env());
    assert!(
        hooks::Runtime::new(&disabled, f.home.path(), &Default::default(), execution)
            .unwrap()
            .tools()
            .is_empty()
    );
}

#[test]
fn invalid_otto_components_fail_installation_without_running_commands() {
    let f = Fixture::new();
    let path = f.package("bad-extension");
    for extension in [
        json!({"version":2}),
        json!({"version":1,"hooks":{"before_tool":[{"command":[]} ]}}),
        json!({"version":1,"tools":[{"name":"bad","description":"Bad tool","command":["/bin/sh"],"input_schema":{"$ref":"file:///etc/passwd"}}]}),
    ] {
        fs::write(path.join("plugin.json"),json!({"$schema":package::PLUGIN_SCHEMA,"name":"bad-extension","extensions":{"dev.otto":extension}}).to_string()).unwrap();
        assert!(f.service().install(path.to_str().unwrap(), None).is_err());
    }
}
