//! Compile-time query description: online (live TiDB) or offline (`.tisqlx/` files).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tisqlx_core::offline;

use crate::params::Site;
use tisqlx_core::{ColumnFlags, ColumnType, Describe, TiConnection, TypeInfo};

/// One offline query file.
#[derive(Debug, Serialize, Deserialize)]
pub struct QueryData {
    pub query: String,
    pub describe: Describe,
    /// Per placeholder, the type of the column it is matched against, when inferred (see
    /// [`crate::params`]). Empty when nothing was inferred: nothing is checked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub param_types: Vec<Option<TypeInfo>>,
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
        let (dotenv, mut vars) = nearest_dotenv(&manifest_dir);
        // The real environment wins over `.env`.
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

/// The nearest `.env` file (in `dir`, else in an ancestor such as the workspace root) and its
/// variables.
fn nearest_dotenv(dir: &Path) -> (Option<PathBuf>, HashMap<String, String>) {
    let mut vars = HashMap::new();
    for dir in dir.ancestors() {
        let path = dir.join(".env");
        // Not a directory: it opens, then every read fails.
        if !path.is_file() {
            continue;
        }
        if let Ok(iter) = dotenvy::from_path_iter(&path) {
            for item in iter {
                match item {
                    Ok((key, value)) => {
                        vars.entry(key).or_insert(value);
                    }
                    // A read error repeats forever; a malformed line is skipped.
                    Err(dotenvy::Error::Io(_)) => break,
                    Err(_) => {}
                }
            }
            return (Some(path), vars);
        }
    }
    (None, vars)
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

/// Describe the query, then infer its parameter types with a probe query (prepared, never run).
async fn describe_and_infer(
    conn: &mut TiConnection,
    sql: &str,
) -> tisqlx_core::Result<(Describe, Vec<Option<TypeInfo>>)> {
    let describe = conn.describe(sql).await?;
    let analysis = crate::params::analyze(sql);
    if analysis.placeholders != describe.params.len() {
        // The lexer and the server disagree: check nothing rather than the wrong parameters.
        return Ok((describe, Vec::new()));
    }
    let mut columns: Vec<Option<TypeInfo>> = vec![None; analysis.partners.len()];
    if let Some(probe) = analysis.probe(None) {
        match conn.describe(&probe).await {
            Ok(d) if d.columns.len() == columns.len() => {
                columns = d.columns.iter().map(|c| Some(c.type_info)).collect();
            }
            // One partner may not resolve (an alias, a typo the real query reports): try each.
            _ if !conn.is_broken() => {
                for (i, column) in columns.iter_mut().enumerate() {
                    let probe = analysis.probe(Some(i)).expect("partners need a FROM");
                    if let Ok(d) = conn.describe(&probe).await {
                        *column = d.columns.first().map(|c| c.type_info);
                    }
                }
            }
            _ => {}
        }
    }
    let count = TypeInfo { flags: ColumnFlags(ColumnFlags::UNSIGNED), ..TypeInfo::new(ColumnType::LongLong) };
    let types = analysis
        .sites
        .iter()
        .map(|site| match site {
            Some(Site::Column(i)) => columns[*i],
            Some(Site::Like(i)) => columns[*i].filter(|ty| {
                crate::rust_types::bind_column_kind(ty).is_some_and(|kind| kind == "Text" || kind == "Bytes")
            }),
            Some(Site::Count) => Some(count),
            None => None,
        })
        .collect();
    Ok((describe, types))
}

fn describe_online(url: &str, sql: &str) -> Result<(Describe, Vec<Option<TypeInfo>>), String> {
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
        let result = match tokio::time::timeout(DESCRIBE_TIMEOUT, describe_and_infer(conn, sql)).await {
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

fn read_offline(env: &Env, sql: &str) -> Result<(QueryData, PathBuf), String> {
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
            return Ok((data, path));
        }
    }
    Err(if env.offline() {
        format!(
            "query not found in offline data ({name}); run `tisqlx prepare` with DATABASE_URL set, or unset TISQLX_OFFLINE"
        )
    } else {
        format!(
            "DATABASE_URL is not set, and the query is not in the offline data ({name}); set it, or run `tisqlx prepare`"
        )
    })
}

fn write_offline(dir: &Path, data: &QueryData) -> Result<(), String> {
    let sql = &data.query;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let json = serde_json::to_string_pretty(data).map_err(|e| e.to_string())?;
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
    /// See [`QueryData::param_types`].
    pub param_types: Vec<Option<TypeInfo>>,
    /// Files the expansion depends on (offline data, `.env`): the generated code
    /// `include_str!`s them so cargo re-expands the macro when they change.
    pub inputs: Vec<PathBuf>,
}

/// Describe `sql` for a macro invocation.
pub fn describe(sql: &str) -> Result<Described, String> {
    let env = Env::load()?;
    let url = env.get("DATABASE_URL").map(str::to_owned);
    let mut inputs: Vec<PathBuf> = env.dotenv.iter().cloned().collect();
    let data = match (&url, env.offline()) {
        (Some(url), false) => {
            let (describe, param_types) = describe_online(url, sql)?;
            QueryData { query: sql.to_owned(), describe, param_types }
        }
        _ => {
            let (data, path) = read_offline(&env, sql)?;
            inputs.push(path);
            data
        }
    };
    if let Some(dir) = env.get("TISQLX_OFFLINE_DIR") {
        write_offline(Path::new(dir), &data)?;
    }
    Ok(Described { describe: data.describe, param_types: data.param_types, inputs })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `.env` is also a common name for a Python virtualenv directory.
    #[test]
    fn a_directory_named_dotenv_is_skipped() {
        let root = std::env::temp_dir().join(format!("tisqlx-dotenv-{}", std::process::id()));
        let krate = root.join("crate");
        std::fs::create_dir_all(krate.join(".env")).unwrap();
        std::fs::write(root.join(".env"), "DATABASE_URL=tidb://x\nnot a valid line\nTISQLX_OFFLINE=true\n").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = krate.clone();
        std::thread::spawn(move || tx.send(nearest_dotenv(&dir)));
        let found = rx.recv_timeout(std::time::Duration::from_secs(10));
        std::fs::remove_dir_all(&root).unwrap();
        let (path, vars) = found.expect("reading `.env` hangs");
        assert_eq!(path, Some(root.join(".env")));
        assert_eq!(vars.get("DATABASE_URL").map(String::as_str), Some("tidb://x"));
        // a malformed line is skipped, not the rest of the file
        assert_eq!(vars.get("TISQLX_OFFLINE").map(String::as_str), Some("true"));
    }
}
