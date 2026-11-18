//! Query builders: [`query`], [`query_as`], [`query_scalar`], [`raw_sql`].

use std::borrow::Cow;
use std::marker::PhantomData;

use futures_core::stream::BoxStream;
use futures_util::{StreamExt, TryStreamExt};

use crate::arguments::Arguments;
use crate::connection::{QueryResult, Step};
use crate::encode::Encode;
use crate::error::{Error, Result};
use crate::executor::Executor;
use crate::from_row::FromRow;
use crate::row::Row;

/// A SQL query with bind parameters, executed as a prepared statement.
#[must_use = "query must be executed to affect the database"]
pub struct Query<'q> {
    sql: Cow<'q, str>,
    args: Option<Arguments>,
    persistent: bool,
}

/// Create a prepared query. Bind parameters with `?` placeholders and [`Query::bind`].
pub fn query<'q>(sql: impl Into<Cow<'q, str>>) -> Query<'q> {
    Query { sql: sql.into(), args: Some(Arguments::new()), persistent: true }
}

/// Create a prepared query with pre-built arguments (used by the `query!` macros).
pub fn query_with<'q>(sql: impl Into<Cow<'q, str>>, args: Arguments) -> Query<'q> {
    Query { sql: sql.into(), args: Some(args), persistent: true }
}

/// Run SQL over the text protocol: no bind parameters, but multiple
/// `;`-separated statements and statements that cannot be prepared are allowed.
pub fn raw_sql<'q>(sql: impl Into<Cow<'q, str>>) -> Query<'q> {
    Query { sql: sql.into(), args: None, persistent: false }
}

impl std::fmt::Debug for Query<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Query").field("sql", &self.sql).field("persistent", &self.persistent).finish()
    }
}

impl<O> std::fmt::Debug for QueryAs<'_, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("QueryAs").field(&self.inner).finish()
    }
}

impl<O> std::fmt::Debug for QueryScalar<'_, O> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("QueryScalar").field(&self.inner).finish()
    }
}

impl<'q> Query<'q> {
    /// Bind the next `?` parameter.
    ///
    /// # Panics
    /// If called on a [`raw_sql`] query.
    pub fn bind<T: Encode + 'q>(mut self, value: T) -> Self {
        self.args.as_mut().expect("raw_sql() queries cannot take bind parameters").add(value);
        self
    }

    /// Whether to cache the prepared statement on the connection (default `true`).
    pub fn persistent(mut self, value: bool) -> Self {
        self.persistent = value;
        self
    }

    pub fn sql(&self) -> &str {
        &self.sql
    }

    pub fn fetch_many<'e, 'c: 'e, E>(self, executor: E) -> BoxStream<'e, Result<Step>>
    where
        'q: 'e,
        E: Executor<'c>,
    {
        executor.fetch_many(self.sql, self.args, self.persistent)
    }

    /// Stream rows.
    pub fn fetch<'e, 'c: 'e, E>(self, executor: E) -> BoxStream<'e, Result<Row>>
    where
        'q: 'e,
        E: Executor<'c>,
    {
        self.fetch_many(executor)
            .try_filter_map(|step| async move {
                Ok(match step {
                    Step::Row(r) => Some(r),
                    Step::Done(_) => None,
                })
            })
            .boxed()
    }

    /// Execute, returning the combined result of all statements.
    pub async fn execute<'c, E: Executor<'c>>(self, executor: E) -> Result<QueryResult> {
        let mut total = QueryResult::default();
        let mut s = self.fetch_many(executor);
        while let Some(step) = s.try_next().await? {
            if let Step::Done(r) = step {
                total.add(r);
            }
        }
        Ok(total)
    }

    pub async fn fetch_all<'c, E: Executor<'c>>(self, executor: E) -> Result<Vec<Row>> {
        self.fetch(executor).try_collect().await
    }

    /// Fetch the first row, or `None`. Remaining rows are discarded.
    pub async fn fetch_optional<'c, E: Executor<'c>>(self, executor: E) -> Result<Option<Row>> {
        let mut s = self.fetch(executor);
        s.try_next().await
    }

    /// Fetch exactly the first row, or [`Error::RowNotFound`].
    pub async fn fetch_one<'c, E: Executor<'c>>(self, executor: E) -> Result<Row> {
        self.fetch_optional(executor).await?.ok_or(Error::RowNotFound)
    }
}

/// A query whose rows are mapped through [`FromRow`].
#[must_use = "query must be executed to affect the database"]
pub struct QueryAs<'q, O> {
    inner: Query<'q>,
    _out: PhantomData<fn() -> O>,
}

/// Create a prepared query whose rows are decoded into `O` via [`FromRow`].
pub fn query_as<'q, O>(sql: impl Into<Cow<'q, str>>) -> QueryAs<'q, O>
where
    O: for<'r> FromRow<'r>,
{
    QueryAs { inner: query(sql), _out: PhantomData }
}

impl<'q, O> QueryAs<'q, O>
where
    O: for<'r> FromRow<'r> + Send + Unpin + 'q,
{
    pub fn bind<T: Encode + 'q>(mut self, value: T) -> Self {
        self.inner = self.inner.bind(value);
        self
    }

    pub fn persistent(mut self, value: bool) -> Self {
        self.inner = self.inner.persistent(value);
        self
    }

    pub fn fetch<'e, 'c: 'e, E>(self, executor: E) -> BoxStream<'e, Result<O>>
    where
        'q: 'e,
        O: 'e,
        E: Executor<'c>,
    {
        self.inner.fetch(executor).and_then(|row| async move { O::from_row(&row) }).boxed()
    }

    pub async fn fetch_all<'c, E: Executor<'c>>(self, executor: E) -> Result<Vec<O>> {
        self.fetch(executor).try_collect().await
    }

    pub async fn fetch_optional<'c, E: Executor<'c>>(self, executor: E) -> Result<Option<O>> {
        match self.inner.fetch_optional(executor).await? {
            Some(row) => O::from_row(&row).map(Some),
            None => Ok(None),
        }
    }

    pub async fn fetch_one<'c, E: Executor<'c>>(self, executor: E) -> Result<O> {
        self.fetch_optional(executor).await?.ok_or(Error::RowNotFound)
    }

    pub async fn execute<'c, E: Executor<'c>>(self, executor: E) -> Result<QueryResult> {
        self.inner.execute(executor).await
    }
}

/// A query returning a single column per row.
#[must_use = "query must be executed to affect the database"]
pub struct QueryScalar<'q, O> {
    inner: QueryAs<'q, (O,)>,
}

/// Create a prepared query returning the first column of each row as `O`.
pub fn query_scalar<'q, O>(sql: impl Into<Cow<'q, str>>) -> QueryScalar<'q, O>
where
    (O,): for<'r> FromRow<'r>,
{
    QueryScalar { inner: query_as(sql) }
}

impl<'q, O> QueryScalar<'q, O>
where
    (O,): for<'r> FromRow<'r>,
    O: Send + Unpin + 'q,
{
    pub fn bind<T: Encode + 'q>(mut self, value: T) -> Self {
        self.inner = self.inner.bind(value);
        self
    }

    pub fn persistent(mut self, value: bool) -> Self {
        self.inner = self.inner.persistent(value);
        self
    }

    pub fn fetch<'e, 'c: 'e, E>(self, executor: E) -> BoxStream<'e, Result<O>>
    where
        'q: 'e,
        O: 'e,
        E: Executor<'c>,
    {
        self.inner.fetch(executor).map_ok(|(v,)| v).boxed()
    }

    pub async fn fetch_all<'c, E: Executor<'c>>(self, executor: E) -> Result<Vec<O>> {
        self.fetch(executor).try_collect().await
    }

    pub async fn fetch_optional<'c, E: Executor<'c>>(self, executor: E) -> Result<Option<O>> {
        Ok(self.inner.fetch_optional(executor).await?.map(|(v,)| v))
    }

    pub async fn fetch_one<'c, E: Executor<'c>>(self, executor: E) -> Result<O> {
        self.fetch_optional(executor).await?.ok_or(Error::RowNotFound)
    }
}

/// A query whose rows are converted by a closure (used by the `query!` macros).
#[must_use = "query must be executed to affect the database"]
pub struct Map<'q, F> {
    inner: Query<'q>,
    mapper: F,
}

impl<F> std::fmt::Debug for Map<'_, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Map").field(&self.inner).finish()
    }
}

impl<'q> Query<'q> {
    /// Convert each row with a fallible closure.
    pub fn try_map<F, O>(self, mapper: F) -> Map<'q, F>
    where
        F: FnMut(Row) -> Result<O> + Send,
    {
        Map { inner: self, mapper }
    }
}

impl<'q, F, O> Map<'q, F>
where
    F: FnMut(Row) -> Result<O> + Send + 'q,
    O: Send + Unpin + 'q,
{
    pub fn persistent(mut self, value: bool) -> Self {
        self.inner = self.inner.persistent(value);
        self
    }

    pub fn fetch<'e, 'c: 'e, E>(self, executor: E) -> BoxStream<'e, Result<O>>
    where
        'q: 'e,
        E: Executor<'c>,
    {
        let mut mapper = self.mapper;
        self.inner.fetch(executor).map(move |r| r.and_then(&mut mapper)).boxed()
    }

    pub async fn fetch_all<'c, E: Executor<'c>>(self, executor: E) -> Result<Vec<O>> {
        self.fetch(executor).try_collect().await
    }

    pub async fn fetch_optional<'c, E: Executor<'c>>(self, executor: E) -> Result<Option<O>> {
        let mut mapper = self.mapper;
        match self.inner.fetch_optional(executor).await? {
            Some(row) => mapper(row).map(Some),
            None => Ok(None),
        }
    }

    pub async fn fetch_one<'c, E: Executor<'c>>(self, executor: E) -> Result<O> {
        self.fetch_optional(executor).await?.ok_or(Error::RowNotFound)
    }

    pub async fn execute<'c, E: Executor<'c>>(self, executor: E) -> Result<QueryResult> {
        self.inner.execute(executor).await
    }
}
