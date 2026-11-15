use super::*;

/// Writes `[mcp.servers.<name>]` config at the default path under `home`
/// and returns `home`'s environment lookup for [`run`].
fn config_with_server(home: &Path, name: &str, body: &str) -> HashMap<String, String> {
    let directory = home.join(".config").join("otto");
    std::fs::create_dir_all(&directory).expect("config dir");
    std::fs::write(
        directory.join("config.toml"),
        format!("[mcp.servers.{name}]\n{body}\n"),
    )
    .expect("write config");
    HashMap::from([("HOME".to_string(), home.to_string_lossy().into_owned())])
}

/// Runs `otto mcp ...` and returns its exit code, stdout and stderr.
async fn mcp(args: &[&str], lookup: &HashMap<String, String>) -> (i32, String, String) {
    let arguments: Vec<String> = args.iter().map(|value| value.to_string()).collect();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let code = run(
        &arguments,
        &mut stdout,
        &mut stderr,
        lookup,
        &CancellationToken::new(),
    )
    .await;
    (
        code,
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    )
}

#[tokio::test]
async fn wrong_argument_counts_and_subcommands_report_usage() {
    let home = tempfile::tempdir().expect("home");
    let lookup = HashMap::from([(
        "HOME".to_string(),
        home.path().to_string_lossy().into_owned(),
    )]);
    for args in [
        vec![],
        vec!["login"],
        vec!["bogus", "docs"],
        vec!["login", "a", "b"],
    ] {
        let (code, _, stderr) = mcp(&args, &lookup).await;
        assert_ne!(code, 0, "{args:?}");
        assert!(stderr.contains(USAGE), "{args:?} -> {stderr}");
    }
}

#[tokio::test]
async fn logging_in_to_an_unknown_server_reports_it() {
    let home = tempfile::tempdir().expect("home");
    let lookup = config_with_server(
        home.path(),
        "docs",
        "transport = \"stdio\"\ncommand = \"true\"\n",
    );

    let (code, _, stderr) = mcp(&["login", "ghost"], &lookup).await;
    assert_ne!(code, 0);
    assert!(stderr.contains("unknown MCP server: ghost"), "{stderr}");
}

#[tokio::test]
async fn logging_in_to_a_stdio_server_is_rejected() {
    let home = tempfile::tempdir().expect("home");
    let lookup = config_with_server(
        home.path(),
        "shell",
        "transport = \"stdio\"\ncommand = \"true\"\n",
    );

    let (code, _, stderr) = mcp(&["login", "shell"], &lookup).await;
    assert_ne!(code, 0);
    assert!(
        stderr
            .contains("shell is a stdio server; MCP login only applies to HTTP servers with OAuth"),
        "{stderr}"
    );
}

#[tokio::test]
async fn logging_in_to_a_non_oauth_http_server_is_rejected() {
    let home = tempfile::tempdir().expect("home");
    let lookup = config_with_server(
        home.path(),
        "docs",
        "transport = \"http\"\nurl = \"https://mcp.example.com\"\n",
    );

    let (code, _, stderr) = mcp(&["login", "docs"], &lookup).await;
    assert_ne!(code, 0);
    assert!(
        stderr.contains("docs does not use OAuth; no login is required"),
        "{stderr}"
    );
}

#[tokio::test]
async fn logging_out_removes_the_stored_token() {
    let home = tempfile::tempdir().expect("home");
    let lookup = config_with_server(
        home.path(),
        "docs",
        "transport = \"http\"\nurl = \"https://mcp.example.com\"\nauth = \"oauth\"\n",
    );
    let token_path = otto::mcp::oauth::token_path(home.path(), "docs");
    std::fs::create_dir_all(token_path.parent().expect("parent")).expect("token dir");
    std::fs::write(&token_path, b"{}").expect("write token");

    let (code, stdout, stderr) = mcp(&["logout", "docs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("logged out of docs"), "{stdout}");
    assert!(!token_path.exists());
}

#[tokio::test]
async fn logging_out_without_a_stored_token_says_so() {
    let home = tempfile::tempdir().expect("home");
    let lookup = config_with_server(
        home.path(),
        "docs",
        "transport = \"http\"\nurl = \"https://mcp.example.com\"\nauth = \"oauth\"\n",
    );

    let (code, stdout, stderr) = mcp(&["logout", "docs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("no token stored for docs"), "{stdout}");
}

#[tokio::test]
async fn add_stdio_server_creates_config_without_literal_secret() {
    let home = tempfile::tempdir().expect("home");
    let lookup = HashMap::from([(
        "HOME".to_string(),
        home.path().to_string_lossy().into_owned(),
    )]);

    let (code, stdout, stderr) = mcp(
        &[
            "add",
            "github",
            "--transport",
            "stdio",
            "--command",
            "npx",
            "--arg",
            "-y",
            "--arg",
            "@modelcontextprotocol/server-github",
            "--env",
            "GITHUB_TOKEN=GITHUB_TOKEN",
        ],
        &lookup,
    )
    .await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("added MCP server github"), "{stdout}");
    assert!(stdout.contains("restart Otto"), "{stdout}");

    let path = home.path().join(".config/otto/config.toml");
    let text = std::fs::read_to_string(path).expect("config");
    assert!(text.contains("[mcp.servers.github]"), "{text}");
    assert!(text.contains("transport = \"stdio\""), "{text}");
    assert!(text.contains("command = \"npx\""), "{text}");
    assert!(
        text.contains("args = [\"-y\", \"@modelcontextprotocol/server-github\"]"),
        "{text}"
    );
    assert!(
        text.contains("GITHUB_TOKEN = \"${GITHUB_TOKEN}\""),
        "{text}"
    );
}

#[tokio::test]
async fn add_http_oauth_server_and_list_it() {
    let home = tempfile::tempdir().expect("home");
    let lookup = HashMap::from([(
        "HOME".to_string(),
        home.path().to_string_lossy().into_owned(),
    )]);

    let (code, stdout, stderr) = mcp(
        &[
            "add",
            "docs",
            "--transport",
            "http",
            "--url",
            "https://mcp.example.com/mcp",
            "--auth",
            "oauth",
            "--scope",
            "mcp:tools",
        ],
        &lookup,
    )
    .await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("run 'otto mcp login docs'"), "{stdout}");

    let (code, stdout, stderr) = mcp(&["list"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("docs\thttp\tenabled\toauth"), "{stdout}");

    let text =
        std::fs::read_to_string(home.path().join(".config/otto/config.toml")).expect("config");
    assert!(text.contains("auth = \"oauth\""), "{text}");
    assert!(text.contains("oauth_scopes = [\"mcp:tools\"]"), "{text}");
}

#[test]
fn writing_refuses_a_config_that_changed_since_it_was_read() {
    let home = tempfile::tempdir().expect("home");
    let lookup = config_with_server(
        home.path(),
        "docs",
        "transport = \"http\"\nurl = \"https://mcp.example.com\"\n",
    );
    let (path, _, original) = load_editable_config(&lookup).expect("load");

    let concurrent = "default_model = \"p/written by another otto\"\n";
    std::fs::write(&path, concurrent).expect("concurrent write");

    let error = write_edited_config(&path, &original, |text| {
        otto_core::config::edit::remove_table(text, &["mcp", "servers", "docs"])
    })
    .expect_err("stale write");

    assert!(error.contains("changed on disk"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).expect("read"), concurrent);
}

#[tokio::test]
async fn remove_enable_and_disable_update_existing_server() {
    let home = tempfile::tempdir().expect("home");
    let lookup = config_with_server(
        home.path(),
        "docs",
        "transport = \"http\"\nurl = \"https://mcp.example.com\"\n",
    );

    let (code, stdout, stderr) = mcp(&["disable", "docs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("disabled MCP server docs"), "{stdout}");
    let text =
        std::fs::read_to_string(home.path().join(".config/otto/config.toml")).expect("config");
    assert!(text.contains("enabled = false"), "{text}");

    let (code, stdout, stderr) = mcp(&["enable", "docs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("enabled MCP server docs"), "{stdout}");
    let text =
        std::fs::read_to_string(home.path().join(".config/otto/config.toml")).expect("config");
    assert!(text.contains("enabled = true"), "{text}");

    let (code, stdout, stderr) = mcp(&["remove", "docs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert!(stdout.contains("removed MCP server docs"), "{stdout}");
    let text =
        std::fs::read_to_string(home.path().join(".config/otto/config.toml")).expect("config");
    assert!(!text.contains("mcp.servers.docs"), "{text}");
}

#[tokio::test]
async fn edits_keep_every_other_byte_of_the_file() {
    let home = tempfile::tempdir().expect("home");
    let directory = home.path().join(".config").join("otto");
    std::fs::create_dir_all(&directory).expect("config dir");
    let path = directory.join("config.toml");
    let original = "# my config\ndefault_model = \"p/a\"\n\n# \
        servers\n[mcp.servers.docs]\ntransport = \"http\" # remote\nurl = \
        \"https://mcp.example.com\"\n\n# providers\n[providers.a]\ntype = \
        \"chatgpt\"\n";
    std::fs::write(&path, original).expect("write config");
    let lookup = HashMap::from([(
        "HOME".to_string(),
        home.path().to_string_lossy().into_owned(),
    )]);
    let read = || std::fs::read_to_string(&path).expect("config");

    let (code, _, stderr) = mcp(&["disable", "docs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    let disabled = original.replace(
        "url = \"https://mcp.example.com\"\n",
        "url = \"https://mcp.example.com\"\nenabled = false\n",
    );
    assert_eq!(read(), disabled);

    let (code, _, stderr) = mcp(&["enable", "docs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    let enabled = disabled.replace("enabled = false", "enabled = true");
    assert_eq!(read(), enabled);

    let (code, _, stderr) = mcp(
        &["add", "fs", "--transport", "stdio", "--command", "true"],
        &lookup,
    )
    .await;
    assert_eq!(code, 0, "stderr = {stderr}");
    let added = read();
    assert!(added.starts_with(&enabled), "{added}");
    assert!(
        otto_core::config::parse(&added)
            .expect("parse")
            .mcp
            .servers
            .contains_key("fs"),
        "{added}"
    );

    let (code, _, stderr) = mcp(&["remove", "fs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert_eq!(read(), enabled);

    let (code, _, stderr) = mcp(&["remove", "docs"], &lookup).await;
    assert_eq!(code, 0, "stderr = {stderr}");
    assert_eq!(
        read(),
        "# my config\ndefault_model = \"p/a\"\n\n# servers\n\n# providers\n[providers.a]\ntype = \"chatgpt\"\n"
    );
}

#[tokio::test]
async fn add_rejects_existing_server_and_bad_names() {
    let home = tempfile::tempdir().expect("home");
    let lookup = config_with_server(
        home.path(),
        "docs",
        "transport = \"http\"\nurl = \"https://mcp.example.com\"\n",
    );

    let (code, _, stderr) = mcp(
        &[
            "add",
            "docs",
            "--transport",
            "http",
            "--url",
            "https://other.example.com/mcp",
        ],
        &lookup,
    )
    .await;
    assert_ne!(code, 0);
    assert!(
        stderr.contains("MCP server already exists: docs"),
        "{stderr}"
    );

    let (code, _, stderr) = mcp(
        &[
            "add",
            "bad.name",
            "--transport",
            "stdio",
            "--command",
            "true",
        ],
        &lookup,
    )
    .await;
    assert_ne!(code, 0);
    assert!(stderr.contains("invalid MCP server name"), "{stderr}");
}
