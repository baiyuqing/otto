//! Single-statement validation using SQLite's boundary recognizer, not splitting.
#![allow(unsafe_code)]

use super::map_error;
use crate::storage::{Error, Result};
use rusqlite::ffi;
use std::ffi::{CString, c_char, c_int, c_void};

// A fixed input and cumulative prefix budget prevents quadratic CPU amplification
// without substituting a partial SQL tokenizer for SQLite's trigger-aware recognizer.
// Callers hitting either limit should bind large values as parameters, simplify
// trigger definitions, or use the explicitly trusted execute_batch API for scripts.
const MAX_SQL_BYTES: usize = 8 * 1024 * 1024;
pub(super) const MAX_BOUNDARY_WORK_BYTES: usize = 64 * 1024 * 1024;

pub(super) fn validate(sql: &str) -> Result<()> {
    validate_with_work(sql, &mut 0)
}

// The counter also makes the resource bound directly testable, without timing.
pub(super) fn validate_with_work(sql: &str, work_bytes: &mut usize) -> Result<()> {
    *work_bytes = 0;
    if sql.len() > MAX_SQL_BYTES {
        return Err(boundary_limit());
    }
    let sql = CString::new(sql).map_err(|_| Error::Misuse("SQL contains a NUL byte".into()))?;
    c_int::try_from(sql.as_bytes().len()).map_err(|_| Error::Misuse("SQL is too long".into()))?;
    let scratch = rusqlite::Connection::open_in_memory().map_err(map_error)?;
    // Do not resolve live schema or allow even scratch PRAGMA/ATTACH effects.
    // Errors (including missing tables and authorization denial) still denote
    // nonempty SQL. Only a successful prepare with no statement is empty.
    // SAFETY: scratch is owned here; the callback has no borrowed context.
    let code =
        unsafe { ffi::sqlite3_set_authorizer(scratch.handle(), Some(deny), std::ptr::null_mut()) };
    if code != ffi::SQLITE_OK {
        return Err(Error::Misuse("cannot guard SQL boundary validation".into()));
    }
    let mut bytes = sql.into_bytes_with_nul();
    let mut start = 0;
    let mut statements = 0;
    for end in 0..bytes.len() - 1 {
        if bytes[end] != b';' {
            continue;
        }
        // Charge the full prefix (including NUL) before entering SQLite. Even
        // dense literals/comments and unfinished trigger bodies cannot exceed
        // this fixed scan budget; subtraction avoids overflow on hostile input.
        let prefix_bytes = end + 2 - start;
        if prefix_bytes > MAX_BOUNDARY_WORK_BYTES - *work_bytes {
            return Err(boundary_limit());
        }
        *work_bytes += prefix_bytes;
        let next = bytes[end + 1];
        bytes[end + 1] = 0;
        // SQLite's recognizer handles quoted semicolons, comments and the
        // internal semicolons of CREATE TRIGGER ... BEGIN ... END bodies.
        // SAFETY: start.. is a readable, NUL-terminated buffer.
        let complete = unsafe { ffi::sqlite3_complete(bytes[start..].as_ptr().cast()) };
        if complete != 0 {
            statements += usize::from(nonempty(&scratch, &bytes[start..]));
            start = end + 1;
        }
        bytes[end + 1] = next;
        if statements > 1 {
            return Err(Error::Misuse(
                "SQL must contain exactly one statement".into(),
            ));
        }
    }
    statements += usize::from(nonempty(&scratch, &bytes[start..]));
    if statements != 1 {
        return Err(Error::Misuse(
            "SQL must contain exactly one statement".into(),
        ));
    }
    // Schema-aware preparation belongs to the caller, only after this pure
    // scratch validation succeeds and the live connection is locked.
    Ok(())
}

fn boundary_limit() -> Error {
    Error::ValidationLimit(
        "boundary validation limit exceeded (8 MiB input / 64 MiB recognizer work); \
         bind large values as parameters, simplify triggers, or use execute_batch for trusted scripts",
    )
}

/// `sql` must be NUL-terminated. No statement is ever stepped.
fn nonempty(connection: &rusqlite::Connection, sql: &[u8]) -> bool {
    let mut statement = std::ptr::null_mut();
    // SAFETY: callers supply a NUL-terminated buffer and own this connection.
    let code = unsafe {
        ffi::sqlite3_prepare_v2(
            connection.handle(),
            sql.as_ptr().cast(),
            -1,
            &mut statement,
            std::ptr::null_mut(),
        )
    };
    let has_statement = !statement.is_null();
    if has_statement {
        // SAFETY: this statement is owned here and has not been stepped.
        unsafe { ffi::sqlite3_finalize(statement) };
    }
    code != ffi::SQLITE_OK || has_statement
}

unsafe extern "C" fn deny(
    _context: *mut c_void,
    _action: c_int,
    _first: *const c_char,
    _second: *const c_char,
    _database: *const c_char,
    _trigger: *const c_char,
) -> c_int {
    ffi::SQLITE_DENY
}
