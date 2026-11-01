//! The `otto plugin` and `otto mcp` commands over an installed package,
//! run in-process through the CLI entry points without a provider.

use std::fs;

use otto::fixtures::plugin_package::Fixture;
use otto::plugin::runtime_name;
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[test]
fn cli_management_requires_no_provider_and_reports_configuration() {
    let f = Fixture::new();
    let path = f.package("example");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let run = |args: Vec<String>, out: &mut Vec<u8>, err: &mut Vec<u8>| {
        otto_cli::plugin::run(&args, out, err, &f.env())
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
    let code = otto_cli::mcp::run(
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

#[tokio::test]
async fn plugin_dispatch_runs_before_provider_configuration() {
    let f = Fixture::new();
    let path = f.package("example");
    let mut out = Vec::new();
    let mut err = Vec::new();
    let code = otto_cli::run::run(
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
        &otto_cli::terminate::Terminate::new(),
    )
    .await;
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    assert_eq!(f.service().list().unwrap().len(), 1);
}

#[test]
fn trust_disclosure_lists_the_memory_command_of_a_memory_only_package() {
    let f = Fixture::new();
    let path = f.package_with_extension(
        "mem",
        json!({"version":1,"memory":{"command":["/opt/mem/serve"]}}),
    );
    f.install(&path);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let args = vec!["trust".to_string(), "mem".to_string()];
    assert_eq!(
        otto_cli::plugin::run(&args, &mut out, &mut err, &f.env()),
        0
    );
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("Otto hooks/tools/memory:") && out.contains("/opt/mem/serve"),
        "{out}"
    );
}
