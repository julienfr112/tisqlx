use futures_util::TryStreamExt;
use tisqlx_core::*;

use crate::common::*;

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn select_one_text_and_binary() {
    require_db!();
    let mut conn = TiConnection::connect(&base_url()).await.unwrap();
    assert!(conn.tidb_version().unwrap().at_least(6, 0));

    let rows = raw_sql("SELECT 1, 'a', NULL").fetch_all(&mut conn).await.unwrap();
    assert_eq!(rows.len(), 1);
    let v: i64 = rows[0].get(0);
    let s: String = rows[0].get(1);
    let n: Option<String> = rows[0].get(2);
    assert_eq!((v, s.as_str(), n), (1, "a", None));

    let (a, b): (i64, String) =
        query_as("SELECT ? + 1, CONCAT(?, '!')").bind(41i64).bind("hi").fetch_one(&mut conn).await.unwrap();
    assert_eq!((a, b.as_str()), (42, "hi!"));
    conn.ping().await.unwrap();
    conn.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn multi_statement_raw() {
    require_db!();
    let mut conn = TiConnection::connect(&base_url()).await.unwrap();
    let mut s = raw_sql("SELECT 1; SELECT 2 UNION ALL SELECT 3").fetch(&mut conn);
    let mut got = vec![];
    while let Some(r) = s.try_next().await.unwrap() {
        got.push(r.get::<i64, _>(0));
    }
    drop(s);
    // Result sets arrive in order; rows inside a UNION ALL have no guaranteed order in TiDB.
    assert_eq!(got[0], 1);
    got[1..].sort();
    assert_eq!(got, vec![1, 2, 3]);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn auth_caching_sha2_and_native() {
    require_db!();
    let o: ConnectOptions = base_url().parse().unwrap();
    // Test accounts with no privileges (CURRENT_USER() needs none).
    let mut admin = o.connect().await.unwrap();
    admin
        .execute_raw(
            "CREATE USER IF NOT EXISTS 'tisqlx_sha'@'%' IDENTIFIED WITH caching_sha2_password BY 'pw';
             CREATE USER IF NOT EXISTS 'tisqlx_nat'@'%' IDENTIFIED WITH mysql_native_password BY 'pw'",
        )
        .await
        .unwrap();
    let who = |user: &'static str, opts: ConnectOptions| async move {
        let mut c = TiConnection::connect_with(&opts).await.unwrap_or_else(|e| panic!("{user}: {e}"));
        let who: String = query_scalar("SELECT CURRENT_USER()").fetch_one(&mut c).await.unwrap();
        assert!(who.starts_with(user), "{who}");
    };
    // native password works on every transport
    who("tisqlx_nat", o.clone().username("tisqlx_nat").password("pw").ssl_mode(SslMode::Disabled)).await;
    who("tisqlx_nat", o.clone().username("tisqlx_nat").password("pw").ssl_mode(SslMode::Required)).await;
    // caching_sha2 full auth over TLS
    who("tisqlx_sha", o.clone().username("tisqlx_sha").password("pw").ssl_mode(SslMode::Required)).await;
    // ... and over the Unix socket, when present
    match socket() {
        Some(path) => {
            who("tisqlx_sha", o.clone().username("tisqlx_sha").password("pw").ssl_mode(SslMode::Disabled).socket(path))
                .await
        }
        None => eprintln!("socket auth not tested: set TISQLX_TEST_SOCKET"),
    }
    // ... but not over plain TCP: clear error
    let e = TiConnection::connect_with(&o.clone().username("tisqlx_sha").password("pw").ssl_mode(SslMode::Disabled))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("requires TLS"), "{e}");
    // wrong password
    for user in ["tisqlx_sha", "tisqlx_nat"] {
        let bad =
            TiConnection::connect_with(&o.clone().username(user).password("nope").ssl_mode(SslMode::Required)).await;
        assert_eq!(bad.unwrap_err().as_database_error().map(|d| d.code()), Some(1045));
    }
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn errors_and_recovery() {
    require_db!();
    let mut conn = TiConnection::connect(&base_url()).await.unwrap();
    let e = query("SELECT * FROM no_such_table_xyz").fetch_all(&mut conn).await.unwrap_err();
    assert!(matches!(e.as_database_error().unwrap().code(), 1046 | 1146), "{e}");
    // Wrong param count.
    assert!(query("SELECT ?").fetch_all(&mut conn).await.is_err());
    // Connection still usable.
    let v: i64 = query_scalar("SELECT 7").fetch_one(&mut conn).await.unwrap();
    assert_eq!(v, 7);

    // Dropping a stream mid-result leaves the connection usable.
    {
        let mut s = raw_sql("SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3").fetch(&mut conn);
        s.try_next().await.unwrap();
    }
    let first = query("SELECT * FROM (SELECT 1 a UNION ALL SELECT 2) t").fetch_optional(&mut conn).await.unwrap();
    assert!(first.is_some());
    let v: i64 = query_scalar("SELECT 8").fetch_one(&mut conn).await.unwrap();
    assert_eq!(v, 8);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn statement_cache_eviction() {
    require_db!();
    let opts: ConnectOptions = base_url().parse::<ConnectOptions>().unwrap().statement_cache_capacity(3);
    let mut conn = opts.connect().await.unwrap();
    for i in 0..10i64 {
        let sql = format!("SELECT ? + {i}");
        let v: i64 = query_scalar(sql).bind(1i64).fetch_one(&mut conn).await.unwrap();
        assert_eq!(v, 1 + i);
    }
    assert_eq!(conn.cached_statements_size(), 3);
    conn.ping().await.unwrap();
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn large_packets() {
    require_db!();
    let mut conn = TiConnection::connect(&base_url()).await.unwrap();
    let n = 17 * 1024 * 1024;
    let s: String = query_scalar("SELECT REPEAT('x', 17*1024*1024)").fetch_one(&mut conn).await.unwrap();
    assert_eq!(s.len(), n);
    let r = raw_sql("SELECT REPEAT('y', 17*1024*1024)").fetch_one(&mut conn).await.unwrap();
    assert_eq!(r.get::<&str, _>(0).len(), n);
    // exactly one full frame + 0-length terminator
    let blob = vec![7u8; 0xFF_FFFF - 30];
    let len: i64 = query_scalar("SELECT LENGTH(?)").bind(&blob).fetch_one(&mut conn).await.unwrap();
    assert_eq!(len as usize, blob.len());
    let blob = vec![7u8; n];
    let len: i64 = query_scalar("SELECT LENGTH(?)").bind(&blob).fetch_one(&mut conn).await.unwrap();
    assert_eq!(len as usize, n);
    let v: i64 = query_scalar("SELECT 1").fetch_one(&mut conn).await.unwrap();
    assert_eq!(v, 1);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn cached_statement_survives_ddl() {
    require_db!();
    let opts = fresh_db("ddlcache").await;
    let mut conn = opts.connect().await.unwrap();
    conn.execute_raw("CREATE TABLE d (id INT PRIMARY KEY); INSERT INTO d VALUES (1)").await.unwrap();
    assert_eq!(query("SELECT * FROM d").fetch_one(&mut conn).await.unwrap().len(), 1);
    conn.execute_raw("ALTER TABLE d ADD COLUMN x INT DEFAULT 5").await.unwrap();
    let r = query("SELECT * FROM d").fetch_one(&mut conn).await.unwrap();
    assert_eq!(r.get::<i32, _>("x"), 5);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn tls_config_is_built_per_options() {
    require_db!();
    let tls: ConnectOptions = base_url().parse::<ConnectOptions>().unwrap().ssl_mode(SslMode::Required);
    // the cached rustls config is reused across connections from the same options
    for _ in 0..2 {
        let mut c = tls.connect().await.unwrap();
        let cipher: String = query_scalar(
            "SELECT VARIABLE_VALUE FROM performance_schema.session_status WHERE VARIABLE_NAME = 'Ssl_cipher'",
        )
        .fetch_optional(&mut c)
        .await
        .unwrap()
        .unwrap_or_else(|| "tls".into());
        assert!(!cipher.is_empty());
    }
    // a clone changed to verify-identity must not reuse the permissive config:
    // the test server's self-signed certificate is rejected
    let strict = tls.clone().ssl_mode(SslMode::VerifyIdentity);
    assert!(matches!(strict.connect().await, Err(Error::Tls(_))));
    // ... while the original still connects
    tls.connect().await.unwrap();
}
