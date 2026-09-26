//! Exercises the real `otto` binary the way the app spawns it
//! ([`otto_desktop_lib::child::spawn_serve`]): starts `otto serve --listen
//! 127.0.0.1:0 --exit-on-stdin-close`, hits `/healthz` on the announced
//! address, and checks the process exits within 5 seconds both when its
//! stdin closes and when it receives `SIGTERM`.
//!
//! Needs local port binding, which the default sandbox for this repository's
//! agent tooling denies; run with that restriction lifted (or outside the
//! sandbox).

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use otto_desktop_lib::{child, serve_url};

fn otto_binary() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("binaries/otto-aarch64-apple-darwin");
    assert!(
        path.is_file(),
        "the otto sidecar binary is missing at {path:?}; \
         `make desktop-check` builds it before running this test"
    );
    path
}

/// A minimal environment: a throwaway `HOME` (isolated from the developer's
/// real config) with a `config.toml` naming a resolvable, offline provider
/// profile, so `otto serve` starts without a real provider account. The API
/// key is a fixture placeholder, never a real credential.
fn test_env(home: &Path) -> Vec<(String, String)> {
    let config_dir = home.join(".config").join("otto");
    std::fs::create_dir_all(&config_dir).expect("creating the config directory");
    std::fs::write(
        config_dir.join("config.toml"),
        "default_profile = \"test\"\n\n\
         [profiles.test]\n\
         provider = \"openai-compatible\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         model = \"test-model\"\n\
         api_key_env = \"OTTO_API_KEY\"\n",
    )
    .expect("writing config.toml");
    vec![
        ("HOME".to_string(), home.to_string_lossy().into_owned()),
        ("OTTO_API_KEY".to_string(), "sk-test-fixture".to_string()),
    ]
}

fn get(addr: SocketAddr, path: &str) -> std::io::Result<u16> {
    let mut stream = TcpStream::connect(addr)?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line)?;
    status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other("malformed HTTP status line"))
}

fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Trusts `workspace` and starts `otto serve` against it, returning the
/// running child and its announced address.
fn start_serve(
    otto_binary: &Path,
    workspace: &Path,
    home: &Path,
    log_path: &Path,
) -> (std::process::Child, SocketAddr) {
    let env = test_env(home);
    let trust = child::run_trust(otto_binary, workspace, &env).expect("running otto trust");
    assert!(trust.success, "otto trust failed: {}", trust.stderr);

    let mut process =
        child::spawn_serve(otto_binary, workspace, &env, log_path).expect("spawning otto serve");
    let stdout = process.stdout.take().expect("otto serve's stdout is piped");
    let lines = child::spawn_line_reader(stdout);
    let url = serve_url::wait_for_serve_url(&lines, Duration::from_secs(10))
        .expect("otto serve announced its URL");
    let addr = serve_url::parse_addr(&url).expect("the announced URL has a host and port");
    (process, addr)
}

#[test]
fn serve_answers_healthz_and_exits_on_stdin_close() {
    let otto_binary = otto_binary();
    let home = tempdir("home");
    let workspace = tempdir("workspace");
    let log_path = home.join("serve.log");

    let (mut process, addr) = start_serve(&otto_binary, &workspace, &home, &log_path);

    let status = get(addr, "/healthz").expect("GET /healthz");
    assert_eq!(status, 200);

    drop(process.stdin.take());
    let exit = wait_for_exit(&mut process, Duration::from_secs(5));
    assert!(
        exit.is_some(),
        "otto serve did not exit within 5s of stdin closing"
    );

    cleanup(&home);
    cleanup(&workspace);
}

#[test]
fn serve_exits_on_sigterm() {
    let otto_binary = otto_binary();
    let home = tempdir("home-sigterm");
    let workspace = tempdir("workspace-sigterm");
    let log_path = home.join("serve.log");

    let (mut process, _addr) = start_serve(&otto_binary, &workspace, &home, &log_path);

    let pid = process.id() as libc::pid_t;
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let exit = wait_for_exit(&mut process, Duration::from_secs(5));
    assert!(
        exit.is_some(),
        "otto serve did not exit within 5s of SIGTERM"
    );

    cleanup(&home);
    cleanup(&workspace);
}

fn tempdir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "otto-desktop-integration-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("creating a temp directory");
    dir
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}
