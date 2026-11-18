//! Command-line options.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "tisqlx",
    version,
    about = "Command-line utility for tisqlx (TiDB): databases, migrations, offline query data"
)]
pub struct Opt {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create, drop or reset the database named in the database URL.
    #[command(alias = "db")]
    Database {
        #[command(subcommand)]
        command: DatabaseCommand,
    },
    /// Create, run, revert and inspect migrations.
    Migrate {
        #[command(subcommand)]
        command: MigrateCommand,
    },
    /// Generate offline query data for the `query!` macros.
    Prepare(PrepareOpts),
}

#[derive(Debug, Subcommand)]
pub enum DatabaseCommand {
    /// Create the database (CREATE DATABASE IF NOT EXISTS).
    Create {
        #[command(flatten)]
        connect: ConnectOpts,
    },
    /// Drop the database (DROP DATABASE IF EXISTS).
    Drop {
        #[command(flatten)]
        confirm: ConfirmOpt,
        #[command(flatten)]
        connect: ConnectOpts,
    },
    /// Drop the database, re-create it and run all migrations.
    Reset {
        #[command(flatten)]
        confirm: ConfirmOpt,
        #[command(flatten)]
        source: SourceOpt,
        #[command(flatten)]
        connect: ConnectOpts,
    },
    /// Create the database and run all migrations.
    Setup {
        #[command(flatten)]
        source: SourceOpt,
        #[command(flatten)]
        connect: ConnectOpts,
    },
}

#[derive(Debug, Subcommand)]
pub enum MigrateCommand {
    /// Create a new migration file (or an .up.sql/.down.sql pair).
    Add(AddOpts),
    /// Apply pending migrations.
    Run {
        #[command(flatten)]
        source: SourceOpt,
        #[command(flatten)]
        connect: ConnectOpts,
        /// Only list the migrations that would be applied.
        #[arg(long)]
        dry_run: bool,
        /// Ignore applied migrations that are missing from the migrations directory.
        #[arg(long)]
        ignore_missing: bool,
        /// Apply migrations up to and including this version.
        #[arg(long)]
        target_version: Option<i64>,
    },
    /// Revert the latest migration, or every migration newer than --target-version.
    Revert {
        #[command(flatten)]
        source: SourceOpt,
        #[command(flatten)]
        connect: ConnectOpts,
        /// Only list the migrations that would be reverted.
        #[arg(long)]
        dry_run: bool,
        /// Ignore applied migrations that are missing from the migrations directory.
        #[arg(long)]
        ignore_missing: bool,
        /// Revert every migration newer than this version.
        #[arg(long, conflicts_with = "all")]
        target_version: Option<i64>,
        /// Revert every applied migration.
        #[arg(long)]
        all: bool,
    },
    /// List every migration and its status.
    Info {
        #[command(flatten)]
        source: SourceOpt,
        #[command(flatten)]
        connect: ConnectOpts,
    },
    /// Clear the dirty flag of a partially applied migration after fixing the database by hand.
    Repair {
        #[command(flatten)]
        source: SourceOpt,
        #[command(flatten)]
        connect: ConnectOpts,
        /// Version of the dirty migration.
        version: i64,
        /// Mark the migration as successfully applied (stores the current local checksum).
        #[arg(long, conflicts_with = "remove", required_unless_present = "remove")]
        applied: bool,
        /// Delete the migration's record so that `migrate run` applies it again.
        #[arg(long)]
        remove: bool,
    },
    /// Write a `build.rs` that recompiles the crate when migrations change (for `migrate!()`).
    BuildScript {
        #[command(flatten)]
        source: SourceOpt,
        /// Overwrite an existing build.rs.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Debug, Args)]
pub struct AddOpts {
    /// Description of the migration; several words are joined, spaces become underscores in the file name.
    #[arg(required = true, num_args = 1..)]
    pub description: Vec<String>,
    #[command(flatten)]
    pub source: SourceOpt,
    /// Create an .up.sql and a .down.sql file.
    #[arg(short, long, conflicts_with = "simple")]
    pub reversible: bool,
    /// Create a single .sql file even if existing migrations are reversible.
    #[arg(short, long)]
    pub simple: bool,
    /// Use the next integer as version.
    #[arg(long, conflicts_with = "timestamp")]
    pub sequential: bool,
    /// Use a UTC timestamp (%Y%m%d%H%M%S) as version even if existing migrations are sequential.
    #[arg(short, long)]
    pub timestamp: bool,
}

#[derive(Debug, Args)]
pub struct PrepareOpts {
    #[command(flatten)]
    pub connect: ConnectOpts,
    /// Fail if the offline query data is out of date instead of writing it.
    #[arg(long)]
    pub check: bool,
    /// Arguments passed to `cargo check`.
    #[arg(last = true)]
    pub args: Vec<String>,
}

#[derive(Debug, Args)]
pub struct SourceOpt {
    /// Path to the migrations directory.
    #[arg(long, default_value = "migrations")]
    pub source: PathBuf,
}

#[derive(Debug, Args)]
pub struct ConnectOpts {
    /// Database URL, e.g. tidb://user:pass@host:4000/db (also read from DATABASE_URL / .env).
    #[arg(short = 'D', long, env = "DATABASE_URL", hide_env_values = true)]
    pub database_url: Option<String>,
}

impl ConnectOpts {
    pub fn url(&self) -> anyhow::Result<&str> {
        self.database_url.as_deref().ok_or_else(|| {
            anyhow::anyhow!("no database URL: pass --database-url or set DATABASE_URL (a .env file works too)")
        })
    }
}

#[derive(Debug, Args)]
pub struct ConfirmOpt {
    /// Do not ask for confirmation.
    #[arg(short)]
    pub yes: bool,
}
