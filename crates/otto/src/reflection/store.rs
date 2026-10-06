//! Domain records for reflection persistence.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Io,
    /// `PRAGMA user_version` did not match this build's schema.
    UnknownVersion,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io => formatter.write_str("reflection store unavailable"),
            Self::UnknownVersion => {
                formatter.write_str("reflection database has an unrecognized schema version")
            }
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The run asked the model and applied its output.
    Ok,
    /// There was nothing to reflect on; no model call was made.
    Noop,
    Failed,
    Canceled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Noop => "noop",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
        }
    }
}

/// One finished run, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRow {
    pub id: String,
    pub session_id: String,
    pub trigger: String,
    pub started_at: String,
    pub finished_at: String,
    pub status: Status,
    pub from_entry: String,
    pub to_entry: String,
    pub input_bytes: usize,
    pub truncated: bool,
    pub memories_proposed: usize,
    pub memories_dropped: usize,
    pub skills_written: usize,
    /// A short, non-sensitive note such as the failure category.
    pub detail: String,
}

/// A skill reflection owns: the name, the hash of the content it last wrote,
/// and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedSkill {
    pub name: String,
    pub hash: String,
    pub run_id: String,
    pub session_id: String,
    pub reason: String,
    pub created_at: String,
    pub updated_at: String,
}

/// One skill write to record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillWrite {
    pub name: String,
    pub hash: String,
    pub run_id: String,
    pub session_id: String,
    pub reason: String,
    pub at: String,
}

pub use crate::storage::backends::turso::reflection::Store;

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;
    use crate::storage::Connection;

    fn row(id: &str, status: Status, to_entry: &str) -> RunRow {
        RunRow {
            id: id.into(),
            session_id: "session-1".into(),
            trigger: "manual".into(),
            started_at: format!("2026-10-02T00:00:0{}Z", id.len() % 10),
            finished_at: "2026-10-02T00:01:00Z".into(),
            status,
            from_entry: String::new(),
            to_entry: to_entry.into(),
            input_bytes: 10,
            truncated: false,
            memories_proposed: 1,
            memories_dropped: 0,
            skills_written: 0,
            detail: String::new(),
        }
    }

    #[test]
    fn a_completed_run_advances_the_watermark() {
        let store = Store::open_in_memory().expect("open");
        assert_eq!(store.watermark("session-1").expect("read"), None);
        store.record(&row("a", Status::Ok, "aaaaaaaa")).expect("ok");
        assert_eq!(
            store.watermark("session-1").expect("read").as_deref(),
            Some("aaaaaaaa")
        );
        store
            .record(&row("bb", Status::Noop, "bbbbbbbb"))
            .expect("noop");
        assert_eq!(
            store.watermark("session-1").expect("read").as_deref(),
            Some("bbbbbbbb")
        );
    }

    #[test]
    fn failed_and_canceled_runs_leave_the_watermark_in_place() {
        let store = Store::open_in_memory().expect("open");
        store.record(&row("a", Status::Ok, "aaaaaaaa")).expect("ok");
        store
            .record(&row("bb", Status::Failed, "bbbbbbbb"))
            .expect("failed");
        store
            .record(&row("ccc", Status::Canceled, "cccccccc"))
            .expect("canceled");
        assert_eq!(
            store.watermark("session-1").expect("read").as_deref(),
            Some("aaaaaaaa")
        );
        assert_eq!(store.runs("session-1").expect("runs").len(), 3);
    }

    #[test]
    fn watermarks_are_per_session() {
        let store = Store::open_in_memory().expect("open");
        store.record(&row("a", Status::Ok, "aaaaaaaa")).expect("ok");
        assert_eq!(store.watermark("session-2").expect("read"), None);
    }

    #[test]
    fn an_unrecognized_schema_version_is_refused() {
        let directory = tempfile::tempdir().expect("directory");
        let path = directory.path().join("reflection.db");
        Connection::open(&path)
            .expect("open")
            .pragma_update("user_version", 999)
            .expect("set version");
        assert_eq!(
            Store::open(&path).expect_err("version"),
            Error::UnknownVersion
        );
    }

    #[test]
    fn a_file_store_reopens_with_its_rows_and_private_modes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested").join("reflection.db");
        Store::open(&path)
            .expect("open")
            .record(&row("a", Status::Ok, "aaaaaaaa"))
            .expect("record");
        let reopened = Store::open(&path).expect("reopen");
        assert_eq!(
            reopened.watermark("session-1").expect("read").as_deref(),
            Some("aaaaaaaa")
        );
        let mode =
            |path: &Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(path.parent().expect("parent")), 0o700);
        assert_eq!(mode(&path), 0o600);
    }

    fn write(name: &str, hash: &str) -> SkillWrite {
        SkillWrite {
            name: name.into(),
            hash: hash.into(),
            run_id: "run-1".into(),
            session_id: "session-1".into(),
            reason: "it worked".into(),
            at: "2026-10-02T00:00:00Z".into(),
        }
    }

    #[test]
    fn skill_ownership_tracks_the_latest_hash_and_every_version() {
        let store = Store::open_in_memory().expect("open");
        assert_eq!(store.generated("lint").expect("read"), None);
        store
            .record_skill_write(&write("lint", "h1"))
            .expect("first");
        store
            .record_skill_write(&write("lint", "h2"))
            .expect("second");
        store
            .record_skill_write(&write("fmt", "h9"))
            .expect("other");
        assert_eq!(
            store.generated("lint").expect("read").expect("row").hash,
            "h2"
        );
        assert_eq!(
            store.skill_versions("lint").expect("versions"),
            ["h1", "h2"]
        );
        assert_eq!(store.generated_count().expect("count"), 2);
        let names: Vec<_> = store
            .list_generated()
            .expect("list")
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        assert_eq!(names, ["fmt", "lint"]);
    }

    #[test]
    fn popping_a_version_restores_the_previous_hash_or_forgets_the_skill() {
        let store = Store::open_in_memory().expect("open");
        store
            .record_skill_write(&write("lint", "h1"))
            .expect("first");
        store
            .record_skill_write(&write("lint", "h2"))
            .expect("second");
        assert_eq!(
            store.pop_skill_version("lint", "t").expect("pop"),
            Some("h1".to_owned())
        );
        assert_eq!(
            store.generated("lint").expect("read").expect("row").hash,
            "h1"
        );
        assert_eq!(store.pop_skill_version("lint", "t").expect("pop"), None);
        assert_eq!(store.generated("lint").expect("read"), None);
        assert!(store.skill_versions("lint").expect("versions").is_empty());
    }

    #[test]
    fn a_version_one_database_is_migrated_and_keeps_its_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("reflection.db");
        {
            let connection = Connection::open(&path).expect("open");
            connection
                .execute_batch(
                    "CREATE TABLE runs (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, \
                     trigger TEXT NOT NULL, started_at TEXT NOT NULL, finished_at TEXT NOT NULL, \
                     status TEXT NOT NULL, from_entry TEXT NOT NULL, to_entry TEXT NOT NULL, \
                     input_bytes INTEGER NOT NULL, truncated INTEGER NOT NULL, \
                     memories_proposed INTEGER NOT NULL, memories_dropped INTEGER NOT NULL, \
                     detail TEXT NOT NULL) STRICT; \
                     CREATE TABLE watermarks (session_id TEXT PRIMARY KEY, entry_id TEXT NOT NULL, \
                     updated_at TEXT NOT NULL) STRICT; \
                     INSERT INTO watermarks VALUES ('session-1', 'aaaaaaaa', 't');",
                )
                .expect("v1 schema");
            connection
                .pragma_update("user_version", 1)
                .expect("version");
        }
        let store = Store::open(&path).expect("migrate");
        assert_eq!(
            store.watermark("session-1").expect("read").as_deref(),
            Some("aaaaaaaa")
        );
        store
            .record_skill_write(&write("lint", "h1"))
            .expect("new tables");
        store
            .record(&row("a", Status::Ok, "bbbbbbbb"))
            .expect("runs column");
    }

    #[test]
    fn the_last_automatic_run_ignores_manual_and_requested_ones() {
        let store = Store::open_in_memory().expect("open");
        assert_eq!(store.last_auto_started("session-1").expect("read"), None);
        for (id, trigger) in [("a", "manual"), ("aa", "requested")] {
            let mut asked = row(id, Status::Ok, "aaaaaaaa");
            asked.trigger = trigger.into();
            asked.started_at = "2026-10-02T05:00:00Z".into();
            store.record(&asked).expect(trigger);
        }
        assert_eq!(store.last_auto_started("session-1").expect("read"), None);
        for (id, trigger, started) in [
            ("bb", "on_compaction", "2026-10-02T01:00:00Z"),
            ("ccc", "on_exit", "2026-10-02T03:00:00Z"),
        ] {
            let mut automatic = row(id, Status::Failed, "");
            automatic.trigger = trigger.into();
            automatic.started_at = started.into();
            store.record(&automatic).expect("automatic");
        }
        assert_eq!(
            store
                .last_auto_started("session-1")
                .expect("read")
                .as_deref(),
            Some("2026-10-02T03:00:00Z")
        );
        assert_eq!(store.last_auto_started("session-2").expect("read"), None);
    }
}
