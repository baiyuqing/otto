//! Parses the `otto serve: <url>` line the child process prints on its
//! stdout once its listener is bound, and waits for it on a background
//! reader thread's channel with a timeout.

use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// `Some(url)` when `line` is the announcement line; `None` for anything
/// else (rc-file banners, warnings, blank lines), which the caller skips.
pub fn parse_serve_line(line: &str) -> Option<&str> {
    line.strip_prefix("otto serve: ")
        .map(|rest| rest.trim_end())
        .filter(|url| !url.is_empty())
}

/// Recovers the `--listen 127.0.0.1:0` announcement's actual bound address
/// from the announced URL, for the app's own `POST /v1/workspaces` call.
pub fn parse_addr(url: &str) -> Option<SocketAddr> {
    let parsed = tauri::Url::parse(url).ok()?;
    let host = parsed.host_str()?;
    let port = parsed.port()?;
    format!("{host}:{port}").parse().ok()
}

/// The path the web UI's **Add workspace…** button navigates to (through
/// `window.__OTTO_DESKTOP__.openFolder()`) to ask the app to run
/// File > Open Folder…. The app cancels that navigation.
pub const OPEN_FOLDER_PATH: &str = "/__otto_desktop/open-folder";

/// Whether a main-window navigation to `url` is the web UI's Open Folder
/// request: [`OPEN_FOLDER_PATH`] on the same origin as `serve`, the URL
/// `otto serve` announced.
pub fn is_open_folder_request(url: &tauri::Url, serve: &tauri::Url) -> bool {
    url.origin() == serve.origin() && url.path() == OPEN_FOLDER_PATH
}

/// Why [`wait_for_serve_url`] gave up before seeing the announcement line.
#[derive(Debug, PartialEq, Eq)]
pub enum WaitError {
    /// No matching line arrived within the timeout.
    Timeout,
    /// The line sender was dropped (the reader thread ended: the child's
    /// stdout closed, usually because it exited).
    Disconnected,
}

/// Reads lines from `rx` until one is the `otto serve: ` announcement, or
/// `timeout` elapses, or the sender end is dropped. Lines that do not match
/// are skipped and do not reset the deadline.
pub fn wait_for_serve_url(rx: &Receiver<String>, timeout: Duration) -> Result<String, WaitError> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(WaitError::Timeout);
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                if let Some(url) = parse_serve_line(&line) {
                    return Ok(url.to_string());
                }
            }
            Err(RecvTimeoutError::Timeout) => return Err(WaitError::Timeout),
            Err(RecvTimeoutError::Disconnected) => return Err(WaitError::Disconnected),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;
    use std::thread;

    #[test]
    fn parses_the_announcement_line() {
        assert_eq!(
            parse_serve_line("otto serve: http://127.0.0.1:8787/?token=tok"),
            Some("http://127.0.0.1:8787/?token=tok")
        );
    }

    #[test]
    fn ignores_other_lines() {
        assert_eq!(parse_serve_line("warning: something"), None);
        assert_eq!(parse_serve_line(""), None);
        assert_eq!(parse_serve_line("otto serve: "), None);
    }

    #[test]
    fn wait_skips_other_lines_before_the_url() {
        let (tx, rx) = channel();
        tx.send("warning: rc file noise".to_string()).unwrap();
        tx.send("otto serve: http://127.0.0.1:9/?token=t".to_string())
            .unwrap();
        let got = wait_for_serve_url(&rx, Duration::from_secs(1)).unwrap();
        assert_eq!(got, "http://127.0.0.1:9/?token=t");
    }

    #[test]
    fn wait_times_out_when_no_line_arrives() {
        let (_tx, rx) = channel::<String>();
        let started = Instant::now();
        let got = wait_for_serve_url(&rx, Duration::from_millis(50));
        assert_eq!(got, Err(WaitError::Timeout));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn wait_reports_disconnect_when_the_reader_thread_ends() {
        let (tx, rx) = channel::<String>();
        thread::spawn(move || {
            drop(tx);
        });
        let got = wait_for_serve_url(&rx, Duration::from_secs(2));
        assert_eq!(got, Err(WaitError::Disconnected));
    }

    #[test]
    fn parses_the_bound_address_out_of_the_url() {
        assert_eq!(
            parse_addr("http://127.0.0.1:54321/?token=tok"),
            Some("127.0.0.1:54321".parse().unwrap())
        );
    }

    #[test]
    fn rejects_a_url_with_no_port() {
        assert_eq!(parse_addr("http://127.0.0.1/?token=tok"), None);
    }

    #[test]
    fn matches_the_open_folder_path_on_the_serve_origin_only() {
        let serve = tauri::Url::parse("http://127.0.0.1:54321/?token=tok").unwrap();
        let url = |s: &str| tauri::Url::parse(s).unwrap();
        assert!(is_open_folder_request(
            &url("http://127.0.0.1:54321/__otto_desktop/open-folder"),
            &serve
        ));
        assert!(!is_open_folder_request(
            &url("http://127.0.0.1:54321/"),
            &serve
        ));
        assert!(!is_open_folder_request(
            &url("http://127.0.0.1:9999/__otto_desktop/open-folder"),
            &serve
        ));
        assert!(!is_open_folder_request(
            &url("http://example.com:54321/__otto_desktop/open-folder"),
            &serve
        ));
    }
}
