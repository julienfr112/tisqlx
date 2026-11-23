//! TiDB-specific features: transaction modes, stale reads, TSO, version
//! detection, retry policy and session helpers.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use crate::connection::TiConnection;
use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::{BoxDynError, Error, Result};
use crate::row::ValueRef;
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;
use crate::util::quote_str;

/// TiDB transaction mode (`tidb_txn_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnMode {
    /// Locks rows as they are written (TiDB default since v3.0.8).
    Pessimistic,
    /// Detects conflicts at commit time; pair with retry ([`RetryPolicy`]).
    Optimistic,
}

impl TxnMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pessimistic => "pessimistic",
            Self::Optimistic => "optimistic",
        }
    }
}

impl FromStr for TxnMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match &*s.to_ascii_lowercase() {
            "pessimistic" => Ok(Self::Pessimistic),
            "optimistic" => Ok(Self::Optimistic),
            other => Err(Error::config(format!("unknown transaction mode {other:?}"))),
        }
    }
}

/// Isolation levels supported by TiDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Snapshot isolation (TiDB's `REPEATABLE READ`, the default).
    RepeatableRead,
    /// Only effective in pessimistic transactions.
    ReadCommitted,
}

impl Isolation {
    pub fn as_sql(self) -> &'static str {
        match self {
            Self::RepeatableRead => "REPEATABLE READ",
            Self::ReadCommitted => "READ COMMITTED",
        }
    }
}

/// The point in time a stale read reads at.
#[derive(Debug, Clone, PartialEq)]
pub enum StaleRead {
    /// Read data as of exactly this long ago.
    ExactStaleness(Duration),
    /// Read the freshest data no older than this, served by the closest replica
    /// (`TIDB_BOUNDED_STALENESS`).
    MaxStaleness(Duration),
    /// Read at a specific instant (in the session time zone).
    At(chrono::NaiveDateTime),
    /// Read at a TiDB TSO.
    Tso(Tso),
}

impl StaleRead {
    /// The SQL expression to put after `AS OF TIMESTAMP`.
    fn as_of_sql(&self) -> String {
        match self {
            Self::ExactStaleness(d) => format!("NOW(6) - INTERVAL {} MICROSECOND", d.as_micros()),
            Self::MaxStaleness(d) => {
                format!("TIDB_BOUNDED_STALENESS(NOW(6) - INTERVAL {} MICROSECOND, NOW(6))", d.as_micros())
            }
            Self::At(t) => quote_str(&t.format("%Y-%m-%d %H:%M:%S%.6f").to_string()),
            Self::Tso(tso) => format!("TIDB_PARSE_TSO({})", tso.0),
        }
    }
}

/// How to begin a transaction: [`TiConnection::begin_with`] / [`Pool::begin_with`](crate::Pool::begin_with).
#[derive(Debug, Clone, PartialEq)]
pub enum TxnOptions {
    /// `BEGIN`, with the session's mode and isolation unless given.
    ReadWrite { mode: Option<TxnMode>, isolation: Option<Isolation> },
    /// Read-only transaction at a past timestamp (served from any replica).
    StaleRead(StaleRead),
    /// `WITH CAUSAL CONSISTENCY ONLY`: async commit/1PC without linearizability.
    CausalConsistencyOnly,
}

impl Default for TxnOptions {
    fn default() -> Self {
        Self::ReadWrite { mode: None, isolation: None }
    }
}

impl TxnOptions {
    pub fn pessimistic() -> Self {
        Self::ReadWrite { mode: Some(TxnMode::Pessimistic), isolation: None }
    }

    pub fn optimistic() -> Self {
        Self::ReadWrite { mode: Some(TxnMode::Optimistic), isolation: None }
    }

    /// Read-only stale-read transaction.
    pub fn stale_read(as_of: StaleRead) -> Self {
        Self::StaleRead(as_of)
    }

    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub(crate) fn begin_sql(&self) -> String {
        match self {
            Self::ReadWrite { mode, isolation } => {
                // One round trip: the isolation applies to the next transaction only.
                let set =
                    isolation.map_or(String::new(), |i| format!("SET TRANSACTION ISOLATION LEVEL {}; ", i.as_sql()));
                let mode = mode.map_or("", |m| match m {
                    TxnMode::Pessimistic => " PESSIMISTIC",
                    TxnMode::Optimistic => " OPTIMISTIC",
                });
                format!("{set}BEGIN{mode}")
            }
            Self::StaleRead(as_of) => format!("START TRANSACTION READ ONLY AS OF TIMESTAMP {}", as_of.as_of_sql()),
            Self::CausalConsistencyOnly => "START TRANSACTION WITH CAUSAL CONSISTENCY ONLY".into(),
        }
    }
}

/// A TiDB server version, parsed from strings like `8.0.11-TiDB-v8.5.8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TiDbVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl TiDbVersion {
    /// The oldest TiDB tisqlx supports (and is tested against): the 8.5 LTS.
    pub const MINIMUM: Self = Self::new(8, 5, 0);

    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }

    /// Parse a server version string; `None` if it is not TiDB.
    pub fn parse(server_version: &str) -> Option<Self> {
        let rest = &server_version[server_version.find("TiDB-")? + 5..];
        let rest = rest.strip_prefix('v').unwrap_or(rest);
        let mut parts = rest.splitn(3, '.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch_str = parts.next().unwrap_or("0");
        let digits: String = patch_str.chars().take_while(char::is_ascii_digit).collect();
        let patch = digits.parse().unwrap_or(0);
        Some(Self { major, minor, patch })
    }
}

impl fmt::Display for TiDbVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A TiDB timestamp-oracle value (physical milliseconds << 18 | logical).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Tso(pub u64);

impl Tso {
    const LOGICAL_BITS: u32 = 18;

    pub fn physical_millis(&self) -> u64 {
        self.0 >> Self::LOGICAL_BITS
    }

    pub fn logical(&self) -> u64 {
        self.0 & ((1 << Self::LOGICAL_BITS) - 1)
    }

    pub fn from_parts(physical_millis: u64, logical: u64) -> Self {
        Self((physical_millis << Self::LOGICAL_BITS) | logical)
    }

    pub fn to_system_time(&self) -> std::time::SystemTime {
        std::time::UNIX_EPOCH + Duration::from_millis(self.physical_millis())
    }
}

impl Type for Tso {
    fn type_info() -> TypeInfo {
        TypeInfo::unsigned(ColumnType::LongLong)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        ty.r#type.is_integer() || ty.r#type.is_textual()
    }
}

impl Encode for Tso {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        self.0.encode(buf)
    }
}

impl<'r> Decode<'r> for Tso {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        if value.type_info.r#type.is_textual() {
            return Ok(Tso(value.as_str()?.trim().parse()?));
        }
        u64::decode(value).map(Tso)
    }
}

/// Storage engines for `tidb_isolation_read_engines`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    TiKV,
    TiFlash,
    TiDB,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TiKV => "tikv",
            Self::TiFlash => "tiflash",
            Self::TiDB => "tidb",
        }
    }
}

/// When to retry a transaction that failed with a retryable TiDB error
/// (write conflict, deadlock, lock wait timeout...).
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts including the first (1 disables retry).
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self { max_attempts: 5, initial_backoff: Duration::from_millis(10), max_backoff: Duration::from_secs(1) }
    }
}

impl RetryPolicy {
    pub fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    /// Exponential backoff with full jitter for the given (1-based) failed attempt.
    pub fn backoff(&self, attempt: u32) -> Duration {
        let exp = self.initial_backoff.saturating_mul(1 << attempt.min(16).saturating_sub(1));
        let cap = exp.min(self.max_backoff);
        let micros = cap.as_micros() as u64;
        Duration::from_micros(if micros == 0 { 0 } else { crate::util::random_u64() % (micros + 1) })
    }
}

/// Errors that know whether retrying the transaction may help.
pub trait Retryable {
    fn is_retryable(&self) -> bool;
}

impl Retryable for Error {
    fn is_retryable(&self) -> bool {
        Error::is_retryable(self)
    }
}

impl TiConnection {
    /// Make every read in this session a stale read at most `staleness` old; `None` turns it
    /// off. `tidb_read_staleness` counts whole seconds: fractions are dropped, and anything
    /// under a second becomes one second.
    pub async fn set_read_staleness(&mut self, staleness: Option<Duration>) -> Result<()> {
        let secs = staleness.map_or(0, |d| d.as_secs().max(1) as i64);
        self.execute_raw(&format!("SET SESSION tidb_read_staleness = '{}'", -secs)).await.map(drop)
    }

    /// Restrict the storage engines the optimizer may read from, e.g. `[Engine::TiFlash]`.
    pub async fn set_isolation_read_engines(&mut self, engines: &[Engine]) -> Result<()> {
        let list: Vec<&str> = engines.iter().map(|e| e.as_str()).collect();
        self.execute_raw(&format!("SET SESSION tidb_isolation_read_engines = {}", quote_str(&list.join(","))))
            .await
            .map(drop)
    }

    /// The start TSO of the current transaction (0 outside a transaction).
    pub async fn current_tso(&mut self) -> Result<Tso> {
        crate::query::query_scalar::<Tso>("SELECT @@tidb_current_ts").fetch_one(self).await
    }

    /// Full output of `TIDB_VERSION()`.
    pub async fn tidb_version_info(&mut self) -> Result<String> {
        crate::query::query_scalar::<String>("SELECT TIDB_VERSION()").fetch_one(self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version() {
        assert_eq!(TiDbVersion::parse("8.0.11-TiDB-v8.5.8"), Some(TiDbVersion::new(8, 5, 8)));
        assert_eq!(TiDbVersion::parse("5.7.25-TiDB-v6.5.0-serverless"), Some(TiDbVersion::new(6, 5, 0)));
        assert_eq!(TiDbVersion::parse("8.0.36"), None);
        assert!(TiDbVersion::new(8, 5, 8) >= TiDbVersion::MINIMUM);
    }

    #[test]
    fn begin_sql() {
        assert_eq!(TxnOptions::optimistic().begin_sql(), "BEGIN OPTIMISTIC");
        assert_eq!(
            TxnOptions::ReadWrite { mode: None, isolation: Some(Isolation::ReadCommitted) }.begin_sql(),
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED; BEGIN"
        );
        assert_eq!(
            TxnOptions::stale_read(StaleRead::ExactStaleness(Duration::from_secs(5))).begin_sql(),
            "START TRANSACTION READ ONLY AS OF TIMESTAMP NOW(6) - INTERVAL 5000000 MICROSECOND"
        );
    }

    #[test]
    fn tso_parts() {
        let t = Tso::from_parts(1_700_000_000_000, 42);
        assert_eq!(t.physical_millis(), 1_700_000_000_000);
        assert_eq!(t.logical(), 42);
    }
}
