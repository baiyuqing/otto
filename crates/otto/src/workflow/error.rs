//! Typed workflow failures shared by the store, controller and HTTP boundary.
//!
//! Messages are written for users and never carry filesystem paths or secrets,
//! so each kind can be rendered as-is; `Internal` is the exception and is only
//! ever returned with a fixed public body after the cause is logged.
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The named run, step or request does not exist in this workspace.
    NotFound(String),
    /// A definition, request body or stored value is malformed.
    Invalid(String),
    /// The run or step cannot change state the way the caller asked.
    Conflict(String),
    /// The workflow store or child-agent runtime cannot be used right now.
    Unavailable(String),
    /// An invariant or internal operation failed; log the cause, return a fixed body.
    Internal(String),
}

impl Error {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::Conflict(message.into())
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::Unavailable(message.into())
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(message)
            | Self::Invalid(message)
            | Self::Conflict(message)
            | Self::Unavailable(message)
            | Self::Internal(message) => message,
        }
    }

    /// Only this kind means "try the next workspace" during a lookup.
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::NotFound(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message())
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
