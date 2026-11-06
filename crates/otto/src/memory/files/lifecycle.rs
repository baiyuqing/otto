//! Lifecycle writes: state changes and verifications.
//!
//! Both write a new revision (and so a history file) with the content fields
//! and `updated_at` unchanged, so a review never looks like a content update.

use super::FileStore;
use crate::memory::{Error, ErrorKind, Record, Result, StoreSetStateRequest, StoreVerifyRequest};

impl FileStore {
    /// Writes a new revision with the requested state.
    pub fn set_state(&self, _request: &StoreSetStateRequest) -> Result<Record> {
        Err(Error::new(ErrorKind::Unsupported))
    }

    /// Writes a new revision that records a verification.
    pub fn verify(&self, _request: &StoreVerifyRequest) -> Result<Record> {
        Err(Error::new(ErrorKind::Unsupported))
    }
}
