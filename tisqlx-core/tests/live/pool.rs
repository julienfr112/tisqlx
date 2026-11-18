use std::time::Duration;

use tisqlx_core::*;

use crate::common::*;

#[tokio::test]
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
    // idle past the timeout: retired, then reopened up to min_connections
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
async fn pool_recovers_from_connections_killed_by_the_server() {
    require_db!();
    let opts = fresh_db("poolkill").await;
    let pool = PoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
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
