//! Expand Feishu `merge_forward` events via `lark-cli im +messages-mget`.
//!
//! Failures keep the original event content. Logs never include the body.

use std::process::Stdio;
use std::time::Duration;

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;
use serde::Deserialize;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub(super) const MGET_TIMEOUT: Duration = Duration::from_secs(8);
pub(super) const FORWARD_MAX_CHARS: usize = 30_000;

#[derive(Debug, Deserialize)]
struct MgetResponse {
    ok: Option<bool>,
    data: Option<MgetData>,
}

#[derive(Debug, Deserialize)]
struct MgetData {
    messages: Option<Vec<MgetMessage>>,
}

#[derive(Debug, Deserialize)]
struct MgetMessage {
    message_id: Option<String>,
    content: Option<String>,
}

pub(super) fn should_expand(message_type: Option<&str>, message_id: Option<&str>) -> bool {
    message_type == Some("merge_forward") && message_id.is_some_and(is_message_id)
}

pub(super) fn is_message_id(id: &str) -> bool {
    id.strip_prefix("om_")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric()))
}

pub(super) fn content_from_mget_json(json: &str, message_id: &str) -> Option<String> {
    let response: MgetResponse = serde_json::from_str(json).ok()?;
    if response.ok != Some(true) {
        return None;
    }
    let messages = response
        .data
        .and_then(|data| data.messages)
        .unwrap_or_default();
    let message = messages
        .iter()
        .find(|item| item.message_id.as_deref() == Some(message_id))
        .or(messages.first())?;
    let content = message
        .content
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(content.to_string())
}

pub(super) fn collapse_nested_forwards(content: &str) -> String {
    const OPEN: &str = "<forwarded_messages>";
    const CLOSE: &str = "</forwarded_messages>";
    const PLACEHOLDER: &str = "[Merged forward]";
    let mut out = String::with_capacity(content.len());
    let mut i = 0;
    let mut depth = 0usize;
    while i < content.len() {
        if content[i..].starts_with(OPEN) {
            if depth == 0 {
                out.push_str(OPEN);
                depth = 1;
                i += OPEN.len();
                continue;
            }
            out.push_str(PLACEHOLDER);
            let mut nested = 1usize;
            i += OPEN.len();
            while i < content.len() && nested > 0 {
                if content[i..].starts_with(OPEN) {
                    nested += 1;
                    i += OPEN.len();
                } else if content[i..].starts_with(CLOSE) {
                    nested -= 1;
                    i += CLOSE.len();
                } else {
                    i += content[i..].chars().next().map(char::len_utf8).unwrap_or(1);
                }
            }
            continue;
        }
        if content[i..].starts_with(CLOSE) {
            if depth == 1 {
                out.push_str(CLOSE);
            }
            depth = depth.saturating_sub(1);
            i += CLOSE.len();
            continue;
        }
        let Some(ch) = content[i..].chars().next() else {
            break;
        };
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

pub(super) fn truncate_forwarded(content: String) -> String {
    if content.len() <= FORWARD_MAX_CHARS {
        return content;
    }
    let mut cut = FORWARD_MAX_CHARS;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n[Truncated: forwarded message content exceeded {} chars]",
        &content[..cut],
        FORWARD_MAX_CHARS
    )
}

pub(super) async fn expand_merge_forward(binary: &str, message_id: &str) -> Result<String, String> {
    expand_merge_forward_with_timeout(binary, message_id, MGET_TIMEOUT).await
}

pub(super) async fn expand_merge_forward_with_timeout(
    binary: &str,
    message_id: &str,
    timeout: Duration,
) -> Result<String, String> {
    if !is_message_id(message_id) {
        return Err("invalid message id".into());
    }
    let mut child = Command::new(binary)
        .args([
            "im",
            "+messages-mget",
            "--message-ids",
            message_id,
            "--as",
            "bot",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(false)
        .spawn()
        .map_err(|error| error.to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "child stdout missing".to_string())?;
    let mut stderr = child.stderr.take();
    let pid = child.id();
    let read = async {
        let mut out = Vec::new();
        stdout
            .read_to_end(&mut out)
            .await
            .map_err(|error| error.to_string())?;
        if let Some(mut err_pipe) = stderr.take() {
            let mut err = Vec::new();
            let _ = err_pipe.read_to_end(&mut err).await;
        }
        Ok::<Vec<u8>, String>(out)
    };
    let stdout = match tokio::time::timeout(timeout, read).await {
        Ok(result) => result?,
        Err(_) => {
            if let Some(pid) = pid {
                let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
            }
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return Err(format!("timeout after {}ms", timeout.as_millis()));
        }
    };
    let status = child.wait().await.map_err(|error| error.to_string())?;
    if !status.success() {
        return Err(format!("lark-cli mget exited {status}"));
    }
    let stdout = String::from_utf8(stdout).map_err(|error| error.to_string())?;
    let content = content_from_mget_json(&stdout, message_id)
        .ok_or_else(|| "mget returned no content".to_string())?;
    Ok(collapse_nested_forwards(&content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    fn write_script(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("script");
        file.write_all(contents.as_bytes()).expect("write");
        file.flush().expect("flush");
        let mut permissions = file.as_file().metadata().expect("meta").permissions();
        permissions.set_mode(0o755);
        file.as_file().set_permissions(permissions).expect("chmod");
        file
    }

    #[test]
    fn only_merge_forward_with_an_om_id_expands() {
        assert!(!should_expand(Some("text"), Some("om_x100")));
        assert!(!should_expand(Some("merge_forward"), None));
        assert!(!should_expand(Some("merge_forward"), Some("bad id")));
        assert!(should_expand(Some("merge_forward"), Some("om_x100")));
    }

    #[test]
    fn mget_json_prefers_the_matching_message() {
        let json = r#"{
            "ok": true,
            "data": {
                "messages": [
                    {"message_id": "om_other", "content": "nope"},
                    {"message_id": "om_x100", "content": "<forwarded_messages>abc</forwarded_messages>"}
                ]
            }
        }"#;
        assert_eq!(
            content_from_mget_json(json, "om_x100").as_deref(),
            Some("<forwarded_messages>abc</forwarded_messages>")
        );
    }

    #[test]
    fn mget_json_falls_back_to_the_first_message() {
        let json =
            r#"{"ok":true,"data":{"messages":[{"message_id":"om_other","content":"first"}]}}"#;
        assert_eq!(
            content_from_mget_json(json, "om_x100").as_deref(),
            Some("first")
        );
    }

    #[test]
    fn mget_failure_and_empty_content_are_none() {
        assert!(content_from_mget_json("{", "om_x100").is_none());
        assert!(
            content_from_mget_json(
                r#"{"ok":false,"data":{"messages":[{"content":"x"}]}}"#,
                "om_x100"
            )
            .is_none()
        );
        assert!(
            content_from_mget_json(
                r#"{"ok":true,"data":{"messages":[{"message_id":"om_x100","content":"  "}]}}"#,
                "om_x100"
            )
            .is_none()
        );
    }

    #[test]
    fn nested_forwarded_blocks_are_not_expanded() {
        let content = concat!(
            "<forwarded_messages>\n",
            "outer\n",
            "<forwarded_messages>inner <forwarded_messages>deep</forwarded_messages></forwarded_messages>\n",
            "tail\n",
            "</forwarded_messages>",
        );
        assert_eq!(
            collapse_nested_forwards(content),
            concat!(
                "<forwarded_messages>\n",
                "outer\n",
                "[Merged forward]\n",
                "tail\n",
                "</forwarded_messages>",
            )
        );
    }

    #[test]
    fn a_single_forwarded_block_is_unchanged() {
        let content = "<forwarded_messages>abc</forwarded_messages>";
        assert_eq!(collapse_nested_forwards(content), content);
    }

    #[test]
    fn forwarded_content_is_truncated_at_the_limit() {
        let content = "a".repeat(FORWARD_MAX_CHARS + 5);
        let truncated = truncate_forwarded(content);
        assert!(truncated.starts_with(&"a".repeat(FORWARD_MAX_CHARS)));
        assert!(truncated.contains("[Truncated: forwarded message content exceeded 30000 chars]"));
        assert!(truncated.len() > FORWARD_MAX_CHARS);
    }

    #[tokio::test]
    async fn a_successful_mget_returns_the_expanded_body() {
        let script = write_script(
            r#"#!/bin/sh
test "$1" = im -a "$2" = +messages-mget || exit 2
printf '%s\n' '{"ok":true,"data":{"messages":[{"message_id":"om_x100","content":"<forwarded_messages>abc</forwarded_messages>"}]}}'
"#,
        );
        let body = expand_merge_forward(script.path().to_str().expect("path"), "om_x100")
            .await
            .expect("expand");
        assert_eq!(body, "<forwarded_messages>abc</forwarded_messages>");
    }

    #[tokio::test]
    async fn a_failing_mget_is_an_error() {
        let script = write_script("#!/bin/sh\nexit 1\n");
        assert!(
            expand_merge_forward(script.path().to_str().expect("path"), "om_x100")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn an_invalid_id_does_not_spawn() {
        let script = write_script("#!/bin/sh\nexit 0\n");
        let error = expand_merge_forward(script.path().to_str().expect("path"), "not an id")
            .await
            .expect_err("invalid");
        assert!(error.contains("invalid message id"), "{error}");
    }

    #[tokio::test]
    async fn a_slow_mget_times_out() {
        let script = write_script("#!/bin/sh\nsleep 5\n");
        let error = expand_merge_forward_with_timeout(
            script.path().to_str().expect("path"),
            "om_x100",
            Duration::from_millis(50),
        )
        .await
        .expect_err("timeout");
        assert!(error.contains("timeout"), "{error}");
    }
}
