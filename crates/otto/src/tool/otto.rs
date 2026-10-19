//! Installed-version help and session-scoped approval controls.
use super::bash::BashApprovals;
use super::{Tool, definition, error_result, text_result};
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::{json, value::RawValue};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

const MANUALS: &[&str] = &[
    include_str!("../../../../docs/manual/getting-started.md"),
    include_str!("../../../../docs/manual/interactive-use.md"),
    include_str!("../../../../docs/manual/safety-and-tools.md"),
    include_str!("../../../../docs/manual/servers-and-connectors.md"),
    include_str!("../../../../docs/manual/automation.md"),
    include_str!("../../../../docs/manual/memory-and-extensions.md"),
    include_str!("../../../../docs/manual/troubleshooting.md"),
];

struct ManualTopic<'a> {
    title: &'a str,
    content: &'a str,
}

/// Uses the canonical manual embedded in the executable, so help works outside
/// the source checkout and follows the installed version.
pub struct Help {
    pub definitions: Vec<ToolDefinition>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HelpArgs {
    #[serde(default)]
    topic: String,
}

pub(crate) const OTTO_HELP_RETRY: (&str, otto_core::model::RetrySafety) =
    ("otto_help", otto_core::model::RetrySafety::ReadOnly);

#[async_trait::async_trait]
impl Tool for Help {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        OTTO_HELP_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            "otto_help",
            "Read Otto's installed-version user manual or current session capabilities. Consult this before answering questions about Otto commands, APIs, configuration, or limitations. Empty topic lists section titles; capabilities lists current tools; a section title or uniquely matching title phrase returns that section; an unknown or ambiguous title fails with related section suggestions when available.",
            json!({"type":"object","additionalProperties":false,"properties":{"topic":{"type":"string","description":"Section title or uniquely matching title phrase, capabilities, or empty to list section titles."}},"required":[]}),
        )
    }
    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(super::CONTEXT_CANCELED);
        }
        let args: HelpArgs = match serde_json::from_str(arguments.get()) {
            Ok(args) => args,
            Err(e) => return error_result(e),
        };
        if args.topic == "capabilities" {
            let content = serde_json::to_string(
                &self
                    .definitions
                    .iter()
                    .map(|tool| json!({"name":tool.name,"description":tool.description}))
                    .collect::<Vec<_>>(),
            )
            .expect("tool definitions serialize");
            return super::result::capped_text_result(
                &format!("Otto {} capabilities:\n{content}", crate::VERSION),
                48_000,
            );
        }
        let topics = manual_topics(MANUALS);
        if args.topic.trim().is_empty() {
            return super::result::capped_text_result(
                &format!(
                    "Otto {} user manual\n\n{}",
                    crate::VERSION,
                    topics
                        .iter()
                        .map(|topic| topic.title)
                        .collect::<Vec<_>>()
                        .join("\n")
                ),
                48_000,
            );
        }
        let topic = args.topic.trim().to_lowercase();
        let exact = topics
            .iter()
            .find(|candidate| topic_title(candidate.title) == topic);
        let normalized = normalized_topic_match(&args.topic, &topics);
        let Some(topic) = exact.or(normalized) else {
            let suggestions = related_topic_titles(&args.topic, &topics);
            let suffix = if suggestions.is_empty() {
                String::new()
            } else {
                format!(" Related section titles: {}.", suggestions.join(", "))
            };
            return error_result(format!(
                "Unknown or ambiguous help topic.{suffix} Call otto_help with an empty topic for all section titles."
            ));
        };
        super::result::capped_text_result(
            &format!("Otto {} user manual\n\n{}", crate::VERSION, topic.content),
            48_000,
        )
    }
}

fn topic_title(title: &str) -> String {
    title.trim_start_matches('#').trim().to_lowercase()
}

fn topic_words(value: &str) -> Vec<String> {
    value
        .split(|char: char| !char.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .filter(|word| {
            !matches!(
                word.as_str(),
                "command" | "commands" | "help" | "topic" | "topics"
            )
        })
        .collect()
}

fn normalized_topic_match<'a>(
    query: &str,
    topics: &'a [ManualTopic<'a>],
) -> Option<&'a ManualTopic<'a>> {
    let query_words = topic_words(query);
    if query_words.is_empty() {
        return None;
    }
    let mut matches = topics
        .iter()
        .filter(|topic| topic_words(&topic_title(topic.title)) == query_words);
    let matched = matches.next()?;
    matches.next().is_none().then_some(matched)
}

fn related_topic_titles<'a>(query: &str, topics: &'a [ManualTopic<'a>]) -> Vec<&'a str> {
    let query_words = topic_words(query);
    if query_words.is_empty() {
        return Vec::new();
    }
    let mut matches = topics
        .iter()
        .filter_map(|topic| {
            let title = topic_title(topic.title);
            let title_words = topic_words(&title);
            let shared = query_words
                .iter()
                .filter(|word| title_words.contains(word))
                .count();
            (shared > 0).then_some((
                topic.title.trim_start_matches('#').trim(),
                shared,
                title_words.len().saturating_sub(shared),
            ))
        })
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        right
            .1
            .cmp(&left.1)
            .then_with(|| left.2.cmp(&right.2))
            .then_with(|| left.0.cmp(right.0))
    });
    matches
        .into_iter()
        .take(5)
        .map(|(title, _, _)| title)
        .collect()
}

fn manual_headings(manual: &str) -> Vec<(usize, &str)> {
    let mut headings = Vec::new();
    let mut offset = 0;
    let mut fence = None;
    for line in manual.split_inclusive('\n') {
        let trimmed = line.trim_start();
        let marker = trimmed
            .chars()
            .next()
            .filter(|char| *char == '`' || *char == '~');
        if let Some(marker) =
            marker.filter(|_| trimmed.starts_with("```") || trimmed.starts_with("~~~"))
        {
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
        } else if fence.is_none() && trimmed.starts_with('#') {
            headings.push((offset, line.trim_end_matches(['\r', '\n'])));
        }
        offset += line.len();
    }
    headings
}

fn manual_topics<'a>(manuals: &'a [&'a str]) -> Vec<ManualTopic<'a>> {
    let mut topics = Vec::new();
    for manual in manuals {
        let headings = manual_headings(manual);
        for (index, (start, title)) in headings.iter().enumerate() {
            let depth = title.bytes().take_while(|byte| *byte == b'#').count();
            if depth == 1 {
                continue;
            }
            let end = headings[index + 1..]
                .iter()
                .find(|(_, heading)| {
                    heading.bytes().take_while(|byte| *byte == b'#').count() <= depth
                })
                .map_or(manual.len(), |(offset, _)| *offset);
            topics.push(ManualTopic {
                title,
                content: &manual[*start..end],
            });
        }
    }
    topics
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

pub struct Approvals {
    pub session: String,
    pub approvals: Arc<BashApprovals>,
    pub revoke: bool,
    pub expected_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeArgs {
    id: String,
}

pub fn approval_definition(revoke: bool) -> ToolDefinition {
    if revoke {
        definition(
            "approval_revoke",
            "Withdraw an unapproved Bash or persistent read-access request in this session by exact ID. Use when the user asks to cancel a pending approval or when your request is no longer needed. Cannot approve, undo execution, or remove permanent grants.",
            json!({"type":"object","additionalProperties":false,"properties":{"id":{"type":"string"}},"required":["id"]}),
        )
    } else {
        definition(
            "approval_pending",
            "Query the current session's unapproved Bash or persistent read-access request. Returns null if none is pending.",
            json!({"type":"object","additionalProperties":false,"properties":{},"required":[]}),
        )
    }
}

#[async_trait::async_trait]
impl Tool for Approvals {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        if self.revoke {
            super::approval::APPROVAL_REVOKE_RETRY.1
        } else {
            super::approval::APPROVAL_PENDING_RETRY.1
        }
    }

    fn definition(&self) -> ToolDefinition {
        approval_definition(self.revoke)
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(super::CONTEXT_CANCELED);
        }
        if self.revoke {
            let args: RevokeArgs = match serde_json::from_str(arguments.get()) {
                Ok(args) => args,
                Err(e) => return error_result(e),
            };
            if self.expected_id.as_ref().is_some_and(|id| id != &args.id) {
                return error_result("request is outside this approval dialogue");
            }
            let _decision = tokio::select! {
                guard = self.approvals.decision.lock() => guard,
                () = cancel.cancelled() => return error_result(super::CONTEXT_CANCELED),
            };
            if cancel.is_cancelled() {
                return error_result(super::CONTEXT_CANCELED);
            }
            match self.approvals.revoke(&self.session, &args.id) {
                Ok(()) => text_result(format!(
                    "Approval {} withdrawn. Its command was not run.",
                    args.id
                )),
                Err(e) => error_result(e),
            }
        } else {
            if serde_json::from_str::<EmptyArgs>(arguments.get()).is_err() {
                return error_result("approval_pending takes no arguments");
            }
            text_result(
                self.approvals
                    .pending(&self.session)
                    .filter(|(id, _, _)| {
                        self.expected_id
                            .as_ref()
                            .is_none_or(|expected| expected == id)
                    })
                    .map_or_else(
                        || "null".to_string(),
                        |(id, command, read_path)| {
                            json!({"id":id,"command":command,"read_path":read_path}).to_string()
                        },
                    ),
            )
        }
    }
}

pub fn controls(session: &str, approvals: &Arc<BashApprovals>) -> Vec<Box<dyn Tool + Send + Sync>> {
    [false, true]
        .into_iter()
        .map(|revoke| {
            Box::new(Approvals {
                session: session.to_owned(),
                approvals: Arc::clone(approvals),
                revoke,
                expected_id: None,
            }) as Box<dyn Tool + Send + Sync>
        })
        .collect()
}

/// Explicitly marks a non-approval task for the frontend's existing queue.
pub struct Queue(pub Arc<std::sync::atomic::AtomicBool>);
pub(crate) const APPROVAL_QUEUE_RETRY: (&str, otto_core::model::RetrySafety) = (
    "approval_queue",
    otto_core::model::RetrySafety::NonIdempotent,
);

#[async_trait::async_trait]
impl Tool for Queue {
    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        APPROVAL_QUEUE_RETRY.1
    }

    fn definition(&self) -> ToolDefinition {
        definition(
            "approval_queue",
            "Mark the user's message as an ordinary task to queue until the pending approval is resolved. Use for requests unrelated to explaining, querying, or withdrawing the approval. Does not resolve the approval.",
            json!({"type":"object","additionalProperties":false,"properties":{},"required":[]}),
        )
    }
    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(super::CONTEXT_CANCELED);
        }
        if serde_json::from_str::<EmptyArgs>(arguments.get()).is_err() {
            return error_result("approval_queue takes no arguments");
        }
        self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        text_result("This message will be queued behind the pending approval.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::run;

    #[tokio::test]
    async fn help_uses_embedded_manual_and_actual_capabilities() {
        let help = Help {
            definitions: vec![definition("fixture_tool", "Only enabled here", json!({}))],
        };
        let index = run(&help, "{}").await;
        assert!(index.content.contains(crate::VERSION));
        assert!(index.content.contains("Approvals inside a turn"));
        let section = run(&help, r#"{"topic":"Approvals inside a turn"}"#).await;
        assert!(section.content.contains(crate::VERSION));
        assert!(section.content.contains("decision"));
        assert!(section.content.contains("deny"));
        assert!(!section.content.contains("### Web UI"));
        let capabilities = run(&help, r#"{"topic":"capabilities"}"#).await;
        assert!(capabilities.content.contains(crate::VERSION));
        assert!(capabilities.content.contains("fixture_tool"));
        assert!(!capabilities.content.contains("bash"));

        let normalized = run(&help, r#"{"topic":"Memory commands"}"#).await;
        assert!(!normalized.is_error);
        assert!(normalized.content.contains("## Memory"));
        assert!(normalized.content.contains("enabled by default"));

        let ambiguous = run(&help, r#"{"topic":"memory service review"}"#).await;
        assert!(ambiguous.is_error);
        assert!(ambiguous.content.contains("Related section titles:"));
        assert!(ambiguous.content.contains("Rust memory service extensions"));
        assert!(ambiguous.content.contains("Memory review"));

        let unrelated = run(&help, r#"{"topic":"invented"}"#).await;
        assert!(unrelated.is_error);
        assert!(!unrelated.content.contains("Related section titles"));
        assert!(
            unrelated
                .content
                .contains("empty topic for all section titles")
        );
    }

    #[test]
    fn manual_heading_parser_ignores_fenced_code() {
        let headings = manual_headings(
            "# Good\n```md\n## Fake\n~~~\n### Also fake\n```\n~~~\n### Also fake\n~~~\n## Real\n",
        );
        assert_eq!(
            headings.iter().map(|(_, title)| *title).collect::<Vec<_>>(),
            ["# Good", "## Real"]
        );
    }

    #[test]
    fn manual_topics_ignore_page_titles_and_keep_page_boundaries() {
        let topics = manual_topics(&[
            "# First page\npage navigation\n\n## First\nfirst body\n",
            "# Second page\npage navigation\n\n## Second\nsecond body\n",
        ]);
        assert_eq!(topics.len(), 2);
        assert_eq!(topics[0].content, "## First\nfirst body\n");
        assert_eq!(topics[1].content, "## Second\nsecond body\n");
    }

    #[test]
    fn manual_titles_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for topic in manual_topics(MANUALS) {
            let title = topic.title.trim_start_matches('#').trim().to_lowercase();
            assert!(seen.insert(title.clone()), "duplicate manual title {title}");
        }
    }

    #[tokio::test]
    async fn revoke_is_session_and_id_scoped_and_wakes_the_waiter() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let (controller, approvals, reloads) =
            crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
        let sid = controller.info().session_id;
        let old = approvals.request(&sid, "old command");
        let current = approvals.request_read_path(&sid, "cat fixture", "/fixture");
        let tool = Approvals {
            session: sid.clone(),
            approvals: Arc::clone(&approvals),
            revoke: true,
            expected_id: None,
        };
        assert!(run(&tool, &json!({"id":old}).to_string()).await.is_error);
        let foreign = approvals.request("another-session", "foreign command");
        assert!(
            run(&tool, &json!({"id":foreign}).to_string())
                .await
                .is_error
        );
        let bound = Approvals {
            session: sid.clone(),
            approvals: Arc::clone(&approvals),
            revoke: true,
            expected_id: Some(old.clone()),
        };
        assert!(
            run(&bound, &json!({"id":current}).to_string())
                .await
                .is_error
        );
        assert_eq!(
            run(
                &Approvals {
                    session: sid.clone(),
                    approvals: Arc::clone(&approvals),
                    revoke: false,
                    expected_id: Some(old.clone()),
                },
                "{}",
            )
            .await
            .content,
            "null",
            "a dialogue tied to a replaced request cannot inspect its replacement"
        );
        let waiter = approvals.withdrawn(&sid, &current);
        let arguments = json!({"id":current}).to_string();
        let (_, result) = tokio::join!(waiter, run(&tool, &arguments));
        assert!(!result.is_error);
        assert_eq!(approvals.pending_count(&sid), 0);
        assert_eq!(approvals.pending_count("another-session"), 1);
        assert_eq!(
            *reloads.lock().unwrap(),
            0,
            "withdrawal never grants read access"
        );
        assert!(
            run(&tool, &json!({"id":current}).to_string())
                .await
                .is_error
        );
        let granted = approvals.request(&sid, "granted command");
        approvals.approve(&sid, &granted).unwrap();
        assert!(
            run(&tool, &json!({"id":granted}).to_string())
                .await
                .is_error
        );
        assert!(approvals.take(&sid, "granted command"));

        let reserved = approvals.request(&sid, "reserved command");
        approvals.reserve(&sid, &reserved).unwrap();
        assert!(
            run(&tool, &json!({"id":reserved}).to_string())
                .await
                .is_error
        );
        approvals.approve(&sid, &reserved).unwrap();
        assert!(approvals.take(&sid, "reserved command"));
    }

    #[tokio::test]
    async fn canceled_controls_cannot_read_queue_or_revoke() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let (controller, approvals, _) =
            crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
        let sid = controller.info().session_id;
        let id = approvals.request(&sid, "fixture command");
        let pending = Approvals {
            session: sid.clone(),
            approvals: Arc::clone(&approvals),
            revoke: false,
            expected_id: None,
        };
        assert!(
            crate::tool::testutil::run_cancelled(&pending, "{}")
                .await
                .is_error
        );
        let revoke = Approvals {
            session: sid.clone(),
            approvals: Arc::clone(&approvals),
            revoke: true,
            expected_id: Some(id.clone()),
        };
        assert!(
            crate::tool::testutil::run_cancelled(&revoke, &json!({"id":id}).to_string())
                .await
                .is_error
        );
        let queued = Queue(Arc::new(std::sync::atomic::AtomicBool::new(false)));
        assert!(
            crate::tool::testutil::run_cancelled(&queued, "{}")
                .await
                .is_error
        );
        assert!(approvals.pending(&sid).is_some());
    }

    #[tokio::test]
    async fn approval_mutex_keeps_withdrawal_out_of_a_configuration_write() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let (controller, approvals, _) =
            crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
        let sid = controller.info().session_id;
        let id = approvals.request(&sid, "fixture command");
        let tool = Approvals {
            session: sid.clone(),
            approvals: Arc::clone(&approvals),
            revoke: true,
            expected_id: None,
        };
        let guard = approvals.decision.lock().await;
        let arguments = json!({"id":id}).to_string();
        let call = run(&tool, &arguments);
        tokio::pin!(call);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut call)
                .await
                .is_err()
        );
        approvals.approve(&sid, &id).unwrap();
        drop(guard);
        assert!(call.await.is_error);
        assert!(approvals.take(&sid, "fixture command"));
    }

    #[tokio::test]
    async fn cancellation_while_waiting_for_decision_lock_cannot_revoke() {
        let workspace = tempfile::tempdir().unwrap();
        let sessions = tempfile::tempdir().unwrap();
        let (controller, approvals, _) =
            crate::app::controller_with_approvals(workspace.path(), sessions.path()).await;
        let sid = controller.info().session_id;
        let id = approvals.request(&sid, "fixture command");
        let tool = Approvals {
            session: sid.clone(),
            approvals: Arc::clone(&approvals),
            revoke: true,
            expected_id: Some(id.clone()),
        };
        let held = approvals.decision.lock().await;
        let cancel = CancellationToken::new();
        let arguments = crate::tool::testutil::raw(&json!({"id":id}).to_string());
        let call = tool.execute(&arguments, &cancel);
        tokio::pin!(call);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut call)
                .await
                .is_err()
        );
        cancel.cancel();
        drop(held);
        assert!(call.await.is_error);
        assert!(approvals.pending(&sid).is_some());
    }
}
