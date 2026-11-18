//! Error types.

use std::fmt;

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub type BoxDynError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Any error that can occur while talking to TiDB.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("error with configuration: {0}")]
    Configuration(#[source] BoxDynError),

    #[error("error communicating with TiDB: {0}")]
    Io(#[from] std::io::Error),

    #[error("error establishing TLS connection: {0}")]
    Tls(#[source] BoxDynError),

    #[error("protocol error: {0}")]
    Protocol(String),

    /// The server returned an error packet.
    #[error(transparent)]
    Database(#[from] Box<DatabaseError>),

    #[error("no rows returned by a query that expected to return at least one row")]
    RowNotFound,

    #[error("no column found for name: {0}")]
    ColumnNotFound(String),

    #[error("column index out of bounds: the len is {len}, but the index is {index}")]
    ColumnIndexOutOfBounds { index: usize, len: usize },

    #[error("error occurred while decoding column {index}: {source}")]
    ColumnDecode {
        index: String,
        #[source]
        source: BoxDynError,
    },

    #[error("error occurred while encoding a value: {0}")]
    Encode(#[source] BoxDynError),

    #[error("pool timed out while waiting for an open connection")]
    PoolTimedOut,

    #[error("attempted to acquire a connection on a closed pool")]
    PoolClosed,

    #[error("connected server is not TiDB (version string: {0:?}); set `require_tidb(false)` to allow it")]
    NotTiDb(String),

    #[error(
        "TiDB {0} is not supported: tisqlx needs TiDB {min} or newer; set `require_tidb(false)` to connect anyway",
        min = crate::tidb::TiDbVersion::MINIMUM
    )]
    UnsupportedTiDb(crate::tidb::TiDbVersion),

    #[error(transparent)]
    Migrate(#[from] Box<crate::migrate::MigrateError>),
}

impl Error {
    /// The server-side error, if this is one.
    pub fn as_database_error(&self) -> Option<&DatabaseError> {
        match self {
            Error::Database(e) => Some(e),
            _ => None,
        }
    }

    /// `true` if retrying the *whole transaction* may succeed.
    ///
    /// Covers TiDB optimistic write conflicts, pessimistic deadlocks and lock
    /// wait timeouts, and schema-changed-during-commit errors.
    pub fn is_retryable(&self) -> bool {
        self.as_database_error().is_some_and(DatabaseError::is_retryable)
    }

    pub(crate) fn protocol(msg: impl fmt::Display) -> Self {
        Error::Protocol(msg.to_string())
    }

    pub(crate) fn config(msg: impl fmt::Display) -> Self {
        Error::Configuration(msg.to_string().into())
    }
}

impl From<crate::migrate::MigrateError> for Error {
    fn from(e: crate::migrate::MigrateError) -> Self {
        Error::Migrate(Box::new(e))
    }
}

/// Whether `error`, or any error in its `source()` chain, is a retryable tisqlx
/// [`Error`] (write conflict, deadlock, ...). For wrapped errors such as `anyhow::Error`:
/// `tisqlx::error::is_retryable(err.as_ref())`.
pub fn is_retryable(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(e) = current {
        if e.downcast_ref::<Error>().is_some_and(Error::is_retryable) {
            return true;
        }
        current = e.source();
    }
    false
}

/// TiDB/MySQL error codes with a special meaning to tisqlx.
pub mod codes {
    /// The server is at its connection limit.
    pub const TOO_MANY_CONNECTIONS: u16 = 1040;
    pub const DUP_ENTRY: u16 = 1062;
    pub const NO_REFERENCED_ROW: u16 = 1452;
    pub const ROW_IS_REFERENCED: u16 = 1451;
    pub const CHECK_CONSTRAINT_VIOLATED: u16 = 3819;
    pub const LOCK_WAIT_TIMEOUT: u16 = 1205;
    pub const DEADLOCK: u16 = 1213;
    /// `SELECT FOR UPDATE` write conflict (optimistic).
    pub const TIDB_SELECT_FOR_UPDATE_CONFLICT: u16 = 8002;
    /// Write conflict caused by stale start timestamp.
    pub const TIDB_WRITE_CONFLICT_STALE_TS: u16 = 8005;
    /// Commit failed and was rolled back; safe to retry.
    pub const TIDB_TXN_RETRYABLE: u16 = 8022;
    /// Schema changed during transaction.
    pub const TIDB_INFO_SCHEMA_CHANGED: u16 = 8028;
    /// Schema changed between PREPARE and EXECUTE.
    pub const TIDB_SCHEMA_CHANGED_AFTER_PREPARE: u16 = 8113;
    /// Lock resolution timeout under heavy contention.
    pub const TIDB_RESOLVE_LOCK_TIMEOUT: u16 = 9004;
    /// Optimistic write conflict.
    pub const TIDB_WRITE_CONFLICT: u16 = 9007;

    pub const RETRYABLE: &[u16] = &[
        LOCK_WAIT_TIMEOUT,
        DEADLOCK,
        TIDB_SELECT_FOR_UPDATE_CONFLICT,
        TIDB_WRITE_CONFLICT_STALE_TS,
        TIDB_TXN_RETRYABLE,
        TIDB_INFO_SCHEMA_CHANGED,
        TIDB_RESOLVE_LOCK_TIMEOUT,
        TIDB_WRITE_CONFLICT,
    ];
}

/// An error packet returned by TiDB.
#[derive(Debug, Clone)]
pub struct DatabaseError {
    pub code: u16,
    pub sql_state: Option<String>,
    pub message: String,
}

impl DatabaseError {
    pub fn is_retryable(&self) -> bool {
        codes::RETRYABLE.contains(&self.code) || self.sql_state.as_deref() == Some("40001")
    }

    pub fn is_unique_violation(&self) -> bool {
        self.code == codes::DUP_ENTRY
    }

    pub fn is_foreign_key_violation(&self) -> bool {
        matches!(self.code, codes::NO_REFERENCED_ROW | codes::ROW_IS_REFERENCED)
    }

    pub fn is_check_violation(&self) -> bool {
        self.code == codes::CHECK_CONSTRAINT_VIOLATED
    }

    pub fn is_write_conflict(&self) -> bool {
        matches!(
            self.code,
            codes::TIDB_WRITE_CONFLICT | codes::TIDB_SELECT_FOR_UPDATE_CONFLICT | codes::TIDB_WRITE_CONFLICT_STALE_TS
        )
    }
}

impl fmt::Display for DatabaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.sql_state {
            Some(state) => write!(f, "{} ({}): {}", self.code, state, self.message),
            None => write!(f, "{}: {}", self.code, self.message),
        }
    }
}

impl std::error::Error for DatabaseError {}
