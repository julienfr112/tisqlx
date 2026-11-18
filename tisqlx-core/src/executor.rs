//! The [`Executor`] trait: anything that can run a query.

use std::borrow::Cow;

use futures_core::future::BoxFuture;
use futures_core::stream::BoxStream;
use futures_util::TryStreamExt;

use crate::arguments::Arguments;
use crate::connection::{Describe, Step, TiConnection, run_on};
use crate::error::Result;
use crate::pool::{Pool, PoolConnection};
use crate::transaction::Transaction;

/// A type that can execute queries: `&mut TiConnection`, `&Pool`,
/// `&mut PoolConnection` and `&mut Transaction`.
pub trait Executor<'c>: Send + Sized {
    /// Execute `sql`, streaming every row and per-result summary.
    ///
    /// `args = None` uses the text protocol (and allows multiple statements);
    /// `Some` uses a prepared statement.
    fn fetch_many<'e, 'q: 'e>(
        self,
        sql: Cow<'q, str>,
        args: Option<Arguments>,
        persistent: bool,
    ) -> BoxStream<'e, Result<Step>>
    where
        'c: 'e;

    /// Prepare `sql` and return its metadata without executing it.
    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe>>
    where
        'c: 'e;
}

impl<'c> Executor<'c> for &'c mut TiConnection {
    fn fetch_many<'e, 'q: 'e>(
        self,
        sql: Cow<'q, str>,
        args: Option<Arguments>,
        persistent: bool,
    ) -> BoxStream<'e, Result<Step>>
    where
        'c: 'e,
    {
        self.run(sql, args, persistent)
    }

    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe>>
    where
        'c: 'e,
    {
        Box::pin(TiConnection::describe(self, sql))
    }
}

impl<'c> Executor<'c> for &'c Pool {
    fn fetch_many<'e, 'q: 'e>(
        self,
        sql: Cow<'q, str>,
        args: Option<Arguments>,
        persistent: bool,
    ) -> BoxStream<'e, Result<Step>>
    where
        'c: 'e,
    {
        let pool = self.clone();
        Box::pin(
            futures_util::stream::once(
                async move { pool.acquire().await.map(|conn| run_on(conn, sql, args, persistent)) },
            )
            .try_flatten(),
        )
    }

    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe>>
    where
        'c: 'e,
    {
        let pool = self.clone();
        Box::pin(async move { pool.acquire().await?.describe(sql).await })
    }
}

impl<'c> Executor<'c> for &'c mut PoolConnection {
    fn fetch_many<'e, 'q: 'e>(
        self,
        sql: Cow<'q, str>,
        args: Option<Arguments>,
        persistent: bool,
    ) -> BoxStream<'e, Result<Step>>
    where
        'c: 'e,
    {
        (&mut **self).fetch_many(sql, args, persistent)
    }

    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe>>
    where
        'c: 'e,
    {
        Executor::describe(&mut **self, sql)
    }
}

impl<'c, 't> Executor<'c> for &'c mut Transaction<'t> {
    fn fetch_many<'e, 'q: 'e>(
        self,
        sql: Cow<'q, str>,
        args: Option<Arguments>,
        persistent: bool,
    ) -> BoxStream<'e, Result<Step>>
    where
        'c: 'e,
    {
        (&mut **self).fetch_many(sql, args, persistent)
    }

    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe>>
    where
        'c: 'e,
    {
        Executor::describe(&mut **self, sql)
    }
}
