pub use otto_core::approval::*;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("approval storage is unavailable")]
    Unavailable,
    #[error("approval commit acknowledgement is unknown; reconcile before retrying")]
    CommitUnknown,
    #[error("approval not found")]
    NotFound,
    #[error("approval scope is unauthorized")]
    Unauthorized,
    #[error("approval changed or was already decided")]
    Conflict,
    #[error("invalid approval input: {0}")]
    Invalid(&'static str),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Constructed by admitted native/HTTP/ACP/chat handlers, never model arguments.
#[derive(Debug, Clone)]
pub struct Actor {
    pub identity: String,
    pub transport: String,
}
impl Actor {
    pub fn local(transport: &str) -> Self {
        Self {
            identity: "local-user".into(),
            transport: transport.into(),
        }
    }
}

pub struct Proposal {
    pub kind: Kind,
    pub scope: Scope,
    pub subject: Subject,
    pub title: String,
    pub reason: String,
    pub persistent: bool,
    pub blocking: bool,
    pub expires_at: Option<i64>,
}

pub trait Domain: Send + Sync {
    fn details(&self, request: &Request) -> Result<String>;
    /// Must be idempotent using a receipt committed with the domain mutation.
    fn apply(&self, request: &Request) -> Result<()>;
}
