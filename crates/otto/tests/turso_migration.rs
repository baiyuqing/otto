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
 (1, '2026-10-01T00:00:00Z', 'nul\x00workspace', 'session-雪', 'task-λ', 'openai-compatible', 'default', 'model-雪', 'provider', 13, 5, 3, 1),
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

#[test]
fn migrates_v1_memory_identity_record_and_rebuilds_fts() {
    let dir = tempdir().unwrap();
    let source = dir.path().join("memory-v1.db");
    let destination = dir.path().join("memory-turso.db");
    let output = Command::new("python3")
        .args(["-I", "-c", "import sqlite3,sys; c=sqlite3.connect(sys.argv[1]); c.executescript(sys.stdin.read()); c.commit(); c.close()"])
        .arg(&source)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = output;
    use std::io::Write;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(include_str!("fixtures/memory-v1.sql").as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "fixture creation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let before = std::fs::read(&source).unwrap();

    let output = migrate(&source, &destination);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&source).unwrap(), before);

    let store =
        otto::memory::turso::Store::open(&destination, otto::memory::turso::Options::default())
            .unwrap();
    let identity = store.identity().unwrap();
    assert_eq!(identity.database_id, "0123456789abcdef0123456789abcdef");
    assert_eq!(identity.user_scope.id, "fedcba9876543210fedcba9876543210");
    assert_eq!(identity.generation, 7);

    let result = store
        .retrieve(&otto::memory::RetrievalRequest {
            query: "quiet editor mode".into(),
            scopes: vec![identity.user_scope],
            kinds: vec![],
            labels: vec![],
            include_expired: false,
            include_baseline: false,
            limit: 10,
            token_budget: 100,
            cursor: String::new(),
            now: chrono::Utc::now(),
            estimate_tokens: None,
        })
        .unwrap();
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].record.id, "memory-record-1");
    assert_eq!(
        result.matches[0].record.text,
        "Use the quiet editor mode for focused work"
    );
}
