//! The `reflect` tool: the model asks for the reflection use case that
//! `/reflect` runs, instead of writing a retrospective file itself.
//!
//! The tool only parses the call and forwards it to a [`Reflect`] port. The
//! port is the consumer boundary: `app` implements it from the parts it
//! already owns (the reflector, the memory service, the single-flight flag)
//! and binds it through the [`Handle`] the runner carries, so this module
//! depends on nothing above `tool`.
//!
//! Ownership: the builder creates one [`Handle`] per runner and gives the
//! tool and the runner an `Arc` each. The controller binds the port after it
//! installs the runner; until then, and when the runner was built without a
//! controller, a call reports that reflection is unavailable.
//!
//! Concurrency and cancellation: `execute` takes `&self` and may run beside
//! other calls; the handle is behind a mutex that is never held across an
//! await. The turn's token is passed to the port, which cancels the run.
//!
//! Errors: every failure is a tool error result with the port's message;
//! nothing is retried.

use std::sync::{Arc, Mutex};

use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::{CONTEXT_CANCELED, Tool, definition, error_result, text_result};

/// The provider-visible name.
pub const NAME: &str = "reflect";

/// The largest accepted focus, in bytes, the same bound as `/reflect`.
const MAXIMUM_FOCUS_BYTES: usize = crate::reflection::prompt::MAXIMUM_FOCUS_BYTES;

/// Runs one requested reflection and returns the text the model reads.
#[async_trait::async_trait]
pub trait Reflect: Send + Sync {
    async fn reflect(&self, focus: &str, cancel: &CancellationToken) -> Result<String, String>;
}

/// Where the controller binds the port for one runner.
#[derive(Default)]
pub struct Handle {
    port: Mutex<Option<Arc<dyn Reflect>>>,
}

impl Handle {
    /// Replaces the bound port.
    pub fn bind(&self, port: Arc<dyn Reflect>) {
        *self.port.lock().unwrap_or_else(|p| p.into_inner()) = Some(port);
    }

    fn port(&self) -> Option<Arc<dyn Reflect>> {
        self.port.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

pub struct ReflectTool {
    handle: Arc<Handle>,
}

impl ReflectTool {
    pub fn new(handle: Arc<Handle>) -> Self {
        Self { handle }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    #[serde(default)]
    focus: String,
}

#[async_trait::async_trait]
impl Tool for ReflectTool {
    fn definition(&self) -> ToolDefinition {
        definition(
            NAME,
            "Reflect on this session the way /reflect does. Writes a retrospective of the session to the run log under ~/.otto/reflection/runs/ and queues memory and skill proposals for the user's review; it writes no memory or skill itself. Call it only when the user asks for a retrospective, a review of the session, or to distil lessons or skills. Do not write retrospective files yourself. Returns the result line, the run log path and the retrospective.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "focus": {
                        "type": "string",
                        "maxLength": MAXIMUM_FOCUS_BYTES,
                        "description": "Optional: what the user wants the reflection to look at."
                    }
                },
                "required": []
            }),
        )
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return error_result(CONTEXT_CANCELED);
        }
        let args: Args = match serde_json::from_str(arguments.get()) {
            Ok(args) => args,
            Err(error) => return error_result(error),
        };
        if args.focus.len() > MAXIMUM_FOCUS_BYTES {
            return error_result(format!("focus is longer than {MAXIMUM_FOCUS_BYTES} bytes"));
        }
        let Some(port) = self.handle.port() else {
            return error_result("reflection is not available in this session");
        };
        match port.reflect(&args.focus, cancel).await {
            Ok(text) => text_result(text),
            Err(message) => error_result(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::raw;

    struct Recording {
        focus: Mutex<Vec<String>>,
        answer: Result<String, String>,
    }

    #[async_trait::async_trait]
    impl Reflect for Recording {
        async fn reflect(&self, focus: &str, cancel: &CancellationToken) -> Result<String, String> {
            assert!(!cancel.is_cancelled());
            self.focus.lock().unwrap().push(focus.to_owned());
            self.answer.clone()
        }
    }

    fn bound(answer: Result<String, String>) -> (ReflectTool, Arc<Recording>) {
        let handle = Arc::new(Handle::default());
        let port = Arc::new(Recording {
            focus: Mutex::new(Vec::new()),
            answer,
        });
        handle.bind(Arc::clone(&port) as Arc<dyn Reflect>);
        (ReflectTool::new(handle), port)
    }

    #[tokio::test]
    async fn a_call_forwards_its_focus_and_returns_the_ports_text() {
        let (tool, port) = bound(Ok("reflection: 1 candidate(s) queued".into()));
        let result = tool
            .execute(
                &raw(r#"{"focus":"search tools"}"#),
                &CancellationToken::new(),
            )
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert_eq!(result.content, "reflection: 1 candidate(s) queued");
        let result = tool.execute(&raw("{}"), &CancellationToken::new()).await;
        assert!(!result.is_error);
        assert_eq!(*port.focus.lock().unwrap(), ["search tools", ""]);
    }

    #[tokio::test]
    async fn a_port_error_is_a_tool_error() {
        let (tool, _) = bound(Err("reflection is turned off".into()));
        let result = tool.execute(&raw("{}"), &CancellationToken::new()).await;
        assert!(result.is_error);
        assert_eq!(result.content, "reflection is turned off");
    }

    #[tokio::test]
    async fn an_unbound_handle_bad_arguments_and_cancellation_never_reach_a_port() {
        let tool = ReflectTool::new(Arc::new(Handle::default()));
        let result = tool.execute(&raw("{}"), &CancellationToken::new()).await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "reflection is not available in this session"
        );

        let (tool, port) = bound(Ok(String::new()));
        for arguments in [
            r#"{"other":1}"#.to_owned(),
            json!({"focus": "x".repeat(MAXIMUM_FOCUS_BYTES + 1)}).to_string(),
        ] {
            assert!(
                tool.execute(&raw(&arguments), &CancellationToken::new())
                    .await
                    .is_error
            );
        }
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(tool.execute(&raw("{}"), &cancelled).await.is_error);
        assert!(port.focus.lock().unwrap().is_empty());
    }
}
