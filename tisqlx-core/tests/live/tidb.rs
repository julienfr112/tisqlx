use std::time::Duration;

use tisqlx_core::*;

use crate::common::*;

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn tidb_features() {
    require_db!();
    let opts = fresh_db("tidbfeat").await;
    let mut conn = opts.clone().txn_mode(TxnMode::Optimistic).connect().await.unwrap();
    let mode: String = query_scalar("SELECT @@tidb_txn_mode").fetch_one(&mut conn).await.unwrap();
    assert_eq!(mode, "optimistic");

    conn.execute_raw("CREATE TABLE ar (id BIGINT PRIMARY KEY AUTO_RANDOM, v INT)").await.unwrap();
    let r = query("INSERT INTO ar (v) VALUES (?)").bind(1).execute(&mut conn).await.unwrap();
    assert!(r.last_insert_id() > 0);
    let id: i64 = query_scalar("SELECT id FROM ar").fetch_one(&mut conn).await.unwrap();
    assert_eq!(id as u64, r.last_insert_id());

    let mut tx = conn.begin().await.unwrap();
    let tso = tx.current_tso().await.unwrap();
    assert!(tso.0 > 0);
    let now = std::time::SystemTime::now();
    let age = now.duration_since(tso.to_system_time()).unwrap_or_default();
    assert!(age < Duration::from_secs(60), "TSO physical time should be recent: {age:?}");
    tx.commit().await.unwrap();

    // Stale reads: table did not exist an hour ago, but `SELECT 1` works.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut tx =
        conn.begin_with(&TxnOptions::stale_read(StaleRead::ExactStaleness(Duration::from_millis(500)))).await.unwrap();
    let n: i64 = query_scalar("SELECT COUNT(*) FROM ar").fetch_one(&mut *tx).await.unwrap();
    assert_eq!(n, 1);
    assert!(query("INSERT INTO ar (v) VALUES (2)").execute(&mut *tx).await.is_err(), "stale read txn is read-only");
    tx.commit().await.unwrap();

    conn.set_read_staleness(Some(Duration::from_secs(1))).await.unwrap();
    conn.set_read_staleness(None).await.unwrap();
    conn.set_isolation_read_engines(&[Engine::TiKV, Engine::TiDB]).await.unwrap();
    assert!(conn.tidb_version_info().await.unwrap().contains("Release Version"));

    // Write conflict surfaces as retryable.
    let mut a = opts.clone().connect().await.unwrap();
    let mut b = opts.connect().await.unwrap();
    a.execute_raw("CREATE TABLE c (id INT PRIMARY KEY, v INT); INSERT INTO c VALUES (1, 0)").await.unwrap();
    let mut ta = a.begin_with(&TxnOptions::optimistic()).await.unwrap();
    let mut tb = b.begin_with(&TxnOptions::optimistic()).await.unwrap();
    query("UPDATE c SET v = 1 WHERE id = 1").execute(&mut *ta).await.unwrap();
    query("UPDATE c SET v = 2 WHERE id = 1").execute(&mut *tb).await.unwrap();
    ta.commit().await.unwrap();
    let err = tb.commit().await.unwrap_err();
    assert!(err.is_retryable(), "{err}");
    assert!(err.as_database_error().unwrap().is_write_conflict());
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn resource_group_option_binds_the_session() {
    require_db!();
    let mut admin = TiConnection::connect(&base_url()).await.unwrap();
    admin.execute_raw("CREATE RESOURCE GROUP IF NOT EXISTS tisqlx_rg RU_PER_SEC = 1000").await.unwrap();
    let opts: ConnectOptions = format!("{}?resource-group=tisqlx_rg", base_url()).parse().unwrap();
    let mut conn = opts.connect().await.unwrap();
    let rg: String = query_scalar("SELECT CURRENT_RESOURCE_GROUP()").fetch_one(&mut conn).await.unwrap();
    assert_eq!(rg, "tisqlx_rg");
    // an unknown group fails at connect time, not later
    let bad: ConnectOptions = format!("{}?resource-group=tisqlx_nope", base_url()).parse().unwrap();
    assert!(bad.connect().await.is_err());
}
