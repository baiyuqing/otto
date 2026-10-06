//! Reflection coverage watermarks and generated-skill ownership/version receipts.
use super::{
    named::{self, Kind},
    *,
};
use crate::storage::backends::turso::reflection::{self, Store};
pub struct ReflectionCodec;
const KINDS: &[Kind] = &[
    Kind {
        record: &crate::reflection::migration::RECORDS[0],
        table: "runs",
    },
    Kind {
        record: &crate::reflection::migration::RECORDS[1],
        table: "watermarks",
    },
    Kind {
        record: &crate::reflection::migration::RECORDS[2],
        table: "generated_skills",
    },
    Kind {
        record: &crate::reflection::migration::RECORDS[3],
        table: "skill_versions",
    },
];
impl SqlDomainCodec for ReflectionCodec {
    fn domain(&self) -> &'static str {
        "reflection"
    }
    fn family(&self) -> &'static str {
        "otto.reflection"
    }
    fn version(&self) -> u32 {
        1
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row("SELECT COALESCE(MAX(id),0) FROM skill_versions", (), |r| {
                r.get::<i64>(0)
            })
            .map(|v| v.to_string())
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        named::initialize_schema(connection, reflection::SCHEMA_VERSION, reflection::SCHEMA)
    }
    fn next_record(
        &self,
        connection: &Connection,
        cursor: &str,
        max_bytes: usize,
    ) -> Result<Option<(String, Record)>> {
        named::next(connection, KINDS, cursor, max_bytes, named::unchanged)
    }
    fn import_record(&self, connection: &Connection, record: &Record) -> Result<()> {
        named::import(connection, KINDS, record).map(|_| ())
    }
    fn validate(&self, connection: &Connection) -> Result<()> {
        named::version(connection, reflection::SCHEMA_VERSION)?;
        named::validate(connection, KINDS, &[])
    }
    fn reopen(&self, connection: Connection) -> Result<()> {
        Store::from_connection(connection)
            .map(|_| ())
            .map_err(|e| Error::Unavailable(e.to_string()))
    }
}
