//! Guarantees checked before a repository receives a connection.
use super::{Error, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SqlDialect {
    /// SQLite syntax, STRICT tables, JSON SQL functions, PRAGMAs,
    /// positional parameters, upserts and RETURNING used by the adapters.
    Sqlite,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteIsolation {
    /// A writer acquires exclusion before reading its mutation preconditions.
    ImmediateSerializable,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlCapabilities {
    pub dialect: SqlDialect,
    pub write_isolation: WriteIsolation,
    pub atomic_transactions: bool,
    pub durable_commits: bool,
    pub multiprocess_writers: bool,
    /// Exact Turso index-method FTS SQL (`USING fts`, weighted index options,
    /// `fts_match`/`fts_score`) and compatible backing schema/search behavior.
    /// Generic SQLite FTS5 support does not satisfy this capability.
    pub fts: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlRequirements {
    pub durable_commits: bool,
    pub multiprocess_writers: bool,
    pub fts: bool,
}

impl SqlRequirements {
    pub const EPHEMERAL: Self = Self {
        durable_commits: false,
        multiprocess_writers: false,
        fts: false,
    };
    pub const DURABLE: Self = Self {
        durable_commits: true,
        multiprocess_writers: true,
        fts: false,
    };
}

impl SqlCapabilities {
    pub fn validate(self, required: SqlRequirements) -> Result<()> {
        if self.dialect != SqlDialect::Sqlite {
            return Err(Error::UnsupportedCapability("SQLite dialect"));
        }
        if self.write_isolation != WriteIsolation::ImmediateSerializable
            || !self.atomic_transactions
        {
            return Err(Error::UnsupportedCapability(
                "atomic immediate serializable transactions",
            ));
        }
        if required.durable_commits && !self.durable_commits {
            return Err(Error::UnsupportedCapability("durable commits"));
        }
        if required.multiprocess_writers && !self.multiprocess_writers {
            return Err(Error::UnsupportedCapability(
                "multiprocess writer coordination",
            ));
        }
        if required.fts && !self.fts {
            return Err(Error::UnsupportedCapability(
                "Turso FTS index dialect and scoring",
            ));
        }
        Ok(())
    }
}
