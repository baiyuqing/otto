//! Canonical reconstruction of supported pre-registry databases.
use super::normalize_schema_sql;
use crate::storage::{Connection, Error, Result};
use std::collections::BTreeMap;

/// Return trusted DDL only after every supplied object matches a canonical object.
/// Source SQL is comparison data and is never prepared or executed.
pub fn canonical_schema(family: &str, version: i64, source: &[String]) -> Result<String> {
    for schema in candidates(family, version)? {
        if validate_objects(source, &schema_objects(&schema)?).is_ok() {
            return Ok(schema);
        }
    }
    Err(Error::Misuse(
        "unsupported legacy schema; use a matching older Otto release to export logical records"
            .into(),
    ))
}

fn candidates(family: &str, version: i64) -> Result<Vec<String>> {
    let schemas = match (family, version) {
        ("usage", 0) => {
            let current = super::super::usage::SCHEMA
                .split("CREATE TABLE IF NOT EXISTS provider_api_operations")
                .next()
                .unwrap_or("")
                .to_string();
            let legacy = current
                .replace("    cached_breakdown_present INTEGER CHECK (cached_breakdown_present IN (0, 1)),\n", "")
                .replace(",\n    CHECK (usage_present = 1 OR cached_breakdown_present IS NULL)", "");
            let upgraded = format!(
                "{legacy}\nALTER TABLE usage_events ADD COLUMN cached_breakdown_present INTEGER CHECK (cached_breakdown_present IN (0, 1));"
            );
            vec![legacy, upgraded, current]
        }
        ("tasks", 1) => vec![
            super::super::tasks::SCHEMA
                .replace("    model_source TEXT NOT NULL DEFAULT 'unknown',\n", ""),
        ],
        ("skill-checks", 1) => vec![super::super::skill_checks::SCHEMA.to_string()],
        ("workflow", 0) => {
            // Delivery tables were introduced after the legacy exporter.
            let current = super::super::workflow::SCHEMA
                .split(';')
                .filter(|sql| {
                    ![
                        "workflow_origins",
                        "workflow_pending",
                        "workflow_delivery_schema",
                    ]
                    .iter()
                    .any(|name| sql.contains(name))
                })
                .collect::<Vec<_>>()
                .join(";");
            let mut legacy = current.clone();
            for column in [
                "    forked_from_run_id TEXT,\n",
                "    forked_from_event_seq INTEGER,\n",
                "    forked_from_step_id TEXT,\n",
                "    source_run_id TEXT,\n",
                "    source_step_id TEXT,\n",
                "    source_attempt INTEGER,\n",
            ] {
                legacy = legacy.replace(column, "");
            }
            // Older stores gained nullable fork columns with ALTER TABLE,
            // which appends them rather than using the fresh-schema order.
            let upgraded = format!(
                "{legacy};\n\
                ALTER TABLE workflow_runs ADD COLUMN forked_from_run_id TEXT;\n\
                ALTER TABLE workflow_runs ADD COLUMN forked_from_event_seq INTEGER;\n\
                ALTER TABLE workflow_runs ADD COLUMN forked_from_step_id TEXT;\n\
                ALTER TABLE workflow_steps ADD COLUMN source_run_id TEXT;\n\
                ALTER TABLE workflow_steps ADD COLUMN source_step_id TEXT;\n\
                ALTER TABLE workflow_steps ADD COLUMN source_attempt INTEGER;"
            );
            vec![legacy, upgraded, current]
        }
        _ => return Err(Error::Misuse("unsupported legacy schema version".into())),
    };
    Ok(schemas)
}

fn schema_objects(schema: &str) -> Result<Vec<String>> {
    let canonical = Connection::open_in_memory()?;
    canonical.execute_batch(schema)?;
    let mut statement = canonical.prepare(
        "SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'",
    )?;
    statement
        .query_map((), |row| row.get::<String>(0))?
        .collect()
}

pub(crate) fn validate_objects(source: &[String], canonical: &[String]) -> Result<()> {
    fn objects(objects: &[String]) -> BTreeMap<String, usize> {
        let mut counts = BTreeMap::new();
        for object in objects {
            *counts.entry(normalize_schema_sql(object)).or_insert(0) += 1;
        }
        counts
    }
    if objects(source) != objects(canonical) {
        return Err(Error::Misuse("unsupported legacy schema".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
