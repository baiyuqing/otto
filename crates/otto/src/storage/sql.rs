//! Owned SQL values and local synchronous driver interfaces.
//!
//! The interface is blocking: a network implementation must own its I/O worker,
//! never poll network futures on the repository thread or enter its runtime.
//! Rows stream one owned row at a time. CommitUnknown is never a rollback claim.
use super::{SqlCapabilities, SqlRequirements};
use std::{path::Path, time::Duration};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("query returned no rows")]
    QueryReturnedNoRows,
    #[error("SQL value conversion failed")]
    ConversionFailure(String),
    #[error("SQL parameter conversion failed")]
    ToSqlConversionFailure(String),
    #[error("storage busy")]
    Busy(String),
    #[error("storage snapshot busy")]
    BusySnapshot(String),
    #[error("storage misuse or closed connection")]
    Misuse(String),
    #[error("SQL validation limit: {0}")]
    ValidationLimit(&'static str),
    #[error("storage constraint conflict")]
    Constraint(String),
    #[error("storage is corrupt")]
    Corrupt(String),
    #[error("invalid database")]
    NotAdb(String),
    #[error("storage unavailable")]
    Unavailable,
    #[error("storage closed")]
    Closed,
    #[error("unsupported storage capability: {0}")]
    UnsupportedCapability(&'static str),
    #[error("incompatible storage schema")]
    IncompatibleSchema,
    #[error("commit acknowledgement unknown")]
    CommitUnknown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

pub trait IntoValue {
    fn into_value(self) -> Result<Value>;
}
impl<T: Into<Value>> IntoValue for T {
    fn into_value(self) -> Result<Value> {
        Ok(self.into())
    }
}
impl IntoValue for u64 {
    fn into_value(self) -> Result<Value> {
        i64::try_from(self)
            .map(Value::Integer)
            .map_err(|_| Error::ToSqlConversionFailure("integer overflow".into()))
    }
}
impl IntoValue for usize {
    fn into_value(self) -> Result<Value> {
        i64::try_from(self)
            .map(Value::Integer)
            .map_err(|_| Error::ToSqlConversionFailure("integer overflow".into()))
    }
}
macro_rules! integer_value {
    ($($ty:ty),*) => { $(impl From<$ty> for Value { fn from(value: $ty) -> Self { Self::Integer(i64::from(value)) } })* };
}
integer_value!(i8, i16, i32, i64, u8, u16, u32, bool);
impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Real(value)
    }
}
impl From<f32> for Value {
    fn from(value: f32) -> Self {
        Self::Real(f64::from(value))
    }
}
impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}
impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::Text(value.into())
    }
}
impl From<Vec<u8>> for Value {
    fn from(value: Vec<u8>) -> Self {
        Self::Blob(value)
    }
}
impl From<&[u8]> for Value {
    fn from(value: &[u8]) -> Self {
        Self::Blob(value.into())
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map(Into::into).unwrap_or(Self::Null)
    }
}

/// Parameters own their data and conversion failures; no engine SDK escapes.
#[derive(Clone, Debug, PartialEq)]
pub struct Params(pub Vec<Value>);
pub trait IntoParams {
    fn into_params(self) -> Result<Params>;
}
impl IntoParams for () {
    fn into_params(self) -> Result<Params> {
        Ok(Params(Vec::new()))
    }
}
impl IntoParams for Params {
    fn into_params(self) -> Result<Params> {
        Ok(self)
    }
}
impl<T: IntoValue, const N: usize> IntoParams for [T; N] {
    fn into_params(self) -> Result<Params> {
        params_from_iter(self).into_params()
    }
}
impl<const N: usize> IntoParams for [Result<Value>; N] {
    fn into_params(self) -> Result<Params> {
        self.into_iter().collect::<Result<Vec<_>>>().map(Params)
    }
}
impl<T: IntoValue> IntoParams for Vec<T> {
    fn into_params(self) -> Result<Params> {
        self.into_iter()
            .map(IntoValue::into_value)
            .collect::<Result<Vec<_>>>()
            .map(Params)
    }
}
pub(crate) fn params_from_iter<T: IntoValue>(iter: impl IntoIterator<Item = T>) -> impl IntoParams {
    iter.into_iter().collect::<Vec<_>>()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row(pub Vec<Value>);
impl Row {
    pub(crate) fn column_count(&self) -> usize {
        self.0.len()
    }
    pub fn get_value(&self, index: usize) -> Result<Value> {
        self.0
            .get(index)
            .cloned()
            .ok_or_else(|| Error::Misuse("column out of bounds".into()))
    }
    pub fn get<T: FromValue>(&self, index: usize) -> Result<T> {
        T::from_value(self.get_value(index)?)
    }
}
pub trait FromValue: Sized {
    fn from_value(value: Value) -> Result<Self>;
}
macro_rules! from_value {
    ($ty:ty, $variant:ident) => {
        impl FromValue for $ty {
            fn from_value(value: Value) -> Result<Self> {
                if let Value::$variant(value) = value {
                    Ok(value)
                } else {
                    Err(Error::ConversionFailure("column type mismatch".into()))
                }
            }
        }
    };
}
from_value!(i64, Integer);
from_value!(f64, Real);
from_value!(String, Text);
from_value!(Vec<u8>, Blob);
macro_rules! from_integer {
    ($($ty:ty),*) => { $(impl FromValue for $ty { fn from_value(value: Value) -> Result<Self> {
        let value = i64::from_value(value)?;
        Self::try_from(value).map_err(|_| Error::ConversionFailure("integer overflow".into()))
    } })* };
}
from_integer!(i8, i16, i32, u8, u16, u32, u64, usize);
impl FromValue for bool {
    fn from_value(value: Value) -> Result<Self> {
        Ok(i64::from_value(value)? != 0)
    }
}
impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: Value) -> Result<Self> {
        if value == Value::Null {
            Ok(None)
        } else {
            T::from_value(value).map(Some)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OpenPolicy {
    #[default]
    CreateIfMissing,
    Existing,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OpenOptions {
    pub read_only: bool,
    /// Existing must never create a missing namespace, including inside the SDK.
    pub policy: OpenPolicy,
}

/// Composition validates capabilities before opening a route and on its connection.
pub trait SqlDriver: Send + Sync {
    fn capabilities(&self, path: &Path) -> SqlCapabilities;
    fn open(&self, path: &Path, options: OpenOptions) -> Result<Box<dyn SqlConnection>>;
}

/// A connection owns driver resources; statements/streams retain their engine handles.
/// Begin must reject nested transactions. Failed rollback must poison the connection.
/// Commit errors must classify an uncertain acknowledgement as CommitUnknown.
pub trait SqlConnection: Send + Sync {
    fn capabilities(&self) -> SqlCapabilities;
    fn busy_timeout(&self, duration: Duration) -> Result<()>;
    fn pragma_update(&self, name: &str, value: &str) -> Result<()>;
    /// Executes controlled trusted schema/migration scripts. Never pass imported DDL.
    fn execute_batch(&self, sql: &str) -> Result<()>;
    /// Executes one statement with bound values. SQLite rejects additional
    /// statements and SQL NUL bytes; literals, comments and trailing separators
    /// are interpreted by SQLite's parser, not a generic SQL sanitizer.
    fn execute(&self, sql: &str, params: Params) -> Result<usize>;
    /// Prepares exactly one statement; queries keep values separate from SQL.
    fn prepare(&self, sql: &str) -> Result<Box<dyn SqlStatement>>;
    fn begin(&self) -> Result<()>;
    fn commit(&self) -> Result<()>;
    fn rollback(&self) -> Result<()>;
    fn checkpoint(&self) -> Result<()>;
    /// Reject new work and drain owned work; do not wait on unrelated readers.
    fn close(self: Box<Self>) -> Result<()>;
}
pub trait SqlStatement: Send {
    fn query(&mut self, params: Params) -> Result<Box<dyn SqlRows>>;
}
pub trait SqlRows: Send {
    fn next(&mut self) -> Result<Option<Row>>;
}

/// Validate additional repository features before schema creation.
pub fn require(connection: &super::Connection, requirements: SqlRequirements) -> Result<()> {
    connection.capabilities().validate(requirements)
}
