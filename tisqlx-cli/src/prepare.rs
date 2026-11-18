//! `tisqlx prepare`: generate (or verify) offline query data for the `query!` macros.
//!
//! Runs `cargo check` with `TISQLX_OFFLINE_DIR` pointing at a staging directory, so every
//! macro invocation describes its query against the live database and records it. The
//! crates using the macros are cleaned first, because cargo would otherwise not recompile
//! them on a warm build. The result replaces `<workspace>/.tisqlx/`, or with `--check` is
//! compared with it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::opt::PrepareOpts;

const MACRO_CRATES: &[&str] = &["tisqlx", "tisqlx-macros"];

struct Workspace {
    root: PathBuf,
    target_dir: PathBuf,
    /// Workspace members that use the macros.
    users: Vec<String>,
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

/// The value of `--manifest-path` among the user's `cargo check` arguments.
fn manifest_path(args: &[String]) -> Option<&str> {
    args.iter().enumerate().find_map(|(i, a)| match a.strip_prefix("--manifest-path") {
        Some("") => args.get(i + 1).map(String::as_str),
        Some(v) => v.strip_prefix('='),
        None => None,
    })
}

/// Whether the user's arguments restrict the check to some workspace members.
fn selects_packages(args: &[String]) -> bool {
    args.iter().any(|a| {
        a.starts_with("--package") || a.starts_with("--exclude") || (a.starts_with("-p") && !a.starts_with("--"))
    })
}

/// `cargo check` flags added unless the user's arguments already have them (cargo rejects a repeated flag).
fn default_check_flags(args: &[String]) -> Vec<&'static str> {
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let mut flags = Vec::new();
    if !has("--all-targets") {
        flags.push("--all-targets");
    }
    if !selects_packages(args) && !has("--workspace") && !has("--all") {
        flags.push("--workspace");
    }
    flags
}

/// Offline data files that differ between the committed directory and a fresh run.
#[derive(Debug, Default, PartialEq)]
struct Diff {
    missing: Vec<String>,
    changed: Vec<String>,
    stale: Vec<String>,
}

/// `partial`: only some packages were checked, so a file they did not produce may belong to
/// another package and is not stale.
fn diff(existing: &BTreeMap<String, String>, fresh: &BTreeMap<String, String>, partial: bool) -> Diff {
    let mut d = Diff::default();
    for (name, content) in fresh {
        match existing.get(name) {
            None => d.missing.push(name.clone()),
            Some(e) if e != content => d.changed.push(name.clone()),
            Some(_) => {}
        }
    }
    if !partial {
        d.stale = existing.keys().filter(|k| !fresh.contains_key(*k)).cloned().collect();
    }
    d
}

fn metadata(args: &[String]) -> anyhow::Result<Workspace> {
    let mut cmd = Command::new(cargo());
    cmd.args(["metadata", "--format-version", "1", "--no-deps"]);
    if let Some(path) = manifest_path(args) {
        cmd.args(["--manifest-path", path]);
    }
    let out = cmd.output().context("running `cargo metadata`")?;
    if !out.status.success() {
        bail!("`cargo metadata` failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).context("parsing `cargo metadata` output")?;
    let root = PathBuf::from(meta["workspace_root"].as_str().context("missing workspace_root")?);
    let target_dir = PathBuf::from(meta["target_directory"].as_str().context("missing target_directory")?);
    let mut users = Vec::new();
    for pkg in meta["packages"].as_array().into_iter().flatten() {
        let name = pkg["name"].as_str().unwrap_or_default();
        let uses_macros = name == "tisqlx"
            || pkg["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|d| MACRO_CRATES.contains(&d["name"].as_str().unwrap_or_default()));
        if uses_macros {
            users.push(name.to_owned());
        }
    }
    Ok(Workspace { root, target_dir, users })
}

fn read_data(dir: &Path) -> anyhow::Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(map) };
    for entry in entries {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        if tisqlx_core::offline::is_file_name(&name) {
            map.insert(name, std::fs::read_to_string(&path)?);
        }
    }
    Ok(map)
}

pub async fn run(opts: PrepareOpts) -> anyhow::Result<()> {
    let url = opts.connect.url()?.to_owned();
    // Fail fast with a clear message if the database is unreachable.
    crate::database::connect(&opts.connect).await?.close().await.ok();

    let ws = metadata(&opts.args)?;
    if ws.users.is_empty() {
        bail!("no workspace member depends on `tisqlx`; nothing to prepare");
    }
    let staging = ws.target_dir.join("tisqlx-prepare");
    // A dedicated target dir: cleaning there never invalidates the user's main
    // build (or deletes a running `tisqlx` binary built in this workspace).
    let prepare_target = ws.target_dir.join("tisqlx-prepare-target");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;

    // Cargo runs in the user's directory, where a relative `--manifest-path` is valid.
    for pkg in &ws.users {
        let mut clean = Command::new(cargo());
        clean.args(["clean", "-q", "-p", pkg, "--target-dir"]).arg(&prepare_target);
        if let Some(path) = manifest_path(&opts.args) {
            clean.args(["--manifest-path", path]);
        }
        let status = clean.status().context("running `cargo clean`")?;
        if !status.success() {
            bail!("`cargo clean -p {pkg}` failed");
        }
    }
    let status = Command::new(cargo())
        .args(["check", "--target-dir"])
        .arg(&prepare_target)
        .args(default_check_flags(&opts.args))
        .args(&opts.args)
        .env("DATABASE_URL", &url)
        .env("TISQLX_OFFLINE_DIR", &staging)
        // Override a `TISQLX_OFFLINE=true` from a .env file too.
        .env("TISQLX_OFFLINE", "false")
        .status()
        .context("running `cargo check`")?;
    if !status.success() {
        bail!("`cargo check` failed; fix the errors above and retry");
    }

    let fresh = read_data(&staging)?;
    let _ = std::fs::remove_dir_all(&staging);
    let data_dir = ws.root.join(tisqlx_core::offline::DIR);
    let existing = read_data(&data_dir)?;
    if fresh.is_empty() && !existing.is_empty() {
        // e.g. a compiler cache (sccache) skipped the macros: never treat that as "no queries".
        bail!("no query was described during `cargo check`; is a compiler cache (RUSTC_WRAPPER) skipping the macros?");
    }

    let diff = diff(&existing, &fresh, selects_packages(&opts.args));
    if opts.check {
        if diff == Diff::default() {
            println!("offline query data in {} is up to date ({} queries)", data_dir.display(), fresh.len());
            return Ok(());
        }
        for k in &diff.missing {
            eprintln!("missing: {k}");
        }
        for k in &diff.changed {
            eprintln!("changed: {k}");
        }
        for k in &diff.stale {
            eprintln!("stale:   {k}");
        }
        bail!("offline query data is out of date; run `tisqlx prepare`");
    }

    std::fs::create_dir_all(&data_dir)?;
    for name in &diff.stale {
        std::fs::remove_file(data_dir.join(name))?;
    }
    for (name, content) in &fresh {
        std::fs::write(data_dir.join(name), content)?;
    }
    println!(
        "wrote {} queries to {}; commit it and build with TISQLX_OFFLINE=true to compile without a database",
        fresh.len(),
        data_dir.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cargo_args() {
        assert_eq!(manifest_path(&args(&["--all-features", "--manifest-path", "x/Cargo.toml"])), Some("x/Cargo.toml"));
        assert_eq!(manifest_path(&args(&["--manifest-path=y/Cargo.toml"])), Some("y/Cargo.toml"));
        assert_eq!(manifest_path(&args(&["--all-features"])), None);
        assert!(selects_packages(&args(&["-p", "app"])));
        assert!(selects_packages(&args(&["-papp"])));
        assert!(selects_packages(&args(&["--package=app"])));
        assert!(selects_packages(&args(&["--exclude", "app"])));
        assert!(!selects_packages(&args(&["--all-features", "--profile", "x"])));
    }

    #[test]
    fn default_flags_are_not_repeated() {
        assert_eq!(default_check_flags(&args(&["--all-features"])), ["--all-targets", "--workspace"]);
        assert!(default_check_flags(&args(&["--workspace", "--all-targets"])).is_empty());
        assert_eq!(default_check_flags(&args(&["-p", "app"])), ["--all-targets"]);
    }

    #[test]
    fn a_partial_run_leaves_other_packages_files() {
        let map = |v: &[(&str, &str)]| -> BTreeMap<String, String> {
            v.iter().map(|(k, c)| (k.to_string(), c.to_string())).collect()
        };
        let existing = map(&[("a", "1"), ("b", "1"), ("c", "1")]);
        let fresh = map(&[("a", "1"), ("b", "2"), ("d", "1")]);
        let full = diff(&existing, &fresh, false);
        assert_eq!(
            (&full.missing[..], &full.changed[..], &full.stale[..]),
            (&["d".to_owned()][..], &["b".to_owned()][..], &["c".to_owned()][..])
        );
        assert!(diff(&existing, &fresh, true).stale.is_empty());
    }
}
