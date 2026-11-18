//! [`Acquire`]: write a function once and call it with a pool, a connection or a transaction.

use std::ops::DerefMut;

use futures_core::future::BoxFuture;

use crate::connection::TiConnection;
use crate::error::Result;
use crate::pool::{Pool, PoolConnection};
use crate::transaction::Transaction;

/// Something a connection can be taken from: `&Pool`, `&mut TiConnection`,
/// `&mut PoolConnection` or `&mut Transaction`.
///
/// ```no_run
/// use tisqlx_core::{Acquire, Result, query};
///
/// /// Callable with `&pool`, `&mut conn` or `&mut tx`.
/// async fn rename<'c>(db: impl Acquire<'c>, id: i64, name: &str) -> Result<()> {
///     // A transaction on a pool or a connection, a savepoint inside a transaction.
///     let mut tx = db.begin().await?;
///     query("UPDATE users SET name = ? WHERE id = ?").bind(name).bind(id).execute(&mut *tx).await?;
///     tx.commit().await
/// }
/// ```
///
/// In a spawned task (`tokio::spawn`), such an `async fn` may fail to compile with "implementation
/// of `Acquire` is not general enough": the compiler cannot prove its future `Send` for every
/// lifetime. Return a boxed future instead (`-> BoxFuture<'c, Result<T>>` with `Box::pin(async
/// move { ... })`, and `A: Acquire<'c> + 'c`), as [`BulkInsert::execute`](crate::BulkInsert::execute) does.
pub trait Acquire<'c>: Send {
    /// The connection [`acquire`](Acquire::acquire) returns: a [`PoolConnection`] from a pool,
    /// else the connection itself (inside a transaction, its connection, still in the transaction).
    type Connection: DerefMut<Target = TiConnection> + Send;

    /// Take a connection (checks one out of a pool).
    fn acquire(self) -> BoxFuture<'c, Result<Self::Connection>>;

    /// Begin a transaction, or a savepoint when called on a [`Transaction`].
    fn begin(self) -> BoxFuture<'c, Result<Transaction<'c>>>;
}

impl<'c> Acquire<'c> for &'c Pool {
    type Connection = PoolConnection;

    fn acquire(self) -> BoxFuture<'c, Result<PoolConnection>> {
        Box::pin(Pool::acquire(self))
    }

    fn begin(self) -> BoxFuture<'c, Result<Transaction<'c>>> {
        Box::pin(async move { Pool::begin(self).await })
    }
}

impl<'c> Acquire<'c> for &'c mut TiConnection {
    type Connection = &'c mut TiConnection;

    fn acquire(self) -> BoxFuture<'c, Result<&'c mut TiConnection>> {
        Box::pin(async move { Ok(self) })
    }

    fn begin(self) -> BoxFuture<'c, Result<Transaction<'c>>> {
        Box::pin(TiConnection::begin(self))
    }
}

impl<'c> Acquire<'c> for &'c mut PoolConnection {
    type Connection = &'c mut TiConnection;

    fn acquire(self) -> BoxFuture<'c, Result<&'c mut TiConnection>> {
        Box::pin(async move { Ok(&mut **self) })
    }

    fn begin(self) -> BoxFuture<'c, Result<Transaction<'c>>> {
        Box::pin(TiConnection::begin(self))
    }
}

impl<'c, 't: 'c> Acquire<'c> for &'c mut Transaction<'t> {
    type Connection = &'c mut TiConnection;

    fn acquire(self) -> BoxFuture<'c, Result<&'c mut TiConnection>> {
        Box::pin(async move { Ok(&mut **self) })
    }

    fn begin(self) -> BoxFuture<'c, Result<Transaction<'c>>> {
        Box::pin(Transaction::begin(self))
    }
}
