use tisqlx_core::migrate::{MigrateError, Migrator};
use tisqlx_core::*;

use crate::common::*;

fn dir(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let d = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("migrations_{name}"));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    for (f, sql) in files {
        std::fs::write(d.join(f), sql).unwrap();
    }
    d
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn run_rerun_mismatch_dirty_repair() {
    require_db!();
    let opts = fresh_db("mig1").await;
    let mut conn = opts.connect().await.unwrap();
    let d = dir(
        "mig1",
        &[
            (
                "1_create_users.sql",
                "CREATE TABLE users (id BIGINT PRIMARY KEY AUTO_RANDOM, name VARCHAR(50) NOT NULL);\n-- trailing comment\n",
            ),
            ("2_seed.sql", "INSERT INTO users (name) VALUES ('a');\nINSERT INTO users (name) VALUES ('b');"),
        ],
    );
    let m = Migrator::new(&d).unwrap();
    m.run_on(&mut conn).await.unwrap();
    m.run_on(&mut conn).await.unwrap(); // no-op
    let n: i64 = query_scalar("SELECT COUNT(*) FROM users").fetch_one(&mut conn).await.unwrap();
    assert_eq!(n, 2);
    assert_eq!(m.applied(&mut conn).await.unwrap().len(), 2);

    // Edited applied migration.
    std::fs::write(d.join("2_seed.sql"), "INSERT INTO users (name) VALUES ('changed');").unwrap();
    let err = Migrator::new(&d).unwrap().run_on(&mut conn).await.unwrap_err();
    assert!(matches!(err, MigrateError::VersionMismatch(2)), "{err}");
    std::fs::write(
        d.join("2_seed.sql"),
        "INSERT INTO users (name) VALUES ('a');\nINSERT INTO users (name) VALUES ('b');",
    )
    .unwrap();

    // Failing second statement => dirty.
    std::fs::write(d.join("3_broken.sql"), "CREATE TABLE ok3 (id INT PRIMARY KEY);\nINSERT INTO nope VALUES (1);")
        .unwrap();
    let m = Migrator::new(&d).unwrap();
    let err = m.run_on(&mut conn).await.unwrap_err();
    assert!(matches!(err, MigrateError::ExecuteMigration(3, _)), "{err}");
    let err = m.run_on(&mut conn).await.unwrap_err();
    assert!(matches!(err, MigrateError::Dirty(3)), "{err}");

    // Fix by hand, repair, continue.
    std::fs::write(d.join("3_broken.sql"), "CREATE TABLE IF NOT EXISTS ok3 (id INT PRIMARY KEY);").unwrap();
    let m = Migrator::new(&d).unwrap();
    m.repair(&mut conn, 3, false).await.unwrap();
    m.run_on(&mut conn).await.unwrap();
    assert_eq!(m.applied(&mut conn).await.unwrap().iter().filter(|a| a.success).count(), 3);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn reversible_and_concurrent() {
    require_db!();
    let opts = fresh_db("mig2").await;
    let d = dir(
        "mig2",
        &[
            ("1_a.up.sql", "CREATE TABLE a (id INT PRIMARY KEY);"),
            ("1_a.down.sql", "DROP TABLE a;"),
            ("2_b.up.sql", "CREATE TABLE b (id INT PRIMARY KEY);"),
            ("2_b.down.sql", "DROP TABLE b;"),
        ],
    );
    let m = Migrator::new(&d).unwrap();
    // Concurrent runners serialize on GET_LOCK.
    let (mut c1, mut c2) = (opts.connect().await.unwrap(), opts.connect().await.unwrap());
    let (r1, r2) = tokio::join!(m.run_on(&mut c1), m.run_on(&mut c2));
    r1.unwrap();
    r2.unwrap();
    assert_eq!(m.applied(&mut c1).await.unwrap().len(), 2);

    m.undo(&mut c1, None).await.unwrap();
    assert_eq!(m.applied(&mut c1).await.unwrap().len(), 1);
    assert!(raw_sql("SELECT * FROM b").execute(&mut c1).await.is_err());
    m.undo(&mut c1, Some(0)).await.unwrap();
    assert!(m.applied(&mut c1).await.unwrap().is_empty());
    m.run_to(&mut c1, 1).await.unwrap();
    assert_eq!(m.applied(&mut c1).await.unwrap().len(), 1);
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn repair_and_applied_edge_cases() {
    require_db!();
    let opts = fresh_db("mig3").await;
    let mut conn = opts.connect().await.unwrap();
    let d = dir("mig3", &[("1_a.sql", "CREATE TABLE a (id INT PRIMARY KEY);")]);
    let m = Migrator::new(&d).unwrap();
    // no migrations table yet: nothing applied, nothing to repair, and no table created
    assert!(m.applied(&mut conn).await.unwrap().is_empty());
    assert!(matches!(m.repair(&mut conn, 1, false).await, Err(MigrateError::NotApplied(1))));
    let tables: i64 = query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE()")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(tables, 0);

    m.run_on(&mut conn).await.unwrap();
    // removing the record of a healthy migration is refused
    assert!(matches!(m.repair(&mut conn, 1, false).await, Err(MigrateError::NotDirty(1))));
    assert!(matches!(m.repair(&mut conn, 9, true).await, Err(MigrateError::VersionNotPresent(9))));
    // accepting an edited file is allowed
    std::fs::write(d.join("1_a.sql"), "CREATE TABLE a (id INT PRIMARY KEY); -- edited").unwrap();
    let edited = Migrator::new(&d).unwrap();
    assert!(matches!(edited.run_on(&mut conn).await, Err(MigrateError::VersionMismatch(1))));
    edited.repair(&mut conn, 1, true).await.unwrap();
    edited.run_on(&mut conn).await.unwrap();
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn cancelled_run_releases_the_migration_lock() {
    require_db!();
    let opts = fresh_db("mig4").await;
    let d = dir("mig4", &[("1_slow.sql", "SELECT SLEEP(5);")]);
    let m = Migrator::new(&d).unwrap();
    let pool = PoolOptions::new().max_connections(2).connect_with(opts.clone()).await.unwrap();
    // cancelled while the migration runs, i.e. while holding the lock
    let cancelled = tokio::time::timeout(std::time::Duration::from_millis(500), m.run(&pool)).await;
    assert!(cancelled.is_err());
    // The connection was dropped mid-command, so the pool discarded it and the server
    // ends its session, lock included: another runner gets the lock (and then reports
    // the interrupted migration as dirty).
    let mut other = opts.connect().await.unwrap();
    let quick = Migrator { lock_timeout: std::time::Duration::from_secs(10), ..Migrator::new(&d).unwrap() };
    let err = quick.run_on(&mut other).await.unwrap_err();
    assert!(matches!(err, MigrateError::Dirty(1)), "{err}");
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn migration_sql_cannot_break_the_bookkeeping() {
    require_db!();
    let opts = fresh_db("mig5").await;
    fresh_db("mig5_other").await;
    let mut conn = opts.connect().await.unwrap();
    // `USE` inside a migration redirects neither the bookkeeping nor the next migration
    let d = dir(
        "mig5",
        &[
            ("1_use.sql", "USE tisqlx_t_mig5_other; CREATE TABLE elsewhere (id INT PRIMARY KEY);"),
            ("2_table.sql", "CREATE TABLE t2 (id INT PRIMARY KEY);"),
        ],
    );
    Migrator::new(&d).unwrap().run_on(&mut conn).await.unwrap();
    let db: String = query_scalar("SELECT DATABASE()").fetch_one(&mut conn).await.unwrap();
    assert_eq!(db, "tisqlx_t_mig5", "the connection is back on its database");
    let t2: i64 = query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = DATABASE() AND table_name = 't2'",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(t2, 1, "migration 2 ran in the original database");
    let applied = Migrator::new(&d).unwrap().applied(&mut conn).await.unwrap();
    assert!(applied.iter().all(|a| a.success) && applied.len() == 2);

    // a migration that leaves a transaction open fails and stays dirty
    // (DDL commits implicitly in TiDB, so the open transaction holds only DML)
    std::fs::write(d.join("3_open.sql"), "BEGIN; INSERT INTO t2 VALUES (1);").unwrap();
    let err = Migrator::new(&d).unwrap().run_on(&mut conn).await.unwrap_err();
    assert!(matches!(&err, MigrateError::ExecuteMigration(3, _)) && err.to_string().contains("COMMIT"), "{err}");
    assert!(!conn.in_transaction());
    let n: i64 = query_scalar("SELECT COUNT(*) FROM t2").fetch_one(&mut conn).await.unwrap();
    assert_eq!(n, 0, "the open transaction was rolled back");
    assert!(matches!(Migrator::new(&d).unwrap().run_on(&mut conn).await, Err(MigrateError::Dirty(3))));
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn bookkeeping_is_committed_when_autocommit_is_off() {
    require_db!();
    let opts = fresh_db("migautocommit").await;
    let mut observer = opts.connect().await.unwrap();
    let d = dir(
        "migautocommit",
        &[
            ("1_off.up.sql", "SET autocommit = 0; CREATE TABLE t (id INT PRIMARY KEY);"),
            ("1_off.down.sql", "DROP TABLE t;"),
        ],
    );
    let m = Migrator::new(&d).unwrap();
    // The migration turns autocommit off: what the migrator records afterwards must not stay
    // in a transaction that the end of the session rolls back.
    let mut conn = opts.connect().await.unwrap();
    m.run_on(&mut conn).await.unwrap();
    assert!(!conn.in_transaction());
    let applied = m.applied(&mut observer).await.unwrap();
    assert!(applied.len() == 1 && applied[0].success, "{applied:?}");
    // The same for a session that had it off from the start.
    m.undo(&mut conn, None).await.unwrap();
    assert!(m.applied(&mut observer).await.unwrap().is_empty());
    m.run_on(&mut conn).await.unwrap();
    m.repair(&mut conn, 1, true).await.unwrap();
    drop(conn);
    let applied = m.applied(&mut observer).await.unwrap();
    assert!(applied.len() == 1 && applied[0].success, "{applied:?}");
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn record_deleted_by_the_migration_fails_the_run() {
    require_db!();
    let opts = fresh_db("migdeleted").await;
    let mut conn = opts.connect().await.unwrap();
    let d = dir("migdeleted", &[("1_self_delete.sql", "DELETE FROM _tisqlx_migrations WHERE version = 1;")]);
    let err = Migrator::new(&d).unwrap().run_on(&mut conn).await.unwrap_err();
    assert!(matches!(err, MigrateError::NotApplied(1)), "{err}");
}

#[tokio::test]
#[ignore = "needs a TiDB: run with `make test-live`"]
async fn lock_held_by_another_runner_times_out() {
    require_db!();
    let opts = fresh_db("miglock").await;
    let d = dir("miglock", &[("1_slow.sql", "SELECT SLEEP(3);")]);
    let slow = Migrator::new(&d).unwrap();
    let quick = Migrator { lock_timeout: std::time::Duration::from_secs(1), ..slow.clone() };
    let (mut c1, mut c2) = (opts.connect().await.unwrap(), opts.connect().await.unwrap());
    let (held, waited) = tokio::join!(slow.run_on(&mut c1), async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        quick.run_on(&mut c2).await
    });
    held.unwrap();
    assert!(matches!(waited, Err(MigrateError::LockTimeout(_))), "{waited:?}");
    // the lock was released: the same connection gets it now
    quick.run_on(&mut c2).await.unwrap();
}
