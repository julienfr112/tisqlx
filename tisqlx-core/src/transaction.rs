//! Transactions, with savepoint-based nesting.

use std::ops::{Deref, DerefMut};

use crate::connection::TiConnection;
use crate::error::{Error, Result};
use crate::pool::PoolConnection;
use crate::tidb::TxnOptions;

const SAVEPOINT_PREFIX: &str = "_tisqlx_sp_";

#[allow(clippy::large_enum_variant)]
pub(crate) enum MaybePoolConnection<'c> {
    Connection(&'c mut TiConnection),
    Pool(PoolConnection),
}

impl Deref for MaybePoolConnection<'_> {
    type Target = TiConnection;

    fn deref(&self) -> &TiConnection {
        match self {
            Self::Connection(c) => c,
            Self::Pool(c) => c,
        }
    }
}

impl DerefMut for MaybePoolConnection<'_> {
    fn deref_mut(&mut self) -> &mut TiConnection {
        match self {
            Self::Connection(c) => c,
            Self::Pool(c) => c,
        }
    }
}

/// An in-progress transaction (or savepoint, when nested).
///
/// Dropping it without calling [`commit`](Self::commit) rolls it back: the
/// rollback is sent before the connection's next use (or before a pooled
/// connection is reused).
pub struct Transaction<'c> {
    conn: MaybePoolConnection<'c>,
    /// 1 for a top-level transaction, 2+ for savepoints.
    depth: usize,
    open: bool,
}

pub(crate) fn begin_sql(depth: usize, options: &TxnOptions) -> Result<String> {
    if depth == 1 {
        Ok(options.begin_sql())
    } else if options.is_default() {
        Ok(format!("SAVEPOINT {SAVEPOINT_PREFIX}{depth}"))
    } else {
        Err(Error::config("transaction options cannot be applied to a nested transaction (savepoint)"))
    }
}

pub(crate) fn commit_sql(depth: usize) -> String {
    if depth == 1 { "COMMIT".into() } else { format!("RELEASE SAVEPOINT {SAVEPOINT_PREFIX}{depth}") }
}

/// SQL rolling back transaction level `depth` (and everything nested in it).
pub(crate) fn rollback_sql(depth: usize) -> String {
    if depth <= 1 { "ROLLBACK".into() } else { format!("ROLLBACK TO SAVEPOINT {SAVEPOINT_PREFIX}{depth}") }
}

impl<'c> Transaction<'c> {
    pub(crate) async fn begin_on(mut conn: MaybePoolConnection<'c>, options: &TxnOptions) -> Result<Self> {
        // Flush abandoned inner transactions first: they change the depth.
        conn.wait_until_ready().await?;
        let depth = conn.transaction_depth + 1;
        let sql = begin_sql(depth, options)?;
        if let Err(e) = conn.execute_raw(&sql).await {
            if matches!(options, TxnOptions::ReadWrite { isolation: Some(_), .. }) {
                // `SET TRANSACTION ISOLATION LEVEL` may have run without the `BEGIN`: its
                // one-shot level would apply to the connection's next transaction.
                conn.mark_broken();
            }
            return Err(e);
        }
        conn.transaction_depth = depth;
        Ok(Self { conn, depth, open: true })
    }

    /// Nesting depth: 1 for a top-level transaction, 2+ for savepoints.
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Start a nested transaction backed by a savepoint.
    pub async fn begin(&mut self) -> Result<Transaction<'_>> {
        Transaction::begin_on(MaybePoolConnection::Connection(&mut self.conn), &TxnOptions::default()).await
    }

    pub async fn commit(mut self) -> Result<()> {
        self.conn.wait_until_ready().await?;
        if self.conn.transaction_depth < self.depth {
            // An inner rollback failed and took this level down with it.
            self.open = false;
            return Err(Error::protocol("the transaction was already rolled back; nothing was committed"));
        }
        self.conn.execute_raw(&commit_sql(self.depth)).await?;
        self.conn.transaction_depth = self.depth - 1;
        self.open = false;
        Ok(())
    }

    pub async fn rollback(mut self) -> Result<()> {
        // Mark closed first: if the rollback fails, the server already discarded
        // the transaction (or the connection is broken).
        self.open = false;
        self.conn.wait_until_ready().await?;
        if self.conn.transaction_depth < self.depth {
            return Ok(()); // already rolled back
        }
        self.conn.rollback_level(self.depth).await
    }
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Transaction").field("depth", &self.depth).field("open", &self.open).finish()
    }
}

impl Deref for Transaction<'_> {
    type Target = TiConnection;

    fn deref(&self) -> &TiConnection {
        &self.conn
    }
}

impl DerefMut for Transaction<'_> {
    fn deref_mut(&mut self) -> &mut TiConnection {
        &mut self.conn
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if self.open {
            let level = self.conn.rollback_to.map_or(self.depth, |l| l.min(self.depth));
            self.conn.rollback_to = Some(level);
        }
    }
}

impl TiConnection {
    /// Begin a transaction (or a savepoint if one is already open on this connection).
    pub async fn begin(&mut self) -> Result<Transaction<'_>> {
        Transaction::begin_on(MaybePoolConnection::Connection(self), &TxnOptions::default()).await
    }

    /// Begin a transaction with TiDB-specific options (mode, stale read, isolation...).
    pub async fn begin_with(&mut self, options: &TxnOptions) -> Result<Transaction<'_>> {
        Transaction::begin_on(MaybePoolConnection::Connection(self), options).await
    }
}

impl PoolConnection {
    /// Begin a transaction that owns this pooled connection.
    pub async fn begin_owned(self, options: &TxnOptions) -> Result<Transaction<'static>> {
        Transaction::begin_on(MaybePoolConnection::Pool(self), options).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn savepoint_sql() {
        assert_eq!(commit_sql(1), "COMMIT");
        assert_eq!(commit_sql(2), "RELEASE SAVEPOINT _tisqlx_sp_2");
        assert_eq!(rollback_sql(1), "ROLLBACK");
        assert_eq!(rollback_sql(3), "ROLLBACK TO SAVEPOINT _tisqlx_sp_3");
        assert_eq!(begin_sql(2, &TxnOptions::default()).unwrap(), "SAVEPOINT _tisqlx_sp_2");
    }
}
