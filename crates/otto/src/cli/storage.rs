//! Offline legacy import and read-only usage inspection.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::storage::{Connection, Value, params_from_iter};

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
enum Export {
    Header {
        family: String,
        version: i64,
        schema: Vec<String>,
        counts: BTreeMap<String, i64>,
    },
    Row {
        table: String,
        values: Vec<Cell>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
enum Cell {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl From<Cell> for Value {
    fn from(value: Cell) -> Self {
        match value {
            Cell::Null => Self::Null,
            Cell::Integer(value) => Self::Integer(value),
            Cell::Real(value) => Self::Real(value),
            Cell::Text(value) => Self::Text(value),
            Cell::Blob(value) => Self::Blob(value),
        }
    }
}

pub fn run(args: &[String], stdout: &mut dyn Write, stderr: &mut dyn Write) -> i32 {
    let result = match args {
        [command, source, destination] if command == "migrate" => {
            migrate(Path::new(source), Path::new(destination))
        }
        [command, database, session] if command == "window-peaks" => {
            window_peaks(Path::new(database), session, stdout)
        }
        _ => {
            let _ = writeln!(
                stderr,
                "usage: otto storage migrate <legacy.db> <new.db> | window-peaks <usage.db> <session-id>"
            );
            return 2;
        }
    };
    match result {
        Ok(()) => 0,
        Err(()) => {
            let _ = writeln!(
                stderr,
                "storage operation failed; migration requires stopped Otto processes, Python 3, a recognized legacy database, and a new destination path"
            );
            1
        }
    }
}

fn window_peaks(path: &Path, session: &str, stdout: &mut dyn Write) -> Result<(), ()> {
    let connection = Connection::open_read_only(path).map_err(|_| ())?;
    let mut statement = connection
        .prepare(
            "SELECT task_id, MAX(input_tokens), COUNT(*) FROM usage_events \
         WHERE session_id=? AND usage_present=1 GROUP BY task_id ORDER BY (task_id<>''),task_id",
        )
        .map_err(|_| ())?;
    let rows = statement
        .query_map([session], |row| {
            Ok((
                row.get::<String>(0)?,
                row.get::<i64>(1)?,
                row.get::<i64>(2)?,
            ))
        })
        .map_err(|_| ())?;
    let mut peaks = Vec::new();
    for row in rows {
        let (task, peak, requests) = row.map_err(|_| ())?;
        peaks.push(serde_json::json!({"window": if task.is_empty() { "main" } else { &task }, "peakInput": peak, "requests": requests}));
    }
    serde_json::to_writer(&mut *stdout, &peaks).map_err(|_| ())?;
    writeln!(stdout).map_err(|_| ())
}

fn import(reader: impl BufRead, connection: &mut Connection) -> Result<(), ()> {
    let mut lines = reader.lines();
    let first = lines.next().ok_or(())?.map_err(|_| ())?;
    let Export::Header {
        family,
        version,
        schema,
        counts,
    } = serde_json::from_str(&first).map_err(|_| ())?
    else {
        return Err(());
    };
    let allowed: &[&str] = match family.as_str() {
        "memory" if version == 1 => &[
            "memory_meta",
            "memory_records",
            "memory_candidates",
            "memory_observations",
            "memory_records_fts",
        ],
        "usage" if version == 0 => &["usage_events"],
        "tasks" if version == 1 => &["tasks"],
        "workflow" if version == 0 => &[
            "workflow_runs",
            "workflow_steps",
            "workflow_attempts",
            "workflow_requests",
            "workflow_events",
        ],
        "reflection" if matches!(version, 1 | 2) => {
            &["runs", "watermarks", "generated_skills", "skill_versions"]
        }
        "skill-checks" if version == 1 => &["skill_checks"],
        _ => return Err(()),
    };
    if counts.is_empty()
        || counts
            .iter()
            .any(|(table, count)| !allowed.contains(&table.as_str()) || *count < 0)
    {
        return Err(());
    }
    let transaction = connection.transaction().map_err(|_| ())?;
    if family == "memory" {
        for sql in crate::memory::turso::schema::SCHEMA_STATEMENTS {
            transaction.execute_batch(sql).map_err(|_| ())?;
        }
    } else {
        for sql in schema {
            // The exporter emits individual CREATE TABLE/INDEX statements.
            // execute prepares one statement, never a script from the input.
            if !sql.starts_with("CREATE TABLE ")
                && !sql.starts_with("CREATE INDEX ")
                && !sql.starts_with("CREATE UNIQUE INDEX ")
            {
                return Err(());
            }
            transaction.execute(&sql, ()).map_err(|_| ())?;
        }
        transaction
            .pragma_update("user_version", version)
            .map_err(|_| ())?;
    }
    for line in lines {
        let line = line.map_err(|_| ())?;
        let Export::Row { table, values } = serde_json::from_str(&line).map_err(|_| ())? else {
            return Err(());
        };
        if !counts.contains_key(&table) || !allowed.contains(&table.as_str()) || values.is_empty() {
            return Err(());
        }
        let placeholders = vec!["?"; values.len()].join(",");
        transaction
            .execute(
                &format!("INSERT INTO {table} VALUES({placeholders})"),
                params_from_iter(values.into_iter().map(Value::from)),
            )
            .map_err(|_| ())?;
    }
    for (table, expected) in counts {
        let actual: i64 = transaction
            .query_row(&format!("SELECT count(*) FROM {table}"), (), |row| {
                row.get(0)
            })
            .map_err(|_| ())?;
        if actual != expected {
            return Err(());
        }
    }
    if family == "memory" {
        crate::memory::turso::schema::finalize_legacy_import(&transaction).map_err(|_| ())?;
    }
    transaction.commit().map_err(|_| ())
}

fn migrate(source: &Path, destination: &Path) -> Result<(), ()> {
    if !source.is_file() || destination.exists() {
        return Err(());
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let staging = tempfile::Builder::new()
        .prefix(".otto-turso-migrate-")
        .tempfile_in(parent)
        .map_err(|_| ())?;
    let mut child = Command::new("python3")
        .args([
            "-I",
            "-c",
            include_str!("../../../../scripts/export-legacy-storage.py"),
        ])
        .arg(source)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let outcome = (|| {
        let mut connection = Connection::open(staging.path()).map_err(|_| ())?;
        import(
            BufReader::new(child.stdout.take().ok_or(())?),
            &mut connection,
        )?;
        if !child.wait().map_err(|_| ())?.success() {
            return Err(());
        }
        connection.checkpoint().map_err(|_| ())?;
        connection.close().map_err(|_| ())?;
        staging.as_file().sync_all().map_err(|_| ())?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = child.kill();
        let _ = child.wait();
    }
    // Turso creates private WAL coordination files next to the staging DB.
    for suffix in ["-wal", "-shm", ".tshm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", staging.path().display()));
    }
    outcome?;
    staging.persist_noclobber(destination).map_err(|_| ())?;
    std::fs::File::open(parent)
        .and_then(|dir| dir.sync_all())
        .map_err(|_| ())
}
