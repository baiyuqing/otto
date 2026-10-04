//! The standing set's token budget.
//!
//! Recall injects every record of the standing set (see
//! [`super::query::build_standing_query`]) up to [`MAX_STANDING_TOKENS`],
//! oldest first. A write that would push the set past that budget is checked
//! here, inside the write transaction that stores it, so the check and the
//! write are serialized by the same store lock as every other write.

use chrono::{DateTime, Utc};

use super::Store;
use super::query::build_standing_query;
use super::retriever::{query_records, record_cost};
use crate::memory::{
    Error, ErrorKind, MAX_STANDING_TOKENS, NAMESPACE_USER, Record, Result, RetrievalRequest, Scope,
    StandingUsage,
};
use crate::storage::Connection;

fn standing_request(now: DateTime<Utc>) -> RetrievalRequest {
    RetrievalRequest {
        query: String::new(),
        scopes: Vec::new(),
        kinds: Vec::new(),
        labels: Vec::new(),
        include_expired: false,
        include_baseline: true,
        limit: 1,
        token_budget: MAX_STANDING_TOKENS,
        cursor: String::new(),
        now,
        estimate_tokens: None,
    }
}

/// Whether `record` belongs to the standing set when it is stored.
fn standing_eligible(record: &Record, now: DateTime<Utc>) -> bool {
    record.scope.namespace == NAMESPACE_USER
        && !record.key.is_empty()
        && matches!(record.kind.as_str(), "preference" | "instruction")
        && record.expires_at.is_none_or(|expires| expires > now)
}

fn read_standing(connection: &Connection, user: &Scope, now: DateTime<Utc>) -> Result<Vec<Record>> {
    let statement = build_standing_query(&standing_request(now), user, usize::MAX);
    query_records(connection, &statement, usize::MAX)
}

/// Rejects `record` when storing it would push its user scope's standing set
/// over the budget. The record being replaced, if any, is not counted.
pub(super) fn check_standing_budget(connection: &Connection, record: &Record) -> Result<()> {
    let now = Utc::now();
    if !standing_eligible(record, now) {
        return Ok(());
    }
    let used: usize = read_standing(connection, &record.scope, now)?
        .iter()
        .filter(|existing| existing.id != record.id)
        .map(|existing| record_cost(None, existing))
        .sum();
    let needed = record_cost(None, record);
    if used + needed > MAX_STANDING_TOKENS {
        return Err(Error::detailed(
            ErrorKind::InvalidRequest,
            format!(
                "standing preferences are full: {used} of {MAX_STANDING_TOKENS} tokens used, this record needs {needed}; forget a user-scope preference or instruction first (/memory forget <id> or otto memory forget)"
            ),
        ));
    }
    Ok(())
}

impl Store {
    /// The standing set of `user`, oldest first: every record recall injects
    /// when the budget allows.
    pub fn standing_records(&self, user: &Scope, now: DateTime<Utc>) -> Result<Vec<Record>> {
        self.with_read(|connection| read_standing(connection, user, now))
    }

    /// How much of the standing budget `user` uses, counting the same way
    /// recall injects: oldest first, stopping at the first record that does
    /// not fit.
    pub fn standing_usage(&self, user: &Scope, now: DateTime<Utc>) -> Result<StandingUsage> {
        let records = self.with_read(|connection| read_standing(connection, user, now))?;
        let mut tokens = 0;
        let mut injected = 0;
        for record in &records {
            let cost = record_cost(None, record);
            if tokens + cost > MAX_STANDING_TOKENS {
                break;
            }
            tokens += cost;
            injected += 1;
        }
        Ok(StandingUsage {
            records: records.len(),
            tokens,
            budget: MAX_STANDING_TOKENS,
            omitted: records.len() - injected,
        })
    }
}
