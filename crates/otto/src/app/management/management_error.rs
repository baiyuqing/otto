//! The typed error of the memory management requests.
//!
//! Frontends that print text keep the exact message through `Display`; the
//! HTTP frontend maps [`ManagementErrorKind`] to a status code instead of
//! parsing the message.

use crate::memory;

/// What a client can do about a failed memory management request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagementErrorKind {
    /// Memory is disabled, closed, busy or not wired for this session.
    Unavailable,
    /// The selected memory source manages its own records.
    Unsupported,
    /// The record, revision or candidate does not exist in the session scopes.
    NotFound,
    /// The record moved to another revision, or the candidate was decided.
    Conflict,
    /// The request or its cursor is malformed or rejected by validation.
    Invalid,
    /// Anything else; the message is for operators, not clients.
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagementError {
    pub kind: ManagementErrorKind,
    pub message: String,
}

impl ManagementError {
    pub fn new(kind: ManagementErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn unavailable() -> Self {
        Self::new(ManagementErrorKind::Unavailable, "memory is not available")
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ManagementErrorKind::NotFound, message)
    }
}

impl std::fmt::Display for ManagementError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ManagementError {}

impl From<memory::Error> for ManagementError {
    fn from(error: memory::Error) -> Self {
        use memory::ErrorKind as Store;
        let kind = match error.kind {
            Store::Disabled
            | Store::Unavailable
            | Store::Closed
            | Store::PersistenceDisabled
            | Store::Busy => ManagementErrorKind::Unavailable,
            Store::Unsupported => ManagementErrorKind::Unsupported,
            Store::NotFound => ManagementErrorKind::NotFound,
            Store::Conflict => ManagementErrorKind::Conflict,
            Store::InvalidRequest
            | Store::InvalidCursor
            | Store::InvalidRecord
            | Store::SensitiveMemory => ManagementErrorKind::Invalid,
            _ => ManagementErrorKind::Internal,
        };
        Self::new(kind, error.to_string())
    }
}
