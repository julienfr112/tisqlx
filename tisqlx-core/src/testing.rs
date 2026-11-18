//! Support for `#[tisqlx::test]`: each test gets its own throwaway database.
//!
//! `DATABASE_URL` names the server (its database part is ignored). The test database
//! is `_tisqlx_test_<test name>_<hash>`. It is recreated at the start of every run and
//! dropped when the test finishes, so a failed test leaves its data behind for inspection.

use std::future::Future;

use sha2::{Digest, Sha256};

use crate::connection::TiConnection;
use crate::migrate::Migrator;
use crate::options::ConnectOptions;
use crate::pool::{Pool, PoolOptions};
use crate::util::quote_ident;

/// Database name for the test at `path` (`module::path::fn_name`), at most 64 characters.
pub fn database_name(path: &str) -> String {
    let hash = crate::util::hex(&Sha256::digest(path.as_bytes())[..6]);
    let name: String = path
        .rsplit("::")
        .next()
        .unwrap_or(path)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .take(32)
        .collect();
    format!("_tisqlx_test_{name}_{hash}")
}

/// Run a `#[tisqlx::test]` body: create the database, apply `migrator`, call `test`
/// with a pool on it, then drop the database. Blocks on a new current-thread runtime.
pub fn run<F, Fut, R>(path: &str, migrator: Option<&Migrator>, test: F) -> R
where
    F: FnOnce(Pool) -> Fut,
    Fut: Future<Output = R>,
{
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    rt.block_on(async {
        let url = std::env::var("DATABASE_URL")
            .ok()
            .filter(|u| !u.is_empty())
            .expect("#[tisqlx::test] needs DATABASE_URL pointing at a TiDB server");
        let server: ConnectOptions = url.parse::<ConnectOptions>().expect("invalid DATABASE_URL").no_database();
        let db = database_name(path);
        let mut admin = TiConnection::connect_with(&server).await.expect("connect to DATABASE_URL");
        admin
            .execute_raw(&format!("DROP DATABASE IF EXISTS {0}; CREATE DATABASE {0}", quote_ident(&db)))
            .await
            .expect("create the test database");
        let pool = PoolOptions::new()
            .max_connections(8)
            .connect_with(server.database(&db))
            .await
            .expect("connect to the test database");
        if let Some(m) = migrator {
            m.run(&pool).await.expect("apply migrations to the test database");
        }
        let result = test(pool.clone()).await;
        pool.close().await;
        admin
            .execute_raw(&format!("DROP DATABASE IF EXISTS {}", quote_ident(&db)))
            .await
            .expect("drop the test database");
        let _ = admin.close().await;
        result
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_are_short_and_unique() {
        let a = super::database_name("app::tests::creates_a_user");
        let b = super::database_name("app::other::creates_a_user");
        assert!(a.starts_with("_tisqlx_test_creates_a_user_"));
        assert_ne!(a, b);
        assert!(super::database_name(&format!("m::{}", "x".repeat(200))).len() <= 64);
    }
}
