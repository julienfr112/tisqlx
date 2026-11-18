//! Connection options.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use percent_encoding::percent_decode_str;
use url::Url;

use crate::error::{Error, Result};
use crate::tidb::TxnMode;

pub const DEFAULT_PORT: u16 = 4000;

/// TLS mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SslMode {
    /// Never use TLS.
    Disabled,
    /// Use TLS if the server supports it, without verifying the certificate.
    #[default]
    Preferred,
    /// Require TLS but do not verify the certificate.
    Required,
    /// Require TLS and verify the certificate chain (not the hostname).
    VerifyCa,
    /// Require TLS and verify the chain and the hostname (use this for TiDB Cloud).
    VerifyIdentity,
}

impl FromStr for SslMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Ok(match &*s.to_ascii_lowercase() {
            "disabled" => Self::Disabled,
            "preferred" => Self::Preferred,
            "required" => Self::Required,
            "verify-ca" => Self::VerifyCa,
            "verify-identity" => Self::VerifyIdentity,
            other => return Err(Error::config(format!("unknown ssl-mode {other:?}"))),
        })
    }
}

/// Options for connecting to TiDB.
///
/// Parse from a URL (`tidb://user:pass@host:4000/db?ssl-mode=required`; the
/// `mysql://` scheme is accepted too) or build with the setters.
#[derive(Clone)]
pub struct ConnectOptions {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) socket: Option<PathBuf>,
    pub(crate) username: String,
    pub(crate) password: Option<String>,
    pub(crate) database: Option<String>,
    pub(crate) ssl_mode: SslMode,
    pub(crate) ssl_ca: Option<PathBuf>,
    pub(crate) statement_cache_capacity: usize,
    pub(crate) timezone: Option<String>,
    pub(crate) require_tidb: bool,
    pub(crate) txn_mode: Option<TxnMode>,
    pub(crate) resource_group: Option<String>,
    pub(crate) session_vars: Vec<(String, String)>,
    pub(crate) connect_timeout: Duration,
    /// TLS client config, built on first use. Reset by the TLS setters.
    pub(crate) tls_config: Arc<OnceLock<Arc<tokio_rustls::rustls::ClientConfig>>>,
}

impl std::fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("socket", &self.socket)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("database", &self.database)
            .field("ssl_mode", &self.ssl_mode)
            .field("txn_mode", &self.txn_mode)
            .field("resource_group", &self.resource_group)
            .field("session_vars", &self.session_vars)
            .finish_non_exhaustive()
    }
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectOptions {
    pub fn new() -> Self {
        Self {
            host: "localhost".into(),
            port: DEFAULT_PORT,
            socket: None,
            username: "root".into(),
            password: None,
            database: None,
            ssl_mode: SslMode::default(),
            ssl_ca: None,
            statement_cache_capacity: 100,
            timezone: Some("+00:00".into()),
            require_tidb: true,
            txn_mode: None,
            resource_group: None,
            session_vars: Vec::new(),
            connect_timeout: Duration::from_secs(30),
            tls_config: Arc::default(),
        }
    }

    pub fn host(mut self, host: &str) -> Self {
        self.host = host.to_owned();
        self
    }

    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Connect over a Unix domain socket instead of TCP.
    pub fn socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = Some(path.into());
        self
    }

    pub fn username(mut self, username: &str) -> Self {
        self.username = username.to_owned();
        self
    }

    pub fn password(mut self, password: &str) -> Self {
        self.password = Some(password.to_owned());
        self
    }

    pub fn database(mut self, database: &str) -> Self {
        self.database = Some(database.to_owned());
        self
    }

    pub fn ssl_mode(mut self, mode: SslMode) -> Self {
        self.ssl_mode = mode;
        self.tls_config = Arc::default();
        self
    }

    /// PEM file with CA certificates for `VerifyCa`/`VerifyIdentity` (defaults to webpki roots).
    pub fn ssl_ca(mut self, path: impl Into<PathBuf>) -> Self {
        self.ssl_ca = Some(path.into());
        self.tls_config = Arc::default();
        self
    }

    /// Number of prepared statements cached per connection (0 disables caching).
    pub fn statement_cache_capacity(mut self, capacity: usize) -> Self {
        self.statement_cache_capacity = capacity;
        self
    }

    /// Session `time_zone` (default `+00:00`; `None` keeps the server default).
    ///
    /// `DateTime<Utc>` values are only correct with a UTC session zone.
    pub fn timezone(mut self, tz: Option<&str>) -> Self {
        self.timezone = tz.map(str::to_owned);
        self
    }

    /// Fail to connect when the server is not TiDB (default `true`).
    pub fn require_tidb(mut self, require: bool) -> Self {
        self.require_tidb = require;
        self
    }

    /// Session default transaction mode (`tidb_txn_mode`).
    pub fn txn_mode(mut self, mode: TxnMode) -> Self {
        self.txn_mode = Some(mode);
        self
    }

    /// Bind the session to a TiDB resource group (`SET RESOURCE GROUP`).
    pub fn resource_group(mut self, name: &str) -> Self {
        self.resource_group = Some(name.to_owned());
        self
    }

    /// Set an arbitrary session variable on connect, e.g. `("tidb_isolation_read_engines", "tiflash")`.
    pub fn session_var(mut self, name: &str, value: &str) -> Self {
        self.session_vars.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Connect without selecting a database (e.g. to `CREATE DATABASE`).
    pub fn no_database(mut self) -> Self {
        self.database = None;
        self
    }

    pub fn get_database(&self) -> Option<&str> {
        self.database.as_deref()
    }

    pub fn get_host(&self) -> &str {
        &self.host
    }

    pub fn get_port(&self) -> u16 {
        self.port
    }

    pub fn get_username(&self) -> &str {
        &self.username
    }

    pub async fn connect(&self) -> Result<crate::TiConnection> {
        crate::TiConnection::connect_with(self).await
    }
}

impl FromStr for ConnectOptions {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let url = Url::parse(s).map_err(|e| Error::config(format!("invalid database URL: {e}")))?;
        match url.scheme() {
            "tidb" | "mysql" => {}
            other => {
                return Err(Error::config(format!("unsupported URL scheme {other:?}; expected tidb:// or mysql://")));
            }
        }
        let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();

        let mut opts = ConnectOptions::new();
        if let Some(host) = url.host_str() {
            opts.host = host.trim_start_matches('[').trim_end_matches(']').to_owned();
        }
        if let Some(port) = url.port() {
            opts.port = port;
        }
        if !url.username().is_empty() {
            opts.username = decode(url.username());
        }
        if let Some(pw) = url.password() {
            opts.password = Some(decode(pw));
        }
        let path = url.path().trim_start_matches('/');
        if !path.is_empty() {
            opts.database = Some(decode(path));
        }
        for (k, v) in url.query_pairs() {
            match &*k {
                "ssl-mode" => opts.ssl_mode = v.parse()?,
                "ssl-ca" => opts.ssl_ca = Some(PathBuf::from(&*v)),
                "statement-cache-capacity" => {
                    opts.statement_cache_capacity =
                        v.parse().map_err(|_| Error::config("invalid statement-cache-capacity"))?
                }
                "timezone" => opts.timezone = if v.is_empty() || v == "server" { None } else { Some(v.into_owned()) },
                "require-tidb" => opts.require_tidb = v.parse().map_err(|_| Error::config("invalid require-tidb"))?,
                "txn-mode" => opts.txn_mode = Some(v.parse()?),
                "resource-group" => opts.resource_group = Some(v.into_owned()),
                "socket" => opts.socket = Some(PathBuf::from(&*v)),
                "connect-timeout" => {
                    let secs: u64 = v.parse().map_err(|_| Error::config("invalid connect-timeout (seconds)"))?;
                    opts.connect_timeout = Duration::from_secs(secs);
                }
                other => {
                    // Any other key is a session variable: ?tidb_isolation_read_engines=tiflash
                    opts.session_vars.push((other.to_owned(), v.into_owned()));
                }
            }
        }
        Ok(opts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_url() {
        let o: ConnectOptions =
            "tidb://u%40x:p%2Fw@db.example:4001/app?ssl-mode=verify-identity&txn-mode=optimistic&tidb_mem_quota_query=1024"
                .parse()
                .unwrap();
        assert_eq!(o.username, "u@x");
        assert_eq!(o.password.as_deref(), Some("p/w"));
        assert_eq!(o.host, "db.example");
        assert_eq!(o.port, 4001);
        assert_eq!(o.database.as_deref(), Some("app"));
        assert_eq!(o.ssl_mode, SslMode::VerifyIdentity);
        assert_eq!(o.txn_mode, Some(TxnMode::Optimistic));
        assert_eq!(o.session_vars, vec![("tidb_mem_quota_query".into(), "1024".into())]);
        assert!(!format!("{o:?}").contains("p/w"), "password must not be in Debug output");
    }

    #[test]
    fn default_port() {
        let o: ConnectOptions = "mysql://root@localhost".parse().unwrap();
        assert_eq!(o.port, 4000);
        assert_eq!(o.database, None);
    }
}
