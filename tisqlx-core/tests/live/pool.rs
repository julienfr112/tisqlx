use std::time::Duration;

use tisqlx_core::*;

use crate::common::*;

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn pool_and_retry_on_write_conflict() {
    require_db!();
    let opts = fresh_db("retry").await;
    let pool = PoolOptions::new().max_connections(8).connect_with(opts).await.unwrap();
    raw_sql("CREATE TABLE counter (id INT PRIMARY KEY, v INT NOT NULL); INSERT INTO counter VALUES (1, 0)")
        .execute(&pool)
        .await
        .unwrap();

    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut handles = vec![];
    for _ in 0..8 {
        let pool = pool.clone();
        let attempts = attempts.clone();
        handles.push(tokio::spawn(async move {
            pool.transaction_with(&TxnOptions::optimistic(), RetryPolicy::default().max_attempts(50), |tx| {
                let attempts = attempts.clone();
                Box::pin(async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let v: i32 = query_scalar("SELECT v FROM counter WHERE id = 1").fetch_one(&mut **tx).await?;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    query("UPDATE counter SET v = ? WHERE id = 1").bind(v + 1).execute(&mut **tx).await?;
                    Ok::<_, Error>(())
                })
            })
            .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    let v: i32 = query_scalar("SELECT v FROM counter WHERE id = 1").fetch_one(&pool).await.unwrap();
    assert_eq!(v, 8, "every increment must survive");
    assert!(attempts.load(std::sync::atomic::Ordering::Relaxed) > 8, "expected at least one conflict retry");
    assert!(pool.size() <= 8);
    pool.close().await;
    assert!(matches!(pool.acquire().await, Err(Error::PoolClosed)));
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn pool_returns_dirty_connections_clean() {
    require_db!();
    let opts = fresh_db("pooldirty").await;
    let pool = PoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    raw_sql("CREATE TABLE t (id INT PRIMARY KEY)").execute(&pool).await.unwrap();
    {
        let mut tx = pool.begin().await.unwrap();
        query("INSERT INTO t VALUES (1)").execute(&mut *tx).await.unwrap();
    }
    // Same single connection; the dropped transaction must have been rolled back.
    let mut c = pool.acquire().await.unwrap();
    assert!(!c.in_transaction());
    let n: i64 = query_scalar("SELECT COUNT(*) FROM t").fetch_one(&mut *c).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn pool_size_tracks_every_exit_path() {
    require_db!();
    let opts = fresh_db("poolsize").await;
    let pool = PoolOptions::new().max_connections(3).connect_with(opts.clone()).await.unwrap();
    assert_eq!(pool.size(), 1);

    let (a, b, c) = (pool.acquire().await.unwrap(), pool.acquire().await.unwrap(), pool.acquire().await.unwrap());
    assert_eq!(pool.size(), 3);
    drop(a); // back to idle
    assert_eq!((pool.size(), pool.num_idle()), (3, 1));
    let raw = b.detach(); // leaves the pool
    assert_eq!(pool.size(), 2);
    raw.close().await.unwrap();
    c.close().await.unwrap(); // closed instead of returned
    assert_eq!(pool.size(), 1);

    // a failed connect does not leak a slot
    let bad = PoolOptions::new().connect_lazy_with(opts.clone().password("wrong"));
    assert!(bad.acquire().await.is_err());
    assert_eq!(bad.size(), 0);

    // an expired idle connection is replaced
    let short = PoolOptions::new().max_lifetime(Some(Duration::from_millis(50))).connect_with(opts).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let conn = short.acquire().await.unwrap();
    assert_eq!(short.size(), 1);
    drop(conn);

    pool.close().await;
    assert_eq!(pool.size(), 0);

    // min_connections above max_connections is clamped instead of stalling on permits
    let opts = fresh_db("poolmin").await;
    let clamped = tokio::time::timeout(
        Duration::from_secs(10),
        PoolOptions::new()
            .max_connections(2)
            .min_connections(5)
            .acquire_timeout(Duration::from_secs(30))
            .connect_with(opts),
    )
    .await
    .expect("connect must not wait for permits")
    .unwrap();
    assert_eq!(clamped.size(), 2);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn maintenance_never_opens_more_than_max_connections() {
    require_db!();
    let opts = fresh_db("poolminmax").await;
    let pool = PoolOptions::new()
        .max_connections(2)
        .min_connections(5)
        .idle_timeout(Some(Duration::from_millis(100))) // maintenance every 50 ms
        .connect_with(opts)
        .await
        .unwrap();
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(pool.size() <= 2, "{} connections open with max_connections = 2", pool.size());
    }
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn dropped_pessimistic_transaction_releases_its_locks() {
    require_db!();
    let opts = fresh_db("poollock").await;
    let pool = PoolOptions::new().max_connections(2).connect_with(opts).await.unwrap();
    raw_sql("CREATE TABLE t (id INT PRIMARY KEY, v INT); INSERT INTO t VALUES (1, 0)").execute(&pool).await.unwrap();
    {
        let mut tx = pool.begin_with(&TxnOptions::pessimistic()).await.unwrap();
        query("SELECT * FROM t WHERE id = 1 FOR UPDATE").execute(&mut *tx).await.unwrap();
        // dropped while holding the row lock
    }
    let mut tx = pool.begin_with(&TxnOptions::pessimistic()).await.unwrap();
    raw_sql("SET SESSION innodb_lock_wait_timeout = 2").execute(&mut *tx).await.unwrap();
    let locked =
        tokio::time::timeout(Duration::from_secs(5), query("UPDATE t SET v = 1 WHERE id = 1").execute(&mut *tx))
            .await
            .expect("lock released by the pool's rollback");
    locked.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn maintenance_reaps_idle_connections_and_keeps_the_minimum() {
    require_db!();
    let opts = fresh_db("poolmaint").await;
    let pool = PoolOptions::new()
        .max_connections(4)
        .min_connections(2)
        .idle_timeout(Some(Duration::from_millis(200)))
        .connect_with(opts)
        .await
        .unwrap();
    assert_eq!(pool.size(), 2);
    let conns: Vec<_> = futures_util::future::try_join_all((0..4).map(|_| pool.acquire())).await.unwrap();
    assert_eq!(pool.size(), 4);
    drop(conns);
    // idle past the timeout: the two above min_connections are retired
    let mut size = 0;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        size = pool.size();
        if size == 2 && pool.num_idle() == 2 {
            break;
        }
    }
    assert_eq!(size, 2);
    // a lost connection is replaced in the background
    let lost = pool.acquire().await.unwrap().detach();
    drop(lost);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(pool.size(), 2);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn idle_timeout_keeps_the_minimum_connections_open() {
    require_db!();
    let opts = fresh_db("poolidlemin").await;
    let opened = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = opened.clone();
    let pool = PoolOptions::new()
        .max_connections(4)
        .min_connections(2)
        .idle_timeout(Some(Duration::from_millis(100))) // maintenance every 50 ms
        .after_connect(move |_| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async { Ok(()) })
        })
        .connect_with(opts)
        .await
        .unwrap();
    let conns: Vec<_> = futures_util::future::try_join_all((0..4).map(|_| pool.acquire())).await.unwrap();
    let ids: Vec<u32> = conns.iter().map(|c| c.connection_id()).collect();
    drop(conns);
    let opened = || opened.load(std::sync::atomic::Ordering::Relaxed);
    let before = opened();
    // Many idle timeouts later: the surplus is gone, and the other two were never reopened.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!((pool.size(), pool.num_idle()), (2, 2));
    assert_eq!(opened(), before, "no connection was opened meanwhile");
    // `acquire` hands them out rather than closing them as idle-expired.
    let (a, b) = (pool.acquire().await.unwrap(), pool.acquire().await.unwrap());
    assert!(ids.contains(&a.connection_id()) && ids.contains(&b.connection_id()));
    assert_eq!(opened(), before);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn idle_connections_are_reused_most_recent_first() {
    require_db!();
    let opts = fresh_db("poollifo").await;
    let pool = PoolOptions::new().max_connections(2).connect_with(opts).await.unwrap();
    let (a, b) = (pool.acquire().await.unwrap(), pool.acquire().await.unwrap());
    let (ida, idb) = (a.connection_id(), b.connection_id());
    drop(a);
    drop(b); // returned last
    assert_eq!(pool.acquire().await.unwrap().connection_id(), idb);
    assert_ne!(ida, idb);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn retry_with_a_foreign_error_type() {
    require_db!();
    // An application error type wrapping tisqlx errors (like anyhow::Error would).
    #[derive(Debug)]
    struct AppError(Box<dyn std::error::Error + Send + Sync>);
    impl From<Error> for AppError {
        fn from(e: Error) -> Self {
            AppError(Box::new(e))
        }
    }
    let opts = fresh_db("retryif").await;
    let pool = PoolOptions::new().max_connections(4).connect_with(opts).await.unwrap();
    raw_sql("CREATE TABLE c (id INT PRIMARY KEY, v INT); INSERT INTO c VALUES (1, 0)").execute(&pool).await.unwrap();
    let mut handles = vec![];
    for _ in 0..4 {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            pool.transaction_retry_if(
                &TxnOptions::optimistic(),
                RetryPolicy::default().max_attempts(50),
                |e: &AppError| error::is_retryable(&*e.0),
                |tx| {
                    Box::pin(async move {
                        let v: i32 = query_scalar("SELECT v FROM c WHERE id = 1").fetch_one(&mut **tx).await?;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        query("UPDATE c SET v = ? WHERE id = 1").bind(v + 1).execute(&mut **tx).await?;
                        Ok::<_, AppError>(())
                    })
                },
            )
            .await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    let v: i32 = query_scalar("SELECT v FROM c WHERE id = 1").fetch_one(&pool).await.unwrap();
    assert_eq!(v, 4);
    // non-retryable errors are returned at once
    let attempts = std::sync::atomic::AtomicU32::new(0);
    let r = pool
        .transaction_retry_if(
            &TxnOptions::default(),
            RetryPolicy::default(),
            |e: &AppError| error::is_retryable(&*e.0),
            |tx| {
                attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Box::pin(async move {
                    raw_sql("SELECT * FROM nope").execute(&mut **tx).await.map(drop).map_err(AppError::from)
                })
            },
        )
        .await;
    assert!(r.is_err());
    assert_eq!(attempts.into_inner(), 1);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn pool_recovers_from_connections_killed_by_the_server() {
    require_db!();
    let opts = fresh_db("poolkill").await;
    let pool = PoolOptions::new().max_connections(1).ping_after_idle(Duration::ZERO).connect_with(opts).await.unwrap();
    let id = pool.acquire().await.unwrap().connection_id();
    let mut admin = TiConnection::connect(&base_url()).await.unwrap();
    admin.execute_raw(&format!("KILL {id}")).await.unwrap();
    // the idle connection is dead: the ping on acquire notices and a new one is opened
    let mut conn = pool.acquire().await.unwrap();
    assert_ne!(conn.connection_id(), id);
    let one: i64 = query_scalar("SELECT 1").fetch_one(&mut *conn).await.unwrap();
    assert_eq!(one, 1);
    assert_eq!(pool.size(), 1);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn recently_used_connection_killed_by_the_server_fails_once() {
    // With the default `ping_after_idle`, a connection returned moments ago is not pinged:
    // the first query on it fails, the pool discards it, and the next checkout is healthy.
    require_db!();
    let opts = fresh_db("poolkillrecent").await;
    let pool = PoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    let id = pool.acquire().await.unwrap().connection_id();
    let mut admin = TiConnection::connect(&base_url()).await.unwrap();
    admin.execute_raw(&format!("KILL {id}")).await.unwrap();
    {
        let mut conn = pool.acquire().await.unwrap();
        assert_eq!(conn.connection_id(), id, "not pinged, so the dead connection is handed out");
        assert!(query_scalar::<i64>("SELECT 1").fetch_one(&mut *conn).await.is_err());
    }
    let mut conn = pool.acquire().await.unwrap();
    assert_ne!(conn.connection_id(), id);
    let one: i64 = query_scalar("SELECT 1").fetch_one(&mut *conn).await.unwrap();
    assert_eq!(one, 1);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn a_dead_host_in_the_list_is_skipped() {
    require_db!();
    let opts = fresh_db("multihost").await;
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port();
    let live = (opts.get_host().to_owned(), opts.get_port());
    let opts = opts.hosts([("127.0.0.1".to_owned(), dead), live]);
    // A single connection tries the hosts in order.
    let mut conn = opts.connect().await.unwrap();
    let one: i64 = query_scalar("SELECT 1").fetch_one(&mut conn).await.unwrap();
    assert_eq!(one, 1);
    // A pool keeps working, and keeps the dead host at zero connections.
    let pool = PoolOptions::new().min_connections(3).max_connections(3).connect_with(opts).await.unwrap();
    for _ in 0..10 {
        let one: i64 = query_scalar("SELECT 1").fetch_one(&pool).await.unwrap();
        assert_eq!(one, 1);
    }
    let counts = pool.host_connections();
    assert!(counts.iter().any(|(h, n)| h.ends_with(&format!(":{dead}")) && *n == 0), "{counts:?}");
    assert_eq!(counts.iter().map(|(_, n)| n).sum::<u32>(), 3, "{counts:?}");
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn localhost_with_and_without_dns_balancing() {
    // `localhost` resolves to ::1 and 127.0.0.1, while the test TiDB listens on 127.0.0.1 only.
    require_db!();
    let opts = fresh_db("localhost").await;
    let port = opts.get_port();
    if opts.get_host() != "127.0.0.1" {
        return; // a remote test server: `localhost` is not it
    }
    let local = opts.host("localhost");
    let pool = PoolOptions::new().max_connections(2).connect_with(local.clone()).await.unwrap();
    let one: i64 = query_scalar("SELECT 1").fetch_one(&pool).await.unwrap();
    assert_eq!(one, 1);
    // Balancing over the addresses: ::1 is refused, backs off, and stays empty.
    let pool = PoolOptions::new()
        .min_connections(2)
        .max_connections(2)
        .connect_with(local.load_balance_dns(true))
        .await
        .unwrap();
    for _ in 0..5 {
        let one: i64 = query_scalar("SELECT 1").fetch_one(&pool).await.unwrap();
        assert_eq!(one, 1);
    }
    let counts = pool.host_connections();
    assert!(counts.iter().any(|(h, n)| h.contains(&format!("127.0.0.1:{port}")) && *n == 2), "{counts:?}");
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn sequential_fetch_one_keeps_using_one_connection() {
    // `fetch_one` reads the whole response, so the connection goes straight back to the idle
    // queue, not through a background reset during which the next query opens another one.
    require_db!();
    let opts = fresh_db("poolfetchone").await;
    let pool = PoolOptions::new().max_connections(5).connect_with(opts).await.unwrap();
    for _ in 0..20 {
        let one: i64 = query_scalar("SELECT 1").fetch_one(&pool).await.unwrap();
        assert_eq!(one, 1);
        assert_eq!((pool.size(), pool.num_idle()), (1, 1));
    }
    // the rows after the first are read and dropped; an error among them is reported
    let first: i64 = query_scalar("SELECT 1 UNION ALL SELECT 2").fetch_one(&pool).await.unwrap();
    assert_eq!((first, pool.num_idle()), (1, 1));
    let err = raw_sql("SELECT 1; SELECT * FROM missing").fetch_optional(&pool).await.unwrap_err();
    assert_eq!(err.as_database_error().unwrap().code, 1146, "{err}");
}
