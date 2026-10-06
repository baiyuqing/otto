//! Temporary compatibility facade for the Turso memory adapter.

pub use crate::storage::backends::turso::memory::{
    Options, Store, candidates, codec, cursor, map_storage_error, query, records, retriever,
    schema, standing,
};

#[cfg(test)]
pub(crate) use crate::storage::backends::turso::memory::testsupport;
