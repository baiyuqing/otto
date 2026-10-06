//! Typed persistence contract for skill-check history.
use crate::typesafe::AnswerMap;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    Io,
    UnknownVersion,
}
impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => formatter.write_str("skill check store unavailable"),
            Self::UnknownVersion => {
                formatter.write_str("skill check database has an unrecognized schema version")
            }
        }
    }
}
impl std::error::Error for StoreError {}
pub type StoreResult<T> = std::result::Result<T, StoreError>;

/// Exact result fields retained in the append-only history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResultJson {
    Rules { rules: Vec<String> },
    Answers { answers: AnswerMap },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub source: String,
    pub model: String,
    pub checked_at: String,
    pub result: ResultJson,
}

pub struct NewRow<'a> {
    pub content_sha256: &'a str,
    pub questions_sha256: &'a str,
    pub skill: &'a str,
    pub path: &'a str,
    pub source: &'static str,
    pub model: &'a str,
    pub checked_at: &'a str,
    pub result: &'a ResultJson,
}

pub trait CheckStore: Send + Sync {
    fn append(&self, row: &NewRow<'_>) -> StoreResult<()>;
    fn latest(&self, content_sha256: &str, questions_sha256: &str) -> StoreResult<Option<Row>>;
}
