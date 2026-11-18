use tisqlx_core::*;

use crate::common::*;

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn transactions_and_savepoints() {
    require_db!();
    let opts = fresh_db("tx").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id INT PRIMARY KEY)").await.unwrap();

    let mut tx = conn.begin().await.unwrap();
    query("INSERT INTO t VALUES (1)").execute(&mut *tx).await.unwrap();
    {
        let mut sp = tx.begin().await.unwrap();
        query("INSERT INTO t VALUES (2)").execute(&mut *sp).await.unwrap();
        // dropped: rolls back to savepoint
    }
    {
        let mut sp = tx.begin().await.unwrap();
        query("INSERT INTO t VALUES (3)").execute(&mut *sp).await.unwrap();
        sp.commit().await.unwrap();
    }
    tx.commit().await.unwrap();
    let ids: Vec<i32> = query_scalar("SELECT id FROM t ORDER BY id").fetch_all(&mut conn).await.unwrap();
    assert_eq!(ids, vec![1, 3]);

    {
        let mut tx = conn.begin().await.unwrap();
        query("INSERT INTO t VALUES (4)").execute(&mut *tx).await.unwrap();
    }
    let n: i64 = query_scalar("SELECT COUNT(*) FROM t").fetch_one(&mut conn).await.unwrap();
    assert_eq!(n, 2);
    assert!(!conn.in_transaction());

    let tx = conn.begin_with(&TxnOptions::optimistic()).await.unwrap();
    tx.rollback().await.unwrap();
}

async fn count(conn: &mut TiConnection) -> i64 {
    query_scalar("SELECT COUNT(*) FROM t").fetch_one(conn).await.unwrap()
}

async fn ids(conn: &mut TiConnection) -> Vec<i32> {
    query_scalar("SELECT id FROM t ORDER BY id").fetch_all(conn).await.unwrap()
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn abandoned_savepoints_roll_back_to_the_right_level() {
    require_db!();
    let opts = fresh_db("txnest").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id INT PRIMARY KEY)").await.unwrap();

    // inner dropped, then outer committed: only the inner work is lost
    let mut tx = conn.begin().await.unwrap();
    query("INSERT INTO t VALUES (1)").execute(&mut *tx).await.unwrap();
    {
        let mut sp = tx.begin().await.unwrap();
        assert_eq!(sp.depth(), 2);
        query("INSERT INTO t VALUES (2)").execute(&mut *sp).await.unwrap();
        let mut sp2 = sp.begin().await.unwrap();
        assert_eq!(sp2.depth(), 3);
        query("INSERT INTO t VALUES (3)").execute(&mut *sp2).await.unwrap();
        // both dropped: rolls back to level 2 in one statement
    }
    {
        // a new savepoint after the abandoned ones reuses level 2
        let mut sp = tx.begin().await.unwrap();
        assert_eq!(sp.depth(), 2);
        query("INSERT INTO t VALUES (4)").execute(&mut *sp).await.unwrap();
        sp.commit().await.unwrap();
    }
    tx.commit().await.unwrap();
    assert_eq!(ids(&mut conn).await, [1, 4]);
    assert!(!conn.in_transaction());

    // inner dropped, then outer rolled back; the connection stays usable
    let mut tx = conn.begin().await.unwrap();
    query("INSERT INTO t VALUES (5)").execute(&mut *tx).await.unwrap();
    {
        let mut sp = tx.begin().await.unwrap();
        query("INSERT INTO t VALUES (6)").execute(&mut *sp).await.unwrap();
    }
    tx.rollback().await.unwrap();
    assert_eq!(ids(&mut conn).await, [1, 4]);
    assert!(!conn.in_transaction());

    // inner committed but outer dropped: everything is rolled back
    {
        let mut tx = conn.begin().await.unwrap();
        let mut sp = tx.begin().await.unwrap();
        query("INSERT INTO t VALUES (7)").execute(&mut *sp).await.unwrap();
        sp.commit().await.unwrap();
    }
    assert_eq!(ids(&mut conn).await, [1, 4]);
    assert!(!conn.in_transaction());

    // a new top-level transaction after all that starts at depth 1
    let tx = conn.begin().await.unwrap();
    assert_eq!(tx.depth(), 1);
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn begin_options_are_applied() {
    require_db!();
    let opts = fresh_db("txopts").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id INT PRIMARY KEY)").await.unwrap();

    // isolation level and BEGIN go out as one multi-statement request; READ COMMITTED
    // (pessimistic only) sees rows another session commits mid-transaction
    let mut other = opts.connect().await.unwrap();
    for (isolation, expected) in [(Isolation::ReadCommitted, 1), (Isolation::RepeatableRead, 0)] {
        conn.execute_raw("DELETE FROM t").await.unwrap();
        let mut tx = conn
            .begin_with(&TxnOptions::ReadWrite { mode: Some(TxnMode::Pessimistic), isolation: Some(isolation) })
            .await
            .unwrap();
        assert!(tx.in_transaction());
        assert_eq!(count(&mut tx).await, 0);
        other.execute_raw("INSERT INTO t VALUES (1)").await.unwrap();
        assert_eq!(count(&mut tx).await, expected, "{isolation:?}");
        tx.commit().await.unwrap();
    }

    // options cannot apply to a savepoint
    let mut tx = conn.begin().await.unwrap();
    let nested = tx.begin_with(&TxnOptions::optimistic()).await.map(drop);
    assert!(matches!(nested, Err(Error::Configuration(_))), "{nested:?}");
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn nesting_inside_a_stale_read() {
    // TiDB accepts `SAVEPOINT` in a stale-read transaction but creates nothing, so
    // `RELEASE SAVEPOINT` and `ROLLBACK TO SAVEPOINT` fail there.
    require_db!();
    let opts = fresh_db("txstale").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id INT PRIMARY KEY); INSERT INTO t VALUES (1)").await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let stale = TxnOptions::stale_read(StaleRead::ExactStaleness(std::time::Duration::from_millis(100)));
    let mut tx = conn.begin_with(&stale).await.unwrap();
    let mut nested = tx.begin().await.unwrap();
    assert_eq!(nested.depth(), 2);
    assert_eq!(count(&mut nested).await, 1);
    nested.commit().await.unwrap();
    drop(tx.begin().await.unwrap()); // abandoned
    tx.begin().await.unwrap().rollback().await.unwrap();
    assert_eq!(count(&mut tx).await, 1, "the stale read is still open");
    assert!(tx.in_transaction());
    tx.commit().await.unwrap();

    // savepoints are back in the next read-write transaction
    let mut tx = conn.begin().await.unwrap();
    {
        let mut sp = tx.begin().await.unwrap();
        query("INSERT INTO t VALUES (2)").execute(&mut *sp).await.unwrap();
    }
    tx.commit().await.unwrap();
    assert_eq!(count(&mut conn).await, 1);
}

/// Written once, callable with a pool, a connection, a pooled connection or a transaction.
async fn add_user<'c>(db: impl Acquire<'c>, name: &str) -> Result<()> {
    let mut tx = db.begin().await?;
    query("INSERT INTO t (name) VALUES (?)").bind(name).execute(&mut *tx).await?;
    tx.commit().await
}

async fn count_users<'c>(db: impl Acquire<'c>) -> i64 {
    let mut conn = db.acquire().await.unwrap();
    query_scalar("SELECT COUNT(*) FROM t").fetch_one(&mut *conn).await.unwrap()
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn acquire_accepts_pools_connections_and_transactions() {
    require_db!();
    let opts = fresh_db("acquire").await;
    let pool = PoolOptions::new().max_connections(2).connect_with(opts.clone()).await.unwrap();
    raw_sql("CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR(20))").execute(&pool).await.unwrap();
    add_user(&pool, "pool").await.unwrap();
    let mut conn = opts.connect().await.unwrap();
    add_user(&mut conn, "connection").await.unwrap();
    let mut pooled = pool.acquire().await.unwrap();
    add_user(&mut pooled, "pooled").await.unwrap();
    assert_eq!(count_users(&mut pooled).await, 3);
    drop(pooled);
    // Inside a transaction, `begin` is a savepoint: the outer rollback undoes the inner commit.
    let mut tx = pool.begin().await.unwrap();
    add_user(&mut tx, "rolled back").await.unwrap();
    assert_eq!(count_users(&mut tx).await, 4);
    tx.rollback().await.unwrap();
    assert_eq!(count_users(&pool).await, 3);
    assert_eq!(count_users(&mut conn).await, 3);
}
