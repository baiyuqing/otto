use std::process::{Command, Output};

use tempfile::tempdir;

fn seed_usage(source: &std::path::Path) {
    let script = r#"
import sqlite3, sys
db = sqlite3.connect(sys.argv[1])
db.executescript('''
CREATE TABLE usage_events (
 id INTEGER PRIMARY KEY, occurred_at TEXT NOT NULL, workspace TEXT NOT NULL,
 session_id TEXT NOT NULL, task_id TEXT NOT NULL, provider TEXT NOT NULL,
 profile TEXT NOT NULL, model TEXT NOT NULL,
 kind TEXT NOT NULL CHECK (kind IN ('provider','compaction')),
 input_tokens INTEGER NOT NULL CHECK (input_tokens >= 0),
 output_tokens INTEGER NOT NULL CHECK (output_tokens >= 0),
 cached_input_tokens INTEGER NOT NULL CHECK (cached_input_tokens >= 0 AND cached_input_tokens <= input_tokens),
 usage_present INTEGER NOT NULL CHECK (usage_present IN (0, 1)),
 CHECK (usage_present = 1 OR (input_tokens = 0 AND output_tokens = 0 AND cached_input_tokens = 0))
) STRICT;
CREATE INDEX usage_events_session ON usage_events(session_id, occurred_at);
CREATE INDEX usage_events_model ON usage_events(provider, model, occurred_at);
''')
db.executemany('INSERT INTO usage_events VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)', [
 (1, '2026-10-01T00:00:00Z', '\'\"; DROP TABLE usage_events; -- /* 雪 */ nul\x00workspace', 'session-雪', 'task-λ', 'openai-compatible', 'default', 'model-雪', 'provider', 13, 5, 3, 1),
 (2, '2026-10-01T00:01:00Z', 'nul\x00workspace', 'session-雪', '', 'chatgpt', 'default', 'model-雪', 'provider', 8, 2, 0, 1),
])
db.commit()
db.close()
"#;
    let output = Command::new("python3")
        .args(["-I", "-c", script])
        .arg(source)
        .output()
        .expect("Python 3 is required for offline migration fixtures");
    assert!(
        output.status.success(),
        "fixture creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn migrate(source: &std::path::Path, destination: &std::path::Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_otto"))
        .args(["storage", "migrate"])
        .arg(source)
        .arg(destination)
        .output()
        .unwrap()
}

#[test]
fn migrates_usage_data_without_modifying_legacy_file() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("legacy.db");
    let destination = dir.path().join("otto.db");
    seed_usage(&source);
    let before = std::fs::read(&source).unwrap();

    let output = migrate(&source, &destination);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(&source).unwrap(),
        before,
        "migration must leave the source byte-for-byte unchanged"
    );

    let connection = otto::storage::Connection::open_read_only(&destination).unwrap();
    let workspace: String = connection
        .query_row(
            "SELECT workspace FROM usage_events WHERE id=?",
            [1i64],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        workspace,
        "'\"; DROP TABLE usage_events; -- /* 雪 */ nul\0workspace"
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM usage_events", (), |row| row
                .get::<i64>(0))
            .unwrap(),
        2
    );

    let output = Command::new(env!("CARGO_BIN_EXE_otto"))
        .args(["storage", "window-peaks"])
        .arg(&destination)
        .arg("session-雪")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!([
            {"window": "main", "peakInput": 8, "requests": 1},
            {"window": "task-λ", "peakInput": 13, "requests": 1}
        ])
    );
}

#[test]
fn refuses_existing_destination_without_changing_either_file() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("legacy.db");
    let destination = dir.path().join("existing.db");
    seed_usage(&source);
    std::fs::write(&destination, b"keep this destination").unwrap();
    let source_before = std::fs::read(&source).unwrap();
    let destination_before = std::fs::read(&destination).unwrap();

    let output = migrate(&source, &destination);
    assert!(!output.status.success());
    assert_eq!(std::fs::read(&source).unwrap(), source_before);
    assert_eq!(std::fs::read(&destination).unwrap(), destination_before);
}

#[test]
fn malformed_or_unrecognized_source_fails_without_leaving_destination() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("unknown.db");
    let destination = dir.path().join("otto.db");
    let output = Command::new("python3")
        .args(["-I", "-c", "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.execute('CREATE TABLE mystery (id INTEGER)'); c.commit(); c.close()"])
        .arg(&source)
        .output()
        .unwrap();
    assert!(output.status.success());
    let before = std::fs::read(&source).unwrap();

    let output = migrate(&source, &destination);
    assert!(!output.status.success());
    assert!(!destination.exists());
    assert_eq!(std::fs::read(&source).unwrap(), before);
}
