//! Streaming query results over a raw `sqlite3_stmt`.
//!
//! `rusqlite::Statement` borrows its `Connection`, so it cannot be returned as
//! a `Box<dyn SqlRows>` that outlives the call. [`SqliteRows`] instead owns the
//! prepared statement and an `Arc` of the shared [`ConnectionState`].
//!
//! Ownership: the statement is created by [`query`] and finalized by `next`
//! (on completion or error) or by `Drop`, whichever happens first. The `Arc`
//! field is dropped after `Drop::drop` ran, so the database handle is closed
//! only after the last statement on it is finalized.
//! Concurrency: the statement is touched only while holding the connection
//! mutex, one `sqlite3_step` per `next` call. Other statements run on the
//! connection between two `next` calls without deadlock.
//! Errors: the extended result code and message are read from the connection
//! while the lock is held, then mapped through `map_error`.

#![allow(unsafe_code)]

use super::{SharedState, check, lock, map_error};
use crate::storage::{Error, Params, Result, Row, SqlRows, Value};
use rusqlite::ffi;
use std::ffi::{CStr, c_int, c_void};
use std::sync::MutexGuard;

type Locked<'a> = MutexGuard<'a, rusqlite::Connection>;

/// Prepares `sql`, binds `params` and returns rows that own the statement.
pub(super) fn query(state: &SharedState, sql: &str, params: Params) -> Result<Box<dyn SqlRows>> {
    check(&state.poisoned)?;
    let connection = lock(state)?;
    let statement = prepare_bound(&connection, sql, &params)?;
    drop(connection);
    Ok(Box::new(SqliteRows {
        statement,
        state: state.clone(),
    }))
}

/// Returns a live statement with every parameter bound. On error the
/// statement is finalized here, so the caller never owns a half-bound one.
fn prepare_bound(
    connection: &Locked<'_>,
    sql: &str,
    params: &Params,
) -> Result<*mut ffi::sqlite3_stmt> {
    let sql_len =
        c_int::try_from(sql.len()).map_err(|_| Error::Misuse("SQL is too long".into()))?;
    let mut statement = std::ptr::null_mut();
    // SAFETY: `connection` is locked, so the handle is valid and not used by
    // another thread; `sql` points to `sql_len` readable bytes.
    let code = unsafe {
        ffi::sqlite3_prepare_v2(
            connection.handle(),
            sql.as_ptr().cast(),
            sql_len,
            &mut statement,
            std::ptr::null_mut(),
        )
    };
    if code != ffi::SQLITE_OK {
        return Err(failure(connection));
    }
    if statement.is_null() {
        return Err(Error::Misuse("SQL contains no statement".into()));
    }
    let bound = bind_all(connection, statement, params);
    if bound.is_err() {
        // SAFETY: `statement` is live, owned here and the connection is locked.
        unsafe { ffi::sqlite3_finalize(statement) };
    }
    bound.map(|()| statement)
}

fn bind_all(
    connection: &Locked<'_>,
    statement: *mut ffi::sqlite3_stmt,
    params: &Params,
) -> Result<()> {
    // SAFETY: `statement` is live and its connection is locked by the caller.
    let expected = unsafe { ffi::sqlite3_bind_parameter_count(statement) };
    if usize::try_from(expected).ok() != Some(params.0.len()) {
        return Err(Error::Misuse("parameter count mismatch".into()));
    }
    for (index, value) in params.0.iter().enumerate() {
        let position = c_int::try_from(index + 1)
            .map_err(|_| Error::ToSqlConversionFailure("invalid SQL parameter".into()))?;
        // SAFETY: `statement` is live and the connection is locked.
        if unsafe { bind(statement, position, value) } != ffi::SQLITE_OK {
            return Err(failure(connection));
        }
    }
    Ok(())
}

/// # Safety
/// `statement` must be a live statement on a connection the caller has locked.
unsafe fn bind(statement: *mut ffi::sqlite3_stmt, position: c_int, value: &Value) -> c_int {
    // SAFETY: the caller guarantees `statement` is live and its connection is
    // locked. Pointers and lengths describe `value`'s own buffers, which
    // outlive the call, and SQLITE_TRANSIENT makes SQLite copy the bytes
    // before the call returns.
    unsafe {
        match value {
            Value::Null => ffi::sqlite3_bind_null(statement, position),
            Value::Integer(value) => ffi::sqlite3_bind_int64(statement, position, *value),
            Value::Real(value) => ffi::sqlite3_bind_double(statement, position, *value),
            Value::Text(value) => ffi::sqlite3_bind_text64(
                statement,
                position,
                value.as_ptr().cast(),
                value.len() as u64,
                ffi::SQLITE_TRANSIENT(),
                ffi::SQLITE_UTF8 as u8,
            ),
            Value::Blob(value) => ffi::sqlite3_bind_blob64(
                statement,
                position,
                value.as_ptr().cast::<c_void>(),
                value.len() as u64,
                ffi::SQLITE_TRANSIENT(),
            ),
        }
    }
}

/// The last error of the locked `connection`, mapped to the driver error type.
fn failure(connection: &Locked<'_>) -> Error {
    // SAFETY: the connection is locked by the caller, so the handle is valid
    // and its error state is not changed concurrently. The message pointer is
    // copied before any other call on the handle.
    let (code, message) = unsafe {
        let handle = connection.handle();
        let message = ffi::sqlite3_errmsg(handle);
        let message = if message.is_null() {
            None
        } else {
            Some(CStr::from_ptr(message).to_string_lossy().into_owned())
        };
        (ffi::sqlite3_extended_errcode(handle), message)
    };
    map_error(rusqlite::Error::SqliteFailure(
        ffi::Error::new(code),
        message,
    ))
}

/// A prepared statement stepped one row per `next` call. A null `statement`
/// means it is finished and finalized.
pub(super) struct SqliteRows {
    statement: *mut ffi::sqlite3_stmt,
    state: SharedState,
}

// SAFETY: the raw statement pointer is the only non-`Send` field. It is used
// only inside `next` and `Drop`, both of which hold the connection mutex in
// `state`, so no two threads use the statement or its database handle at the
// same time. `state` is an `Arc<ConnectionState>` and is `Send` on its own.
unsafe impl Send for SqliteRows {}

impl SqliteRows {
    /// Finalizes the statement once. The caller holds the connection lock.
    fn finalize(&mut self) {
        if !self.statement.is_null() {
            // SAFETY: the statement is live and the caller holds the lock of
            // its connection; the pointer is cleared so it is finalized once.
            unsafe { ffi::sqlite3_finalize(self.statement) };
            self.statement = std::ptr::null_mut();
        }
    }
}

impl SqlRows for SqliteRows {
    fn next(&mut self) -> Result<Option<Row>> {
        check(&self.state.poisoned)?;
        if self.statement.is_null() {
            return Ok(None);
        }
        let state = self.state.clone();
        let connection = lock(&state)?;
        // SAFETY: the statement is live and its connection is locked.
        let code = unsafe { ffi::sqlite3_step(self.statement) };
        match code {
            // SAFETY: `step` returned a row and the lock is still held.
            ffi::SQLITE_ROW => {
                unsafe { read_columns(self.statement) }.map(|values| Some(Row(values)))
            }
            ffi::SQLITE_DONE => {
                self.finalize();
                Ok(None)
            }
            _ => {
                let error = failure(&connection);
                self.finalize();
                Err(error)
            }
        }
    }
}

impl Drop for SqliteRows {
    fn drop(&mut self) {
        if self.statement.is_null() {
            return;
        }
        // Finalize even when another thread panicked while holding the lock:
        // the connection cannot close while a statement is unfinalized.
        let state = self.state.clone();
        let _connection = state
            .connection
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.finalize();
    }
}

/// # Safety
/// `statement` must be live, its connection locked, and its last `step` must
/// have returned `SQLITE_ROW`.
/// Text that is not valid UTF-8 is a conversion error, never replaced.
unsafe fn read_columns(statement: *mut ffi::sqlite3_stmt) -> Result<Vec<Value>> {
    // SAFETY: guaranteed by the caller. Pointers returned by `column_text` and
    // `column_blob` stay valid until the next call on the statement and are
    // copied before then; `column_bytes` is read after the pointer, as SQLite
    // requires for text.
    unsafe {
        let count = ffi::sqlite3_column_count(statement);
        (0..count)
            .map(|index| match ffi::sqlite3_column_type(statement, index) {
                ffi::SQLITE_INTEGER => {
                    Ok(Value::Integer(ffi::sqlite3_column_int64(statement, index)))
                }
                ffi::SQLITE_FLOAT => Ok(Value::Real(ffi::sqlite3_column_double(statement, index))),
                ffi::SQLITE_TEXT => {
                    let text = ffi::sqlite3_column_text(statement, index);
                    let len = usize::try_from(ffi::sqlite3_column_bytes(statement, index))
                        .unwrap_or_default();
                    let bytes = if text.is_null() {
                        &[][..]
                    } else {
                        std::slice::from_raw_parts(text, len)
                    };
                    String::from_utf8(bytes.to_vec())
                        .map(Value::Text)
                        .map_err(|_| {
                            Error::ConversionFailure("column text is not valid UTF-8".into())
                        })
                }
                ffi::SQLITE_BLOB => {
                    let blob = ffi::sqlite3_column_blob(statement, index);
                    let len = usize::try_from(ffi::sqlite3_column_bytes(statement, index))
                        .unwrap_or_default();
                    if blob.is_null() {
                        Ok(Value::Blob(Vec::new()))
                    } else {
                        Ok(Value::Blob(
                            std::slice::from_raw_parts(blob.cast::<u8>(), len).to_vec(),
                        ))
                    }
                }
                _ => Ok(Value::Null),
            })
            .collect()
    }
}
