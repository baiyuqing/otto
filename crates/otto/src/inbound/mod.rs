//! Host-side inbound event sources that push the session inbox.

mod feishu;

pub use feishu::maybe_start;

use otto_core::agent::inbox::{Notification, NotificationKind};
use serde::Deserialize;

/// The EventKey `otto serve` consumes when Feishu inbound is enabled.
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
/// interactive cards, and chats outside `chat_ids` (when that filter is
/// non-empty) are skipped.
pub(crate) fn notification_from_ndjson(line: &str, chat_ids: &[String]) -> Option<Notification> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let event: FeishuMessage = serde_json::from_str(line).ok()?;
    if event.message_type.as_deref() == Some("interactive") {
        return None;
    }
    let content = event
        .content
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    if !chat_ids.is_empty() {
        let chat_id = event.chat_id.as_deref().unwrap_or("").trim();
        if !chat_ids.iter().any(|id| id == chat_id) {
            return None;
        }
    }
    Some(Notification {
        task_id: String::new(),
        kind: Some(NotificationKind::Message),
        text: render_inbound(&event, content),
        usage: None,
    })
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

    fn line() -> &'static str {
        r#"{"chat_id":"oc_1","sender_id":"ou_1","message_id":"om_1","message_type":"text","chat_type":"group","content":"hello","extra":true}"#
    }

    #[test]
    fn a_text_event_becomes_a_message_notification() {
        let notification = notification_from_ndjson(line(), &[]).expect("parsed");
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
        assert!(notification_from_ndjson("   ", &[]).is_none());
        assert!(notification_from_ndjson("{", &[]).is_none());
        assert!(
            notification_from_ndjson(r#"{"message_type":"interactive","content":"card"}"#, &[])
                .is_none()
        );
        assert!(notification_from_ndjson(r#"{"content":"  "}"#, &[]).is_none());
    }

    #[test]
    fn chat_ids_filter_when_non_empty() {
        assert!(notification_from_ndjson(line(), &["oc_other".into()]).is_none());
        let kept = notification_from_ndjson(line(), &["oc_1".into()]).expect("kept");
        assert!(kept.text.contains("oc_1"));
    }
}
