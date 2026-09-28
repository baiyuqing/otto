//! The tool registry.
//!
//! A [`Registry`] owns a fixed set of tools, keyed by the name each one
//! advertises, and serves them through [`otto_core::tool::ToolExecutor`].
//!
//! Ownership: the registry takes ownership of its tools at construction and
//! never mutates them afterwards, so `&Registry` is `Send + Sync` whenever its
//! tools are. Concurrency: `execute` takes `&self` and may run concurrently.
//! Cancellation and errors follow the executor contract: a cancelled token
//! produces an error result, and failures are reported in band, never as a
//! `Result`. An unregistered name produces [`ToolResult::unknown_tool`] and
//! calls neither guard hook.

use std::collections::HashMap;
use std::sync::Arc;

use otto_core::model::ToolDefinition;
use otto_core::tool::{ToolExecutor, ToolResult};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::Tool;

/// A check run around every registered tool call. `before` can refuse the
/// call before the tool runs; `after` can flag a call that ran but whose
/// effects the caller should not trust. Both return operator-facing text
/// on failure.
pub trait CallGuard: Send + Sync {
    /// Runs before the tool call. An `Err` refuses the call instead of
    /// running it, so it never runs against state the guard has already
    /// decided not to trust, such as a session lease that has been lost.
    fn before(&self) -> Result<(), String>;
    /// Runs after the tool call returns. An `Err` flags a call that ran but
    /// whose effects the caller should not trust as durable, such as a
    /// workspace sync that failed after the call finished.
    fn after(&self) -> Result<(), String>;
}

/// A named, ordered set of tools.
pub struct Registry {
    ordered: Vec<Box<dyn Tool + Send + Sync>>,
    by_name: HashMap<String, usize>,
    guard: Option<Arc<dyn CallGuard>>,
}

impl Registry {
    /// Builds a registry from `tools`, preserving their order.
    ///
    /// Returns `duplicate tool: {name}` when two tools advertise the same name.
    pub fn new(tools: Vec<Box<dyn Tool + Send + Sync>>) -> Result<Self, String> {
        let mut by_name = HashMap::with_capacity(tools.len());
        for (index, tool) in tools.iter().enumerate() {
            let name = tool.definition().name;
            if by_name.insert(name.clone(), index).is_some() {
                return Err(format!("duplicate tool: {name}"));
            }
        }
        Ok(Self {
            ordered: tools,
            by_name,
            guard: None,
        })
    }

    /// Runs `guard.before()`/`guard.after()` around every registered tool
    /// call. Replaces any guard set by an earlier call.
    pub fn with_guard(mut self, guard: Arc<dyn CallGuard>) -> Self {
        self.guard = Some(guard);
        self
    }

    /// The registered tool with this name, if any.
    pub fn lookup(&self, name: &str) -> Option<&(dyn Tool + Send + Sync)> {
        self.by_name
            .get(name)
            .map(|&index| self.ordered[index].as_ref())
    }

    /// The registered tools in registration order.
    pub fn tools(&self) -> &[Box<dyn Tool + Send + Sync>] {
        &self.ordered
    }
}

#[async_trait::async_trait]
impl ToolExecutor for Registry {
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.ordered.iter().map(|tool| tool.definition()).collect()
    }

    async fn execute(
        &self,
        name: &str,
        arguments: &RawValue,
        cancel: &CancellationToken,
    ) -> ToolResult {
        let Some(tool) = self.lookup(name) else {
            return ToolResult::unknown_tool(name);
        };

        if let Some(guard) = &self.guard
            && let Err(text) = guard.before()
        {
            return ToolResult::error(text);
        }

        let mut result = tool.execute(arguments, cancel).await;

        if let Some(guard) = &self.guard
            && let Err(text) = guard.after()
        {
            result.content = format!("{text}\n\n{}", result.content);
            result.is_error = true;
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::testutil::raw;
    use crate::tool::{definition, text_result};

    struct FakeTool {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl Tool for FakeTool {
        fn definition(&self) -> ToolDefinition {
            definition(self.name, "", serde_json::json!({"type": "object"}))
        }

        async fn execute(&self, _arguments: &RawValue, _cancel: &CancellationToken) -> ToolResult {
            text_result("ok")
        }
    }

    fn fake(name: &'static str) -> Box<dyn Tool + Send + Sync> {
        Box::new(FakeTool { name })
    }

    #[test]
    fn duplicate_names_are_rejected() {
        let error = Registry::new(vec![fake("read"), fake("read")])
            .err()
            .expect("a duplicate name must fail");
        assert_eq!(error, "duplicate tool: read");
    }

    #[test]
    fn definitions_lookup_and_tools_preserve_the_input_order() {
        let registry = Registry::new(vec![fake("first"), fake("second")]).unwrap();
        let definitions = registry.definitions();
        assert_eq!(definitions.len(), 2);
        assert_eq!(definitions[0].name, "first");
        assert_eq!(definitions[1].name, "second");

        assert_eq!(
            registry.lookup("second").unwrap().definition().name,
            "second"
        );
        assert!(registry.lookup("missing").is_none());

        let tools = registry.tools();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].definition().name, "first");
        assert_eq!(tools[1].definition().name, "second");
    }

    #[tokio::test]
    async fn an_unregistered_name_reports_the_fixed_error_text() {
        let registry = Registry::new(vec![fake("read")]).unwrap();
        let result = registry
            .execute("missing", &raw("{}"), &CancellationToken::new())
            .await;
        assert!(result.is_error);
        assert_eq!(result.content, "unknown tool: missing");
    }

    #[tokio::test]
    async fn a_registered_name_reaches_its_tool() {
        let registry = Registry::new(vec![fake("read")]).unwrap();
        let result = registry
            .execute("read", &raw("{}"), &CancellationToken::new())
            .await;
        assert!(!result.is_error && result.content == "ok", "{result:?}");
    }

    struct FakeGuard {
        before_result: Result<(), String>,
        after_result: Result<(), String>,
    }

    impl CallGuard for FakeGuard {
        fn before(&self) -> Result<(), String> {
            self.before_result.clone()
        }

        fn after(&self) -> Result<(), String> {
            self.after_result.clone()
        }
    }

    #[tokio::test]
    async fn a_before_failure_skips_the_tool() {
        let registry = Registry::new(vec![fake("read")])
            .unwrap()
            .with_guard(Arc::new(FakeGuard {
                before_result: Err("lease lost".to_string()),
                after_result: Ok(()),
            }));

        let result = registry
            .execute("read", &raw("{}"), &CancellationToken::new())
            .await;

        assert!(result.is_error);
        assert_eq!(result.content, "lease lost");
    }

    #[tokio::test]
    async fn an_after_failure_wraps_a_successful_result() {
        let registry = Registry::new(vec![fake("read")])
            .unwrap()
            .with_guard(Arc::new(FakeGuard {
                before_result: Ok(()),
                after_result: Err("sync failed".to_string()),
            }));

        let result = registry
            .execute("read", &raw("{}"), &CancellationToken::new())
            .await;

        assert!(result.is_error);
        assert_eq!(result.content, "sync failed\n\nok");
    }

    #[tokio::test]
    async fn an_unregistered_name_calls_neither_hook() {
        let registry = Registry::new(vec![fake("read")])
            .unwrap()
            .with_guard(Arc::new(FakeGuard {
                before_result: Err("should not be called".to_string()),
                after_result: Err("should not be called".to_string()),
            }));

        let result = registry
            .execute("missing", &raw("{}"), &CancellationToken::new())
            .await;

        assert_eq!(result.content, "unknown tool: missing");
    }
}
