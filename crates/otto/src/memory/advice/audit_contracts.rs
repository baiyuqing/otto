//! Domain contract for the append-only memory-advice audit record.

/// A storage failure carries no database text, paths, or row values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Io,
    UnknownVersion,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Io => "memory advice audit log unavailable",
            Self::UnknownVersion => "memory advice audit log has an unrecognized schema version",
        })
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// One request, before the candidate id is known.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRow {
    pub model: String,
    pub auxiliary: bool,
    /// The namespace of the candidate's scope.
    pub scope: String,
    pub request: String,
    pub response: Option<String>,
    /// `ok`, `timeout`, `error` or `invalid`.
    pub outcome: &'static str,
    pub error: Option<String>,
    pub latency_ms: i64,
}

pub trait AuditStore: Send + Sync {
    fn append(&self, row: &NewRow, candidate_id: Option<&str>) -> Result<()>;
}
