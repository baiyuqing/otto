use super::*;
use crate::sandbox::{Executor, FilesystemMode, NetworkMode, Policy, direct::DirectDriver};
use otto_core::config::{File, PluginBindings};
use serde_json::json;
use std::{collections::HashMap, fs, sync::Arc};
use tokio_util::sync::CancellationToken;

fn file_with_binding(package: &str, binding: PluginBindings) -> File {
    let mut file = File::default();
    file.plugins.bindings.insert(package.into(), binding);
    file
}
fn env_binding(name: &str, value: &str) -> PluginBindings {
    PluginBindings {
        env: Some([(name.to_string(), value.to_string())].into()),
        ..Default::default()
    }
}
fn host_environment() -> HashMap<String, String> {
    HashMap::from([("PACKAGE_BOUND_VAR".into(), "test-bound-value".into())])
}

#[test]
fn package_binding_resolves_template_from_environment() {
    let file = file_with_binding("alpha", env_binding("BOUND_NAME", "${PACKAGE_BOUND_VAR}"));
    let bound = runtime::package_environment(&file, "alpha", &host_environment()).unwrap();
    assert_eq!(
        bound,
        vec![("BOUND_NAME".to_string(), "test-bound-value".to_string())]
    );
    assert!(
        runtime::package_environment(&file, "beta", &host_environment())
            .unwrap()
            .is_empty()
    );
    // Dotted names over 32 bytes are valid package names but not MCP server names.
    let dotted = "dev.example.memory-plugin-with-a-long-name";
    let file = file_with_binding(dotted, env_binding("BOUND_NAME", "${PACKAGE_BOUND_VAR}"));
    assert_eq!(
        runtime::package_environment(&file, dotted, &host_environment()).unwrap(),
        vec![("BOUND_NAME".to_string(), "test-bound-value".to_string())]
    );
}

#[test]
fn package_binding_rejects_mcp_only_fields_literals_and_bad_names() {
    let headers = PluginBindings {
        headers: Some([("X".to_string(), "${PACKAGE_BOUND_VAR}".to_string())].into()),
        ..Default::default()
    };
    for binding in [
        headers,
        env_binding("BOUND_NAME", "sk-literal-credential-value"),
        env_binding("1BAD", "${PACKAGE_BOUND_VAR}"),
        env_binding("BAD=NAME", "${PACKAGE_BOUND_VAR}"),
    ] {
        let file = file_with_binding("alpha", binding);
        let error = runtime::package_environment(&file, "alpha", &host_environment()).unwrap_err();
        assert!(
            error.contains("alpha") && !error.contains("test-bound-value"),
            "{error}"
        );
    }
}

fn install_hook_package(home: &Path, name: &str) {
    let source = home.join(format!("source-{name}"));
    fs::create_dir_all(&source).unwrap();
    let script = "read input; printf '{\"context\":\"%s\"}' \"$BOUND_NAME\"";
    let hooks = json!({"version":1,"hooks":{"before_prompt":[{
        "command":["/bin/sh","-c",script]
    }]}});
    fs::write(
        source.join("plugin.json"),
        json!({"$schema":package::PLUGIN_SCHEMA,"name":name,"extensions":{"dev.otto":hooks}})
            .to_string(),
    )
    .unwrap();
    let service = Service::new(home);
    service.install(source.to_str().unwrap(), None).unwrap();
    service.change(name, "trust").unwrap();
}

#[tokio::test]
async fn package_binding_reaches_only_its_own_package_commands() {
    let home = tempfile::tempdir().unwrap();
    install_hook_package(home.path(), "alpha");
    install_hook_package(home.path(), "beta");
    let workspace = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(workspace.path()).unwrap();
    let executor = Executor::new(
        Arc::new(DirectDriver::new()),
        Policy {
            filesystem: FilesystemMode::Unconfined,
            network: NetworkMode::Allow,
        },
        &root,
    )
    .unwrap();
    let base_environment = vec!["PATH=/usr/bin:/bin".to_string()];
    let execution = hooks::Execution {
        executor: Some(Arc::new(executor)),
        workspace: root,
        environment: base_environment.clone(),
        package_environment: [(
            "alpha".to_string(),
            vec![("BOUND_NAME".to_string(), "test-bound-value".to_string())],
        )]
        .into(),
        guard: None,
        secrets: Vec::new(),
        logger: Arc::new(crate::log::Logger::discard()),
    };
    let host = HashMap::from([(
        "HOME".to_string(),
        home.path().to_string_lossy().into_owned(),
    )]);
    let snapshot = Snapshot::load(home.path(), &File::default(), &host);
    let runtime = hooks::Runtime::new(
        &snapshot,
        home.path(),
        &Default::default(),
        execution.clone(),
    )
    .unwrap();
    let response = runtime
        .bind("s".into(), None, String::new(), "new", None)
        .run("before_prompt", json!({}), &CancellationToken::new())
        .await
        .unwrap();
    // Only alpha's command sees the bound value; beta's prints an empty string.
    let contexts = format!("{:?}", response);
    assert_eq!(
        contexts.matches("test-bound-value").count(),
        1,
        "{contexts}"
    );
    assert_eq!(execution.environment, base_environment);
}
