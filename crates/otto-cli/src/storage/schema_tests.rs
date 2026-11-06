use super::*;

#[test]
fn altered_legacy_schemas_never_publish_a_destination() {
    for alteration in [
        "ALTER TABLE usage_events ADD COLUMN surprise TEXT",
        "ALTER TABLE usage_events ADD COLUMN surprise TEXT GENERATED ALWAYS AS (lower(workspace)) VIRTUAL",
        "CREATE INDEX surprise ON usage_events(lower(workspace))",
        "CREATE INDEX surprise ON usage_events(workspace)",
        "CREATE TABLE surprise(value TEXT)",
        "CREATE TRIGGER surprise AFTER INSERT ON usage_events BEGIN DELETE FROM usage_events; END",
        "DROP INDEX usage_events_model",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.db");
        let destination = directory.path().join("destination.db");
        let output = Command::new("python3").args(["-I", "-c", r#"
import sqlite3,sys
c=sqlite3.connect(sys.argv[1])
c.executescript('''
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
c.executescript(sys.argv[2]); c.commit(); c.close()
"#]).arg(&source).arg(alteration).output().unwrap();
        assert!(output.status.success());
        let before = std::fs::read(&source).unwrap();
        assert!(migrate(&source, &destination).is_err(), "{alteration}");
        assert!(!destination.exists(), "{alteration}");
        assert_eq!(std::fs::read(&source).unwrap(), before);
    }
}
