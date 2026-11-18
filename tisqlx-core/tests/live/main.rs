//! Integration tests against a live, dedicated TiDB. See `common.rs`.

mod common;

mod cluster;
mod connection;
mod migrate;
mod pool;
mod tidb;
#[cfg(all(feature = "chrono", feature = "time", feature = "decimal"))]
mod timezone;
mod transactions;
mod types;
