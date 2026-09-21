//! Spawns `lark-cli event consume` and delivers each message to open sessions.

use std::io::ErrorKind;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use otto_core::agent::inbox::Notification;
use otto_core::config::FeishuRuntime;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader};
use tokio::process::Child;
use tokio_util::sync::CancellationToken;

use super::{FEISHU_MESSAGE_EVENT, notification_from_line};
use crate::server::{Logger, Server};

const RECONNECT_DELAY: Duration = Duration::from_secs(2);
const SHUTDOWN_WAIT: Duration = Duration::from_secs(5);

/// Starts the Feishu consumer when `[inbound.feishu].enabled` is true and
/// `[inbound.feishu].chat_ids` lists at least one chat. Missing `lark-cli`
/// logs an error and disables inbound; it is not a serve failure.
pub fn maybe_start(
    server: Arc<Server>,
    runtime: FeishuRuntime,
    cancel: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    if !runtime.enabled {
        return None;
    }
    // `chat_ids` is the only authorization an inbound message passes before it
    // drives a turn that runs tools, so an empty list is a misconfiguration
    // rather than "every chat".
    if runtime.chat_ids.is_empty() {
        server.logger().error(
            "feishu inbound: [inbound.feishu].chat_ids is empty; inbound disabled",
            &[],
        );
        return None;
    }
    Some(tokio::spawn(async move {
        supervisor(server, runtime, cancel).await;
    }))
}

async fn supervisor(server: Arc<Server>, runtime: FeishuRuntime, cancel: CancellationToken) {
    let log = Arc::clone(server.logger());
    loop {
        if cancel.is_cancelled() {
            return;
        }
        match run_once(
            &runtime,
            |notification| {
                server.notify_open_sessions(notification);
            },
            &log,
            &cancel,
        )
        .await
        {
            RunEnd::Cancelled | RunEnd::Disabled => return,
            RunEnd::Exited => {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(RECONNECT_DELAY) => {}
                }
            }
        }
    }
}

enum RunEnd {
    Cancelled,
    Disabled,
    Exited,
}

async fn run_once(
    runtime: &FeishuRuntime,
    deliver: impl FnMut(Notification),
    log: &Arc<Logger>,
    cancel: &CancellationToken,
) -> RunEnd {
    let mut child = match spawn_lark_cli(&runtime.binary) {
        Ok(child) => child,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            log.error(
                "feishu inbound: lark-cli not found; inbound disabled",
                &[("binary", runtime.binary.clone())],
            );
            return RunEnd::Disabled;
        }
        Err(error) => {
            log.error(
                "feishu inbound: spawn failed",
                &[
                    ("binary", runtime.binary.clone()),
                    ("error", error.to_string()),
                ],
            );
            return RunEnd::Exited;
        }
    };
    let stderr = child.stderr.take();
    let Some(stdout) = child.stdout.take() else {
        log.error("feishu inbound: child stdout missing", &[]);
        shutdown_child(child).await;
        return RunEnd::Exited;
    };
    if let Some(stderr) = stderr {
        let log = Arc::clone(log);
        tokio::spawn(async move {
            drain_stderr(BufReader::new(stderr), log).await;
        });
    }

    consume_stdout(BufReader::new(stdout), runtime, deliver, log, cancel).await;
    shutdown_child(child).await;
    if cancel.is_cancelled() {
        RunEnd::Cancelled
    } else {
        RunEnd::Exited
    }
}

fn spawn_lark_cli(binary: &str) -> std::io::Result<Child> {
    tokio::process::Command::new(binary)
        .args(["event", "consume", FEISHU_MESSAGE_EVENT, "--as", "bot"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn()
}

/// Reads NDJSON until stdout EOF or cancel. Extra whitespace-only lines are
/// ignored; parse skips are silent.
async fn consume_stdout(
    mut stdout: impl AsyncBufRead + Unpin,
    runtime: &FeishuRuntime,
    mut deliver: impl FnMut(Notification),
    log: &Arc<Logger>,
    cancel: &CancellationToken,
) {
    let mut line = String::new();
    loop {
        line.clear();
        tokio::select! {
            () = cancel.cancelled() => return,
            result = stdout.read_line(&mut line) => {
                match result {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if let Some(notification) = notification_from_line(
                            &line,
                            &runtime.chat_ids,
                            &runtime.binary,
                            log,
                        )
                        .await
                        {
                            deliver(notification);
                        }
                    }
                }
            }
        }
    }
}

async fn drain_stderr(mut stderr: impl AsyncBufRead + Unpin, log: Arc<Logger>) {
    let mut line = String::new();
    loop {
        line.clear();
        match stderr.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                let trimmed = line.trim_end();
                if trimmed.contains("[event] ready event_key=") {
                    log.info(
                        "feishu inbound ready",
                        &[("event", FEISHU_MESSAGE_EVENT.to_string())],
                    );
                } else if trimmed.contains("\"ok\":false") || trimmed.contains("\"ok\": false") {
                    log.error(
                        "feishu inbound: lark-cli error",
                        &[("stderr", trimmed.to_string())],
                    );
                }
            }
        }
    }
}

async fn shutdown_child(mut child: Child) {
    drop(child.stdin.take());
    if let Some(pid) = child.id() {
        let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
    }
    if tokio::time::timeout(SHUTDOWN_WAIT, child.wait())
        .await
        .is_err()
    {
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;
    use std::time::Instant;

    fn runtime(binary: &str) -> FeishuRuntime {
        FeishuRuntime {
            enabled: true,
            binary: binary.to_string(),
            chat_ids: vec!["oc_1".to_string()],
        }
    }

    fn logger() -> Arc<Logger> {
        Arc::new(Logger::new(Box::new(std::io::sink())))
    }

    fn write_script(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("script");
        file.write_all(contents.as_bytes()).expect("write");
        file.flush().expect("flush");
        let mut permissions = file.as_file().metadata().expect("meta").permissions();
        permissions.set_mode(0o755);
        file.as_file().set_permissions(permissions).expect("chmod");
        file
    }

    #[tokio::test]
    async fn a_missing_binary_disables_inbound() {
        let end = run_once(
            &runtime("/no/such/lark-cli-otto-inbound"),
            |_| unreachable!("must not deliver"),
            &logger(),
            &CancellationToken::new(),
        )
        .await;
        assert!(matches!(end, RunEnd::Disabled));
    }

    #[tokio::test]
    async fn a_fake_lark_cli_delivers_then_stops_on_cancel() {
        let marker = tempfile::NamedTempFile::new().expect("marker");
        let marker_path = marker.path().to_string_lossy().into_owned();
        let script = write_script(&format!(
            r#"#!/bin/sh
printf '%s\n' '{{"chat_id":"oc_1","sender_id":"ou_1","message_type":"text","chat_type":"group","content":"hello"}}'
printf '%s\n' '[event] ready event_key=im.message.receive_v1' >&2
trap 'printf term > "{marker_path}"; exit 0' TERM
while :; do sleep 1; done
"#
        ));
        let collected = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&collected);
        let stop = CancellationToken::new();
        let cancel = stop.clone();
        let runtime = runtime(&script.path().to_string_lossy());
        let log = logger();
        let handle = tokio::spawn(async move {
            run_once(
                &runtime,
                |notification| sink.lock().expect("lock").push(notification),
                &log,
                &cancel,
            )
            .await
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !collected.lock().expect("lock").is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.cancel();
        let end = handle.await.expect("join");
        assert!(matches!(end, RunEnd::Cancelled));
        let items = collected.lock().expect("lock");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "[feishu] group oc_1 from ou_1\nhello");
        let marked = std::fs::read_to_string(marker.path()).unwrap_or_default();
        assert_eq!(marked.trim(), "term", "child must see SIGTERM, not SIGKILL");
    }

    #[tokio::test]
    async fn a_merge_forward_event_is_expanded_via_mget() {
        let marker = tempfile::NamedTempFile::new().expect("marker");
        let marker_path = marker.path().to_string_lossy().into_owned();
        let script = write_script(&format!(
            r#"#!/bin/sh
if [ "$1" = im ]; then
  printf '%s\n' '{{"ok":true,"data":{{"messages":[{{"message_id":"om_1","content":"<forwarded_messages>abc</forwarded_messages>"}}]}}}}'
  exit 0
fi
printf '%s\n' '{{"chat_id":"oc_1","sender_id":"ou_1","message_id":"om_1","message_type":"merge_forward","chat_type":"p2p","content":"[Merged forward]"}}'
printf '%s\n' '[event] ready event_key=im.message.receive_v1' >&2
trap 'printf term > "{marker_path}"; exit 0' TERM
while :; do sleep 1; done
"#
        ));
        let collected = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&collected);
        let stop = CancellationToken::new();
        let cancel = stop.clone();
        let runtime = runtime(&script.path().to_string_lossy());
        let log = logger();
        let handle = tokio::spawn(async move {
            run_once(
                &runtime,
                |notification| sink.lock().expect("lock").push(notification),
                &log,
                &cancel,
            )
            .await
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if !collected.lock().expect("lock").is_empty() {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stop.cancel();
        let end = handle.await.expect("join");
        assert!(matches!(end, RunEnd::Cancelled));
        let items = collected.lock().expect("lock");
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].text,
            "[feishu] p2p oc_1 om_1 from ou_1\n<forwarded_messages>abc</forwarded_messages>"
        );
    }
}
