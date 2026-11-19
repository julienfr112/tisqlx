//! Behaviour that needs a real cluster (PD + TiKV + TiFlash, two TiDB nodes):
//! `make db-cluster && make test-cluster`. Skipped otherwise.

use std::time::Duration;

use tisqlx_core::*;

use crate::common::*;

macro_rules! require_cluster {
    () => {
        match cluster() {
            Some(c) => c,
            None => return,
        }
    };
}

#[tokio::test]
#[ignore = "needs a TiDB cluster: run with `make test-cluster`"]
async fn reads_from_tiflash() {
    let c = require_cluster!();
    let (a, _) = fresh_cluster_db("tiflash", &c).await;
    let mut conn = a.connect().await.unwrap();
    conn.execute_raw(
        "CREATE TABLE t (id INT PRIMARY KEY, v INT); INSERT INTO t VALUES (1, 10), (2, 20); \
         ALTER TABLE t SET TIFLASH REPLICA 1",
    )
    .await
    .unwrap();
    let mut available = false;
    for _ in 0..120 {
        let n: i64 = query_scalar(
            "SELECT COUNT(*) FROM information_schema.tiflash_replica WHERE table_schema = DATABASE() AND available = 1",
        )
        .fetch_one(&mut conn)
        .await
        .unwrap();
        if n == 1 {
            available = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(available, "TiFlash replica became available");
    // TiFlash only: the plan must use it, or the query fails
    conn.set_isolation_read_engines(&[Engine::TiFlash]).await.unwrap();
    let sum: rust_decimal::Decimal = query_scalar("SELECT SUM(v) FROM t").fetch_one(&mut conn).await.unwrap();
    assert_eq!(sum, rust_decimal::Decimal::from(30));
    let plan: Vec<String> = query_scalar("EXPLAIN SELECT SUM(v) FROM t").fetch_all(&mut conn).await.unwrap();
    let plan = plan.join("\n");
    let explain = raw_sql("EXPLAIN SELECT SUM(v) FROM t").fetch_all(&mut conn).await.unwrap();
    let tasks: Vec<String> = explain.iter().map(|r| r.get::<String, _>("task")).collect();
    assert!(tasks.iter().any(|t| t.contains("tiflash")), "plan uses TiFlash: {tasks:?}\n{plan}");
}

#[tokio::test]
#[ignore = "needs a TiDB cluster: run with `make test-cluster`"]
async fn ddl_on_another_node_invalidates_cached_statements() {
    let c = require_cluster!();
    let (a, b) = fresh_cluster_db("ddl2", &c).await;
    let mut on_a = a.connect().await.unwrap();
    let mut on_b = b.connect().await.unwrap();
    assert_ne!(on_a.connection_id(), 0);
    on_a.execute_raw("CREATE TABLE t (id INT PRIMARY KEY); INSERT INTO t VALUES (1)").await.unwrap();
    let row = query("SELECT * FROM t WHERE id = ?").bind(1).fetch_one(&mut on_a).await.unwrap();
    assert_eq!(row.len(), 1);
    // node B changes the schema; node A's cached prepared statement is now stale
    on_b.execute_raw("ALTER TABLE t ADD COLUMN x INT NOT NULL DEFAULT 7").await.unwrap();
    let row = query("SELECT * FROM t WHERE id = ?").bind(1).fetch_one(&mut on_a).await.unwrap();
    assert_eq!(row.len(), 2, "the new column is visible through the cached statement");
    assert_eq!(row.get::<i32, _>("x"), 7);
}

#[tokio::test]
#[ignore = "needs a TiDB cluster: run with `make test-cluster`"]
async fn stale_reads_see_the_past() {
    let c = require_cluster!();
    let (a, _) = fresh_cluster_db("stale", &c).await;
    let mut conn = a.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id INT PRIMARY KEY, v INT); INSERT INTO t VALUES (1, 1)").await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let before: chrono::NaiveDateTime = query_scalar("SELECT NOW(6)").fetch_one(&mut conn).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    conn.execute_raw("UPDATE t SET v = 2 WHERE id = 1").await.unwrap();

    let mut tx = conn.begin_with(&TxnOptions::stale_read(StaleRead::At(before))).await.unwrap();
    let v: i32 = query_scalar("SELECT v FROM t WHERE id = 1").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(v, 1, "read as of before the update");
    tx.commit().await.unwrap();

    // Bounded staleness begins and reads; TiDB may pick any timestamp in the window, so either
    // version. The window must not reach back before the table existed (~2s ago).
    let mut tx =
        conn.begin_with(&TxnOptions::stale_read(StaleRead::MaxStaleness(Duration::from_millis(500)))).await.unwrap();
    let _: i32 = query_scalar("SELECT v FROM t WHERE id = 1").fetch_one(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let v: i32 = query_scalar("SELECT v FROM t WHERE id = 1").fetch_one(&mut conn).await.unwrap();
    assert_eq!(v, 2);
}

#[tokio::test]
#[ignore = "needs a TiDB cluster: run with `make test-cluster`"]
async fn pessimistic_lock_wait_timeout_is_retryable() {
    let c = require_cluster!();
    let (a, b) = fresh_cluster_db("locks", &c).await;
    let mut holder = a.connect().await.unwrap();
    let mut waiter = b.connect().await.unwrap();
    holder.execute_raw("CREATE TABLE t (id INT PRIMARY KEY, v INT); INSERT INTO t VALUES (1, 0)").await.unwrap();
    let mut tx = holder.begin_with(&TxnOptions::pessimistic()).await.unwrap();
    query("SELECT * FROM t WHERE id = 1 FOR UPDATE").execute(&mut *tx).await.unwrap();

    waiter.execute_raw("SET SESSION innodb_lock_wait_timeout = 1").await.unwrap();
    let mut wtx = waiter.begin_with(&TxnOptions::pessimistic()).await.unwrap();
    let err = query("UPDATE t SET v = 1 WHERE id = 1").execute(&mut *wtx).await.unwrap_err();
    assert_eq!(err.as_database_error().map(|d| d.code), Some(1205), "{err}");
    assert!(err.is_retryable());
    drop(wtx);
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "needs a TiDB cluster: run with `make test-cluster`"]
async fn pool_spreads_over_both_tidb_nodes() {
    let nodes = require_cluster!();
    let (a, _) = fresh_cluster_db("spread", &nodes).await;
    let urls = [&nodes.0, &nodes.1].map(|u| u.parse::<ConnectOptions>().unwrap());
    let hosts = urls.iter().map(|o| (o.get_host().to_owned(), o.get_port()));
    let pool = PoolOptions::new().min_connections(6).max_connections(6).connect_with(a.hosts(hosts)).await.unwrap();
    let conns: Vec<_> = futures_util::future::try_join_all((0..6).map(|_| pool.acquire())).await.unwrap();
    let mut ports: Vec<u16> = conns.iter().map(|c| c.server_addr().unwrap().port()).collect();
    ports.sort();
    let (p0, p1) = (urls[0].get_port(), urls[1].get_port());
    assert_eq!(ports, [p0, p0, p0, p1, p1, p1], "3 connections on each node");
}
