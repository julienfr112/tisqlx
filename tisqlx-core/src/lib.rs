//! Core of [tisqlx](https://docs.rs/tisqlx): an async, pure-Rust driver built
//! specifically for [TiDB](https://www.pingcap.com/tidb/).
//!
//! Most users should depend on the `tisqlx` facade crate instead.

pub mod acquire;
pub mod arguments;
mod balance;
#[doc(hidden)]
pub mod bind_check;
pub mod bulk;
pub mod connection;
pub mod decode;
pub mod encode;
pub mod error;
pub mod executor;
pub mod from_row;
pub mod migrate;
pub mod offline;
pub mod options;
pub mod pool;
mod protocol;
pub mod query;
#[cfg(all(test, any(feature = "chrono", feature = "time")))]
mod robustness;
pub mod row;
pub mod testing;
pub mod tidb;
mod tls;
pub mod transaction;
pub mod type_info;
pub mod types;
mod tz;
pub mod util;

pub use acquire::Acquire;
pub use arguments::Arguments;
pub use bulk::{BulkInsert, BulkRow, BulkWriter};
pub use connection::{Describe, QueryResult, Step, TiConnection};
pub use decode::Decode;
pub use encode::{Encode, IsNull};
pub use error::{DatabaseError, Error, Result};
pub use executor::Executor;
pub use from_row::FromRow;
pub use options::{ConnectOptions, SslMode};
pub use pool::{Pool, PoolConnection, PoolOptions};
pub use query::{Map, Query, QueryAs, QueryScalar, query, query_as, query_scalar, query_with, raw_sql};
pub use row::{Column, ColumnIndex, Row, ValueRef};
pub use tidb::{Engine, Isolation, RetryPolicy, Retryable, StaleRead, TiDbVersion, Tso, TxnMode, TxnOptions};
pub use transaction::Transaction;
pub use type_info::{ColumnFlags, ColumnType, TypeInfo};
pub use types::{Json, Set, Type, Vector};
