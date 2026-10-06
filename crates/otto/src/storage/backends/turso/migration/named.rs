//! Fixed domain record mappings shared by the three relational codecs.
use super::*;
use crate::storage::{Params, Value};

pub(super) use crate::storage::migration::record::LogicalRecord as NamedRecord;

pub(super) struct Kind {
    pub record: &'static crate::storage::migration::record::RecordSchema,
    pub table: &'static str,
}
impl std::ops::Deref for Kind {
    type Target = crate::storage::migration::record::RecordSchema;
    fn deref(&self) -> &Self::Target {
        self.record
    }
}
pub(super) fn next(
    connection: &Connection,
    kinds: &[Kind],
    cursor: &str,
    max_bytes: usize,
    normalize: fn(&mut NamedRecord) -> Result<()>,
) -> Result<Option<(String, Record)>> {
    let (mut section, mut offset): (usize, usize) = if cursor.is_empty() {
        (0, 0)
    } else {
        let (section, offset) = cursor
            .split_once(':')
            .ok_or_else(|| Error::Invalid("invalid domain cursor".into()))?;
        (
            section
                .parse()
                .map_err(|_| Error::Invalid("invalid domain cursor section".into()))?,
            offset
                .parse()
                .map_err(|_| Error::Invalid("invalid domain cursor offset".into()))?,
        )
    };
    // ponytail: bounded offline OFFSET scans; use keyset cursors when snapshot
    // volume makes this cost material. No live writes run under this cursor.
    while section < kinds.len() {
        let kind = &kinds[section];
        let order = kind.keys.join(",");
        // SQL computes escaped JSON size before any row field enters memory.
        // Each payload byte needs at most four bytes in neutral Vec<u8> JSON.
        let fields_json = kind
            .fields
            .iter()
            .map(|field| format!("'{field}',{field}"))
            .collect::<Vec<_>>()
            .join(",");
        let length: Option<i64> = connection.query_row(
            &format!("SELECT length(CAST(json_object('version',1,'kind','{}','fields',json_object({fields_json})) AS BLOB)) FROM {} ORDER BY {order} LIMIT 1 OFFSET ?1",kind.name,kind.table),
            [offset], |r|r.get(0),
        ).optional().map_err(sql_error)?;
        let Some(length) = length else {
            section += 1;
            offset = 0;
            continue;
        };
        if length < 0
            || (length as usize)
                .checked_mul(6)
                .and_then(|n| n.checked_add(2048))
                .is_none_or(|n| n > max_bytes)
        {
            return Err(Error::Invalid(
                "domain record exceeds encoded chunk bound".into(),
            ));
        }
        let mut record = connection
            .query_row(
                &format!(
                    "SELECT {} FROM {} ORDER BY {order} LIMIT 1 OFFSET ?1",
                    kind.fields.join(","),
                    kind.table
                ),
                [offset],
                |r| {
                    let mut fields = BTreeMap::new();
                    for (index, name) in kind.fields.iter().enumerate() {
                        let value = match r.get_value(index)? {
                            Value::Null => serde_json::Value::Null,
                            Value::Integer(n) => serde_json::Value::from(n),
                            Value::Real(n) => serde_json::Number::from_f64(n)
                                .map(serde_json::Value::Number)
                                .ok_or_else(|| {
                                    crate::storage::Error::ConversionFailure(
                                        "non-finite domain value".into(),
                                    )
                                })?,
                            Value::Text(s) => serde_json::Value::String(s),
                            Value::Blob(_) => {
                                return Err(crate::storage::Error::ConversionFailure(
                                    "unsupported domain blob".into(),
                                ));
                            }
                        };
                        fields.insert((*name).into(), value);
                    }
                    Ok(NamedRecord {
                        version: 1,
                        kind: kind.name.into(),
                        fields,
                    })
                },
            )
            .map_err(sql_error)?;
        normalize(&mut record)?;
        let (id, revision) = record.metadata(kind)?;
        return Ok(Some((
            format!("{section}:{}", offset + 1),
            Record {
                id,
                revision,
                payload: serde_json::to_vec(&record).map_err(json_error)?,
            },
        )));
    }
    if section != kinds.len() {
        return Err(Error::Invalid("unsupported domain cursor section".into()));
    }
    Ok(None)
}
pub(super) fn import(
    connection: &Connection,
    kinds: &[Kind],
    record: &Record,
) -> Result<NamedRecord> {
    let data: NamedRecord = serde_json::from_slice(&record.payload).map_err(json_error)?;
    let kind = kinds
        .iter()
        .find(|kind| kind.name == data.kind)
        .ok_or_else(|| Error::Invalid("unsupported domain record kind".into()))?;
    if data.version != 1
        || data.fields.len() != kind.fields.len()
        || kind
            .fields
            .iter()
            .any(|field| !data.fields.contains_key(*field))
    {
        return Err(Error::Invalid(
            "incompatible domain record fields/version".into(),
        ));
    }
    let metadata = data.metadata(kind)?;
    if record.id != metadata.0 || record.revision != metadata.1 {
        return Err(Error::Invalid("domain identity/revision mismatch".into()));
    }
    let values = kind
        .fields
        .iter()
        .map(|field| match &data.fields[*field] {
            serde_json::Value::Null => Ok(Value::Null),
            serde_json::Value::String(s) => Ok(Value::Text(s.clone())),
            serde_json::Value::Number(n) => {
                if let Some(n) = n.as_i64() {
                    Ok(Value::Integer(n))
                } else {
                    n.as_f64()
                        .map(Value::Real)
                        .ok_or_else(|| Error::Invalid("invalid domain numeric value".into()))
                }
            }
            _ => Err(Error::Invalid("unsupported domain field value".into())),
        })
        .collect::<Result<Vec<_>>>()?;
    let placeholders = vec!["?"; values.len()].join(",");
    connection
        .execute(
            &format!(
                "INSERT INTO {} ({}) VALUES ({placeholders})",
                kind.table,
                kind.fields.join(",")
            ),
            Params(values),
        )
        .map_err(sql_error)?;
    Ok(data)
}
pub(super) fn validate(
    connection: &Connection,
    kinds: &[Kind],
    extra_tables: &[&str],
) -> Result<()> {
    let mut statement = connection
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .map_err(sql_error)?;
    for table in statement
        .query_map((), |r| r.get::<String>(0))
        .map_err(sql_error)?
    {
        let table = table.map_err(sql_error)?;
        if table != "sqlite_sequence"
            && table != "otto_migration_stage"
            && table != "otto_migration_chunks"
            && !kinds.iter().any(|kind| kind.table == table)
            && !extra_tables.contains(&table.as_str())
        {
            return Err(Error::Invalid(format!(
                "unsupported domain state table: {table}"
            )));
        }
    }
    for kind in kinds {
        let mut statement = connection
            .prepare(&format!("PRAGMA table_info({})", kind.table))
            .map_err(sql_error)?;
        let fields = statement
            .query_map((), |r| r.get::<String>(1))
            .map_err(sql_error)?
            .collect::<crate::storage::Result<Vec<_>>>()
            .map_err(sql_error)?;
        if fields.len() != kind.fields.len()
            || kind
                .fields
                .iter()
                .any(|field| !fields.iter().any(|actual| actual == field))
        {
            return Err(Error::Invalid(format!(
                "incompatible {} schema fields",
                kind.name
            )));
        }
    }
    if connection
        .prepare("PRAGMA foreign_key_check")
        .map_err(sql_error)?
        .query(())
        .map_err(sql_error)?
        .next()
        .map_err(sql_error)?
        .is_some()
    {
        return Err(Error::Invalid("domain reference closure failed".into()));
    }
    Ok(())
}
pub(super) fn unchanged(_: &mut NamedRecord) -> Result<()> {
    Ok(())
}

pub(super) fn initialize_schema(
    connection: &mut Connection,
    version: i64,
    schema: &str,
) -> Result<()> {
    let actual: i64 = connection
        .query_row("PRAGMA user_version", (), |r| r.get(0))
        .map_err(sql_error)?;
    if actual == version && version != 0 {
        return Ok(());
    }
    if actual != 0 {
        return Err(Error::Invalid(
            "unsupported domain staging schema version".into(),
        ));
    }
    let tx = connection.transaction().map_err(sql_error)?;
    tx.execute_batch(schema).map_err(sql_error)?;
    tx.pragma_update("user_version", version)
        .map_err(sql_error)?;
    tx.commit().map_err(sql_error)
}
pub(super) fn version(connection: &Connection, expected: i64) -> Result<()> {
    let version: i64 = connection
        .query_row("PRAGMA user_version", (), |r| r.get(0))
        .map_err(sql_error)?;
    if version != expected {
        return Err(Error::Invalid(
            "unsupported domain source schema version".into(),
        ));
    }
    Ok(())
}
