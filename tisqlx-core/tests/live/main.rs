//! Integration tests against a live, dedicated TiDB. See `common.rs`.

mod common;

mod bulk;
mod cluster;
mod connection;
mod migrate;
mod pool;
mod tidb;
mod timezone;
mod transactions;
mod types;
