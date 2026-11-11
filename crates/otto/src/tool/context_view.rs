//! A read-only view of the executing session's summary tree.

use std::sync::Arc;

use otto_core::agent::context_view::ContextView;
use otto_core::model::ToolDefinition;
use otto_core::tool::ToolResult;
use serde::Deserialize;
use serde_json::value::RawValue;
use tokio_util::sync::CancellationToken;

use super::{Tool, definition, error_result, preflight_error, text_result};

pub(crate) struct Zoom {
    pub(crate) context: Arc<ContextView>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    id: u64,
    n: u64,
}

#[async_trait::async_trait]
impl Tool for Zoom {
    fn definition(&self) -> ToolDefinition {
        definition(
            "zoom",
            "Read a node in this session's summary-tree history view. Takes id (first source chunk, >=0) \
             and n (power of two, >=1; id must be a multiple of n). Returns two child summary lines for n>1, \
             or the original source chunk for n=1. Invalid or unavailable ranges return an error. \
             Read-only; cannot access other sessions.",
            serde_json::json!({"type":"object","properties":{
                "id":{"type":"integer","minimum":0,"description":"First source chunk ID shown in the view."},
                "n":{"type":"integer","minimum":1,"description":"Number of source chunks, a power of two shown in the view."}
            },"required":["id","n"],"additionalProperties":false}),
        )
    }

    fn retry_safety(&self) -> otto_core::model::RetrySafety {
        otto_core::model::RetrySafety::ReadOnly
    }

    async fn execute(&self, arguments: &RawValue, cancel: &CancellationToken) -> ToolResult {
        if cancel.is_cancelled() {
            return preflight_error(super::CONTEXT_CANCELED);
        }
        let args = match super::result::decode_strict_json::<Args>(arguments.get(), &["id", "n"]) {
            Ok(args) => args,
            Err(error) => return error_result(error.to_string()),
        };
        match self.context.zoom(args.id, args.n) {
            Ok(text) => text_result(text),
            Err(error) => error_result(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn zoom_validates_arguments_and_cancellation_without_accessing_other_sessions() {
        let tool = Zoom {
            context: Arc::default(),
        };
        assert_eq!(tool.retry_safety(), otto_core::model::RetrySafety::ReadOnly);
        for args in [
            r#"{"id":0,"n":1,"session":"other"}"#,
            r#"{"id":-1,"n":1}"#,
            r#"{"id":18446744073709551615,"n":1}"#,
            r#"{"id":0,"n":1}"#,
            r#"{"id":0,"n":3}"#,
            r#"{"id":0,"n":1,"id":1}"#,
        ] {
            let arguments = RawValue::from_string(args.into()).unwrap();
            assert!(
                tool.execute(&arguments, &CancellationToken::new())
                    .await
                    .is_error,
                "{args}"
            );
        }
        let cancel = CancellationToken::new();
        cancel.cancel();
        let arguments = RawValue::from_string(r#"{"id":0,"n":1}"#.into()).unwrap();
        assert!(
            tool.execute(&arguments, &cancel)
                .await
                .content
                .contains(super::super::CONTEXT_CANCELED)
        );
    }
}
