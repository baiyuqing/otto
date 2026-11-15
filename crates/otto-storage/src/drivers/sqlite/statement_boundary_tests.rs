use super::*;

#[test]
fn boundary_recognizer_work_is_bounded_for_dense_literals_comments_and_triggers() {
    let dense = ";".repeat(32 * 1024);
    let trigger_body = "SELECT 1;".repeat(4096);
    for sql in [
        format!("SELECT '{dense}'"),
        format!("SELECT 1 /* {dense} */"),
        format!("CREATE TRIGGER dense AFTER INSERT ON source BEGIN {trigger_body} END;"),
    ] {
        let mut work_bytes = 0;
        let error = statement_boundary::validate_with_work(&sql, &mut work_bytes).unwrap_err();
        assert!(work_bytes <= statement_boundary::MAX_BOUNDARY_WORK_BYTES);
        // Regression must actually exercise the work bound, not a parse error.
        assert!(work_bytes > statement_boundary::MAX_BOUNDARY_WORK_BYTES / 2);
        assert!(matches!(error, Error::ValidationLimit(_)));
        let message = error.to_string();
        assert!(
            message.contains("boundary validation limit exceeded"),
            "{message}"
        );
        assert!(
            message.contains("bind large values as parameters"),
            "{message}"
        );
    }
    let mut work_bytes = 1;
    assert!(
        statement_boundary::validate_with_work(&" ".repeat(8 * 1024 * 1024 + 1), &mut work_bytes)
            .is_err()
    );
    assert_eq!(work_bytes, 0, "oversized input never enters the recognizer");
}

#[test]
fn normal_dense_literals_and_trigger_bodies_remain_supported() {
    let dense = ";".repeat(256);
    let connection = SqliteDriver
        .open(Path::new(":memory:"), OpenOptions::default())
        .unwrap();
    let mut statement = connection
        .prepare(&format!("SELECT '{dense}' /* {dense} */;"))
        .unwrap();
    let mut rows = statement.query(Params(vec![])).unwrap();
    assert_eq!(rows.next().unwrap().unwrap().0, vec![Value::Text(dense)]);
    drop(rows);
    connection
        .execute_batch("CREATE TABLE source(value); CREATE TABLE audit(value);")
        .unwrap();
    let body = "INSERT INTO audit VALUES (';');".repeat(64);
    connection
        .execute(
            &format!("CREATE TRIGGER dense AFTER INSERT ON source BEGIN {body} END;"),
            Params(vec![]),
        )
        .unwrap();
    connection
        .execute("INSERT INTO source VALUES (1)", Params(vec![]))
        .unwrap();
    let mut statement = connection.prepare("SELECT count(*) FROM audit").unwrap();
    let mut rows = statement.query(Params(vec![])).unwrap();
    assert_eq!(rows.next().unwrap().unwrap().0, vec![Value::Integer(64)]);
}

#[test]
fn work_limit_rejection_does_not_touch_live_connection() {
    let connection = SqliteConnection {
        state: Arc::new(ConnectionState {
            connection: Mutex::new(rusqlite::Connection::open_in_memory().unwrap()),
            poisoned: AtomicBool::new(false),
        }),
    };
    connection
        .execute_batch(
            "PRAGMA foreign_keys=ON; CREATE TABLE intact(value); INSERT INTO intact VALUES (1);",
        )
        .unwrap();
    let sql = format!(
        "PRAGMA foreign_keys=OFF /* {} */; DELETE FROM intact",
        ";".repeat(32 * 1024)
    );
    for api in 0..3 {
        let result = match api {
            0 => connection.execute(&sql, Params(vec![])).map(|_| ()),
            1 => connection.prepare(&sql).map(|_| ()),
            _ => row_stream::query(&connection.state, &sql, Params(vec![])).map(|_| ()),
        };
        let error = result.unwrap_err();
        assert!(matches!(error, Error::ValidationLimit(_)));
        assert!(
            error
                .to_string()
                .contains("bind large values as parameters")
        );
        let inner = connection.guard().unwrap();
        assert_eq!(
            inner
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            inner
                .query_row("SELECT count(*) FROM intact", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}

#[test]
fn regular_apis_reject_scripts_before_effects_but_allow_sql_literals_and_comments() {
    let connection = SqliteDriver
        .open(Path::new(":memory:"), OpenOptions::default())
        .unwrap();
    connection
        .execute_batch("CREATE TABLE intact(value TEXT); INSERT INTO intact VALUES ('seed');")
        .unwrap();
    for sql in [
        "DELETE FROM intact; DROP TABLE intact",
        "DELETE FROM intact; SELECT 1",
        "DELETE FROM intact; -- tail\n SELECT 2",
        "DELETE FROM intact\0; DROP TABLE intact",
        "SELECT 1; SELECT 2",
        "; -- no statement",
    ] {
        assert!(connection.execute(sql, Params(vec![])).is_err(), "{sql:?}");
        assert!(connection.prepare(sql).is_err(), "{sql:?}");
    }
    let mut statement = connection
        .prepare("SELECT value, ';--/*雪*/' FROM intact; -- trailing\n /* allowed */ ;")
        .unwrap();
    let mut rows = statement.query(Params(vec![])).unwrap();
    assert_eq!(
        rows.next().unwrap().unwrap().0[0],
        Value::Text("seed".into())
    );
    assert!(rows.next().unwrap().is_none());
}

#[test]
fn rejected_sql_never_changes_security_pragmas_or_rows() {
    let inner = rusqlite::Connection::open_in_memory().unwrap();
    inner
        .execute_batch(
            "PRAGMA foreign_keys=ON; CREATE TABLE parent(id INTEGER PRIMARY KEY); \
         CREATE TABLE child(parent_id INTEGER REFERENCES parent(id)); \
         INSERT INTO parent VALUES (1); INSERT INTO child VALUES (1);",
        )
        .unwrap();
    let connection = SqliteConnection {
        state: Arc::new(ConnectionState {
            connection: Mutex::new(inner),
            poisoned: AtomicBool::new(false),
        }),
    };
    for sql in [
        "SELECT 1; PRAGMA foreign_keys=OFF",
        "PRAGMA foreign_keys=OFF; SELECT 1",
        "PRAGMA foreign_keys=OFF; this is malformed",
        "this is malformed; PRAGMA foreign_keys=OFF",
        "PRAGMA foreign_keys=OFF; /* tail */ SELECT (",
        "PRAGMA foreign_keys=OFF; CREATE TRIGGER malformed BEGIN SELECT 1;",
        "DELETE FROM child; PRAGMA foreign_keys=OFF",
        "PRAGMA foreign_keys=OFF; DELETE FROM child",
        "DELETE FROM child; this is malformed",
        "PRAGMA foreign_keys=OFF\0; SELECT 1",
    ] {
        for api in 0..3 {
            let rejected = match api {
                0 => connection.execute(sql, Params(vec![])).is_err(),
                1 => connection.prepare(sql).is_err(),
                // Exercise the query-time guard directly, without prepare's
                // earlier validation masking a missing streaming-path check.
                _ => row_stream::query(&connection.state, sql, Params(vec![])).is_err(),
            };
            assert!(rejected, "api={api}, SQL={sql:?}");
            let inner = connection.guard().unwrap();
            assert_eq!(
                inner
                    .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                1,
                "api={api}, SQL={sql:?}"
            );
            assert_eq!(
                inner
                    .query_row("SELECT count(*) FROM child WHERE parent_id=1", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap(),
                1,
                "api={api}, SQL={sql:?}"
            );
            assert_eq!(
                inner
                    .query_row("SELECT count(*) FROM parent WHERE id=1", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert!(
                inner.execute("INSERT INTO child VALUES (999)", []).is_err(),
                "foreign keys must still enforce constraints"
            );
        }
    }
}

#[test]
fn sqlite_boundaries_preserve_schema_references_and_trigger_bodies() {
    let connection = SqliteDriver
        .open(Path::new(":memory:"), OpenOptions::default())
        .unwrap();
    connection
        .execute("CREATE TABLE source(\"semi;colon\" TEXT)", Params(vec![]))
        .unwrap();
    connection
        .execute("CREATE TABLE audit(value TEXT)", Params(vec![]))
        .unwrap();
    connection
        .execute(
            "CREATE TRIGGER record_insert AFTER INSERT ON source BEGIN \
         INSERT INTO audit VALUES (new.\"semi;colon\"); \
         INSERT INTO audit VALUES ('trigger;value'); END; /* trailing ; */ ; -- done",
            Params(vec![]),
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO source VALUES ('literal;value')",
            Params(vec![]),
        )
        .unwrap();
    for sql in [
        "; /* leading ; */ SELECT \"semi;colon\" FROM source; -- trailing ;\n ;",
        "SELECT \"semi;colon\" FROM source /* unterminated trailing comment ;",
        "SELECT \"semi;colon\" FROM source",
    ] {
        let mut statement = connection.prepare(sql).unwrap();
        let mut rows = statement.query(Params(vec![])).unwrap();
        assert_eq!(
            rows.next().unwrap().unwrap().0,
            vec![Value::Text("literal;value".into())]
        );
        assert!(rows.next().unwrap().is_none());
    }
    let mut statement = connection.prepare("SELECT count(*) FROM audit").unwrap();
    let mut rows = statement.query(Params(vec![])).unwrap();
    assert_eq!(rows.next().unwrap().unwrap().0, vec![Value::Integer(2)]);
    assert!(connection.prepare("SELECT missing FROM source").is_err());
    assert!(connection.prepare("SELECT (").is_err());
}

#[test]
fn hostile_parameters_round_trip_without_changing_table_integrity() {
    let connection = SqliteDriver
        .open(Path::new(":memory:"), OpenOptions::default())
        .unwrap();
    connection
        .execute_batch("CREATE TABLE intact(value TEXT NOT NULL) STRICT;")
        .unwrap();
    let payload = "'\"; DROP TABLE intact; -- /* comment */ 雪 λ\0tail";
    connection
        .execute(
            "INSERT INTO intact VALUES (?)",
            Params(vec![Value::Text(payload.into())]),
        )
        .unwrap();
    let mut statement = connection
        .prepare("SELECT value FROM intact WHERE value=?")
        .unwrap();
    let mut rows = statement
        .query(Params(vec![Value::Text(payload.into())]))
        .unwrap();
    assert_eq!(
        rows.next().unwrap().unwrap().0,
        vec![Value::Text(payload.into())]
    );
    assert!(rows.next().unwrap().is_none());
    connection
        .execute("INSERT INTO intact VALUES ('still intact')", Params(vec![]))
        .unwrap();
}
