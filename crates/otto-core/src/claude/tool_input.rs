//! Content-free tool input validation and completed-response failure metadata.
use super::json;
use serde_json::{Value, error::Category, value::RawValue};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolInputFailureKind {
    Incomplete,
    Syntax,
    DuplicateKey,
    NonObject,
}
impl fmt::Display for ToolInputFailureKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Incomplete => "incomplete_json",
            Self::Syntax => "invalid_json",
            Self::DuplicateKey => "duplicate_key",
            Self::NonObject => "non_object",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolInputFailure {
    pub kind: ToolInputFailureKind,
    pub line: usize,
    pub column: usize,
}
impl ToolInputFailure {
    fn from_error(error: serde_json::Error) -> Self {
        Self {
            kind: match error.classify() {
                Category::Eof => ToolInputFailureKind::Incomplete,
                // The duplicate-key visitor is the only source of data errors
                // in this untyped decoder. Never format the serde error: it may
                // include the offending input character.
                Category::Data => ToolInputFailureKind::DuplicateKey,
                Category::Syntax | Category::Io => ToolInputFailureKind::Syntax,
            },
            line: error.line(),
            column: error.column(),
        }
    }
    pub(super) fn parse(input: String) -> Result<Box<RawValue>, Self> {
        let value = json::parse(input.as_bytes()).map_err(Self::from_error)?;
        if !matches!(value, Value::Object(_)) {
            return Err(Self {
                kind: ToolInputFailureKind::NonObject,
                line: 0,
                column: 0,
            });
        }
        RawValue::from_string(input).map_err(Self::from_error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub struct ToolInputError {
    pub index: u64,
    pub failure: ToolInputFailure,
    /// Fixed allowlisted protocol value, never an upstream free-text string.
    pub stop_reason: &'static str,
    pub output_tokens: Option<i64>,
}
impl ToolInputError {
    /// Only an incomplete JSON input in a completed max_tokens response can
    /// qualify for a retry with a larger request budget.
    pub fn is_token_limit_truncation(&self) -> bool {
        self.failure.kind == ToolInputFailureKind::Incomplete && self.stop_reason == "max_tokens"
    }
}
impl fmt::Display for ToolInputError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "malformed Claude tool input: {} (block={}, line={}, column={}, stop_reason={}, output_tokens=",
            self.failure.kind, self.index, self.failure.line, self.failure.column, self.stop_reason
        )?;
        match self.output_tokens {
            Some(tokens) => write!(f, "{tokens})"),
            None => f.write_str("unknown)"),
        }
    }
}
