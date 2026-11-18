//! Deterministic protocol tests against a scripted server (`server.rs`).
//!
//! These cover what a live TiDB cannot reproduce on demand: futures dropped at
//! precise points, malformed packets, re-prepares, hostile handshakes. No
//! database is needed.

mod server;

use std::time::Duration;

use futures_util::{StreamExt, TryStreamExt};
use server::*;
use tisqlx_core::*;

/// Drain a 100-row result the client dropped after the first row, then serve `SELECT 2`.
async fn script_drop_then_select(mut c: Conn) {
    c.accept().await;
    c.expect(QUERY, Some("SELECT n FROM big")).await;
    c.columns(&[("n", LONGLONG, 0)]).await;
    for i in 0..100 {
        c.text_row(&[Some(&i.to_string())]).await;
    }
    c.end_rows(AUTOCOMMIT).await;
    c.expect(QUERY, Some("SELECT 2")).await;
    c.columns(&[("2", LONGLONG, 0)]).await;
    c.text_row(&[Some("2")]).await;
    c.end_rows(AUTOCOMMIT).await;
}

#[tokio::test]
async fn dropped_stream_is_drained_before_next_command() {
    let mock = Mock::start(script_drop_then_select).await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    {
        let mut rows = raw_sql("SELECT n FROM big").fetch(&mut conn);
        let first = rows.try_next().await.unwrap().unwrap();
        assert_eq!(first.get::<i64, _>(0), 0);
    } // dropped with 99 rows unread
    let two: i64 = raw_sql("SELECT 2").fetch_one(&mut conn).await.unwrap().get(0);
    assert_eq!(two, 2);
    mock.finish().await;
}

#[tokio::test]
async fn multiple_result_sets() {
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        for _ in 0..2 {
            c.expect(QUERY, Some("SELECT 1; UPDATE t SET x = 1")).await;
            c.columns(&[("1", LONGLONG, 0)]).await;
            c.text_row(&[Some("1")]).await;
            c.end_rows(AUTOCOMMIT | MORE_RESULTS).await;
            c.ok(3, 0, AUTOCOMMIT).await;
        }
        c.expect(QUERY, Some("SELECT 2")).await;
        c.ok(0, 0, AUTOCOMMIT).await;
    })
    .await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    let r = raw_sql("SELECT 1; UPDATE t SET x = 1").execute(&mut conn).await.unwrap();
    assert_eq!(r.rows_affected(), 3);
    {
        // dropped inside the first of two results: the drain must cross MORE_RESULTS
        let mut s = raw_sql("SELECT 1; UPDATE t SET x = 1").fetch(&mut conn);
        s.try_next().await.unwrap();
    }
    raw_sql("SELECT 2").execute(&mut conn).await.unwrap();
    mock.finish().await;
}

#[tokio::test]
async fn cancelled_while_awaiting_response_marks_broken() {
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(QUERY, Some("SELECT SLEEP(1)")).await;
        // never answer; the client gives up
        let _ = c.recv().await;
    })
    .await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    let r = tokio::time::timeout(Duration::from_millis(100), raw_sql("SELECT SLEEP(1)").execute(&mut conn)).await;
    assert!(r.is_err(), "timed out");
    assert!(conn.is_broken());
    let e = raw_sql("SELECT 1").execute(&mut conn).await.unwrap_err();
    assert!(e.to_string().contains("unusable"), "{e}");
    drop(conn);
    mock.finish().await;
}

#[tokio::test]
async fn malformed_packet_marks_broken_instead_of_hanging() {
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(QUERY, Some("SELECT 1")).await;
        c.send(&[0x00]).await; // truncated OK packet
        let _ = c.recv().await;
    })
    .await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    assert!(matches!(raw_sql("SELECT 1").execute(&mut conn).await, Err(Error::Protocol(_))));
    assert!(conn.is_broken());
    let next = tokio::time::timeout(Duration::from_secs(2), raw_sql("SELECT 1").execute(&mut conn)).await;
    assert!(next.expect("must not hang").is_err());
    drop(conn);
    mock.finish().await;
}

#[tokio::test]
async fn schema_changed_after_prepare_is_reprepared_once() {
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(PREPARE, Some("SELECT ?")).await;
        c.prepare_ok(1, 1, &[("v", LONGLONG, 0)]).await;
        assert_eq!(stmt_id(&c.expect(EXECUTE, None).await), 1);
        c.err(8113, "Schema change caused error: schema changed after prepare").await;
        // the stale statement is closed (no response), then re-prepared
        assert_eq!(stmt_id(&c.expect(CLOSE, None).await), 1);
        c.expect(PREPARE, Some("SELECT ?")).await;
        c.prepare_ok(2, 1, &[("v", LONGLONG, 0)]).await;
        assert_eq!(stmt_id(&c.expect(EXECUTE, None).await), 2);
        c.columns(&[("v", LONGLONG, 0)]).await;
        c.binary_row_i64(&[7]).await;
        c.end_rows(AUTOCOMMIT).await;
    })
    .await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    let v: i64 = query_scalar("SELECT ?").bind(1i64).fetch_one(&mut conn).await.unwrap();
    assert_eq!(v, 7);
    assert_eq!(conn.cached_statements_size(), 1);
    mock.finish().await;
}

#[tokio::test]
async fn result_metadata_overrides_stale_prepare_metadata() {
    // Prepared as signed BIGINT, executed as BIGINT UNSIGNED (e.g. after an ALTER):
    // decoding must follow the execute-time metadata.
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(PREPARE, Some("SELECT v FROM t WHERE id = ?")).await;
        c.prepare_ok(1, 1, &[("v", LONGLONG, 0)]).await;
        c.expect(EXECUTE, None).await;
        c.columns(&[("v", LONGLONG, UNSIGNED)]).await;
        c.binary_row_i64(&[u64::MAX]).await;
        c.end_rows(AUTOCOMMIT).await;
    })
    .await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    let v: u64 = query_scalar("SELECT v FROM t WHERE id = ?").bind(1i64).fetch_one(&mut conn).await.unwrap();
    assert_eq!(v, u64::MAX);
    mock.finish().await;
}

#[tokio::test]
async fn non_persistent_statement_is_closed_before_next_command() {
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(PREPARE, Some("SELECT ?")).await;
        c.prepare_ok(5, 1, &[("v", LONGLONG, 0)]).await;
        c.expect(EXECUTE, None).await;
        c.columns(&[("v", LONGLONG, 0)]).await;
        c.binary_row_i64(&[1]).await;
        c.end_rows(AUTOCOMMIT).await;
        assert_eq!(stmt_id(&c.expect(CLOSE, None).await), 5);
        c.expect(PING, None).await;
        c.ok(0, 0, AUTOCOMMIT).await;
    })
    .await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    let v: i64 = query_scalar("SELECT ?").bind(1i64).persistent(false).fetch_one(&mut conn).await.unwrap();
    assert_eq!(v, 1);
    assert_eq!(conn.cached_statements_size(), 0);
    conn.ping().await.unwrap();
    mock.finish().await;
}

#[tokio::test]
async fn clear_text_password_never_sent_unprompted() {
    // A hostile server (or MITM) advertises clear-text auth on a plain connection.
    let mock = Mock::start(|mut c: Conn| async move {
        c.handshake("mysql_clear_password", DEFAULT_CAPS).await;
        let response = c.recv().await.unwrap();
        assert!(!response.windows(6).any(|w| w == b"s3cret"), "password sent in clear text");
        assert!(response.windows(21).any(|w| w == b"mysql_native_password"));
        // then explicitly ask for clear text: refused client-side
        let mut switch = vec![0xFE];
        switch.extend_from_slice(b"mysql_clear_password\0");
        c.send(&switch).await;
        assert_eq!(c.recv().await, None, "client must hang up");
    })
    .await;
    let url = mock.url.replace("root@", "root:s3cret@");
    let e = TiConnection::connect(&url).await.unwrap_err();
    assert!(e.to_string().contains("clear-text"), "{e}");
    mock.finish().await;
}

#[tokio::test]
async fn unknown_default_auth_plugin_falls_back_to_auth_switch() {
    // The handshake names the server default; the account may use a supported plugin.
    let mock = Mock::start(|mut c: Conn| async move {
        c.handshake("tidb_sm3_password", DEFAULT_CAPS).await;
        c.recv().await.unwrap();
        let mut switch = vec![0xFE];
        switch.extend_from_slice(b"mysql_native_password\0");
        switch.extend_from_slice(b"01234567890123456789\0");
        c.send(&switch).await;
        let scramble = c.recv().await.unwrap();
        assert_eq!(scramble.len(), 20);
        c.ok(0, 0, AUTOCOMMIT).await;
        c.recv().await.unwrap(); // session SET
        c.ok(0, 0, AUTOCOMMIT).await;
    })
    .await;
    let url = mock.url.replace("root@", "root:pw@");
    TiConnection::connect(&url).await.unwrap();
    mock.finish().await;
}

#[tokio::test]
async fn non_tidb_server_is_rejected() {
    let mock = Mock::start(|mut c: Conn| async move {
        let mut p = vec![10u8];
        p.extend_from_slice(b"8.0.36\0");
        p.extend_from_slice(&[0; 40]);
        c.send(&p).await;
    })
    .await;
    assert!(matches!(TiConnection::connect(&mock.url).await, Err(Error::NotTiDb(v)) if v == "8.0.36"));
    mock.finish().await;
}

#[tokio::test]
async fn tidb_older_than_the_minimum_is_rejected() {
    let old = Mock::start(|mut c: Conn| async move {
        c.handshake_as("8.0.11-TiDB-v8.4.0", "mysql_native_password", DEFAULT_CAPS).await;
    })
    .await;
    let err = TiConnection::connect(&old.url).await.unwrap_err();
    assert!(matches!(err, Error::UnsupportedTiDb(v) if v == TiDbVersion::new(8, 4, 0)), "{err}");
    assert!(err.to_string().contains("needs TiDB v8.5.0 or newer"), "{err}");
    old.finish().await;

    // `require_tidb(false)` connects anyway.
    let older = Mock::start(|mut c: Conn| async move {
        c.handshake_as("8.0.11-TiDB-v7.5.6", "mysql_native_password", DEFAULT_CAPS).await;
        c.recv().await.expect("handshake response");
        c.ok(0, 0, AUTOCOMMIT).await;
        c.recv().await.expect("session SET");
        c.ok(0, 0, AUTOCOMMIT).await;
    })
    .await;
    older.url.parse::<ConnectOptions>().unwrap().require_tidb(false).connect().await.unwrap();
    older.finish().await;
}

#[tokio::test]
async fn pool_resets_connection_returned_mid_result() {
    // `SELECT ...; BEGIN` dropped after one row: the unread BEGIN would leak an open
    // transaction to the next borrower unless the pool drains and rolls back.
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(PING, None).await; // ping_after_idle(ZERO): ping on every checkout
        c.ok(0, 0, AUTOCOMMIT).await;
        c.expect(QUERY, Some("SELECT 1; BEGIN")).await;
        c.columns(&[("1", LONGLONG, 0)]).await;
        c.text_row(&[Some("1")]).await;
        c.text_row(&[Some("1")]).await;
        c.end_rows(AUTOCOMMIT | MORE_RESULTS).await;
        c.ok(0, 0, AUTOCOMMIT | IN_TRANS).await;
        c.expect(QUERY, Some("ROLLBACK")).await;
        c.ok(0, 0, AUTOCOMMIT).await;
        c.expect(PING, None).await; // test_before_acquire
        c.ok(0, 0, AUTOCOMMIT).await;
    })
    .await;
    let pool = PoolOptions::new().max_connections(1).ping_after_idle(Duration::ZERO).connect(&mock.url).await.unwrap();
    {
        let mut conn = pool.acquire().await.unwrap();
        let mut s = raw_sql("SELECT 1; BEGIN").fetch(&mut *conn);
        s.next().await.unwrap().unwrap();
    }
    let conn = tokio::time::timeout(Duration::from_secs(5), pool.acquire()).await.unwrap().unwrap();
    assert!(!conn.in_transaction());
    assert_eq!(pool.size(), 1);
    mock.finish().await;
}

#[tokio::test]
async fn unknown_column_type_is_read_as_a_string() {
    // A column type this driver does not know (as a newer TiDB might send), in both protocols.
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(QUERY, Some("SELECT v")).await;
        c.columns(&[("v", 0xE1, 0)]).await;
        c.text_row(&[Some("[1,2]")]).await;
        c.end_rows(AUTOCOMMIT).await;
        c.expect(PREPARE, Some("SELECT ?")).await;
        c.prepare_ok(1, 1, &[("v", 0xE1, 0)]).await;
        c.expect(EXECUTE, None).await;
        c.columns(&[("v", 0xE1, 0)]).await;
        let mut row = vec![0x00, 0x00];
        lenenc_str(&mut row, b"[3]");
        c.send(&row).await;
        c.end_rows(AUTOCOMMIT).await;
    })
    .await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    let row = raw_sql("SELECT v").fetch_one(&mut conn).await.unwrap();
    assert_eq!(row.get::<Vector, _>(0), Vector(vec![1.0, 2.0]));
    let v: String = query_scalar("SELECT ?").bind(1i64).fetch_one(&mut conn).await.unwrap();
    assert_eq!(v, "[3]");
    assert!(!conn.is_broken());
    mock.finish().await;
}

#[tokio::test]
async fn failed_savepoint_rollback_aborts_the_whole_transaction() {
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(QUERY, Some("BEGIN")).await;
        c.ok(0, 0, AUTOCOMMIT | IN_TRANS).await;
        c.expect(QUERY, Some("SAVEPOINT _tisqlx_sp_2")).await;
        c.ok(0, 0, AUTOCOMMIT | IN_TRANS).await;
        // the savepoint vanished (e.g. a raw COMMIT inside the transaction)
        c.expect(QUERY, Some("ROLLBACK TO SAVEPOINT _tisqlx_sp_2")).await;
        c.err(1305, "SAVEPOINT _tisqlx_sp_2 does not exist").await;
        c.expect(QUERY, Some("ROLLBACK")).await;
        c.ok(0, 0, AUTOCOMMIT).await;
        // no COMMIT may follow
        assert_eq!(c.recv().await, None);
    })
    .await;
    let mut conn = TiConnection::connect(&mock.url).await.unwrap();
    let mut tx = conn.begin().await.unwrap();
    let sp = tx.begin().await.unwrap();
    let e = sp.rollback().await.unwrap_err();
    assert_eq!(e.as_database_error().map(|d| d.code()), Some(1305));
    let e = tx.commit().await.unwrap_err();
    assert!(e.to_string().contains("already rolled back"), "{e}");
    assert!(!conn.in_transaction());
    drop(conn);
    mock.finish().await;
}

#[tokio::test]
async fn hung_reset_is_abandoned_and_frees_its_slot() {
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(QUERY, Some("BEGIN")).await;
        c.ok(0, 0, AUTOCOMMIT | IN_TRANS).await;
        c.expect(QUERY, Some("ROLLBACK")).await;
        // never answer; the client gives up and closes the socket
        assert_eq!(c.recv().await, None);
    })
    .await;
    let opts: ConnectOptions = mock.url.parse::<ConnectOptions>().unwrap().connect_timeout(Duration::from_millis(300));
    let pool = PoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    drop(pool.begin().await.unwrap()); // dropped open: the pool resets it in the background
    let mut freed = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if pool.size() == 0 {
            freed = true;
            break;
        }
    }
    assert!(freed, "the stuck connection was discarded");
    mock.finish().await;
}

#[tokio::test]
async fn retried_transaction_rolls_back_before_the_next_attempt() {
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        for attempt in 0..2 {
            c.expect(QUERY, Some("BEGIN OPTIMISTIC")).await;
            c.ok(0, 0, AUTOCOMMIT | IN_TRANS).await;
            c.expect(QUERY, Some("UPDATE t SET v = v + 1")).await;
            if attempt == 0 {
                c.err(9007, "Write conflict").await;
                // the failed attempt is rolled back before the retry begins
                c.expect(QUERY, Some("ROLLBACK")).await;
                c.ok(0, 0, AUTOCOMMIT).await;
            } else {
                c.ok(1, 0, AUTOCOMMIT | IN_TRANS).await;
                c.expect(QUERY, Some("COMMIT")).await;
                c.ok(0, 0, AUTOCOMMIT).await;
            }
        }
    })
    .await;
    let pool = PoolOptions::new().max_connections(1).connect(&mock.url).await.unwrap();
    let attempts = std::sync::atomic::AtomicU32::new(0);
    pool.transaction_with(&TxnOptions::optimistic(), RetryPolicy::default(), |tx| {
        attempts.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Box::pin(async move { raw_sql("UPDATE t SET v = v + 1").execute(&mut **tx).await.map(drop) })
    })
    .await
    .unwrap();
    assert_eq!(attempts.into_inner(), 2);
    mock.finish().await;
}

/// Answer a prepared statement with one row holding one column.
async fn prepared_one_row(c: &mut Conn, sql: &str, params: u16, column: (&str, u8), value: &[u8]) {
    c.expect(PREPARE, Some(sql)).await;
    c.prepare_ok(1, params, &[(column.0, column.1, 0)]).await;
    c.expect(EXECUTE, None).await;
    c.columns(&[(column.0, column.1, 0)]).await;
    let mut row = vec![0x00, 0x00];
    if column.1 == VAR_STRING {
        lenenc_str(&mut row, value);
    } else {
        row.extend_from_slice(value);
    }
    c.send(&row).await;
    c.end_rows(AUTOCOMMIT).await;
}

#[tokio::test]
async fn migration_lock_is_released_when_release_lock_fails() {
    // RELEASE_LOCK errors, so the migrator cannot know whether it still holds the lock;
    // the connection goes back to the pool and must be cleaned with RELEASE_ALL_LOCKS().
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        prepared_one_row(&mut c, "SELECT DATABASE()", 0, ("db", VAR_STRING), b"db").await;
        prepared_one_row(&mut c, "SELECT GET_LOCK(?, ?)", 2, ("l", LONGLONG), &1u64.to_le_bytes()).await;
        c.expect(QUERY, None).await; // CREATE TABLE IF NOT EXISTS
        c.ok(0, 0, AUTOCOMMIT).await;
        c.expect(PREPARE, None).await; // SELECT ... FROM migrations table
        c.prepare_ok(2, 0, &[("version", LONGLONG, 0)]).await;
        c.expect(EXECUTE, None).await;
        c.columns(&[("version", LONGLONG, 0)]).await;
        c.end_rows(AUTOCOMMIT).await;
        c.expect(PREPARE, Some("SELECT RELEASE_LOCK(?)")).await;
        c.prepare_ok(3, 1, &[("r", LONGLONG, 0)]).await;
        c.expect(EXECUTE, None).await;
        c.err(1105, "transient error").await;
        // the pool's reset
        c.expect(QUERY, Some("SELECT RELEASE_ALL_LOCKS()")).await;
        c.columns(&[("n", LONGLONG, 0)]).await;
        c.text_row(&[Some("1")]).await;
        c.end_rows(AUTOCOMMIT).await;
    })
    .await;
    let pool = PoolOptions::new().max_connections(1).connect(&mock.url).await.unwrap();
    let m = migrate::Migrator::from_static(&[]);
    assert!(m.run(&pool).await.is_err(), "RELEASE_LOCK failed");
    let conn = tokio::time::timeout(Duration::from_secs(5), pool.acquire()).await.unwrap().unwrap();
    drop(conn);
    mock.finish().await;
}

#[tokio::test]
async fn recently_returned_connection_is_not_pinged() {
    // Default `ping_after_idle` (30 s): a connection idle for less than that is handed out as is.
    let mock = Mock::start(|mut c: Conn| async move {
        c.accept().await;
        c.expect(QUERY, Some("SELECT 1")).await;
        c.columns(&[("1", LONGLONG, 0)]).await;
        c.text_row(&[Some("1")]).await;
        c.end_rows(AUTOCOMMIT).await;
    })
    .await;
    let pool = PoolOptions::new().max_connections(1).connect(&mock.url).await.unwrap();
    drop(pool.acquire().await.unwrap());
    let mut conn = pool.acquire().await.unwrap();
    raw_sql("SELECT 1").fetch_one(&mut *conn).await.unwrap();
    mock.finish().await;
}

// ---- several hosts

/// `host:port` of a mock server URL.
fn addr_of(url: &str) -> (String, u16) {
    let hostport = url.trim_start_matches("tidb://root@").split('/').next().unwrap();
    let (h, p) = hostport.rsplit_once(':').unwrap();
    (h.to_owned(), p.parse().unwrap())
}

/// A port nothing listens on (refuses at once).
async fn dead_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port()
}

/// Accept the connection, then hold it open until the client goes away.
async fn serve_until_closed(mut c: Conn) {
    c.accept().await;
    while c.recv().await.is_some() {}
}

fn options(hosts: Vec<(String, u16)>) -> ConnectOptions {
    ConnectOptions::new().hosts(hosts).ssl_mode(SslMode::Disabled).connect_timeout(Duration::from_secs(2))
}

#[tokio::test]
async fn pool_spreads_connections_and_skips_a_dead_host() {
    let (a, b) = (Mock::start(serve_until_closed).await, Mock::start(serve_until_closed).await);
    let dead = ("127.0.0.1".to_owned(), dead_port().await);
    let opts = options(vec![addr_of(&a.url), dead.clone(), addr_of(&b.url)]);
    let pool = PoolOptions::new().min_connections(2).max_connections(2).connect_with(opts).await.unwrap();
    let (c1, c2) = (pool.acquire().await.unwrap(), pool.acquire().await.unwrap());
    let mut used = [c1.server_addr().unwrap().port(), c2.server_addr().unwrap().port()];
    used.sort();
    let mut expected = [addr_of(&a.url).1, addr_of(&b.url).1];
    expected.sort();
    assert_eq!(used, expected, "one connection on each live host");
    let counts = pool.host_connections();
    assert!(counts.iter().any(|(h, n)| h.ends_with(&format!(":{}", dead.1)) && *n == 0), "{counts:?}");
    drop((c1, c2));
    pool.close().await;
    assert!(pool.host_connections().iter().all(|(_, n)| *n == 0), "{:?}", pool.host_connections());
    a.finish().await;
    b.finish().await;
}

#[tokio::test]
async fn unresponsive_host_only_costs_its_share_of_the_timeout() {
    // Accepts TCP (kernel backlog) but never sends a handshake, like a hung server.
    let hung = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hung_addr = ("127.0.0.1".to_owned(), hung.local_addr().unwrap().port());
    let live = Mock::start(serve_until_closed).await;
    let start = std::time::Instant::now();
    let conn = options(vec![hung_addr, addr_of(&live.url)]).connect().await.unwrap();
    assert!(start.elapsed() < Duration::from_millis(1900), "took {:?} of the 2 s", start.elapsed());
    assert_eq!(conn.server_addr().unwrap().port(), addr_of(&live.url).1);
    conn.close().await.unwrap();
    live.finish().await;
}

#[tokio::test]
async fn non_network_errors_do_not_fail_over() {
    // A wrong password would fail on every host: return it without trying the next one.
    let denied = Mock::start(|mut c: Conn| async move {
        c.handshake("mysql_native_password", DEFAULT_CAPS).await;
        c.recv().await.expect("handshake response");
        c.err(1045, "Access denied for user 'root'@'127.0.0.1'").await;
    })
    .await;
    let next = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let next_addr = ("127.0.0.1".to_owned(), next.local_addr().unwrap().port());
    let err = options(vec![addr_of(&denied.url), next_addr]).connect().await.unwrap_err();
    assert!(matches!(&err, Error::Database(e) if e.code == 1045), "{err}");
    let tried = tokio::time::timeout(Duration::from_millis(200), next.accept()).await;
    assert!(tried.is_err(), "the second host must not be contacted");
    denied.finish().await;
}
