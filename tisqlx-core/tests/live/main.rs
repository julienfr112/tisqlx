//! Integration tests against a live, dedicated TiDB. See `common.rs`.

mod common;

mod cluster;
mod connection;
mod migrate;
mod pool;
mod tidb;
mod transactions;
mod types;
