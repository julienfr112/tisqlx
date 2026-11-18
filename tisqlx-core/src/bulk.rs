//! Bulk inserts: rows batched into multi-row `INSERT` statements sized to TiDB's limits.
//!
//! ```no_run
//! # async fn f(pool: &tisqlx_core::Pool, users: Vec<(String, Option<String>)>) -> tisqlx_core::Result<()> {
//! use tisqlx_core::bulk::BulkInsert;
//!
//! // All at once...
//! let n = BulkInsert::new("users", &["name", "email"]).execute(pool, users).await?;
//!
//! // ...or streamed: each batch is sent as soon as it is full.
//! let mut w = BulkInsert::new("users", &["name", "email"]).start(pool).await?;
//! w.push(("alice", Some("a@example.com"))).await?;
//! w.push(("bob", None::<&str>)).await?;
//! w.finish().await?;
//! # Ok(()) }
//! ```
//!
//! **Batches.** A batch is sent when it reaches `max_rows` (default 1000), or when the next row
//! would make it larger than the byte budget. That budget is the smaller of the server's
//! `max_allowed_packet` and a quarter of TiDB's `txn-total-size-limit`, because index entries
//! make a transaction larger than its values. Both limits are read from the server when the
//! writer starts. Full batches reuse one prepared statement; the others are not cached.
//!
//! **Transactions.** Outside a transaction each batch commits on its own: if one fails, the
//! batches before it stay. [`rows_written`](BulkWriter::rows_written) says how many rows those
//! held, and the writer refuses further rows. Inside a transaction (`start(&mut tx)`) the load
//! is all or nothing, but then all of it must fit `txn-total-size-limit`; TiDB rejects it
//! otherwise (error 8004).

use std::ops::DerefMut;

use futures_core::future::BoxFuture;

use crate::acquire::Acquire;
use crate::arguments::Arguments;
use crate::connection::TiConnection;
use crate::encode::Encode;
use crate::error::{Error, Result};
use crate::query::{query_scalar, query_with, raw_sql};
use crate::util::quote_ident;

/// Prepared-statement parameters are numbered with 16 bits.
const MAX_PARAMS: usize = 65_535;
/// Room for the COM_STMT_EXECUTE header and the packet framing.
const PACKET_MARGIN: usize = 1024;
/// TiDB's default `txn-total-size-limit`, when the server does not tell.
const DEFAULT_TXN_LIMIT: usize = 100 << 20;

/// What to do with rows whose key already exists.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Conflict {
    Fail,
    Ignore,
    Replace,
    Update(Vec<String>),
}

/// A bulk insert into one table (see the [module docs](self)).
#[derive(Debug, Clone)]
pub struct BulkInsert {
    table: String,
    columns: Vec<String>,
    conflict: Conflict,
    max_rows: usize,
    max_batch_bytes: Option<usize>,
}

impl BulkInsert {
    /// Insert into `table` (`name` or `db.name`) these columns, in the order rows give them.
    pub fn new(table: &str, columns: &[&str]) -> Self {
        Self {
            table: table.to_owned(),
            columns: columns.iter().map(|c| (*c).to_owned()).collect(),
            conflict: Conflict::Fail,
            max_rows: 1000,
            max_batch_bytes: None,
        }
    }

    /// `INSERT IGNORE`: skip rows whose key already exists.
    pub fn ignore(mut self) -> Self {
        self.conflict = Conflict::Ignore;
        self
    }

    /// `REPLACE`: replace rows whose key already exists.
    pub fn replace(mut self) -> Self {
        self.conflict = Conflict::Replace;
        self
    }

    /// `ON DUPLICATE KEY UPDATE col = VALUES(col), ...` for these columns.
    pub fn on_duplicate_key_update(mut self, columns: &[&str]) -> Self {
        self.conflict = Conflict::Update(columns.iter().map(|c| (*c).to_owned()).collect());
        self
    }

    /// At most this many rows per statement (default 1000; also capped by the 65 535
    /// parameters a statement can have).
    pub fn max_rows(mut self, rows: usize) -> Self {
        self.max_rows = rows.max(1);
        self
    }

    /// At most this many bytes of values per statement, instead of the budget derived from the
    /// server's limits.
    pub fn max_batch_bytes(mut self, bytes: usize) -> Self {
        self.max_batch_bytes = Some(bytes.max(1));
        self
    }

    /// Insert every row, then [`finish`](BulkWriter::finish). Returns the rows written.
    ///
    /// Returns a boxed `Send` future (as [`start`](Self::start) does) so it can run in a spawned
    /// task: the compiler cannot prove an `async fn` generic over [`Acquire`]'s lifetime `Send`.
    pub fn execute<'c, A, I>(self, db: A, rows: I) -> BoxFuture<'c, Result<u64>>
    where
        A: Acquire<'c> + 'c,
        I: IntoIterator + Send + 'c,
        I::IntoIter: Send,
        I::Item: BulkRow + Send,
    {
        Box::pin(async move {
            let mut w = self.start(db).await?;
            for row in rows {
                w.push(row).await?;
            }
            w.finish().await
        })
    }

    /// Start a writer on a connection from `db` (a pool's connection is held until it is dropped).
    pub fn start<'c, A: Acquire<'c> + 'c>(self, db: A) -> BoxFuture<'c, Result<BulkWriter<A::Connection>>> {
        Box::pin(self.start_on(db))
    }

    async fn start_on<'c, A: Acquire<'c>>(self, db: A) -> Result<BulkWriter<A::Connection>> {
        if self.columns.is_empty() {
            return Err(Error::config("a bulk insert needs at least one column"));
        }
        for (i, c) in self.columns.iter().enumerate() {
            if self.columns[..i].contains(c) {
                return Err(Error::config(format!("column `{c}` is listed twice")));
            }
        }
        let mut conn = db.acquire().await?;
        let max_bytes = match self.max_batch_bytes {
            Some(b) => b,
            None => {
                let packet: u64 =
                    query_scalar("SELECT CAST(@@max_allowed_packet AS UNSIGNED)").fetch_one(&mut *conn).await?;
                let txn = txn_size_limit(&mut conn).await;
                (packet as usize).saturating_sub(PACKET_MARGIN).min(txn / 4).max(1)
            }
        };
        let max_rows = self.max_rows.min(MAX_PARAMS / self.columns.len()).max(1);
        Ok(BulkWriter {
            conn,
            prefix: self.prefix(),
            row_sql: format!("({})", vec!["?"; self.columns.len()].join(", ")),
            suffix: self.suffix(),
            columns: self.columns.len(),
            max_rows,
            max_bytes,
            batch: Arguments::new(),
            rows: 0,
            rows_written: 0,
            rows_affected: 0,
            failed: false,
            full_sql: None,
        })
    }

    fn prefix(&self) -> String {
        let verb = match self.conflict {
            Conflict::Ignore => "INSERT IGNORE INTO",
            Conflict::Replace => "REPLACE INTO",
            Conflict::Fail | Conflict::Update(_) => "INSERT INTO",
        };
        let table: Vec<String> = self.table.splitn(2, '.').map(quote_ident).collect();
        let columns: Vec<String> = self.columns.iter().map(|c| quote_ident(c)).collect();
        format!("{verb} {} ({}) VALUES ", table.join("."), columns.join(", "))
    }

    fn suffix(&self) -> String {
        match &self.conflict {
            Conflict::Update(cols) => {
                let sets: Vec<String> =
                    cols.iter().map(|c| quote_ident(c)).map(|c| format!("{c} = VALUES({c})")).collect();
                format!(" ON DUPLICATE KEY UPDATE {}", sets.join(", "))
            }
            _ => String::new(),
        }
    }
}

/// `performance.txn-total-size-limit`, or TiDB's default where `SHOW CONFIG` is not allowed.
async fn txn_size_limit(conn: &mut TiConnection) -> usize {
    let sql = "SHOW CONFIG WHERE type = 'tidb' AND name = 'performance.txn-total-size-limit'";
    match raw_sql(sql).fetch_optional(&mut *conn).await {
        Ok(Some(row)) => row.try_get::<String, _>(3).ok().and_then(|v| v.parse().ok()).unwrap_or(DEFAULT_TXN_LIMIT),
        Ok(None) => DEFAULT_TXN_LIMIT,
        Err(e) => {
            log::debug!("cannot read txn-total-size-limit ({e}); assuming {DEFAULT_TXN_LIMIT} bytes");
            DEFAULT_TXN_LIMIT
        }
    }
}

/// Streams rows into batched `INSERT`s. Call [`finish`](Self::finish) to send the last batch.
pub struct BulkWriter<C: DerefMut<Target = TiConnection>> {
    conn: C,
    prefix: String,
    row_sql: String,
    suffix: String,
    columns: usize,
    max_rows: usize,
    max_bytes: usize,
    batch: Arguments,
    rows: usize,
    rows_written: u64,
    rows_affected: u64,
    failed: bool,
    /// The SQL of a full batch, built once.
    full_sql: Option<String>,
}

impl<C: DerefMut<Target = TiConnection>> std::fmt::Debug for BulkWriter<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BulkWriter")
            .field("insert", &self.prefix)
            .field("pending_rows", &self.rows)
            .field("rows_written", &self.rows_written)
            .field("max_rows", &self.max_rows)
            .field("max_bytes", &self.max_bytes)
            .finish()
    }
}

impl<C: DerefMut<Target = TiConnection>> BulkWriter<C> {
    /// Add a row; sends a batch when one is full. A row with the wrong number of columns or a
    /// value that fails to encode is rejected here, and the writer carries on without it.
    pub async fn push(&mut self, row: impl BulkRow) -> Result<()> {
        if self.failed {
            return Err(Error::config("a previous batch of this bulk insert failed"));
        }
        let mut args = Arguments::new();
        row.bind(&mut args);
        if let Some(e) = args.error.take() {
            return Err(e);
        }
        if args.len() != self.columns {
            return Err(Error::config(format!(
                "bulk insert row has {} values for {} columns",
                args.len(),
                self.columns
            )));
        }
        if self.rows > 0 && self.batch.wire_len() + args.wire_len() > self.max_bytes {
            self.flush().await?;
        }
        self.batch.append(&args);
        self.rows += 1;
        if self.rows >= self.max_rows {
            self.flush().await?;
        }
        Ok(())
    }

    /// Send the last batch. Returns the rows written in all.
    pub async fn finish(&mut self) -> Result<u64> {
        if self.failed {
            return Err(Error::config("a previous batch of this bulk insert failed"));
        }
        if self.rows > 0 {
            self.flush().await?;
        }
        Ok(self.rows_written)
    }

    /// Rows in the batches sent successfully so far (committed, outside a transaction).
    pub fn rows_written(&self) -> u64 {
        self.rows_written
    }

    /// The servers' affected-row counts so far: fewer than written with `ignore`, more with
    /// `on_duplicate_key_update` (2 per updated row).
    pub fn rows_affected(&self) -> u64 {
        self.rows_affected
    }

    async fn flush(&mut self) -> Result<()> {
        let rows = std::mem::take(&mut self.rows);
        let args = std::mem::take(&mut self.batch);
        // Full batches share one prepared statement; odd sizes stay out of the cache.
        let full = rows == self.max_rows;
        let sql = match (&self.full_sql, full) {
            (Some(sql), true) => sql.clone(),
            _ => {
                let sql = self.sql(rows);
                if full {
                    self.full_sql = Some(sql.clone());
                }
                sql
            }
        };
        match query_with(sql, args).persistent(full).execute(&mut *self.conn).await {
            Ok(r) => {
                self.rows_written += rows as u64;
                self.rows_affected += r.rows_affected;
                Ok(())
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    fn sql(&self, rows: usize) -> String {
        let mut sql = String::with_capacity(self.prefix.len() + rows * (self.row_sql.len() + 2) + self.suffix.len());
        sql.push_str(&self.prefix);
        for i in 0..rows {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&self.row_sql);
        }
        sql.push_str(&self.suffix);
        sql
    }
}

impl<C: DerefMut<Target = TiConnection>> Drop for BulkWriter<C> {
    fn drop(&mut self) {
        if self.rows > 0 && !self.failed {
            log::warn!("bulk insert dropped with {} rows not sent; call `finish()`", self.rows);
        }
    }
}

/// A row for [`BulkWriter::push`]: a tuple of [`Encode`] values (up to 16), or [`Arguments`].
pub trait BulkRow {
    fn bind(self, args: &mut Arguments);
}

impl BulkRow for Arguments {
    fn bind(self, args: &mut Arguments) {
        args.append(&self);
        if let Some(e) = self.error {
            args.error.get_or_insert(e);
        }
    }
}

macro_rules! tuple_row {
    ($($t:ident),+) => {
        impl<$($t: Encode),+> BulkRow for ($($t,)+) {
            #[allow(non_snake_case)]
            fn bind(self, args: &mut Arguments) {
                let ($($t,)+) = self;
                $( args.add($t); )+
            }
        }
    };
}
tuple_row!(A);
tuple_row!(A, B);
tuple_row!(A, B, C);
tuple_row!(A, B, C, D);
tuple_row!(A, B, C, D, E);
tuple_row!(A, B, C, D, E, F);
tuple_row!(A, B, C, D, E, F, G);
tuple_row!(A, B, C, D, E, F, G, H);
tuple_row!(A, B, C, D, E, F, G, H, I);
tuple_row!(A, B, C, D, E, F, G, H, I, J);
tuple_row!(A, B, C, D, E, F, G, H, I, J, K);
tuple_row!(A, B, C, D, E, F, G, H, I, J, K, L);
tuple_row!(A, B, C, D, E, F, G, H, I, J, K, L, M);
tuple_row!(A, B, C, D, E, F, G, H, I, J, K, L, M, N);
tuple_row!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O);
tuple_row!(A, B, C, D, E, F, G, H, I, J, K, L, M, N, O, P);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements() {
        let b = BulkInsert::new("db.t", &["a", "b`c"]);
        assert_eq!(b.prefix(), "INSERT INTO `db`.`t` (`a`, `b``c`) VALUES ");
        assert_eq!(b.clone().ignore().prefix(), "INSERT IGNORE INTO `db`.`t` (`a`, `b``c`) VALUES ");
        assert_eq!(b.clone().replace().prefix(), "REPLACE INTO `db`.`t` (`a`, `b``c`) VALUES ");
        assert_eq!(b.on_duplicate_key_update(&["a"]).suffix(), " ON DUPLICATE KEY UPDATE `a` = VALUES(`a`)");
    }

    #[test]
    fn appended_rows_keep_their_instants() {
        let mut row = Arguments::new();
        row.add(1i64);
        row.instants.push(0); // as if the first value were an instant
        let mut batch = Arguments::new();
        batch.append(&row);
        batch.append(&row);
        assert_eq!(batch.instants, [0, 8], "the second row's offset is shifted by the first row's bytes");
        assert_eq!(batch.len(), 2);
        assert_eq!(batch.wire_len(), 1 + 1 + 4 + 16);
    }
}
