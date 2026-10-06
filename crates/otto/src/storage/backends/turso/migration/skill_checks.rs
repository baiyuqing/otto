//! Skill-check audit IDs, keys and complete structured results.
use super::{
    named::{self, Kind},
    *,
};
use crate::storage::backends::turso::skill_checks::{self, Store};
pub struct SkillChecksCodec;
const KINDS: &[Kind] = &[Kind {
    name: "skill_check_audit",
    table: "skill_checks",
    fields: &[
        "id",
        "content_sha256",
        "questions_sha256",
        "skill",
        "path",
        "source",
        "model",
        "checked_at",
        "result",
    ],
    keys: &["id"],
    revision: None,
}];
impl SqlDomainCodec for SkillChecksCodec {
    fn domain(&self) -> &'static str {
        "skill_checks"
    }
    fn family(&self) -> &'static str {
        "otto.skill_checks"
    }
    fn version(&self) -> u32 {
        1
    }
    fn committed_cursor(&self, connection: &Connection) -> Result<String> {
        connection
            .query_row("SELECT COALESCE(MAX(id),0) FROM skill_checks", (), |r| {
                r.get::<i64>(0)
            })
            .map(|v| v.to_string())
            .map_err(sql_error)
    }
    fn initialize(&self, connection: &mut Connection) -> Result<()> {
        named::initialize_schema(
            connection,
            skill_checks::SCHEMA_VERSION,
            skill_checks::SCHEMA,
        )
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
        named::version(connection, skill_checks::SCHEMA_VERSION)?;
        named::validate(connection, KINDS, &[])
    }
    fn reopen(&self, connection: Connection) -> Result<()> {
        Store::from_connection(connection)
            .map(|_| ())
            .map_err(|e| Error::Unavailable(e.to_string()))
    }
}
