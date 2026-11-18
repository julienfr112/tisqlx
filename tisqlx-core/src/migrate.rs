//! Versioned SQL migrations.
//!
//! Migration files live in a directory and are named either
//! `<VERSION>_<DESCRIPTION>.sql` (simple) or
//! `<VERSION>_<DESCRIPTION>.up.sql` / `.down.sql` (reversible).
//!
//! TiDB commits DDL implicitly, so a migration cannot be atomic. tisqlx records
//! the migration as *dirty* before running it and marks it successful afterwards;
//! a failure leaves it dirty, and later runs refuse to continue until it is fixed
//! (see [`Migrator::repair`]).

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha384};

use crate::connection::TiConnection;
use crate::error::{BoxDynError, Error};
use crate::pool::Pool;
use crate::query::{query, query_as};
use crate::util::quote_ident;

pub const DEFAULT_TABLE: &str = "_tisqlx_migrations";

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MigrateError {
    #[error("while executing migrations: {0}")]
    Execute(Error),

    #[error("while executing migration {0}: {1}")]
    ExecuteMigration(i64, Error),

    #[error("while resolving migrations: {0}")]
    Source(BoxDynError),

    #[error("migration {0} was previously applied but is missing in the resolved migrations")]
    VersionMissing(i64),

    #[error("migration {0} was previously applied but has been modified")]
    VersionMismatch(i64),

    #[error("migration {0} is not present in the migration source")]
    VersionNotPresent(i64),

    #[error("migration {0} is not reversible: it has no .down.sql file")]
    Irreversible(i64),

    #[error("migration {0} has no record in the migrations table")]
    NotApplied(i64),

    #[error("migration {0} is not dirty; refusing to remove the record of an applied migration")]
    NotDirty(i64),

    #[error("migration {0} is older than the latest applied migration {1}")]
    VersionTooOld(i64, i64),

    #[error("migration {0} is partially applied (dirty); fix the database manually, then run `repair`")]
    Dirty(i64),

    #[error("cannot mix reversible migrations with simple migrations")]
    InvalidMixReversibleAndSimple,

    #[error("could not acquire the migration lock within {0:?}")]
    LockTimeout(Duration),
}

impl From<Error> for MigrateError {
    fn from(e: Error) -> Self {
        MigrateError::Execute(e)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationType {
    Simple,
    ReversibleUp,
    ReversibleDown,
}

impl MigrationType {
    pub fn is_up(self) -> bool {
        !matches!(self, Self::ReversibleDown)
    }

    pub fn is_reversible(self) -> bool {
        !matches!(self, Self::Simple)
    }

    fn from_filename(name: &str) -> Self {
        if name.ends_with(".up.sql") {
            Self::ReversibleUp
        } else if name.ends_with(".down.sql") {
            Self::ReversibleDown
        } else {
            Self::Simple
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            Self::Simple => ".sql",
            Self::ReversibleUp => ".up.sql",
            Self::ReversibleDown => ".down.sql",
        }
    }
}

/// A single migration.
#[derive(Debug, Clone)]
pub struct Migration {
    pub version: i64,
    pub description: Cow<'static, str>,
    pub migration_type: MigrationType,
    pub sql: Cow<'static, str>,
    pub checksum: Cow<'static, [u8]>,
}

impl Migration {
    pub fn new(
        version: i64,
        description: Cow<'static, str>,
        migration_type: MigrationType,
        sql: Cow<'static, str>,
    ) -> Self {
        let checksum = Cow::Owned(Sha384::digest(sql.as_bytes()).to_vec());
        Self { version, description, migration_type, sql, checksum }
    }
}

/// A migration recorded in the database.
#[derive(Debug, Clone)]
pub struct AppliedMigration {
    pub version: i64,
    pub description: String,
    /// `false` while the migration runs, and after it failed (dirty).
    pub success: bool,
    pub checksum: Vec<u8>,
    /// Nanoseconds; -1 while running, or when marked applied by [`Migrator::repair`].
    pub execution_time: i64,
}

/// Read every migration in `dir` with its file path, sorted by version.
pub fn resolve_dir(dir: impl AsRef<Path>) -> Result<Vec<(Migration, PathBuf)>, MigrateError> {
    let dir = dir.as_ref();
    let entries = std::fs::read_dir(dir)
        .map_err(|e| MigrateError::Source(format!("reading migrations directory {}: {e}", dir.display()).into()))?;
    let mut migrations = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| MigrateError::Source(e.into()))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Err(MigrateError::Source(format!("migration file name {path:?} is not UTF-8").into()));
        };
        if !name.ends_with(".sql") {
            continue;
        }
        let (version, rest) = name.split_once('_').ok_or_else(|| {
            MigrateError::Source(format!("migration file {name:?} must be named <VERSION>_<DESCRIPTION>.sql").into())
        })?;
        let version: i64 = version
            .parse()
            .map_err(|_| MigrateError::Source(format!("migration file {name:?} has a non-numeric version").into()))?;
        let migration_type = MigrationType::from_filename(name);
        let description = rest.trim_end_matches(migration_type.suffix()).replace('_', " ");
        let sql = std::fs::read_to_string(&path)
            .map_err(|e| MigrateError::Source(format!("reading {}: {e}", path.display()).into()))?;
        migrations.push((Migration::new(version, description.into(), migration_type, sql.into()), path));
    }
    migrations.sort_by_key(|(m, _)| (m.version, m.migration_type.is_up()));
    // e.g. `0001_a.sql` and `1_b.sql`, or `2_x.sql` and `2_x.up.sql`
    for pair in migrations.windows(2) {
        let (a, b) = (&pair[0].0, &pair[1].0);
        if a.version == b.version && a.migration_type.is_up() == b.migration_type.is_up() {
            return Err(MigrateError::Source(
                format!(
                    "duplicate migration version {}: {} and {}",
                    a.version,
                    pair[0].1.display(),
                    pair[1].1.display()
                )
                .into(),
            ));
        }
    }
    Ok(migrations)
}

/// Applies and reverts migrations.
#[derive(Debug, Clone)]
pub struct Migrator {
    pub migrations: Cow<'static, [Migration]>,
    /// Tolerate applied migrations that are missing locally, and pending ones older than the latest applied.
    pub ignore_missing: bool,
    pub table_name: Cow<'static, str>,
    /// How long to wait for the migration lock (`GET_LOCK`).
    pub lock_timeout: Duration,
}

impl Migrator {
    /// Construct from pre-built migrations (used by the `migrate!` macro).
    pub const fn from_static(migrations: &'static [Migration]) -> Self {
        Self {
            migrations: Cow::Borrowed(migrations),
            ignore_missing: false,
            table_name: Cow::Borrowed(DEFAULT_TABLE),
            lock_timeout: Duration::from_secs(600),
        }
    }

    /// Load migrations from a directory at runtime.
    pub fn new(dir: impl AsRef<Path>) -> Result<Self, MigrateError> {
        let migrations = resolve_dir(dir)?.into_iter().map(|(m, _)| m).collect::<Vec<_>>();
        Ok(Self { migrations: Cow::Owned(migrations), ..Self::from_static(&[]) })
    }

    /// The local up (or simple) migration with this version.
    pub fn find_up(&self, version: i64) -> Option<&Migration> {
        self.migrations.iter().find(|m| m.version == version && m.migration_type.is_up())
    }

    fn table(&self) -> String {
        quote_ident(&self.table_name)
    }

    fn check_mix(&self) -> Result<(), MigrateError> {
        // Simple migrations are allowed only before the first reversible one.
        let first_rev = self.migrations.iter().find(|m| m.migration_type.is_reversible()).map(|m| m.version);
        match first_rev {
            Some(first) if self.migrations.iter().any(|m| !m.migration_type.is_reversible() && m.version > first) => {
                Err(MigrateError::InvalidMixReversibleAndSimple)
            }
            _ => Ok(()),
        }
    }

    /// Check the recorded migrations against the local ones: none dirty, none
    /// modified, none missing (unless `ignore_missing`).
    pub fn validate(&self, applied: &[AppliedMigration]) -> Result<(), MigrateError> {
        if let Some(dirty) = applied.iter().find(|m| !m.success) {
            return Err(MigrateError::Dirty(dirty.version));
        }
        for a in applied {
            match self.find_up(a.version) {
                Some(m) if *m.checksum != *a.checksum => return Err(MigrateError::VersionMismatch(a.version)),
                Some(_) => {}
                None if self.ignore_missing => {}
                None => return Err(MigrateError::VersionMissing(a.version)),
            }
        }
        Ok(())
    }

    /// The migrations [`run_to`](Self::run_to) would apply, in order.
    pub fn pending(&self, applied: &[AppliedMigration], target: Option<i64>) -> Result<Vec<&Migration>, MigrateError> {
        self.check_mix()?;
        self.validate(applied)?;
        let latest = applied.iter().map(|a| a.version).max();
        let mut pending = Vec::new();
        for m in self.migrations.iter().filter(|m| m.migration_type.is_up()) {
            if applied.iter().any(|a| a.version == m.version) {
                continue;
            }
            if target.is_some_and(|t| m.version > t) {
                break;
            }
            if let Some(latest) = latest.filter(|&l| m.version < l)
                && !self.ignore_missing
            {
                return Err(MigrateError::VersionTooOld(m.version, latest));
            }
            pending.push(m);
        }
        Ok(pending)
    }

    /// The down migrations [`undo`](Self::undo) would run, newest first: every
    /// applied migration newer than `target`, or only the latest one if `None`.
    pub fn to_revert(
        &self,
        applied: &[AppliedMigration],
        target: Option<i64>,
    ) -> Result<Vec<&Migration>, MigrateError> {
        self.check_mix()?;
        self.validate(applied)?;
        let mut versions: Vec<i64> = applied.iter().map(|a| a.version).collect();
        versions.sort_unstable_by(|a, b| b.cmp(a));
        let versions = match target {
            Some(t) => versions.into_iter().filter(|&v| v > t).collect(),
            None => versions.into_iter().take(1).collect::<Vec<_>>(),
        };
        versions
            .into_iter()
            .map(|v| {
                self.migrations
                    .iter()
                    .find(|m| m.version == v && m.migration_type == MigrationType::ReversibleDown)
                    .ok_or(if self.find_up(v).is_some() {
                        MigrateError::Irreversible(v)
                    } else {
                        MigrateError::VersionNotPresent(v)
                    })
            })
            .collect()
    }

    /// Apply every pending migration using a connection from `pool`.
    pub async fn run(&self, pool: &Pool) -> Result<(), MigrateError> {
        let mut conn = pool.acquire().await?;
        self.run_on(&mut conn).await
    }

    /// Apply every pending migration on `conn`.
    pub async fn run_on(&self, conn: &mut TiConnection) -> Result<(), MigrateError> {
        self.with_lock(conn, |this, conn, table| Box::pin(this.run_locked(conn, table, None))).await
    }

    /// Apply pending migrations up to and including `target` version.
    pub async fn run_to(&self, conn: &mut TiConnection, target: i64) -> Result<(), MigrateError> {
        self.with_lock(conn, move |this, conn, table| Box::pin(this.run_locked(conn, table, Some(target)))).await
    }

    /// Revert applied reversible migrations newer than `target` (or only the latest one if `None`).
    pub async fn undo(&self, conn: &mut TiConnection, target: Option<i64>) -> Result<(), MigrateError> {
        self.with_lock(conn, move |this, conn, table| Box::pin(this.undo_locked(conn, table, target))).await
    }

    /// Clear the dirty flag of `version` after fixing the database by hand,
    /// marking it applied (`applied = true`, also accepting a modified file) or
    /// removing its record (`false`, only for a dirty migration).
    pub async fn repair(&self, conn: &mut TiConnection, version: i64, applied: bool) -> Result<(), MigrateError> {
        // Under the lock: a migration that is still running also looks dirty.
        self.with_lock(conn, move |this, conn, table| Box::pin(this.repair_locked(conn, table, version, applied))).await
    }

    async fn repair_locked(
        &self,
        conn: &mut TiConnection,
        table: String,
        version: i64,
        applied: bool,
    ) -> Result<(), MigrateError> {
        if !applied && self.applied(conn).await?.iter().any(|a| a.version == version && a.success) {
            return Err(MigrateError::NotDirty(version));
        }
        let done = if applied {
            let m = self.find_up(version).ok_or(MigrateError::VersionNotPresent(version))?;
            query(format!("UPDATE {table} SET success = TRUE, checksum = ? WHERE version = ?"))
                .bind(&*m.checksum)
                .bind(version)
                .execute(&mut *conn)
                .await
        } else {
            query(format!("DELETE FROM {table} WHERE version = ?")).bind(version).execute(&mut *conn).await
        };
        match done {
            Ok(r) if r.rows_affected > 0 => Ok(()),
            // A missing table (1146) means nothing was ever applied.
            Ok(_) => Err(MigrateError::NotApplied(version)),
            Err(Error::Database(e)) if e.code == 1146 => Err(MigrateError::NotApplied(version)),
            Err(e) => Err(e.into()),
        }
    }

    /// Migrations recorded in the database, oldest first (empty if the table does not exist).
    pub async fn applied(&self, conn: &mut TiConnection) -> Result<Vec<AppliedMigration>, MigrateError> {
        type Record = (i64, String, bool, Vec<u8>, i64);
        let rows: Result<Vec<Record>, Error> = query_as(format!(
            "SELECT version, description, success, checksum, execution_time FROM {} ORDER BY version",
            self.table()
        ))
        .fetch_all(&mut *conn)
        .await;
        let rows = match rows {
            Err(Error::Database(e)) if e.code == 1146 => Vec::new(),
            other => other?,
        };
        Ok(rows
            .into_iter()
            .map(|(version, description, success, checksum, execution_time)| AppliedMigration {
                version,
                description,
                success,
                checksum,
                execution_time,
            })
            .collect())
    }

    /// Run `f` holding the migration lock. `f` gets the migrations table name qualified
    /// with the current database, so migration SQL that runs `USE other` cannot redirect
    /// the bookkeeping.
    async fn with_lock<'a, F>(&'a self, conn: &'a mut TiConnection, f: F) -> Result<(), MigrateError>
    where
        F: for<'c> FnOnce(
            &'c Self,
            &'c mut TiConnection,
            String,
        ) -> futures_core::future::BoxFuture<'c, Result<(), MigrateError>>,
    {
        self.check_mix()?;
        let db: Option<String> = crate::query::query_scalar("SELECT DATABASE()").fetch_one(&mut *conn).await?;
        let db = db.ok_or_else(|| {
            MigrateError::Execute(Error::config("no database selected: put the database name in the URL"))
        })?;
        // A previous run on this connection may have been cancelled while holding the lock.
        conn.release_user_locks().await?;
        let lock_name = Self::lock_name(&db, &self.table_name);
        // Set before GET_LOCK: if this future is dropped from here on, a pooled
        // connection releases the lock when it is returned (`reset_session`).
        conn.user_locks = true;
        let got: Option<i64> = crate::query::query_scalar("SELECT GET_LOCK(?, ?)")
            .bind(&lock_name)
            .bind(lock_timeout_secs(self.lock_timeout))
            .fetch_one(&mut *conn)
            .await?;
        if got != Some(1) {
            conn.user_locks = false;
            return Err(MigrateError::LockTimeout(self.lock_timeout));
        }
        let table = format!("{}.{}", quote_ident(&db), self.table());
        let result = f(self, &mut *conn, table).await;
        let released = query("SELECT RELEASE_LOCK(?)").bind(&lock_name).execute(&mut *conn).await;
        if released.is_ok() {
            conn.user_locks = false;
        }
        result?;
        released?;
        Ok(())
    }

    /// `GET_LOCK` names are limited to 64 characters.
    fn lock_name(db: &str, table: &str) -> String {
        let hash = crate::util::hex(&Sha384::digest(format!("{db}.{table}").as_bytes())[..16]);
        format!("tisqlx_migrate_{hash}")
    }

    async fn ensure_table(&self, conn: &mut TiConnection) -> Result<(), MigrateError> {
        conn.execute_raw(&format!(
            "CREATE TABLE IF NOT EXISTS {} (
                version BIGINT PRIMARY KEY,
                description TEXT NOT NULL,
                installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
                success BOOLEAN NOT NULL,
                checksum VARBINARY(64) NOT NULL,
                execution_time BIGINT NOT NULL
            )",
            self.table()
        ))
        .await?;
        Ok(())
    }

    /// A migration must not leave a transaction open: the bookkeeping that follows would
    /// join it (and be rolled back with it). Roll it back and fail, leaving the migration dirty.
    async fn no_open_transaction(&self, conn: &mut TiConnection, version: i64) -> Result<(), MigrateError> {
        if conn.in_transaction() {
            conn.execute_raw("ROLLBACK").await?;
            return Err(MigrateError::ExecuteMigration(
                version,
                Error::config("the migration left a transaction open (missing COMMIT); it was rolled back"),
            ));
        }
        Ok(())
    }

    async fn run_locked(
        &self,
        conn: &mut TiConnection,
        table: String,
        target: Option<i64>,
    ) -> Result<(), MigrateError> {
        self.ensure_table(conn).await?;
        let applied = self.applied(conn).await?;
        for m in self.pending(&applied, target)? {
            log::info!("applying migration {} {}", m.version, m.description);
            query(format!(
                "INSERT INTO {table} (version, description, success, checksum, execution_time) VALUES (?, ?, FALSE, ?, -1)"
            ))
            .bind(m.version)
            .bind(&*m.description)
            .bind(&*m.checksum)
            .execute(&mut *conn)
            .await?;
            let start = Instant::now();
            conn.execute_raw(&m.sql).await.map_err(|e| MigrateError::ExecuteMigration(m.version, e))?;
            self.no_open_transaction(conn, m.version).await?;
            let elapsed = start.elapsed().as_nanos() as i64;
            let done = query(format!("UPDATE {table} SET success = TRUE, execution_time = ? WHERE version = ?"))
                .bind(elapsed)
                .bind(m.version)
                .execute(&mut *conn)
                .await?;
            if done.rows_affected == 0 {
                // The record was removed while the migration ran.
                return Err(MigrateError::NotApplied(m.version));
            }
        }
        Ok(())
    }

    async fn undo_locked(
        &self,
        conn: &mut TiConnection,
        table: String,
        target: Option<i64>,
    ) -> Result<(), MigrateError> {
        let applied = self.applied(conn).await?;
        for down in self.to_revert(&applied, target)? {
            let version = down.version;
            log::info!("reverting migration {version} {}", down.description);
            query(format!("UPDATE {table} SET success = FALSE WHERE version = ?"))
                .bind(version)
                .execute(&mut *conn)
                .await?;
            conn.execute_raw(&down.sql).await.map_err(|e| MigrateError::ExecuteMigration(version, e))?;
            self.no_open_transaction(conn, version).await?;
            query(format!("DELETE FROM {table} WHERE version = ?")).bind(version).execute(&mut *conn).await?;
        }
        Ok(())
    }
}

/// `GET_LOCK` takes whole seconds: round up, so a sub-second timeout still waits.
fn lock_timeout_secs(d: Duration) -> i64 {
    d.as_secs_f64().ceil() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_timeout_rounds_up() {
        assert_eq!(lock_timeout_secs(Duration::from_millis(1)), 1);
        assert_eq!(lock_timeout_secs(Duration::from_secs(2)), 2);
        assert_eq!(lock_timeout_secs(Duration::from_millis(2500)), 3);
    }

    fn m(version: i64, ty: MigrationType, sql: &'static str) -> Migration {
        Migration::new(version, Cow::Borrowed("m"), ty, Cow::Borrowed(sql))
    }

    fn simple(versions: &[i64]) -> Migrator {
        let ms = versions.iter().map(|&v| m(v, MigrationType::Simple, "SELECT 1")).collect::<Vec<_>>();
        Migrator { migrations: Cow::Owned(ms), ..Migrator::from_static(&[]) }
    }

    fn reversible(versions: &[i64]) -> Migrator {
        let ms = versions
            .iter()
            .flat_map(|&v| [m(v, MigrationType::ReversibleDown, "DOWN"), m(v, MigrationType::ReversibleUp, "UP")])
            .collect::<Vec<_>>();
        Migrator { migrations: Cow::Owned(ms), ..Migrator::from_static(&[]) }
    }

    fn applied(m: &Migrator, versions: &[i64]) -> Vec<AppliedMigration> {
        versions
            .iter()
            .map(|&v| AppliedMigration {
                version: v,
                description: "m".into(),
                success: true,
                checksum: m.find_up(v).map_or_else(Vec::new, |m| m.checksum.to_vec()),
                execution_time: 1,
            })
            .collect()
    }

    fn versions(ms: Vec<&Migration>) -> Vec<i64> {
        ms.into_iter().map(|m| m.version).collect()
    }

    #[test]
    fn pending_in_order_up_to_target() {
        let mig = simple(&[1, 2, 3]);
        assert_eq!(versions(mig.pending(&[], None).unwrap()), [1, 2, 3]);
        assert_eq!(versions(mig.pending(&applied(&mig, &[1]), None).unwrap()), [2, 3]);
        assert_eq!(versions(mig.pending(&applied(&mig, &[1]), Some(2)).unwrap()), [2]);
        assert!(mig.pending(&applied(&mig, &[1, 2, 3]), None).unwrap().is_empty());
        // reversible pairs yield only the up half
        let rev = reversible(&[1, 2]);
        let pending = rev.pending(&[], None).unwrap();
        assert!(pending.iter().all(|m| m.migration_type == MigrationType::ReversibleUp));
        assert_eq!(versions(pending), [1, 2]);
    }

    #[test]
    fn validation_errors() {
        let mig = simple(&[1, 2, 3]);
        let mut a = applied(&mig, &[1, 2]);
        a[1].success = false;
        assert!(matches!(mig.pending(&a, None), Err(MigrateError::Dirty(2))));

        let mut a = applied(&mig, &[1]);
        a[0].checksum = vec![0];
        assert!(matches!(mig.pending(&a, None), Err(MigrateError::VersionMismatch(1))));

        // applied 9 is unknown locally
        let mut a = applied(&mig, &[1]);
        a.push(AppliedMigration { version: 9, ..a[0].clone() });
        assert!(matches!(mig.pending(&a, None), Err(MigrateError::VersionMissing(9))));
        let lenient = Migrator { ignore_missing: true, ..mig.clone() };
        assert_eq!(versions(lenient.pending(&a, None).unwrap()), [2, 3]);

        // 2 was added after 3 was applied
        let a = applied(&mig, &[1, 3]);
        assert!(matches!(mig.pending(&a, None), Err(MigrateError::VersionTooOld(2, 3))));
        assert_eq!(versions(lenient.pending(&a, None).unwrap()), [2]);
    }

    #[test]
    fn revert_plan() {
        let rev = reversible(&[1, 2, 3]);
        let a = applied(&rev, &[1, 2, 3]);
        assert_eq!(versions(rev.to_revert(&a, None).unwrap()), [3]);
        assert_eq!(versions(rev.to_revert(&a, Some(1)).unwrap()), [3, 2]);
        assert_eq!(versions(rev.to_revert(&a, Some(0)).unwrap()), [3, 2, 1]);
        assert!(rev.to_revert(&[], None).unwrap().is_empty());
        assert!(rev.to_revert(&a, None).unwrap().iter().all(|m| m.migration_type == MigrationType::ReversibleDown));

        let mig = simple(&[1, 2]);
        assert!(matches!(mig.to_revert(&applied(&mig, &[1, 2]), None), Err(MigrateError::Irreversible(2))));
        let lenient = Migrator { ignore_missing: true, ..rev.clone() };
        let mut a = applied(&rev, &[1]);
        a.push(AppliedMigration { version: 7, ..a[0].clone() });
        assert!(matches!(lenient.to_revert(&a, None), Err(MigrateError::VersionNotPresent(7))));
    }

    #[test]
    fn simple_after_reversible_is_rejected() {
        let mut ms = reversible(&[1]).migrations.into_owned();
        ms.push(m(2, MigrationType::Simple, "S"));
        let mig = Migrator { migrations: Cow::Owned(ms), ..Migrator::from_static(&[]) };
        assert!(matches!(mig.pending(&[], None), Err(MigrateError::InvalidMixReversibleAndSimple)));
        // simple before the first reversible one is fine
        let mut ms = simple(&[1]).migrations.into_owned();
        ms.extend(reversible(&[2]).migrations.iter().cloned());
        let mig = Migrator { migrations: Cow::Owned(ms), ..Migrator::from_static(&[]) };
        assert_eq!(versions(mig.pending(&[], None).unwrap()), [1, 2]);
    }

    #[test]
    fn resolve_dir_names_and_duplicates() {
        let dir = std::env::temp_dir().join(format!("tisqlx_resolve_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str| std::fs::write(dir.join(name), "SELECT 1").unwrap();
        write("0001_create_users.up.sql");
        write("0001_create_users.down.sql");
        write("2_add_index.up.sql");
        write("2_add_index.down.sql");
        write("README.md");
        let ms = resolve_dir(&dir).unwrap();
        let got: Vec<_> = ms.iter().map(|(m, _)| (m.version, &*m.description, m.migration_type)).collect();
        assert_eq!(
            got,
            [
                (1, "create users", MigrationType::ReversibleDown),
                (1, "create users", MigrationType::ReversibleUp),
                (2, "add index", MigrationType::ReversibleDown),
                (2, "add index", MigrationType::ReversibleUp),
            ]
        );
        write("1_other.up.sql"); // same version as 0001
        let err = resolve_dir(&dir).unwrap_err();
        assert!(err.to_string().contains("duplicate migration version 1"), "{err}");
        write("x_bad.sql");
        std::fs::remove_file(dir.join("1_other.up.sql")).unwrap();
        assert!(resolve_dir(&dir).unwrap_err().to_string().contains("non-numeric"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
