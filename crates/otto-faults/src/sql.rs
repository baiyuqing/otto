//! A fault-injecting [`SqlDriver`] decorator.
//!
//! [`FaultySqlDriver`] wraps another driver and every connection, statement
//! and row cursor it opens. Each call asks the shared [`FaultPlan`] for a
//! fault before it reaches the inner driver. The filter subject of a call is:
//!
//! - the database path for [`SqlOp::Open`] and for the connection-level
//!   operations `Begin`, `Commit`, `Rollback`, `Checkpoint` and `Close`;
//! - the SQL text for `ExecuteBatch`, `Execute` and `Prepare`;
//! - the SQL text the statement was prepared with, for `Query` and `Step`.
//!
//! `Before(e)` returns `e` without calling the inner operation; `After(e)`
//! calls it and then returns `e`, unless the inner operation failed, in which
//! case its own error is returned. `Exit` and `ExitAfter` end the process
//! with [`crate::crash::exit`]. `Hang` and `Short*` have no meaning
//! on this synchronous, non-byte boundary and panic: a plan that uses them is
//! a test bug.
//!
//! The decorator models the poisoning contract of the real driver
//! (`SqliteConnection`), because repositories rely on it:
//!
//! - A commit error other than `Busy`, `BusySnapshot` and `Constraint` is
//!   reported as `CommitUnknown`, as the real `commit` does.
//! - After `commit` returns `CommitUnknown`, or `rollback` returns any error,
//!   every later call on that connection, its statements and its cursors
//!   returns `Closed` without reaching the plan or the inner driver.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::{Fault, FaultError, FaultPlan};
use otto_storage::{
    Error, OpenOptions, Params, Result, Row, SqlConnection, SqlDriver, SqlRows, SqlStatement,
};

/// The driver operations a fault rule can target. `Step` is
/// [`SqlRows::next`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlOp {
    Open,
    ExecuteBatch,
    Execute,
    Prepare,
    Query,
    Step,
    Begin,
    Commit,
    Rollback,
    Checkpoint,
    Close,
}

pub type SqlPlan = FaultPlan<SqlOp>;

pub struct FaultySqlDriver {
    inner: Arc<dyn SqlDriver>,
    plan: Arc<SqlPlan>,
}

impl FaultySqlDriver {
    pub fn new(inner: Arc<dyn SqlDriver>, plan: Arc<SqlPlan>) -> Self {
        Self { inner, plan }
    }
}

impl SqlDriver for FaultySqlDriver {
    fn open(&self, path: &Path, options: OpenOptions) -> Result<Box<dyn SqlConnection>> {
        let name = path.to_string_lossy();
        let inner = apply(&self.plan, SqlOp::Open, &name, || {
            self.inner.open(path, options)
        })?;
        Ok(Box::new(FaultyConnection {
            inner,
            plan: self.plan.clone(),
            path: name.into_owned(),
            poisoned: Arc::new(AtomicBool::new(false)),
        }))
    }
}

fn error(fault: FaultError) -> Error {
    match fault {
        FaultError::Sql(fault) => fault.into(),
        other => panic!("{other:?} injected into an SQL boundary; use FaultError::Sql"),
    }
}

/// Run `call` under the plan's fault for `op`, if any.
fn apply<T>(
    plan: &SqlPlan,
    op: SqlOp,
    subject: &str,
    call: impl FnOnce() -> Result<T>,
) -> Result<T> {
    match plan.check(op, subject) {
        None => call(),
        Some(Fault::Before(fault)) => Err(error(fault)),
        Some(Fault::After(fault)) => {
            call()?;
            Err(error(fault))
        }
        Some(Fault::Exit) => super::crash::exit(),
        Some(Fault::ExitAfter) => {
            let _ = call();
            super::crash::exit()
        }
        Some(fault @ (Fault::Hang | Fault::Short { .. } | Fault::ShortExit { .. })) => {
            panic!("{fault:?} has no meaning on the synchronous SQL boundary ({op:?})")
        }
    }
}

struct FaultyConnection {
    inner: Box<dyn SqlConnection>,
    plan: Arc<SqlPlan>,
    path: String,
    poisoned: Arc<AtomicBool>,
}

fn check(poisoned: &AtomicBool) -> Result<()> {
    if poisoned.load(Ordering::Acquire) {
        Err(Error::Closed)
    } else {
        Ok(())
    }
}

impl FaultyConnection {
    /// A statement-level call: refused when poisoned, otherwise faultable.
    fn run<T>(&self, op: SqlOp, subject: &str, call: impl FnOnce() -> Result<T>) -> Result<T> {
        check(&self.poisoned)?;
        apply(&self.plan, op, subject, call)
    }
}

impl SqlConnection for FaultyConnection {
    fn busy_timeout(&self, duration: Duration) -> Result<()> {
        check(&self.poisoned)?;
        self.inner.busy_timeout(duration)
    }
    fn pragma_update(&self, name: &str, value: &str) -> Result<()> {
        check(&self.poisoned)?;
        self.inner.pragma_update(name, value)
    }
    fn execute_batch(&self, sql: &str) -> Result<()> {
        self.run(SqlOp::ExecuteBatch, sql, || self.inner.execute_batch(sql))
    }
    fn execute(&self, sql: &str, params: Params) -> Result<usize> {
        self.run(SqlOp::Execute, sql, || self.inner.execute(sql, params))
    }
    fn prepare(&self, sql: &str) -> Result<Box<dyn SqlStatement>> {
        let inner = self.run(SqlOp::Prepare, sql, || self.inner.prepare(sql))?;
        Ok(Box::new(FaultyStatement {
            inner,
            plan: self.plan.clone(),
            sql: sql.to_owned(),
            poisoned: self.poisoned.clone(),
        }))
    }
    fn begin(&self) -> Result<()> {
        self.run(SqlOp::Begin, &self.path, || self.inner.begin())
    }
    fn commit(&self) -> Result<()> {
        check(&self.poisoned)?;
        let result = apply(&self.plan, SqlOp::Commit, &self.path, || {
            self.inner.commit()
        });
        match result {
            Err(Error::Busy(_) | Error::BusySnapshot(_) | Error::Constraint(_)) | Ok(()) => result,
            Err(_) => {
                self.poisoned.store(true, Ordering::Release);
                Err(Error::CommitUnknown)
            }
        }
    }
    fn rollback(&self) -> Result<()> {
        let result = self.run(SqlOp::Rollback, &self.path, || self.inner.rollback());
        if result.is_err() {
            self.poisoned.store(true, Ordering::Release);
        }
        result
    }
    fn checkpoint(&self) -> Result<()> {
        self.run(SqlOp::Checkpoint, &self.path, || self.inner.checkpoint())
    }
    fn close(self: Box<Self>) -> Result<()> {
        self.poisoned.store(true, Ordering::Release);
        let Self {
            inner, plan, path, ..
        } = *self;
        apply(&plan, SqlOp::Close, &path, || inner.close())
    }
}

struct FaultyStatement {
    inner: Box<dyn SqlStatement>,
    plan: Arc<SqlPlan>,
    sql: String,
    poisoned: Arc<AtomicBool>,
}

impl SqlStatement for FaultyStatement {
    fn query(&mut self, params: Params) -> Result<Box<dyn SqlRows>> {
        check(&self.poisoned)?;
        let inner = apply(&self.plan, SqlOp::Query, &self.sql, || {
            self.inner.query(params)
        })?;
        Ok(Box::new(FaultyRows {
            inner,
            plan: self.plan.clone(),
            sql: self.sql.clone(),
            poisoned: self.poisoned.clone(),
        }))
    }
}

struct FaultyRows {
    inner: Box<dyn SqlRows>,
    plan: Arc<SqlPlan>,
    sql: String,
    poisoned: Arc<AtomicBool>,
}

impl SqlRows for FaultyRows {
    fn next(&mut self) -> Result<Option<Row>> {
        check(&self.poisoned)?;
        apply(&self.plan, SqlOp::Step, &self.sql, || self.inner.next())
    }
}
