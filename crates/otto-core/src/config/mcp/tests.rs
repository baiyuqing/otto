use super::*;
use crate::config::File;

type FieldRejectionCase = (fn(&mut McpServer), &'static str);

fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn stdio_server(command: &str) -> McpServer {
    McpServer {
        transport: "stdio".into(),
        command: Some(command.into()),
        ..Default::default()
    }
}

fn http_server(url: &str) -> McpServer {
    McpServer {
        transport: "http".into(),
        url: Some(url.into()),
        ..Default::default()
    }
}

fn file_with(servers: Vec<(&str, McpServer)>) -> File {
    let mut file = File::default();
    for (name, server) in servers {
        file.mcp.servers.insert(name.to_string(), server);
    }
    file
}

#[test]
fn disabled_skips_server_validation() {
    let mut file = file_with(vec![("bad name", stdio_server(""))]);
    file.mcp.enabled = Some(false);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    assert!(!runtime.enabled);
    assert!(runtime.servers.is_empty());
}

#[test]
fn defaults_enabled_with_no_servers() {
    let runtime = resolve_mcp(&File::default(), &HashMap::new(), "/work").expect("resolve");
    assert!(runtime.enabled);
    assert_eq!(runtime.call_timeout_secs, 60);
    assert_eq!(runtime.connect_timeout_secs, 20);
    assert!(runtime.servers.is_empty());
}

#[test]
fn rejects_invalid_server_name() {
    let file = file_with(vec![("bad name!", stdio_server("./run"))]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("invalid name"), "{err}");
}

#[test]
fn accepts_max_length_name() {
    let name = "a".repeat(32);
    let file = file_with(vec![(&name, stdio_server("./run"))]);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    assert_eq!(runtime.servers[0].name, name);
}

#[test]
fn rejects_too_long_name() {
    let name = "a".repeat(33);
    let file = file_with(vec![(&name, stdio_server("./run"))]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("invalid name"), "{err}");
}

#[test]
fn rejects_unknown_transport() {
    let mut server = stdio_server("./run");
    server.transport = "websocket".into();
    let file = file_with(vec![("ws", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("transport must be"), "{err}");
}

#[test]
fn stdio_requires_command() {
    let mut server = stdio_server("./run");
    server.command = None;
    let file = file_with(vec![("s", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("command is required"), "{err}");
}

#[test]
fn stdio_rejects_http_only_fields() {
    let cases: [FieldRejectionCase; 4] = [
        (
            (|s: &mut McpServer| s.url = Some("https://example.com".into())) as fn(&mut McpServer),
            "url is not allowed",
        ),
        (
            (|s: &mut McpServer| s.headers = Some(BTreeMap::from([("X".into(), "y".into())])))
                as fn(&mut McpServer),
            "headers is not allowed",
        ),
        (
            (|s: &mut McpServer| s.auth = Some("none".into())) as fn(&mut McpServer),
            "auth is not allowed",
        ),
        (
            (|s: &mut McpServer| s.oauth_client_id = Some("id".into())) as fn(&mut McpServer),
            "oauth_client_id and oauth_scopes are not allowed",
        ),
    ];
    for (mutate, expected) in cases {
        let mut server = stdio_server("./run");
        mutate(&mut server);
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[test]
fn http_requires_url() {
    let mut server = http_server("https://example.com/mcp");
    server.url = None;
    let file = file_with(vec![("s", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("url is required"), "{err}");
}

#[test]
fn http_rejects_stdio_only_fields() {
    let cases: [FieldRejectionCase; 4] = [
        (
            (|s: &mut McpServer| s.command = Some("./run".into())) as fn(&mut McpServer),
            "command is not allowed",
        ),
        (
            (|s: &mut McpServer| s.args = Some(vec!["-y".into()])) as fn(&mut McpServer),
            "args is not allowed",
        ),
        (
            (|s: &mut McpServer| s.env = Some(BTreeMap::from([("X".into(), "y".into())])))
                as fn(&mut McpServer),
            "env is not allowed",
        ),
        (
            (|s: &mut McpServer| s.cwd = Some(".".into())) as fn(&mut McpServer),
            "cwd is not allowed",
        ),
    ];
    for (mutate, expected) in cases {
        let mut server = http_server("https://example.com/mcp");
        mutate(&mut server);
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }
}

#[test]
fn auth_defaults_to_none_and_rejects_unknown_value() {
    let file = file_with(vec![("s", http_server("https://example.com/mcp"))]);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    match &runtime.servers[0].transport {
        McpTransport::Http { auth, .. } => assert_eq!(*auth, McpAuth::None),
        _ => panic!("expected http transport"),
    }

    let mut server = http_server("https://example.com/mcp");
    server.auth = Some("basic".into());
    let file = file_with(vec![("s", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("auth must be"), "{err}");
}

#[test]
fn oauth_fields_require_auth_oauth() {
    let mut server = http_server("https://example.com/mcp");
    server.oauth_client_id = Some("client".into());
    let file = file_with(vec![("s", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(
        err.to_string().contains("oauth_client_id requires"),
        "{err}"
    );

    let mut server = http_server("https://example.com/mcp");
    server.oauth_scopes = Some(vec!["mcp:tools".into()]);
    let file = file_with(vec![("s", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("oauth_scopes requires"), "{err}");

    let mut server = http_server("https://example.com/mcp");
    server.auth = Some("oauth".into());
    server.oauth_client_id = Some("client".into());
    server.oauth_scopes = Some(vec!["mcp:tools".into()]);
    let file = file_with(vec![("s", server)]);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    match &runtime.servers[0].transport {
        McpTransport::Http {
            auth,
            oauth_client_id,
            oauth_scopes,
            ..
        } => {
            assert_eq!(*auth, McpAuth::OAuth);
            assert_eq!(oauth_client_id.as_deref(), Some("client"));
            assert_eq!(oauth_scopes, &vec!["mcp:tools".to_string()]);
        }
        _ => panic!("expected http transport"),
    }
}

#[test]
fn rejects_authorization_header_with_oauth() {
    let mut server = http_server("https://example.com/mcp");
    server.auth = Some("oauth".into());
    server.headers = Some(BTreeMap::from([("Authorization".into(), "x".into())]));
    let file = file_with(vec![("s", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(
        err.to_string().contains("cannot set Authorization"),
        "{err}"
    );
}

#[test]
fn normalizes_url_trailing_slash_and_fragment() {
    let file = file_with(vec![(
        "s",
        http_server("https://mcp.example.com/mcp/#frag"),
    )]);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    match &runtime.servers[0].transport {
        McpTransport::Http { url, .. } => assert_eq!(url, "https://mcp.example.com/mcp"),
        _ => panic!("expected http transport"),
    }
}

#[test]
fn rejects_non_http_scheme() {
    let file = file_with(vec![("s", http_server("ftp://example.com/mcp"))]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("must use http or https"), "{err}");
}

#[test]
fn rejects_invalid_url() {
    let file = file_with(vec![("s", http_server("not a url"))]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("invalid url"), "{err}");
}

#[test]
fn expands_var_and_default_across_fields() {
    let mut server = stdio_server("${BIN}");
    server.args = Some(vec!["${MODE:-prod}".into()]);
    server.env = Some(BTreeMap::from([("TOKEN".into(), "${GITHUB_TOKEN}".into())]));
    server.cwd = Some("${WORKDIR}".into());
    let file = file_with(vec![("s", server)]);
    let env = env(&[
        ("BIN", "/usr/bin/run"),
        ("GITHUB_TOKEN", "secret-token"),
        ("WORKDIR", "sub"),
    ]);
    let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
    let entry = &runtime.servers[0];
    match &entry.transport {
        McpTransport::Stdio {
            command,
            args,
            env: env_table,
            cwd,
        } => {
            assert_eq!(command, "/usr/bin/run");
            assert_eq!(args, &vec!["prod".to_string()]);
            assert!(env_table.contains(&("TOKEN".to_string(), "secret-token".to_string())));
            assert_eq!(cwd, "/work/sub");
        }
        _ => panic!("expected stdio transport"),
    }
    assert!(entry.secrets.contains(&"secret-token".to_string()));
    // "prod" came from a default, not an env substitution: not a secret.
    assert!(!entry.secrets.contains(&"prod".to_string()));
}

#[test]
fn stdio_command_args_and_cwd_substitutions_are_not_collected_as_secrets() {
    let mut server = stdio_server("${BIN}");
    server.args = Some(vec!["${ARG_VAR}".into()]);
    server.env = Some(BTreeMap::from([("TOKEN".into(), "${ENV_VAR}".into())]));
    server.cwd = Some("${CWD_VAR}".into());
    let file = file_with(vec![("s", server)]);
    let env = env(&[
        ("BIN", "/usr/bin/run"),
        ("ARG_VAR", "arg-value"),
        ("ENV_VAR", "env-secret"),
        ("CWD_VAR", "sub"),
    ]);
    let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
    let entry = &runtime.servers[0];
    assert_eq!(entry.secrets, vec!["env-secret".to_string()]);
    assert!(!entry.secrets.contains(&"/usr/bin/run".to_string()));
    assert!(!entry.secrets.contains(&"arg-value".to_string()));
    assert!(!entry.secrets.contains(&"sub".to_string()));
}

#[test]
fn http_url_substitutions_are_not_collected_as_secrets_but_headers_are() {
    let mut server = http_server("https://${HOST_VAR}/mcp");
    server.headers = Some(BTreeMap::from([("X-Token".into(), "${HEADER_VAR}".into())]));
    let file = file_with(vec![("s", server)]);
    let env = env(&[("HOST_VAR", "example.com"), ("HEADER_VAR", "header-secret")]);
    let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
    let entry = &runtime.servers[0];
    assert_eq!(entry.secrets, vec!["header-secret".to_string()]);
    assert!(!entry.secrets.contains(&"example.com".to_string()));
}

#[test]
fn dollar_dollar_and_bare_dollar_are_literal() {
    let mut server = stdio_server("echo $$5 and $HOME");
    server.env = None;
    let file = file_with(vec![("s", server)]);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    match &runtime.servers[0].transport {
        McpTransport::Stdio { command, .. } => {
            assert_eq!(command, "echo $$5 and $HOME");
        }
        _ => panic!("expected stdio transport"),
    }
}

#[test]
fn errors_on_unset_variable_without_default() {
    let server = stdio_server("${MISSING_VAR}");
    let file = file_with(vec![("s", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    let message = err.to_string();
    assert!(message.contains("command"), "{message}");
    assert!(
        message.contains("unset environment variable MISSING_VAR"),
        "{message}"
    );
}

#[test]
fn rejects_literal_bearer_token_in_header() {
    let mut server = http_server("https://example.com/mcp");
    server.headers = Some(BTreeMap::from([(
        "Authorization".into(),
        "Bearer abc123".into(),
    )]));
    let file = file_with(vec![("s", server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(
        err.to_string().contains("looks like a literal credential"),
        "{err}"
    );
}

#[test]
fn rejects_known_credential_prefixes_in_env() {
    for value in ["ghp_abcdef", "sk-abcdef", "xoxb-abcdef", "xoxp-abcdef"] {
        let mut server = stdio_server("./run");
        server.env = Some(BTreeMap::from([("TOKEN".into(), value.into())]));
        let file = file_with(vec![("s", server)]);
        let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
        assert!(
            err.to_string().contains("looks like a literal credential"),
            "{value}: {err}"
        );
    }
}

#[test]
fn allows_credential_prefix_inside_var_reference() {
    let mut server = http_server("https://example.com/mcp");
    server.headers = Some(BTreeMap::from([(
        "Authorization".into(),
        "Bearer ${TOKEN}".into(),
    )]));
    let file = file_with(vec![("s", server)]);
    let env = env(&[("TOKEN", "sk-should-not-be-checked-again")]);
    let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
    match &runtime.servers[0].transport {
        McpTransport::Http { headers, .. } => {
            assert_eq!(
                headers,
                &vec![(
                    "Authorization".to_string(),
                    "Bearer sk-should-not-be-checked-again".to_string()
                )]
            );
        }
        _ => panic!("expected http transport"),
    }
}

#[test]
fn stdio_child_env_includes_inherited_keys_when_present_and_not_overridden() {
    let mut server = stdio_server("./run");
    server.env = Some(BTreeMap::from([("PATH".into(), "/custom/bin".into())]));
    let file = file_with(vec![("s", server)]);
    let env = env(&[
        ("PATH", "/usr/bin"),
        ("HOME", "/home/u"),
        ("TMPDIR", "/tmp"),
        ("LANG", "en_US.UTF-8"),
        ("TERM", "xterm"),
        ("OTHER", "not-copied"),
    ]);
    let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
    match &runtime.servers[0].transport {
        McpTransport::Stdio { env: env_table, .. } => {
            assert!(env_table.contains(&("PATH".to_string(), "/custom/bin".to_string())));
            assert!(env_table.contains(&("HOME".to_string(), "/home/u".to_string())));
            assert!(env_table.contains(&("TMPDIR".to_string(), "/tmp".to_string())));
            assert!(env_table.contains(&("LANG".to_string(), "en_US.UTF-8".to_string())));
            assert!(env_table.contains(&("TERM".to_string(), "xterm".to_string())));
            assert!(!env_table.iter().any(|(k, _)| k == "OTHER"));
        }
        _ => panic!("expected stdio transport"),
    }
}

#[test]
fn cwd_defaults_to_workspace_path() {
    let file = file_with(vec![("s", stdio_server("./run"))]);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    match &runtime.servers[0].transport {
        McpTransport::Stdio { cwd, .. } => assert_eq!(cwd, "/work"),
        _ => panic!("expected stdio transport"),
    }
}

#[test]
fn cwd_resolves_home_relative_and_rejects_unresolvable_home() {
    let mut server = stdio_server("./run");
    server.cwd = Some("~/x".into());
    let file = file_with(vec![("s", server)]);

    let home_env = env(&[("HOME", "/home/u")]);
    let runtime = resolve_mcp(&file, &home_env, "/work").expect("resolve");
    match &runtime.servers[0].transport {
        McpTransport::Stdio { cwd, .. } => assert_eq!(cwd, "/home/u/x"),
        _ => panic!("expected stdio transport"),
    }

    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("cwd cannot be resolved"), "{err}");
}

#[test]
fn disabled_server_stays_listed_but_still_validated() {
    let mut server = stdio_server("./run");
    server.enabled = Some(false);
    let file = file_with(vec![("s", server)]);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    assert_eq!(runtime.servers.len(), 1);
    assert!(!runtime.servers[0].enabled);

    let mut bad_server = stdio_server("./run");
    bad_server.enabled = Some(false);
    bad_server.command = None;
    let file = file_with(vec![("s", bad_server)]);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("command is required"), "{err}");
}

#[test]
fn rejects_zero_timeouts() {
    let mut file = File::default();
    file.mcp.call_timeout_secs = Some(0);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("call_timeout_secs"), "{err}");

    let mut file = File::default();
    file.mcp.connect_timeout_secs = Some(0);
    let err = resolve_mcp(&file, &HashMap::new(), "/work").unwrap_err();
    assert!(err.to_string().contains("connect_timeout_secs"), "{err}");
}

#[test]
fn accepts_custom_positive_timeouts() {
    let mut file = File::default();
    file.mcp.call_timeout_secs = Some(5);
    file.mcp.connect_timeout_secs = Some(3);
    let runtime = resolve_mcp(&file, &HashMap::new(), "/work").expect("resolve");
    assert_eq!(runtime.call_timeout_secs, 5);
    assert_eq!(runtime.connect_timeout_secs, 3);
}

#[test]
fn rejects_unknown_field_in_mcp_table() {
    let err = crate::config::parse("[mcp]\nunknown = true\n").unwrap_err();
    assert!(err.to_string().contains("unknown"), "{err}");
}

#[test]
fn rejects_unknown_field_in_server_table() {
    let err = crate::config::parse(
        "[mcp.servers.s]\ntransport = \"stdio\"\ncommand = \"./run\"\nunknown = true\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("unknown"), "{err}");
}

#[test]
fn parses_toml_example_from_spec() {
    let file = crate::config::parse(
        r#"[mcp]
enabled = true
call_timeout_secs = 60
connect_timeout_secs = 20

[mcp.servers.github]
transport = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]
env = { GITHUB_TOKEN = "${GITHUB_TOKEN}" }
cwd = "."

[mcp.servers.docs]
transport = "http"
url = "https://mcp.example.com/mcp"
headers = { Authorization = "Bearer ${DOCS_MCP_TOKEN}" }

[mcp.servers.remote]
transport = "http"
url = "https://remote.example.com/mcp"
auth = "oauth"
oauth_client_id = "otto"
oauth_scopes = ["mcp:tools"]

[mcp.servers.legacy]
transport = "stdio"
command = "./bin/legacy-server"
enabled = false
"#,
    )
    .expect("parse");
    let env = env(&[
        ("GITHUB_TOKEN", "gh-secret"),
        ("DOCS_MCP_TOKEN", "docs-secret"),
    ]);
    let runtime = resolve_mcp(&file, &env, "/work").expect("resolve");
    assert_eq!(runtime.servers.len(), 4);
}
