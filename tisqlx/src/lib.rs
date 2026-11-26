//! # tisqlx
//!
//! An async, pure-Rust SQL toolkit built specifically for [TiDB], on tokio and rustls.
//!
//! - Compile-time checked queries against a live TiDB (or offline data): [`query!`],
//!   [`query_as!`], [`query_scalar!`] and their `_file` variants.
//! - TiDB-native transactions: optimistic/pessimistic modes, stale reads
//!   ([`StaleRead`]), causal consistency, and automatic retry on write conflicts
//!   ([`Pool::transaction`]).
//! - A connection pool, savepoint-based nested transactions, embedded
//!   migrations ([`migrate!`]) aware that TiDB DDL is not transactional.
//!
//! ```no_run
//! # async fn demo() -> tisqlx::Result<()> {
//! let pool = tisqlx::Pool::connect("tidb://root@127.0.0.1:4000/test").await?;
//! let (n,): (i64,) = tisqlx::query_as("SELECT ? + 1").bind(41i64).fetch_one(&pool).await?;
//! assert_eq!(n, 42);
//! # Ok(()) }
//! ```
//!
//! [TiDB]: https://www.pingcap.com/tidb/

pub use tisqlx_core::*;

pub use tisqlx_macros::{
    FromRow, Type, migrate, query, query_as, query_file, query_file_as, query_file_scalar, query_scalar, test,
};

/// Rust types used by generated code and for convenience.
pub mod types {
    pub use serde_json::Value as JsonValue;
    pub use tisqlx_core::types::*;

    pub use bigdecimal::{self, BigDecimal};
    pub use chrono;
    pub use rust_decimal::Decimal;
    pub use uuid::{self, Uuid};
}

#[doc(hidden)]
pub mod __rt {
    pub use serde;
}
