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
    /// TiDB servers, each with its own port or `None` for [`port`](Self::port).
    pub(crate) hosts: Vec<(String, Option<u16>)>,
    /// Port of the hosts given without one.
    pub(crate) port: u16,
    /// Spread pooled connections over every address each host name resolves to.
    pub(crate) load_balance_dns: bool,
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
            .field("hosts", &self.get_hosts())
            .field("load_balance_dns", &self.load_balance_dns)
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
            hosts: vec![("localhost".into(), None)],
            port: DEFAULT_PORT,
            load_balance_dns: false,
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

    /// Connect to this one host (replaces any list given before).
    pub fn host(mut self, host: &str) -> Self {
        self.hosts = vec![(host.to_owned(), None)];
        self
    }

    /// The port of every host given without its own (default 4000).
    pub fn port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Several TiDB servers (`host:port`) behind one pool or connection.
    ///
    /// A [`Pool`](crate::Pool) opens each new connection on the reachable host with the fewest
    /// of its connections, and skips a host that fails to connect for a while (backoff from 1 s
    /// up to 30 s). A single connection tries the hosts in order. Each attempt gets its share of
    /// `connect_timeout`, so an unresponsive host cannot use it all up. Only network errors
    /// and a host at its connection limit (error 1040) move on to the next host; a wrong
    /// password, say, fails at once. In the URL:
    /// `tidb://user@tidb-0:4000,tidb-1:4000,tidb-2/db`.
    ///
    /// Existing connections are not moved: after an outage, a recovered host fills up again as
    /// connections are replaced (`max_lifetime`, `idle_timeout`) or added.
    pub fn hosts<H: Into<String>>(mut self, hosts: impl IntoIterator<Item = (H, u16)>) -> Self {
        self.hosts = hosts.into_iter().map(|(h, p)| (h.into(), Some(p))).collect();
        self
    }

    /// Spread a pool's connections over all the addresses each host name resolves to, as if
    /// they were listed one by one (e.g. a Kubernetes headless service). Off by default: then
    /// a name's addresses are tried in order, like any client does. URL: `load-balance-dns=true`.
    pub fn load_balance_dns(mut self, yes: bool) -> Self {
        self.load_balance_dns = yes;
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

    /// Session `time_zone` (default `+00:00`; `None` keeps the server default). Instant types
    /// (`DateTime<Utc>`, `OffsetDateTime`, ...) are converted to and from it on the client.
    pub fn timezone(mut self, tz: Option<&str>) -> Self {
        self.timezone = tz.map(str::to_owned);
        self
    }

    /// Fail to connect when the server is not TiDB, or is older than
    /// [`TiDbVersion::MINIMUM`](crate::TiDbVersion::MINIMUM) (8.5) (default `true`).
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

    /// Time allowed for opening a connection, authentication included (default 30 s). It must
    /// not be zero: connecting then fails with a configuration error.
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

    /// The first host (`localhost` if the list is empty).
    pub fn get_host(&self) -> &str {
        self.hosts.first().map_or("localhost", |(h, _)| h)
    }

    /// The first host's port.
    pub fn get_port(&self) -> u16 {
        self.hosts.first().and_then(|(_, p)| *p).unwrap_or(self.port)
    }

    /// Every host, with its port.
    pub fn get_hosts(&self) -> Vec<(&str, u16)> {
        self.hosts.iter().map(|(h, p)| (h.as_str(), p.unwrap_or(self.port))).collect()
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
        // `url` accepts one host only: parse a list here and give it the first.
        let (s, host_list) = split_host_list(s)?;
        let url = Url::parse(&s).map_err(|e| Error::config(format!("invalid database URL: {e}")))?;
        match url.scheme() {
            "tidb" | "mysql" => {}
            other => {
                return Err(Error::config(format!("unsupported URL scheme {other:?}; expected tidb:// or mysql://")));
            }
        }

        let mut opts = ConnectOptions::new();
        match host_list {
            Some(list) => opts.hosts = list,
            None => {
                // One host: its port is *the* port, so `host()`/`port()` keep working on it.
                if let Some(host) = url.host_str() {
                    opts.hosts = vec![(host.trim_start_matches('[').trim_end_matches(']').to_owned(), None)];
                }
                if let Some(port) = url.port() {
                    opts.port = port;
                }
            }
        }
        if !url.username().is_empty() {
            opts.username = decode(url.username())?;
        }
        if let Some(pw) = url.password() {
            opts.password = Some(decode(pw)?);
        }
        let path = url.path().trim_start_matches('/');
        if !path.is_empty() {
            opts.database = Some(decode(path)?);
        }
        // Not `query_pairs()`: it reads `+` as a space, which would break `timezone=+08:00`.
        for pair in url.query().unwrap_or_default().split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let (k, v) = (decode(k)?, decode(v)?);
            match &*k {
                "ssl-mode" => opts.ssl_mode = v.parse()?,
                "ssl-ca" => opts.ssl_ca = Some(PathBuf::from(v)),
                "statement-cache-capacity" => {
                    opts.statement_cache_capacity =
                        v.parse().map_err(|_| Error::config("invalid statement-cache-capacity"))?
                }
                "timezone" => opts.timezone = if v.is_empty() || v == "server" { None } else { Some(v) },
                "require-tidb" => opts.require_tidb = v.parse().map_err(|_| Error::config("invalid require-tidb"))?,
                "txn-mode" => opts.txn_mode = Some(v.parse()?),
                "resource-group" => opts.resource_group = Some(v),
                "socket" => opts.socket = Some(PathBuf::from(v)),
                "load-balance-dns" => {
                    opts.load_balance_dns = v.parse().map_err(|_| Error::config("invalid load-balance-dns"))?
                }
                "connect-timeout" => {
                    let secs = v.parse().ok().filter(|&s: &u64| s > 0);
                    let secs = secs.ok_or_else(|| Error::config("invalid connect-timeout (seconds, at least 1)"))?;
                    opts.connect_timeout = Duration::from_secs(secs);
                }
                // Any other key is a session variable: ?tidb_isolation_read_engines=tiflash
                _ => opts.session_vars.push((k, v)),
            }
        }
        Ok(opts)
    }
}

/// Percent-decode one part of the URL.
fn decode(s: &str) -> Result<String> {
    match percent_decode_str(s).decode_utf8() {
        Ok(decoded) => Ok(decoded.into_owned()),
        Err(_) => Err(Error::config(format!("{s:?} in the database URL is not UTF-8 once percent-decoded"))),
    }
}

/// If the URL lists several hosts (`a:4000,b,[::1]:4001`), return it with only the first one,
/// plus the list.
#[allow(clippy::type_complexity)]
fn split_host_list(url: &str) -> Result<(std::borrow::Cow<'_, str>, Option<Vec<(String, Option<u16>)>>)> {
    let Some(scheme_end) = url.find("://") else { return Ok((url.into(), None)) };
    let start = scheme_end + 3;
    let end = url[start..].find(['/', '?', '#']).map_or(url.len(), |i| start + i);
    let authority = &url[start..end];
    let hosts_start = authority.rfind('@').map_or(0, |i| i + 1);
    let list = &authority[hosts_start..];
    if !list.contains(',') {
        return Ok((url.into(), None));
    }
    let bad = |h: &str| Error::config(format!("invalid host {h:?} in the database URL"));
    let mut hosts = Vec::new();
    for h in list.split(',') {
        let (host, port) = match h.strip_prefix('[') {
            // [v6] or [v6]:port
            Some(rest) => {
                let (v6, after) = rest.split_once(']').ok_or_else(|| bad(h))?;
                let port = match after {
                    "" => None,
                    p => Some(p.strip_prefix(':').ok_or_else(|| bad(h))?),
                };
                (v6, port)
            }
            None => match h.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (h, None),
            },
        };
        if host.is_empty() {
            return Err(bad(h));
        }
        let port = port.map(|p| p.parse::<u16>().map_err(|_| bad(h))).transpose()?;
        hosts.push((decode(host)?, port));
    }
    let first = list.split(',').next().unwrap_or_default();
    let rewritten = format!("{}{}{}", &url[..start + hosts_start], first, &url[end..]);
    Ok((rewritten.into(), Some(hosts)))
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
        assert_eq!(o.get_hosts(), [("db.example", 4001)]);
        assert_eq!(o.database.as_deref(), Some("app"));
        assert_eq!(o.ssl_mode, SslMode::VerifyIdentity);
        assert_eq!(o.txn_mode, Some(TxnMode::Optimistic));
        assert_eq!(o.session_vars, vec![("tidb_mem_quota_query".into(), "1024".into())]);
        assert!(!format!("{o:?}").contains("p/w"), "password must not be in Debug output");
    }

    #[test]
    fn query_values_are_percent_decoded_only() {
        // `+` is a plus sign, not a form-encoded space: `timezone=+08:00` must work as written.
        let o: ConnectOptions = "tidb://h/db?timezone=+08:00&ssl-ca=/etc/a+b.pem&x=a%20b%2Bc&".parse().unwrap();
        assert_eq!(o.timezone.as_deref(), Some("+08:00"));
        assert_eq!(o.ssl_ca.as_deref(), Some(std::path::Path::new("/etc/a+b.pem")));
        assert_eq!(o.session_vars, vec![("x".into(), "a b+c".into())]);
    }

    #[test]
    fn invalid_urls_are_configuration_errors() {
        for bad in [
            // percent-encoded bytes that are not UTF-8, in every decoded part
            "tidb://%FF@h/db",
            "tidb://u:%FF@h/db",
            "tidb://u@h/d%FFb",
            "tidb://u@h/db?timezone=%FF",
            "tidb://u@h/db?%FF=1",
            "tidb://u@a,b%FF/db",
            // a zero timeout would fail every connect
            "tidb://u@h/db?connect-timeout=0",
        ] {
            assert!(matches!(bad.parse::<ConnectOptions>(), Err(Error::Configuration(_))), "{bad}");
        }
    }

    #[test]
    fn default_port() {
        let o: ConnectOptions = "mysql://root@localhost".parse().unwrap();
        assert_eq!(o.get_hosts(), [("localhost", 4000)]);
        assert_eq!(o.database, None);
    }

    #[test]
    fn host_lists() {
        let o: ConnectOptions =
            "tidb://u:p%2C%40w@tidb-0:4001,tidb-1,[::1]:4002,[fe80::1]/app?load-balance-dns=true".parse().unwrap();
        assert_eq!(o.get_hosts(), [("tidb-0", 4001), ("tidb-1", 4000), ("::1", 4002), ("fe80::1", 4000)]);
        assert_eq!((o.username.as_str(), o.password.as_deref()), ("u", Some("p,@w")));
        assert_eq!(o.database.as_deref(), Some("app"));
        assert!(o.load_balance_dns);
        // no user info, no path
        let o: ConnectOptions = "tidb://a,b:5".parse().unwrap();
        assert_eq!(o.get_hosts(), [("a", 4000), ("b", 5)]);
        for bad in ["tidb://a,,b", "tidb://a,b:x", "tidb://a,[::1", "tidb://a,[::1]x"] {
            assert!(bad.parse::<ConnectOptions>().is_err(), "{bad}");
        }
        // `port` is the default for hosts without one, set before or after them
        let o = ConnectOptions::new().host("a").port(4001);
        assert_eq!(o.get_hosts(), [("a", 4001)]);
        let o = ConnectOptions::new().port(4001).host("a");
        assert_eq!(o.get_hosts(), [("a", 4001)]);
        let o = ConnectOptions::new().hosts([("a", 1), ("b", 2)]).port(4001);
        assert_eq!(o.get_hosts(), [("a", 1), ("b", 2)]);
        // an empty list is refused at connect time; the getters must not panic on it
        let o = ConnectOptions::new().hosts(Vec::<(String, u16)>::new());
        assert_eq!((o.get_host(), o.get_port()), ("localhost", 4000));
        // a one-host URL's port survives `host()`, and `port()` still changes it
        let o: ConnectOptions = "tidb://root@127.0.0.1:4100/db".parse().unwrap();
        assert_eq!(o.clone().host("localhost").get_hosts(), [("localhost", 4100)]);
        assert_eq!(o.port(4200).get_hosts(), [("127.0.0.1", 4200)]);
    }
}
