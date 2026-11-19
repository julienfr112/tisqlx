use chrono::{DateTime, TimeZone, Utc};
use tisqlx_core::*;

use crate::common::*;

async fn table(name: &str) -> (ConnectOptions, TiConnection) {
    let opts = fresh_db(name).await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id BIGINT PRIMARY KEY, name VARCHAR(50), n INT NOT NULL DEFAULT 0)")
        .await
        .unwrap();
    (opts, conn)
}

async fn count(conn: &mut TiConnection, sql: &str) -> i64 {
    query_scalar(sql).fetch_one(&mut *conn).await.unwrap()
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn batches_by_rows_and_by_bytes() {
    require_db!();
    let (opts, mut conn) = table("bulkbatches").await;
    // 2 full batches of 1000 and a partial one of 500
    let rows = (1..=2500i64).map(|i| (i, format!("row-{i}")));
    let n = BulkInsert::new("t", &["id", "name"]).execute(&mut conn, rows).await.unwrap();
    assert_eq!(n, 2500);
    assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM t").await, 2500);
    assert_eq!(count(&mut conn, "SELECT CAST(SUM(id) AS SIGNED) FROM t").await, 2500 * 2501 / 2);
    // A small byte budget: many batches cut by size, each the same as a full one would be.
    conn.execute_raw("TRUNCATE TABLE t").await.unwrap();
    let pool = PoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    let mut w = BulkInsert::new("t", &["id", "name"]).max_batch_bytes(500).start(&pool).await.unwrap();
    for i in 1..=300i64 {
        w.push((i, format!("row-{i}"))).await.unwrap();
    }
    assert!(w.rows_written() > 250, "most rows went out in byte-sized batches: {w:?}");
    assert_eq!(w.finish().await.unwrap(), 300);
    drop(w);
    assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM t").await, 300);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn instants_and_nulls_in_a_non_utc_session() {
    require_db!();
    let opts = fresh_db("bulktz").await.timezone(Some("Europe/Paris"));
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE t (id INT PRIMARY KEY, at TIMESTAMP(6) NULL, note VARCHAR(10))").await.unwrap();
    let at = |i: i64| Utc.timestamp_opt(1_780_000_000 + i * 86_400 * 13, 123_000).unwrap();
    let rows = (0..50i64).map(|i| (i, (i % 3 != 0).then(|| at(i)), (i % 2 == 0).then(|| format!("n{i}"))));
    BulkInsert::new("t", &["id", "at", "note"]).max_rows(7).execute(&mut conn, rows).await.unwrap();
    let back: Vec<(i64, Option<DateTime<Utc>>, Option<String>)> =
        query_as("SELECT id, at, note FROM t ORDER BY id").fetch_all(&mut conn).await.unwrap();
    for (i, a, note) in back {
        assert_eq!(a, (i % 3 != 0).then(|| at(i)), "row {i}");
        assert_eq!(note, (i % 2 == 0).then(|| format!("n{i}")), "row {i}");
    }
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn conflicts_ignore_replace_update() {
    require_db!();
    let (_, mut conn) = table("bulkconflict").await;
    conn.execute_raw("INSERT INTO t VALUES (1, 'old', 5)").await.unwrap();
    let mut w = BulkInsert::new("t", &["id", "name"]).ignore().start(&mut conn).await.unwrap();
    w.push((1i64, "ignored")).await.unwrap();
    w.push((2i64, "two")).await.unwrap();
    w.finish().await.unwrap();
    assert_eq!(w.rows_affected(), 1, "the duplicate is skipped");
    drop(w);
    let name: String = query_scalar("SELECT name FROM t WHERE id = 1").fetch_one(&mut conn).await.unwrap();
    assert_eq!(name, "old");

    BulkInsert::new("t", &["id", "name"]).replace().execute(&mut conn, [(1i64, "replaced")]).await.unwrap();
    let (name, n): (String, i32) = query_as("SELECT name, n FROM t WHERE id = 1").fetch_one(&mut conn).await.unwrap();
    assert_eq!((name.as_str(), n), ("replaced", 0), "REPLACE deletes and reinserts");

    BulkInsert::new("t", &["id", "name", "n"])
        .on_duplicate_key_update(&["n"])
        .execute(&mut conn, [(1i64, "kept", 9), (3, "three", 1)])
        .await
        .unwrap();
    let (name, n): (String, i32) = query_as("SELECT name, n FROM t WHERE id = 1").fetch_one(&mut conn).await.unwrap();
    assert_eq!((name.as_str(), n), ("replaced", 9), "only `n` is updated");
    assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM t").await, 3);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn a_failed_batch_reports_what_was_written() {
    require_db!();
    let (_, mut conn) = table("bulkfail").await;
    conn.execute_raw("INSERT INTO t (id) VALUES (15)").await.unwrap();
    let mut w = BulkInsert::new("t", &["id"]).max_rows(10).start(&mut conn).await.unwrap();
    // a bad row is rejected on its own, the writer carries on
    let err = w.push((1i64, "extra")).await.unwrap_err();
    assert!(err.to_string().contains("2 values for 1 columns"), "{err}");
    let mut failed = None;
    for i in 1..=30i64 {
        if let Err(e) = w.push((i,)).await {
            failed = Some((i, e));
            break;
        }
    }
    // rows 11..=20 hold the duplicate 15: that batch fails when row 20 completes it
    let (at, e) = failed.expect("the second batch fails");
    assert_eq!(at, 20);
    assert!(matches!(&e, Error::Database(d) if d.code == 1062), "{e}");
    assert_eq!(w.rows_written(), 10);
    assert!(w.push((99i64,)).await.is_err(), "no more rows after a failed batch");
    assert!(w.finish().await.is_err());
    drop(w);
    assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM t").await, 11, "batch 1 plus the pre-existing row");
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn a_cancelled_batch_fails_the_writer() {
    require_db!();
    let (opts, mut conn) = table("bulkcancel").await;
    // Another session holds the lock on id 10: the batch that inserts it has to wait.
    let mut blocker = opts.connect().await.unwrap();
    let mut tx = blocker.begin_with(&TxnOptions::pessimistic()).await.unwrap();
    tx.execute_raw("INSERT INTO t (id) VALUES (10)").await.unwrap();

    let mut w = BulkInsert::new("t", &["id"]).max_rows(10).start(&mut conn).await.unwrap();
    for i in 1..10i64 {
        w.push((i,)).await.unwrap();
    }
    let cancelled = tokio::time::timeout(std::time::Duration::from_millis(300), w.push((10i64,))).await;
    assert!(cancelled.is_err(), "the batch waits for the lock");
    // The ten rows are neither written nor pending: the writer must not report success.
    let err = w.finish().await.unwrap_err();
    assert!(err.to_string().contains("interrupted"), "{err}");
    assert!(w.push((11i64,)).await.is_err());
    assert_eq!(w.rows_written(), 0);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn configuration_errors() {
    require_db!();
    let (_, mut conn) = table("bulkconfig").await;
    for (insert, expected) in [
        (BulkInsert::new("t", &[]), "at least one column"),
        (BulkInsert::new("t", &["id", "n", "id"]), "`id` is listed twice"),
        (BulkInsert::new("t", &["id"]).on_duplicate_key_update(&[]), "`on_duplicate_key_update` needs"),
    ] {
        let err = insert.execute(&mut conn, [(1i64,)]).await.unwrap_err();
        assert!(matches!(err, Error::Configuration(_)) && err.to_string().contains(expected), "{err}");
    }
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn inside_a_transaction_it_is_all_or_nothing() {
    require_db!();
    let (_, mut conn) = table("bulktx").await;
    let mut tx = conn.begin().await.unwrap();
    BulkInsert::new("t", &["id"]).max_rows(100).execute(&mut tx, (1..=1000i64).map(|i| (i,))).await.unwrap();
    assert_eq!(count(&mut tx, "SELECT COUNT(*) FROM t").await, 1000);
    tx.rollback().await.unwrap();
    assert_eq!(count(&mut conn, "SELECT COUNT(*) FROM t").await, 0);
}
