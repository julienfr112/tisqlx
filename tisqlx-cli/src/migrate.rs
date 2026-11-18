//! `tisqlx migrate ...`

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, anyhow, bail};
use chrono::{DateTime, Utc};
use tisqlx_core::TiConnection;
use tisqlx_core::migrate::{AppliedMigration, MigrateError, Migration, MigrationType, Migrator};

use crate::opt::{AddOpts, ConnectOpts};

// ---------------------------------------------------------------------------
// migrate add
// ---------------------------------------------------------------------------

/// How to number a new migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionStyle {
    Sequential,
    Timestamp,
}

/// Infer the numbering style from existing (up/simple, deduplicated, sorted) versions, like sqlx:
/// sequential if the last two versions are consecutive, or if the only migration is 0 or 1.
pub fn infer_style(versions: &[i64]) -> VersionStyle {
    // Timestamps bumped past a same-second collision are consecutive too.
    if versions.last().is_some_and(|&v| v >= 10_000_000_000_000) {
        return VersionStyle::Timestamp;
    }
    match versions {
        [.., a, b] if b - a == 1 => VersionStyle::Sequential,
        [only] if *only == 0 || *only == 1 => VersionStyle::Sequential,
        _ => VersionStyle::Timestamp,
    }
}

/// Compute the next version. A timestamp is bumped past the latest existing
/// version if needed (e.g. two migrations added within the same second).
pub fn next_version(versions: &[i64], style: VersionStyle, now: DateTime<Utc>) -> i64 {
    let max = versions.iter().copied().max();
    match style {
        VersionStyle::Sequential => max.map_or(1, |m| m + 1),
        VersionStyle::Timestamp => {
            let ts: i64 = now.format("%Y%m%d%H%M%S").to_string().parse().expect("timestamp is numeric");
            max.map_or(ts, |m| ts.max(m + 1))
        }
    }
}

/// `<VERSION>_<description_with_underscores><suffix>`
pub fn file_name(version: i64, style: VersionStyle, description: &str, ty: MigrationType) -> String {
    let desc = description.split_whitespace().collect::<Vec<_>>().join("_");
    match style {
        VersionStyle::Sequential => format!("{version:04}_{desc}{}", ty.suffix()),
        VersionStyle::Timestamp => format!("{version}_{desc}{}", ty.suffix()),
    }
}

fn file_template(ty: MigrationType) -> &'static str {
    match ty {
        MigrationType::Simple => "-- Add migration script here\n",
        MigrationType::ReversibleUp => "-- Add up migration script here\n",
        MigrationType::ReversibleDown => "-- Add down migration script here\n",
    }
}

pub fn add(opts: &AddOpts) -> anyhow::Result<()> {
    let description = opts.description.join(" ");
    let description = description.trim();
    if description.is_empty() {
        bail!("the migration description must not be empty");
    }
    if description.contains(['/', '\\', '.']) {
        bail!("the migration description must not contain `/`, `\\` or `.`");
    }

    let source = &opts.source.source;
    let first = fs::read_dir(source).map(|mut d| d.next().is_none()).unwrap_or(true);
    fs::create_dir_all(source)
        .with_context(|| format!("failed to create migrations directory {}", source.display()))?;
    let migrations: Vec<Migration> = tisqlx_core::migrate::resolve_dir(source)
        .with_context(|| format!("failed to read migrations in {}", source.display()))?
        .into_iter()
        .map(|(m, _)| m)
        .collect();

    let ups: Vec<&Migration> = migrations.iter().filter(|m| m.migration_type.is_up()).collect();
    let versions: Vec<i64> = ups.iter().map(|m| m.version).collect::<BTreeSet<_>>().into_iter().collect();

    let style = if opts.sequential {
        VersionStyle::Sequential
    } else if opts.timestamp {
        VersionStyle::Timestamp
    } else {
        infer_style(&versions)
    };
    let version = next_version(&versions, style, Utc::now());

    let reversible = if opts.reversible {
        true
    } else if opts.simple {
        false
    } else {
        ups.last().is_some_and(|m| m.migration_type.is_reversible())
    };
    if !reversible && ups.iter().any(|m| m.migration_type.is_reversible()) {
        bail!(
            "cannot add a simple migration after reversible ones: the migrator only allows simple migrations \
             older than the first reversible one (drop --simple)"
        );
    }
    let types: &[MigrationType] = if reversible {
        &[MigrationType::ReversibleUp, MigrationType::ReversibleDown]
    } else {
        &[MigrationType::Simple]
    };

    for &ty in types {
        let path = source.join(file_name(version, style, description, ty));
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        f.write_all(file_template(ty).as_bytes())?;
        println!("Creating {}", path.display());
    }

    if first {
        let arg = if source == Path::new("migrations") {
            String::new()
        } else {
            format!("{:?}", source.display().to_string())
        };
        println!(
            r#"
Congratulations on creating your first migration!

Did you know you can embed your migrations in your application binary?
On startup, after connecting, add:

    tisqlx::migrate!({arg}).run(&pool).await?;

Cargo does not notice new migration files unless a Rust source file changes,
so add a build.rs containing:

    fn main() {{
        {rerun}
    }}

(`tisqlx migrate build-script` writes it for you.)"#,
            rerun = rerun_if_changed(source)
        );
    }
    Ok(())
}

/// The build-script statement that watches `source`. The path is an argument, not part of the
/// format string, so its braces need no escaping.
fn rerun_if_changed(source: &Path) -> String {
    format!("println!(\"cargo:rerun-if-changed={{}}\", {:?});", source.display().to_string())
}

pub fn build_script(source: &Path, force: bool) -> anyhow::Result<()> {
    let path = Path::new("build.rs");
    if path.exists() && !force {
        bail!("build.rs already exists; pass --force to overwrite it");
    }
    let contents = format!(
        "// generated by `tisqlx migrate build-script`\nfn main() {{\n    // trigger recompilation when a new migration is added\n    {}\n}}\n",
        rerun_if_changed(source)
    );
    fs::write(path, contents).context("failed to write build.rs")?;
    println!("Created build.rs");
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn load(source: &Path, ignore_missing: bool) -> anyhow::Result<Migrator> {
    let mut m =
        Migrator::new(source).with_context(|| format!("failed to load migrations from {}", source.display()))?;
    m.ignore_missing = ignore_missing;
    Ok(m)
}

async fn applied(conn: &mut TiConnection, migrator: &Migrator) -> anyhow::Result<Vec<AppliedMigration>> {
    migrator.applied(conn).await.context("failed to read the migrations table")
}

fn repair_hint(version: i64) -> String {
    format!(
        "migration {version} is dirty: it failed part-way and TiDB DDL cannot be rolled back.\n\
         Inspect the database and fix it by hand, then either:\n  \
         tisqlx migrate repair {version} --applied   # its changes are now fully in place\n  \
         tisqlx migrate repair {version} --remove    # its changes were undone; `migrate run` will retry it"
    )
}

/// Turn a MigrateError into an anyhow error with an actionable hint.
fn explain(e: MigrateError) -> anyhow::Error {
    let hint = match &e {
        MigrateError::Dirty(v) | MigrateError::ExecuteMigration(v, _) => Some(repair_hint(*v)),
        MigrateError::VersionMismatch(v) => Some(format!(
            "the file for migration {v} changed after it was applied; restore it, or accept the new \
             contents with `tisqlx migrate repair {v} --applied`"
        )),
        MigrateError::VersionMissing(_) => Some("pass --ignore-missing to ignore it".to_owned()),
        MigrateError::NotDirty(v) => Some(format!(
            "to re-run it, revert it with `tisqlx migrate revert`, or delete its row from the migrations table by hand ({v})"
        )),
        MigrateError::VersionTooOld(..) => {
            Some("pass --ignore-missing to apply it anyway, or give it a newer version".to_owned())
        }
        _ => None,
    };
    match hint {
        Some(h) => anyhow!("{e}\n\n{h}"),
        None => anyhow::Error::new(e),
    }
}

fn fmt_duration(nanos: i64) -> String {
    if nanos < 0 {
        "-".to_owned()
    } else if nanos < 1_000_000 {
        format!("{}µs", nanos / 1_000)
    } else if nanos < 10_000_000_000 {
        format!("{}ms", nanos / 1_000_000)
    } else {
        format!("{:.1}s", nanos as f64 / 1e9)
    }
}

// ---------------------------------------------------------------------------
// migrate run / revert
// ---------------------------------------------------------------------------

pub async fn run(
    source: &Path,
    connect: &ConnectOpts,
    dry_run: bool,
    ignore_missing: bool,
    target: Option<i64>,
) -> anyhow::Result<()> {
    let migrator = load(source, ignore_missing)?;
    if let Some(t) = target
        && migrator.find_up(t).is_none()
    {
        bail!("target version {t} does not exist in {}", source.display());
    }
    let mut conn = crate::database::connect(connect).await?;
    let result = run_inner(&migrator, &mut conn, dry_run, target).await;
    let _ = conn.close().await;
    result
}

async fn run_inner(
    migrator: &Migrator,
    conn: &mut TiConnection,
    dry_run: bool,
    target: Option<i64>,
) -> anyhow::Result<()> {
    let before = applied(conn, migrator).await?;
    if let Some(t) = target
        && let Some(latest) = before.last().map(|a| a.version).filter(|&l| l > t)
    {
        bail!("target version {t} is older than the latest applied migration {latest}; use `migrate revert` instead");
    }

    if dry_run {
        let pending = migrator.pending(&before, target).map_err(explain)?;
        for m in &pending {
            println!(
                "Would apply {}/{} ({})",
                m.version,
                m.description,
                m.migration_type.suffix().trim_start_matches('.')
            );
        }
        if pending.is_empty() {
            println!("No pending migrations.");
        }
        return Ok(());
    }

    let result = match target {
        Some(t) => migrator.run_to(conn, t).await,
        None => migrator.run_on(conn).await,
    };

    // Report what changed, even if a later migration failed.
    match applied(conn, migrator).await {
        Ok(after) => {
            let new: Vec<_> = after.iter().filter(|a| !before.iter().any(|b| b.version == a.version)).collect();
            for a in &new {
                if a.success {
                    println!("Applied {}/{} ({})", a.version, a.description, fmt_duration(a.execution_time));
                } else {
                    println!("FAILED  {}/{} (now DIRTY)", a.version, a.description);
                }
            }
            if new.is_empty() && result.is_ok() {
                println!("No pending migrations.");
            }
        }
        Err(e) if result.is_ok() => return Err(e),
        Err(_) => {}
    }
    result.map_err(explain)
}

pub async fn revert(
    source: &Path,
    connect: &ConnectOpts,
    dry_run: bool,
    ignore_missing: bool,
    target: Option<i64>,
) -> anyhow::Result<()> {
    let migrator = load(source, ignore_missing)?;
    let mut conn = crate::database::connect(connect).await?;
    let result = revert_inner(&migrator, &mut conn, dry_run, target).await;
    let _ = conn.close().await;
    result
}

async fn revert_inner(
    migrator: &Migrator,
    conn: &mut TiConnection,
    dry_run: bool,
    target: Option<i64>,
) -> anyhow::Result<()> {
    let before = applied(conn, migrator).await?;
    let to_revert = migrator.to_revert(&before, target).map_err(explain)?;
    if to_revert.is_empty() {
        println!("No migrations to revert.");
        return Ok(());
    }
    if dry_run {
        for m in &to_revert {
            println!("Would revert {}/{}", m.version, m.description);
        }
        return Ok(());
    }

    let result = migrator.undo(conn, target).await;
    let after = match applied(conn, migrator).await {
        Ok(rows) => Some(rows),
        Err(e) if result.is_ok() => return Err(e),
        Err(_) => None,
    };
    if let Some(after) = after {
        for m in &to_revert {
            match after.iter().find(|a| a.version == m.version) {
                None => println!("Reverted {}/{}", m.version, m.description),
                Some(a) if !a.success => println!("FAILED   {}/{} (now DIRTY)", m.version, m.description),
                Some(_) => {}
            }
        }
    }
    result.map_err(explain)
}

// ---------------------------------------------------------------------------
// migrate info / repair
// ---------------------------------------------------------------------------

pub async fn info(source: &Path, connect: &ConnectOpts) -> anyhow::Result<()> {
    let migrator = load(source, false)?;
    let mut conn = crate::database::connect(connect).await?;
    let rows = applied(&mut conn, &migrator).await;
    let _ = conn.close().await;
    let applied: BTreeMap<i64, AppliedMigration> = rows?.into_iter().map(|a| (a.version, a)).collect();

    let local: BTreeMap<i64, &Migration> =
        migrator.migrations.iter().filter(|m| m.migration_type.is_up()).map(|m| (m.version, m)).collect();
    let versions: BTreeSet<i64> = local.keys().chain(applied.keys()).copied().collect();

    if versions.is_empty() {
        println!("No migrations found in {} and none applied.", source.display());
        return Ok(());
    }

    let mut rows = Vec::new();
    let mut dirty = Vec::new();
    for v in versions {
        let (l, a) = (local.get(&v), applied.get(&v));
        let status = match (l, a) {
            (_, Some(a)) if !a.success => {
                dirty.push(v);
                "DIRTY"
            }
            (Some(l), Some(a)) if *l.checksum != *a.checksum => "checksum mismatch",
            (Some(_), Some(_)) => "applied",
            (None, Some(_)) => "applied (missing locally)",
            (Some(_), None) => "pending",
            (None, None) => unreachable!(),
        };
        let kind = match l {
            Some(m) if m.migration_type.is_reversible() => "reversible",
            Some(_) => "simple",
            None => "-",
        };
        let desc =
            l.map(|m| m.description.to_string()).or_else(|| a.map(|a| a.description.clone())).unwrap_or_default();
        let time = a.filter(|a| a.success).map_or_else(|| "-".to_owned(), |a| fmt_duration(a.execution_time));
        rows.push([v.to_string(), status.to_owned(), kind.to_owned(), time, desc]);
    }

    let headers = ["VERSION", "STATUS", "TYPE", "TIME", "DESCRIPTION"];
    let mut widths = headers.map(str::len);
    for r in &rows {
        for (w, c) in widths.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    let print_row = |cells: [&str; 5]| {
        let line: Vec<String> = cells.iter().zip(widths).map(|(c, w)| format!("{c:<w$}")).collect();
        println!("{}", line.join("  ").trim_end());
    };
    print_row(headers);
    for r in &rows {
        print_row([&r[0], &r[1], &r[2], &r[3], &r[4]]);
    }
    for v in dirty {
        println!("\n{}", repair_hint(v));
    }
    Ok(())
}

pub async fn repair(source: &Path, connect: &ConnectOpts, version: i64, mark_applied: bool) -> anyhow::Result<()> {
    let migrator = load(source, false)?;
    let mut conn = crate::database::connect(connect).await?;
    let result = async {
        let rows = applied(&mut conn, &migrator).await?;
        if mark_applied && rows.iter().any(|r| r.version == version && r.success) {
            eprintln!("note: migration {version} is not dirty; accepting its current checksum");
        }
        migrator.repair(&mut conn, version, mark_applied).await.map_err(explain)?;
        if mark_applied {
            println!("Marked migration {version} as applied.");
        } else {
            println!("Removed the record of migration {version}; `migrate run` will apply it again.");
        }
        Ok(())
    }
    .await;
    let _ = conn.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 18, 9, 5, 7).unwrap()
    }

    #[test]
    fn style_inference() {
        assert_eq!(infer_style(&[]), VersionStyle::Timestamp);
        assert_eq!(infer_style(&[1]), VersionStyle::Sequential);
        assert_eq!(infer_style(&[0]), VersionStyle::Sequential);
        assert_eq!(infer_style(&[2]), VersionStyle::Timestamp);
        assert_eq!(infer_style(&[1, 2, 3]), VersionStyle::Sequential);
        assert_eq!(infer_style(&[20260101000000, 20260102000000]), VersionStyle::Timestamp);
        // a same-second collision bump must not flip to sequential numbering
        assert_eq!(infer_style(&[20261118090507, 20261118090508]), VersionStyle::Timestamp);
    }

    #[test]
    fn versions() {
        assert_eq!(next_version(&[], VersionStyle::Sequential, now()), 1);
        assert_eq!(next_version(&[1, 2, 5], VersionStyle::Sequential, now()), 6);
        assert_eq!(next_version(&[], VersionStyle::Timestamp, now()), 20261118090507);
        assert_eq!(next_version(&[20200101000000], VersionStyle::Timestamp, now()), 20261118090507);
        // Same-second collision bumps past the latest version.
        assert_eq!(next_version(&[20261118090507], VersionStyle::Timestamp, now()), 20261118090508);
    }

    #[test]
    fn file_names() {
        assert_eq!(
            file_name(20261118090507, VersionStyle::Timestamp, "create users table", MigrationType::Simple),
            "20261118090507_create_users_table.sql"
        );
        assert_eq!(
            file_name(3, VersionStyle::Sequential, "  add  index ", MigrationType::ReversibleUp),
            "0003_add_index.up.sql"
        );
        assert_eq!(
            file_name(3, VersionStyle::Sequential, "add_index", MigrationType::ReversibleDown),
            "0003_add_index.down.sql"
        );
    }
}
