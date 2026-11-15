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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SqlRequirements {
    pub durable_commits: bool,
    pub multiprocess_writers: bool,
}

impl SqlRequirements {
    pub(crate) const EPHEMERAL: Self = Self {
        durable_commits: false,
        multiprocess_writers: false,
    };
    pub(crate) const DURABLE: Self = Self {
        durable_commits: true,
        multiprocess_writers: true,
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
        Ok(())
    }
}
