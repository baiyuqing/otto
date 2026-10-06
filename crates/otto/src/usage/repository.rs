//! Read and write operations over persisted usage.

use super::{Analysis, Result, Sink, Summary};

/// Usage persistence used by collection and the runtime's aggregate queries.
pub trait UsageRepository: Sink {
    fn summary(&self, session_id: Option<&str>) -> Result<Summary>;
    fn daily(&self, days: u16, session_id: Option<&str>) -> Result<Analysis>;
}
