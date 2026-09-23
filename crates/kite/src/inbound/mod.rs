//! Host-side inbound event sources that push the session inbox.

mod feishu;
mod forward;

pub use feishu::maybe_start;

use kite_core::agent::inbox::{Notification, NotificationKind};
use serde::Deserialize;

/// The EventKey `kite serve` consumes when Feishu inbound is enabled.
pub(crate) const FEISHU_MESSAGE_EVENT: &str = "im.message.receive_v1";

/// Flattened `im.message.receive_v1` fields as `lark-cli event consume`
/// writes them. Extra JSON keys are ignored.
#[derive(Debug, Deserialize)]
struct FeishuMessage {
    chat_id: Option<String>,
    sender_id: Option<String>,
    message_id: Option<String>,
    message_type: Option<String>,
    chat_type: Option<String>,
    content: Option<String>,
}

/// Turns one NDJSON line from `lark-cli event consume im.message.receive_v1`
/// into an inbox notification. Blank lines, invalid JSON, empty content,
/// interactive cards, and chats outside `chat_ids` are skipped. `chat_ids` is
/// an allowlist: an empty one admits nothing. `merge_forward` bodies stay
/// as-is unless the caller expands them first.
#[cfg(test)]
pub(crate) fn notification_from_ndjson(line: &str, chat_ids: &[String]) -> Option<Notification> {
    let event = parse_event(line, chat_ids)?;
    notification_from_event(&event, event.content.as_deref().unwrap_or(""))
}

fn parse_event(line: &str, chat_ids: &[String]) -> Option<FeishuMessage> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let event: FeishuMessage = serde_json::from_str(line).ok()?;
    if event.message_type.as_deref() == Some("interactive") {
        return None;
    }
    // A delivered message drives the agent turn loop, which runs tools in the
    // operator's workspace, so the chat allowlist is the authorization check:
    // no entry means no sender is authorized.
    let chat_id = event.chat_id.as_deref().unwrap_or("").trim();
    if !chat_ids.iter().any(|id| id == chat_id) {
        return None;
    }
    Some(event)
}

fn notification_from_event(event: &FeishuMessage, content: &str) -> Option<Notification> {
    let content = content.trim();
    if content.is_empty() {
        return None;
    }
    Some(Notification {
        task_id: String::new(),
        kind: Some(NotificationKind::Message),
        text: render_inbound(event, content),
        usage: None,
    })
}

async fn notification_from_line(
    line: &str,
    chat_ids: &[String],
    binary: &str,
    log: &crate::server::Logger,
) -> Option<Notification> {
    let event = parse_event(line, chat_ids)?;
    let mut content = event.content.clone().unwrap_or_default();
    if forward::should_expand(event.message_type.as_deref(), event.message_id.as_deref()) {
        let message_id = event.message_id.as_deref().unwrap_or_default();
        match forward::expand_merge_forward(binary, message_id).await {
            Ok(expanded) => {
                let expanded = forward::truncate_forwarded(expanded);
                log.info(
                    "feishu inbound: expanded merge_forward",
                    &[
                        ("message_id", message_id.to_string()),
                        ("chars", expanded.len().to_string()),
                    ],
                );
                content = expanded;
            }
            Err(error) => {
                log.error(
                    "feishu inbound: merge_forward expand failed",
                    &[("message_id", message_id.to_string()), ("error", error)],
                );
            }
        }
    }
    notification_from_event(&event, &content)
}

fn push_trimmed(header: &mut String, prefix: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        header.push_str(prefix);
        header.push_str(value);
    }
}

fn render_inbound(event: &FeishuMessage, content: &str) -> String {
    let mut header = String::from("[feishu]");
    push_trimmed(&mut header, " ", event.chat_type.as_deref());
    push_trimmed(&mut header, " ", event.chat_id.as_deref());
    push_trimmed(&mut header, " ", event.message_id.as_deref());
    push_trimmed(&mut header, " from ", event.sender_id.as_deref());
    format!("{header}\n{content}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allowed() -> Vec<String> {
        vec!["oc_1".to_string()]
    }

    fn line() -> &'static str {
        r#"{"chat_id":"oc_1","sender_id":"ou_1","message_id":"om_1","message_type":"text","chat_type":"group","content":"hello","extra":true}"#
    }

    #[test]
    fn a_text_event_becomes_a_message_notification() {
        let notification = notification_from_ndjson(line(), &allowed()).expect("parsed");
        assert_eq!(notification.kind, Some(NotificationKind::Message));
        assert!(notification.task_id.is_empty());
        assert_eq!(
            notification.text,
            "[feishu] group oc_1 om_1 from ou_1\nhello"
        );
        assert!(notification.usage.is_none());
    }

    #[test]
    fn interactive_empty_and_invalid_lines_are_skipped() {
        assert!(notification_from_ndjson("   ", &allowed()).is_none());
        assert!(notification_from_ndjson("{", &allowed()).is_none());
        assert!(
            notification_from_ndjson(
                r#"{"chat_id":"oc_1","message_type":"interactive","content":"card"}"#,
                &allowed()
            )
            .is_none()
        );
        assert!(
            notification_from_ndjson(r#"{"chat_id":"oc_1","content":"  "}"#, &allowed()).is_none()
        );
    }

    #[test]
    fn chat_ids_filter_when_non_empty() {
        assert!(notification_from_ndjson(line(), &["oc_other".into()]).is_none());
        let kept = notification_from_ndjson(line(), &["oc_1".into()]).expect("kept");
        assert!(kept.text.contains("oc_1"));
    }

    /// An inbound message reaches the agent turn loop, which runs tools in the
    /// operator's workspace. `chat_ids` is the only authorization the consumer
    /// has, so an empty list denies rather than admits.
    #[test]
    fn an_empty_chat_id_allowlist_accepts_nothing() {
        assert!(notification_from_ndjson(line(), &[]).is_none());
    }

    #[test]
    fn merge_forward_keeps_the_placeholder_without_expand() {
        let line = r#"{"chat_id":"oc_1","sender_id":"ou_1","message_id":"om_1","message_type":"merge_forward","chat_type":"p2p","content":"[Merged forward]"}"#;
        let notification = notification_from_ndjson(line, &allowed()).expect("parsed");
        assert_eq!(
            notification.text,
            "[feishu] p2p oc_1 om_1 from ou_1\n[Merged forward]"
        );
    }
}
