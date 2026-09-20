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
//! `Result`. An unregistered name produces [`ToolResult::unknown_tool`].

use std::collections::HashMap;

use otto_core::model::ToolDefinition;
use otto_core::tool::{ToolExecutor, ToolResult};
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::Tool;

/// A named, ordered set of tools.
pub struct Registry {
    ordered: Vec<Box<dyn Tool + Send + Sync>>,
    by_name: HashMap<String, usize>,
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
        })
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
        match self.lookup(name) {
            Some(tool) => tool.execute(arguments, cancel).await,
            None => ToolResult::unknown_tool(name),
        }
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
}
