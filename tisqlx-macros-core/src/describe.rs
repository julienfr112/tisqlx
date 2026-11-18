//! Compile-time query description: online (live TiDB) or offline (`.tisqlx/` files).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tisqlx_core::offline;
use tisqlx_core::{Describe, TiConnection};

/// One offline query file.
#[derive(Debug, Serialize, Deserialize)]
pub struct QueryData {
    pub query: String,
    pub describe: Describe,
}

pub(crate) fn manifest_dir() -> Result<PathBuf, String> {
    std::env::var_os("CARGO_MANIFEST_DIR").map(PathBuf::from).ok_or_else(|| "CARGO_MANIFEST_DIR is not set".to_owned())
}

struct Env {
    manifest_dir: PathBuf,
    vars: HashMap<String, String>,
    /// The `.env` file that was read, if any.
    dotenv: Option<PathBuf>,
}

impl Env {
    fn load() -> Result<Self, String> {
        let manifest_dir = manifest_dir()?;
        let mut vars = HashMap::new();
        let mut dotenv = None;
        // The nearest `.env` (crate dir, else an ancestor such as the workspace root); real env wins.
        for dir in manifest_dir.ancestors() {
            let path = dir.join(".env");
            if let Ok(iter) = dotenvy::from_path_iter(&path) {
                for item in iter.flatten() {
                    vars.entry(item.0).or_insert(item.1);
                }
                dotenv = Some(path);
                break;
            }
        }
        for key in ["DATABASE_URL", "TISQLX_OFFLINE", "TISQLX_OFFLINE_DIR"] {
            if let Ok(v) = std::env::var(key) {
                vars.insert(key.to_owned(), v);
            }
        }
        Ok(Self { manifest_dir, vars, dotenv })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.vars.get(key).map(String::as_str).filter(|v| !v.is_empty())
    }

    fn offline(&self) -> bool {
        self.get("TISQLX_OFFLINE").is_some_and(|v| matches!(&*v.to_ascii_lowercase(), "true" | "1" | "yes"))
    }

    /// Directories searched for offline data: crate dir, then ancestors.
    ///
    /// The search stops at the workspace root (the directory holding `Cargo.lock`), so
    /// a `.tisqlx/` further up (e.g. in `$HOME`) is never picked up.
    fn offline_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        for dir in self.manifest_dir.ancestors() {
            dirs.push(dir.join(offline::DIR));
            if dir.join("Cargo.lock").exists() {
                break;
            }
        }
        dirs
    }
}

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread().enable_all().build().expect("failed to start tokio runtime")
    })
}

enum Cached {
    Connected(String, Box<TiConnection>),
    /// Remembered briefly, so one build does not wait on an unreachable server per
    /// macro call, while a long-lived process (rust-analyzer) recovers.
    Failed(String, String, std::time::Instant),
}

const DESCRIBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const RETRY_FAILED_AFTER: std::time::Duration = std::time::Duration::from_secs(10);

fn describe_online(url: &str, sql: &str) -> Result<Describe, String> {
    static CONN: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    let mut guard = CONN.get_or_init(|| Mutex::new(None)).lock().unwrap_or_else(|e| e.into_inner());
    runtime().block_on(async {
        match &*guard {
            Some(Cached::Connected(u, c)) if u == url && !c.is_broken() => {}
            // Don't retry an unreachable server for every macro invocation.
            Some(Cached::Failed(u, e, at)) if u == url && at.elapsed() < RETRY_FAILED_AFTER => return Err(e.clone()),
            _ => {
                let opts = url
                    .parse::<tisqlx_core::ConnectOptions>()
                    .map_err(|e| format!("invalid DATABASE_URL: {e}"))?
                    .connect_timeout(std::time::Duration::from_secs(5));
                match TiConnection::connect_with(&opts).await {
                    Ok(conn) => *guard = Some(Cached::Connected(url.to_owned(), Box::new(conn))),
                    Err(e) => {
                        let msg = format!(
                            "error connecting to DATABASE_URL: {e} (set TISQLX_OFFLINE=true to use offline data)"
                        );
                        *guard = Some(Cached::Failed(url.to_owned(), msg.clone(), std::time::Instant::now()));
                        return Err(msg);
                    }
                }
            }
        }
        let Some(Cached::Connected(_, conn)) = guard.as_mut() else { unreachable!() };
        // A server that accepts the query but never answers must not hang the build.
        let result = match tokio::time::timeout(DESCRIBE_TIMEOUT, conn.describe(sql)).await {
            Ok(r) => r,
            Err(_) => {
                *guard = None;
                return Err(format!("describing the query timed out after {DESCRIBE_TIMEOUT:?}"));
            }
        };
        if conn.is_broken() {
            *guard = None;
        }
        result.map_err(|e| format!("error describing query: {e}"))
    })
}

fn read_offline(env: &Env, sql: &str) -> Result<(Describe, PathBuf), String> {
    let name = offline::file_name(sql);
    for dir in env.offline_dirs() {
        let path = dir.join(&name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            let data: QueryData = serde_json::from_str(&text)
                .map_err(|e| format!("corrupt offline query file {}: {e}", path.display()))?;
            if data.query != sql {
                return Err(format!(
                    "offline query file {} does not match the query (hash collision?)",
                    path.display()
                ));
            }
            return Ok((data.describe, path));
        }
    }
    Err(format!(
        "query not found in offline data ({name}); run `tisqlx prepare` with DATABASE_URL set, or unset TISQLX_OFFLINE"
    ))
}

fn write_offline(dir: &Path, sql: &str, describe: &Describe) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let data = QueryData { query: sql.to_owned(), describe: describe.clone() };
    let json = serde_json::to_string_pretty(&data).map_err(|e| e.to_string())?;
    let path = dir.join(offline::file_name(sql));
    // Write atomically: cargo may build several crates (and rustc several threads) in parallel.
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{n}", std::process::id()));
    std::fs::write(&tmp, json)
        .and_then(|_| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("writing {}: {e}", path.display()))
}

/// The result of describing a query, plus the files it was derived from.
#[derive(Debug)]
pub struct Described {
    pub describe: Describe,
    /// Files the expansion depends on (offline data, `.env`): the generated code
    /// `include_str!`s them so cargo re-expands the macro when they change.
    pub inputs: Vec<PathBuf>,
}

/// Describe `sql` for a macro invocation.
pub fn describe(sql: &str) -> Result<Described, String> {
    let env = Env::load()?;
    let url = env.get("DATABASE_URL").map(str::to_owned);
    let mut inputs: Vec<PathBuf> = env.dotenv.iter().cloned().collect();
    let describe = match (&url, env.offline()) {
        (Some(url), false) => describe_online(url, sql)?,
        _ => {
            let (describe, path) = read_offline(&env, sql)?;
            inputs.push(path);
            describe
        }
    };
    if let Some(dir) = env.get("TISQLX_OFFLINE_DIR") {
        write_offline(Path::new(dir), sql, &describe)?;
    }
    Ok(Described { describe, inputs })
}
