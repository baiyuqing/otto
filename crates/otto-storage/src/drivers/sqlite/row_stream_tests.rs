//! Tests that rows stream from an open statement instead of being buffered.
use super::*;
use crate::SqlRows;

fn open_memory() -> Box<dyn SqlConnection> {
    SqliteDriver
        .open(Path::new(":memory:"), OpenOptions::default())
        .expect("open")
}

fn open_with_numbers(count: i64) -> Box<dyn SqlConnection> {
    let connection = open_memory();
    connection
        .execute_batch("CREATE TABLE numbers(value INTEGER NOT NULL)")
        .expect("create");
    for value in 1..=count {
        connection
            .execute(
                "INSERT INTO numbers(value) VALUES (?1)",
                Params(vec![Value::Integer(value)]),
            )
            .expect("insert");
    }
    connection
}

fn integer(rows: &mut Box<dyn SqlRows>) -> Option<i64> {
    rows.next()
        .expect("next")
        .map(|row| row.get::<i64>(0).expect("integer"))
}

#[test]
fn rows_stay_open_while_the_same_connection_writes() {
    let connection = open_with_numbers(3);
    connection
        .execute_batch("CREATE TABLE audit(value INTEGER)")
        .expect("create audit");
    let mut statement = connection
        .prepare("SELECT value FROM numbers ORDER BY value")
        .expect("prepare");
    let mut rows = statement.query(Params(Vec::new())).expect("query");
    assert_eq!(integer(&mut rows), Some(1));
    // A connection lock held across `next` calls deadlocks here: the
    // statements below need the connection while `rows` is mid-iteration.
    assert_eq!(
        connection
            .execute("INSERT INTO audit(value) VALUES (7)", Params(Vec::new()))
            .expect("insert while rows are open"),
        1
    );
    connection
        .execute("UPDATE audit SET value = 8", Params(Vec::new()))
        .expect("update while rows are open");
    assert_eq!(integer(&mut rows), Some(2));
    assert_eq!(integer(&mut rows), Some(3));
    assert_eq!(integer(&mut rows), None);
    assert_eq!(integer(&mut rows), None);
}

#[test]
fn rows_dropped_before_exhaustion_do_not_block_commit_or_close() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("rows.db");
    let connection = SqliteDriver
        .open(&path, OpenOptions::default())
        .expect("open");
    connection
        .execute_batch(
            "CREATE TABLE numbers(value INTEGER); INSERT INTO numbers VALUES (1),(2),(3)",
        )
        .expect("seed");
    connection.begin().expect("begin");
    connection
        .execute("INSERT INTO numbers(value) VALUES (4)", Params(Vec::new()))
        .expect("insert");
    let mut statement = connection
        .prepare("SELECT value FROM numbers")
        .expect("prepare");
    let mut abandoned = statement.query(Params(Vec::new())).expect("query");
    assert!(abandoned.next().expect("first row").is_some());
    drop(abandoned);
    connection.commit().expect("commit");

    let mut kept = statement.query(Params(Vec::new())).expect("query");
    assert!(kept.next().expect("first row").is_some());
    connection.close().expect("close with open rows");
    assert!(matches!(kept.next(), Err(Error::Closed)));
    drop(kept);
    drop(statement);

    let reopened = SqliteDriver
        .open(&path, OpenOptions::default())
        .expect("reopen");
    let mut statement = reopened
        .prepare("SELECT COUNT(*) FROM numbers")
        .expect("prepare");
    let mut rows = statement.query(Params(Vec::new())).expect("query");
    assert_eq!(integer(&mut rows), Some(4));
}

#[test]
fn a_step_error_surfaces_from_next() {
    let connection = open_memory();
    connection
        .execute_batch("CREATE TABLE numbers(value INTEGER); INSERT INTO numbers VALUES (1),(-9223372036854775808)")
        .expect("seed");
    let mut statement = connection
        .prepare("SELECT abs(value) FROM numbers ORDER BY rowid")
        .expect("prepare");
    // A driver that buffers every row fails this `expect`: the overflow
    // happens while stepping the second row, before `query` could return.
    let mut rows = statement
        .query(Params(Vec::new()))
        .expect("query succeeds before any step");
    assert_eq!(integer(&mut rows), Some(1));
    // abs(i64::MIN) overflows during the second step.
    assert!(rows.next().is_err());
    assert!(matches!(rows.next(), Ok(None)));
}

#[test]
fn a_constraint_error_keeps_its_category_when_it_comes_from_a_step() {
    let connection = open_memory();
    connection
        .execute_batch("CREATE TABLE ids(id INTEGER PRIMARY KEY); INSERT INTO ids VALUES (1)")
        .expect("seed");
    let mut statement = connection
        .prepare("INSERT INTO ids VALUES (1) RETURNING id")
        .expect("prepare");
    let mut rows = statement.query(Params(Vec::new())).expect("query");
    assert!(matches!(rows.next(), Err(Error::Constraint(_))));
}

#[test]
fn a_closed_connection_fails_open_rows() {
    let connection = open_with_numbers(2);
    let mut statement = connection
        .prepare("SELECT value FROM numbers")
        .expect("prepare");
    let mut rows = statement.query(Params(Vec::new())).expect("query");
    connection.close().expect("close");
    assert!(matches!(rows.next(), Err(Error::Closed)));
    assert!(matches!(
        statement.query(Params(Vec::new())),
        Err(Error::Closed)
    ));
}

#[test]
fn a_parameter_count_mismatch_is_an_error() {
    let connection = open_with_numbers(2);
    let mut statement = connection
        .prepare("SELECT value FROM numbers WHERE value > ?1")
        .expect("prepare");
    assert!(statement.query(Params(Vec::new())).is_err());
    let too_many = Params(vec![Value::Integer(0), Value::Integer(1)]);
    assert!(statement.query(too_many).is_err());
    let mut rows = statement
        .query(Params(vec![Value::Integer(1)]))
        .expect("query");
    assert_eq!(integer(&mut rows), Some(2));
}

#[test]
fn every_value_type_round_trips_through_bind_and_read() {
    let connection = open_memory();
    let mut statement = connection
        .prepare("SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7")
        .expect("prepare");
    let params = vec![
        Value::Null,
        Value::Integer(i64::MIN),
        Value::Real(1.5),
        Value::Text("héllo".into()),
        Value::Text(String::new()),
        Value::Blob(vec![0, 1, 255]),
        Value::Blob(Vec::new()),
    ];
    let mut rows = statement.query(Params(params.clone())).expect("query");
    let row = rows.next().expect("next").expect("row");
    assert_eq!(row.0, params);
}

#[test]
fn text_that_is_not_utf8_is_a_conversion_error() {
    let connection = open_memory();
    let mut statement = connection
        .prepare("SELECT CAST(X'FF' AS TEXT)")
        .expect("prepare");
    let mut rows = statement.query(Params(Vec::new())).expect("query");
    assert!(matches!(rows.next(), Err(Error::ConversionFailure(_))));
}
