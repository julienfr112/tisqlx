//! A pure-Rust async RawKV client for [TiKV](https://tikv.org), part of
//! [tisqlx](https://docs.rs/tisqlx).
//!
//! RawKV is TiKV's plain key-value API: no transactions, no MVCC, just keys and values, routed
//! by region. This crate speaks gRPC to PD (to find regions and stores) and to TiKV, on tokio,
//! with no `protoc` and no C dependency.
//!
//! ```no_run
//! # async fn demo() -> tisqlx_rawkv::Result<()> {
//! use tisqlx_rawkv::RawClient;
//!
//! let kv = RawClient::connect(["127.0.0.1:2379"]).await?;
//! kv.put("app/user/1", "alice").await?;
//! assert_eq!(kv.get("app/user/1").await?, Some(b"alice".to_vec()));
//! let users = kv.scan("app/user/", "app/user0", 100).await?;
//! kv.delete_range("app/", "app0").await?;
//!
//! // Compare-and-swap needs every writer of those keys in atomic mode.
//! let atomic = kv.with_atomic_for_cas();
//! let r = atomic.compare_and_swap("app/counter", None, "1").await?;
//! assert!(r.swapped);
//! # Ok(()) }
//! ```
//!
//! See [`RawClient`] for how it routes and retries, and which TiKV settings it needs.

mod client;
pub mod error;
mod grpc;
mod pd;
mod proto;
mod region;

pub use client::{CasResult, ColumnFamily, Config, KvPair, MAX_SCAN_LIMIT, RawClient};
pub use error::{Error, Result};
