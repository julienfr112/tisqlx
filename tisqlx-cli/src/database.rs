//! `tisqlx database ...`

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, bail};
use tisqlx_core::util::quote_ident;
use tisqlx_core::{ConnectOptions, TiConnection};

use crate::opt::ConnectOpts;

/// Parse the URL into options for the target database, and options for the
/// same server without a default database (for CREATE/DROP DATABASE).
pub fn split_url(url: &str) -> anyhow::Result<(ConnectOptions, ConnectOptions, String)> {
    let opts: ConnectOptions = url.parse().context("invalid database URL")?;
    let Some(db) = opts.get_database().map(str::to_owned) else {
        bail!("the database URL does not name a database (expected tidb://user:pass@host:port/<database>)");
    };
    let server = opts.clone().no_database();
    Ok((opts, server, db))
}

pub async fn connect(connect: &ConnectOpts) -> anyhow::Result<TiConnection> {
    connect_server(&connect.url()?.parse().context("invalid database URL")?).await
}

/// `host:port`, or every host of a list.
fn hosts(server: &ConnectOptions) -> String {
    server.get_hosts().iter().map(|(h, p)| format!("{h}:{p}")).collect::<Vec<_>>().join(", ")
}

async fn connect_server(server: &ConnectOptions) -> anyhow::Result<TiConnection> {
    TiConnection::connect_with(server).await.with_context(|| format!("failed to connect to {}", hosts(server)))
}

pub async fn create(connect: &ConnectOpts) -> anyhow::Result<()> {
    let (_, server, db) = split_url(connect.url()?)?;
    let mut conn = connect_server(&server).await?;
    let res = conn.execute_raw(&format!("CREATE DATABASE IF NOT EXISTS {}", quote_ident(&db))).await;
    let _ = conn.close().await;
    res.with_context(|| format!("failed to create database `{db}`"))?;
    println!("Database `{db}` is ready.");
    Ok(())
}

pub async fn drop(connect: &ConnectOpts, confirm: bool) -> anyhow::Result<()> {
    let (_, server, db) = split_url(connect.url()?)?;
    if confirm && !ask(&format!("Drop database `{db}` on {}? This cannot be undone. [y/N] ", hosts(&server)))? {
        bail!("aborted");
    }
    let mut conn = connect_server(&server).await?;
    let res = conn.execute_raw(&format!("DROP DATABASE IF EXISTS {}", quote_ident(&db))).await;
    let _ = conn.close().await;
    res.with_context(|| format!("failed to drop database `{db}`"))?;
    println!("Database `{db}` dropped.");
    Ok(())
}

pub async fn reset(source: &Path, connect: &ConnectOpts, confirm: bool) -> anyhow::Result<()> {
    drop(connect, confirm).await?;
    setup(source, connect).await
}

pub async fn setup(source: &Path, connect: &ConnectOpts) -> anyhow::Result<()> {
    create(connect).await?;
    crate::migrate::run(source, connect, false, false, None).await
}

fn ask(prompt: &str) -> anyhow::Result<bool> {
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    // EOF (non-interactive stdin) reads 0 bytes and counts as "no".
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_url_strips_database_keeps_query() {
        let (opts, server, db) = split_url("tidb://u:p@h:4001/my%20db?ssl-mode=disabled&tidb_x=1").unwrap();
        assert_eq!(db, "my db");
        assert_eq!(opts.get_database(), Some("my db"));
        assert_eq!(server.get_database(), None);
        assert_eq!(server.get_host(), "h");
        assert_eq!(server.get_port(), 4001);
        assert_eq!(server.get_username(), "u");
        assert!(format!("{server:?}").contains("tidb_x"));
    }

    #[test]
    fn split_url_requires_database() {
        assert!(split_url("tidb://root@127.0.0.1:4000").is_err());
        assert!(split_url("tidb://root@127.0.0.1:4000/").is_err());
    }
}
