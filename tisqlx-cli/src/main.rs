//! `tisqlx`: command-line utility for tisqlx.

// A CLI talks to its user on stdout.
#![allow(clippy::print_stdout, clippy::print_stderr)]

use clap::Parser;

mod database;
mod migrate;
mod opt;
mod prepare;

use opt::{Command, DatabaseCommand, MigrateCommand, Opt};

#[tokio::main]
async fn main() {
    // Before parsing, so clap's `env = "DATABASE_URL"` sees it. The nearest `.env` file of the
    // current directory and its ancestors; variables already set win.
    if let Err(e) = dotenvy::dotenv()
        && !e.not_found()
    {
        eprintln!("warning: .env: {e}; the lines after it are ignored");
    }
    let opt = Opt::parse();
    if let Err(e) = run(opt).await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

async fn run(opt: Opt) -> anyhow::Result<()> {
    match opt.command {
        Command::Database { command } => match command {
            DatabaseCommand::Create { connect } => database::create(&connect).await,
            DatabaseCommand::Drop { confirm, connect } => database::drop(&connect, !confirm.yes).await,
            DatabaseCommand::Reset { confirm, source, connect } => {
                database::reset(&source.source, &connect, !confirm.yes).await
            }
            DatabaseCommand::Setup { source, connect } => database::setup(&source.source, &connect).await,
        },
        Command::Migrate { command } => match command {
            MigrateCommand::Add(opts) => migrate::add(&opts),
            MigrateCommand::Run { source, connect, dry_run, ignore_missing, target_version } => {
                migrate::run(&source.source, &connect, dry_run, ignore_missing, target_version).await
            }
            MigrateCommand::Revert { source, connect, dry_run, ignore_missing, target_version, all } => {
                let target = if all { Some(i64::MIN) } else { target_version };
                migrate::revert(&source.source, &connect, dry_run, ignore_missing, target).await
            }
            MigrateCommand::Info { source, connect } => migrate::info(&source.source, &connect).await,
            MigrateCommand::Repair { source, connect, version, applied, remove: _ } => {
                migrate::repair(&source.source, &connect, version, applied).await
            }
            MigrateCommand::BuildScript { source, force } => migrate::build_script(&source.source, force),
        },
        Command::Prepare(opts) => prepare::run(opts).await,
    }
}
