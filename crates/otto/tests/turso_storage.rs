use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use otto::storage::{Connection, Value, params};
use tempfile::tempdir;

fn child(mode: &str, path: &std::path::Path) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "subprocess_helper", "--ignored", "--nocapture"])
        .env("OTTO_TURSO_TEST_MODE", mode)
        .env("OTTO_TURSO_TEST_DB", path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[test]
fn persists_and_reopens_with_typed_parameters() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("storage.db");
    {
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE values_table (n INTEGER, s TEXT, b BLOB, nullable TEXT)")
            .unwrap();
        db.execute(
            "INSERT INTO values_table VALUES (?, ?, ?, ?)",
            params![7_i64, "hello", vec![0, 1, 255], Option::<String>::None],
        )
        .unwrap();
        let values = db
            .query_row("SELECT n, s, b, nullable FROM values_table", (), |row| {
                Ok((
                    row.get_value(0)?,
                    row.get_value(1)?,
                    row.get_value(2)?,
                    row.get_value(3)?,
                ))
            })
            .unwrap();
        assert_eq!(
            values,
            (
                Value::Integer(7),
                Value::Text("hello".into()),
                Value::Blob(vec![0, 1, 255]),
                Value::Null
            )
        );
    }
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM values_table", (), |r| r.get_value(0))
            .unwrap(),
        Value::Integer(1)
    );
}

#[test]
fn transaction_drop_and_statement_error_roll_back() {
    let dir = tempdir().unwrap();
    let db = Connection::open(dir.path().join("storage.db")).unwrap();
    db.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY, value TEXT UNIQUE)")
        .unwrap();
    {
        let mut db = db;
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO items VALUES (1, 'drop')", ())
            .unwrap();
        drop(tx);
        assert!(
            db.execute("INSERT INTO items VALUES (2, 'kept')", ())
                .is_ok()
        );
        let tx = db.transaction().unwrap();
        tx.execute("INSERT INTO items VALUES (3, 'rolled back')", ())
            .unwrap();
        assert!(
            tx.execute("INSERT INTO items VALUES (4, 'kept')", ())
                .is_err()
        );
        drop(tx);
        assert_eq!(
            db.query_row("SELECT count(*) FROM items", (), |r| r.get_value(0))
                .unwrap(),
            Value::Integer(1)
        );
    }
}

#[test]
fn rejects_unique_constraint_violation() {
    let dir = tempdir().unwrap();
    let db = Connection::open(dir.path().join("storage.db")).unwrap();
    db.execute_batch("CREATE TABLE items (id INTEGER PRIMARY KEY)")
        .unwrap();
    db.execute("INSERT INTO items VALUES (?)", params![1_i64])
        .unwrap();
    assert!(
        db.execute("INSERT INTO items VALUES (?)", params![1_i64])
            .is_err()
    );
}

#[test]
fn synchronous_calls_work_inside_current_thread_runtime() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("storage.db");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    runtime.block_on(async {
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE items (value INTEGER)")
            .unwrap();
        db.execute("INSERT INTO items VALUES (?)", params![42_i64])
            .unwrap();
        assert_eq!(
            db.query_row("SELECT value FROM items", (), |r| r.get_value(0))
                .unwrap(),
            Value::Integer(42)
        );
    });
}

#[test]
fn independent_processes_observe_contention_then_shared_writes() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("storage.db");
    let mut db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE items (value TEXT)").unwrap();
    db.busy_timeout(Duration::from_millis(150)).unwrap();
    let tx = db.transaction().unwrap();
    let status = child("contend", &path).wait().unwrap();
    assert!(status.success());
    drop(tx);
    assert!(child("write", &path).wait().unwrap().success());
    assert_eq!(
        db.query_row("SELECT count(*) FROM items", (), |r| r.get_value(0))
            .unwrap(),
        Value::Integer(1)
    );
}

#[test]
fn close_does_not_wait_for_another_connection_read_transaction() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("storage.db");
    let mut writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("CREATE TABLE items (value INTEGER); INSERT INTO items VALUES (1)")
        .unwrap();
    let reader = Connection::open(&path).unwrap();
    reader.execute_batch("BEGIN").unwrap();
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM items", (), |row| row.get_value(0))
            .unwrap(),
        Value::Integer(1)
    );

    let transaction = writer.transaction().unwrap();
    transaction
        .execute("INSERT INTO items VALUES (2)", ())
        .unwrap();
    transaction.commit().unwrap();
    writer.close().unwrap();

    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM items", (), |row| row.get_value(0))
            .unwrap(),
        Value::Integer(1),
        "an open read transaction retains its snapshot"
    );
    reader.execute_batch("COMMIT").unwrap();
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM items", (), |row| row.get_value(0))
            .unwrap(),
        Value::Integer(2),
        "the committed write remains visible after the reader releases its snapshot"
    );
    reader.close().unwrap();
}

#[test]
fn killed_process_recovers_committed_rows_and_discards_open_transaction() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("storage.db");
    let ready = dir.path().join("ready");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE items (value TEXT)").unwrap();
    db.execute("INSERT INTO items VALUES ('committed')", ())
        .unwrap();
    drop(db);
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "subprocess_helper", "--ignored", "--nocapture"])
        .env("OTTO_TURSO_TEST_MODE", "crash")
        .env("OTTO_TURSO_TEST_DB", &path)
        .env("OTTO_TURSO_TEST_READY", &ready)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(ready.exists(), "child did not reach uncommitted write");
    child.kill().unwrap();
    let _ = child.wait();
    let db = Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM items WHERE value = 'committed'",
            (),
            |r| r.get_value(0)
        )
        .unwrap(),
        Value::Integer(1)
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM items WHERE value = 'uncommitted'",
            (),
            |r| r.get_value(0)
        )
        .unwrap(),
        Value::Integer(0)
    );
}

#[test]
#[ignore]
fn subprocess_helper() {
    let Some(mode) = std::env::var_os("OTTO_TURSO_TEST_MODE") else {
        return;
    };
    let path = std::env::var_os("OTTO_TURSO_TEST_DB").unwrap();
    let db = Connection::open(path).unwrap();
    match mode.to_str().unwrap() {
        "contend" => assert!(
            db.execute("INSERT INTO items VALUES ('contender')", ())
                .is_err()
        ),
        "write" => {
            db.execute("INSERT INTO items VALUES ('child')", ())
                .unwrap();
        }
        "crash" => {
            let mut db = db;
            let tx = db.transaction().unwrap();
            tx.execute("INSERT INTO items VALUES ('uncommitted')", ())
                .unwrap();
            std::fs::write(std::env::var_os("OTTO_TURSO_TEST_READY").unwrap(), "ready").unwrap();
            thread::sleep(Duration::from_secs(60));
        }
        other => panic!("unknown helper mode: {other}"),
    }
}
