//! Workflow schema DDL and the legacy schema migration.

use super::*;

pub(super) const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;
CREATE TABLE IF NOT EXISTS workflow_runs (
    id TEXT PRIMARY KEY,
    workflow TEXT NOT NULL,
    definition_json TEXT NOT NULL,
    definition_hash TEXT NOT NULL,
    workspace TEXT NOT NULL,
    profile TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    input TEXT NOT NULL,
    forked_from_run_id TEXT,
    forked_from_event_seq INTEGER,
    forked_from_step_id TEXT,
    status TEXT NOT NULL CHECK (status IN (
        'running','waiting','paused','succeeded','failed','canceled'
    )),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_steps (
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    kind TEXT NOT NULL CHECK (kind IN ('agent','approval','handoff')),
    agent TEXT NOT NULL,
    prompt TEXT NOT NULL,
    needs_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN (
        'pending','ready','running','waiting','succeeded','failed','canceled','interrupted'
    )),
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    result TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    transcript_path TEXT NOT NULL DEFAULT '',
    source_run_id TEXT,
    source_step_id TEXT,
    source_attempt INTEGER,
    PRIMARY KEY (run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_attempts (
    run_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    attempt INTEGER NOT NULL CHECK (attempt > 0),
    status TEXT NOT NULL CHECK (status IN (
        'running','succeeded','failed','canceled','interrupted'
    )),
    transcript_path TEXT NOT NULL,
    result TEXT NOT NULL DEFAULT '',
    error TEXT NOT NULL DEFAULT '',
    started_at TEXT NOT NULL,
    finished_at TEXT,
    PRIMARY KEY (run_id, step_id, attempt),
    FOREIGN KEY (run_id, step_id) REFERENCES workflow_steps(run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_requests (
    id TEXT PRIMARY KEY,
    run_id TEXT NOT NULL,
    step_id TEXT NOT NULL,
    prompt TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending','approved','rejected','canceled')),
    created_at TEXT NOT NULL,
    responded_at TEXT,
    UNIQUE (run_id, step_id),
    FOREIGN KEY (run_id, step_id) REFERENCES workflow_steps(run_id, id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_origins (
    workspace TEXT NOT NULL,
    session_id TEXT NOT NULL,
    operation_id TEXT NOT NULL,
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    definition_hash TEXT NOT NULL,
    input TEXT NOT NULL,
    PRIMARY KEY(workspace, session_id, operation_id)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_events (
    seq INTEGER PRIMARY KEY,
    run_id TEXT NOT NULL REFERENCES workflow_runs(id),
    occurred_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    step_id TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT ''
) STRICT;
CREATE INDEX IF NOT EXISTS workflow_events_run ON workflow_events(run_id, seq);
CREATE INDEX IF NOT EXISTS workflow_origins_run ON workflow_origins(run_id);
CREATE TABLE IF NOT EXISTS workflow_pending (
    workspace TEXT NOT NULL,
    session_id TEXT NOT NULL,
    seq INTEGER NOT NULL REFERENCES workflow_events(seq),
    PRIMARY KEY(workspace, session_id, seq)
) STRICT;
CREATE TABLE IF NOT EXISTS workflow_delivery_schema (version INTEGER PRIMARY KEY) STRICT;
"#;

pub(super) fn migrate_schema(connection: &Connection) -> Result<(), String> {
    // One-time legacy backfill, never repeated after acknowledgement or on reopen.
    // Import initializes an empty target first; logical imports do not execute event writers.
    connection.execute_batch("BEGIN IMMEDIATE;
        INSERT OR IGNORE INTO workflow_pending(workspace,session_id,seq)
        SELECT o.workspace,o.session_id,e.seq FROM workflow_origins o JOIN workflow_events e ON e.run_id=o.run_id
        WHERE NOT EXISTS(SELECT 1 FROM workflow_delivery_schema WHERE version=1);
        INSERT OR IGNORE INTO workflow_delivery_schema(version) VALUES(1);
        COMMIT;").map_err(|_| unavailable())?;
    for (table, column, definition) in [
        ("workflow_runs", "forked_from_run_id", "TEXT"),
        ("workflow_runs", "forked_from_event_seq", "INTEGER"),
        ("workflow_runs", "forked_from_step_id", "TEXT"),
        ("workflow_steps", "source_run_id", "TEXT"),
        ("workflow_steps", "source_step_id", "TEXT"),
        ("workflow_steps", "source_attempt", "INTEGER"),
    ] {
        if !column_exists(connection, table, column)? {
            connection
                .execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    (),
                )
                .map_err(|_| unavailable())?;
        }
    }
    Ok(())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|_| unavailable())?;
    let rows = statement
        .query_map((), |row| row.get::<String>(1))
        .map_err(|_| unavailable())?;
    for name in rows {
        if name.map_err(|_| unavailable())? == column {
            return Ok(true);
        }
    }
    Ok(false)
}
