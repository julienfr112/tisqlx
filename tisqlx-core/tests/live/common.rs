//! Shared harness for the live tests.
//!
//! Live tests run against a **dedicated** TiDB given by `TISQLX_TEST_URL`
//! (`make db-up` starts one). Without it they are skipped, unless
//! `TISQLX_TEST_REQUIRE=1` (set by `make test-live` and CI) turns that into a failure.

use tisqlx_core::{ConnectOptions, TiConnection};

/// Whether live tests can run; prints why not.
pub fn enabled() -> bool {
    if std::env::var("TISQLX_TEST_URL").is_ok_and(|u| !u.is_empty()) {
        return true;
    }
    assert!(std::env::var_os("TISQLX_TEST_REQUIRE").is_none(), "TISQLX_TEST_REQUIRE is set but TISQLX_TEST_URL is not");
    eprintln!("skipped: set TISQLX_TEST_URL to a dedicated TiDB (`make db-up`) to run live tests");
    false
}

/// Return early from a test when no test database is configured.
macro_rules! require_db {
    () => {
        if !$crate::common::enabled() {
            return;
        }
    };
}
pub(crate) use require_db;

/// Whether the server is at least `major.minor`; prints why a test is skipped otherwise.
pub fn tidb_at_least(conn: &TiConnection, major: u32, minor: u32, feature: &str) -> bool {
    let v = conn.tidb_version().expect("TiDB");
    let ok = v.at_least(major, minor);
    if !ok {
        eprintln!("skipped: {feature} needs TiDB {major}.{minor}+, server is {v:?}");
    }
    ok
}

/// The two TiDB nodes of the cluster tier (`TISQLX_TEST_CLUSTER=url1,url2`, set by
/// `make test-cluster`), or `None` (test skipped). `TISQLX_TEST_REQUIRE_CLUSTER=1` makes
/// a missing cluster a failure.
pub fn cluster() -> Option<(String, String)> {
    let urls = std::env::var("TISQLX_TEST_CLUSTER").ok().filter(|u| !u.is_empty());
    let Some((a, b)) = urls.as_deref().and_then(|u| u.split_once(',')) else {
        assert!(
            std::env::var_os("TISQLX_TEST_REQUIRE_CLUSTER").is_none(),
            "TISQLX_TEST_REQUIRE_CLUSTER is set but TISQLX_TEST_CLUSTER is not"
        );
        eprintln!("skipped: cluster test; set TISQLX_TEST_CLUSTER=url1,url2 (`make db-cluster`)");
        return None;
    };
    Some((a.trim().to_owned(), b.trim().to_owned()))
}

/// Create (or recreate) `tisqlx_c_<name>` on the cluster and return options for each node.
pub async fn fresh_cluster_db(name: &str, (a, b): &(String, String)) -> (ConnectOptions, ConnectOptions) {
    let db = format!("tisqlx_c_{name}");
    let mut admin = TiConnection::connect(a).await.expect("connect to the cluster");
    admin.execute_raw(&format!("DROP DATABASE IF EXISTS {db}; CREATE DATABASE {db}")).await.unwrap();
    (a.parse::<ConnectOptions>().unwrap().database(&db), b.parse::<ConnectOptions>().unwrap().database(&db))
}

/// Server URL without a database.
pub fn base_url() -> String {
    std::env::var("TISQLX_TEST_URL").expect("TISQLX_TEST_URL")
}

/// Unix socket of the test server, if it has one (`TISQLX_TEST_SOCKET`).
pub fn socket() -> Option<std::path::PathBuf> {
    std::env::var_os("TISQLX_TEST_SOCKET").map(Into::into).filter(|p: &std::path::PathBuf| p.exists())
}

/// Create (or recreate) the database `tisqlx_t_<name>` and return options pointing at it.
pub async fn fresh_db(name: &str) -> ConnectOptions {
    let db = format!("tisqlx_t_{name}");
    let mut admin = TiConnection::connect(&base_url()).await.expect("connect to TISQLX_TEST_URL");
    admin.execute_raw(&format!("DROP DATABASE IF EXISTS {db}; CREATE DATABASE {db}")).await.unwrap();
    admin.close().await.unwrap();
    base_url().parse::<ConnectOptions>().unwrap().database(&db)
}
