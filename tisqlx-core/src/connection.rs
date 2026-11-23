//! A single connection to a TiDB server.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt;
use std::ops::DerefMut;
use std::sync::Arc;

use futures_core::stream::BoxStream;
use tokio::net::TcpStream;

use crate::arguments::Arguments;
use crate::error::{Error, Result};
use crate::options::{ConnectOptions, SslMode};
use crate::protocol::auth::{self, AuthPlugin};
use crate::protocol::buf::{BufMutExt, Reader};
use crate::protocol::row::{decode_binary_row, decode_text_row};
use crate::protocol::stream::{PacketStream, Socket};
use crate::protocol::{self, ColumnDefinition, Handshake, OkPacket, PrepareOk, caps, cmd, status};
use crate::row::{Column, ResultColumns, Row, RowFormat};
use crate::tidb::TiDbVersion;
use crate::type_info::TypeInfo;
use crate::tz::SessionZone;
use crate::util::{quote_ident, quote_str};

/// The result of executing a statement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryResult {
    pub(crate) rows_affected: u64,
    pub(crate) last_insert_id: u64,
}

impl QueryResult {
    /// Rows the statement matched. An `UPDATE` counts every row its `WHERE` matched, even one
    /// whose values did not change (tisqlx sets `CLIENT_FOUND_ROWS`).
    pub fn rows_affected(&self) -> u64 {
        self.rows_affected
    }

    /// Last generated `AUTO_INCREMENT` or `AUTO_RANDOM` id (0 if none).
    pub fn last_insert_id(&self) -> u64 {
        self.last_insert_id
    }

    /// Combine the results of consecutive statements.
    pub(crate) fn add(&mut self, r: QueryResult) {
        self.rows_affected += r.rows_affected;
        if r.last_insert_id != 0 {
            self.last_insert_id = r.last_insert_id;
        }
    }
}

/// Either the end-of-result summary or a row.
#[derive(Debug)]
pub enum Step {
    Done(QueryResult),
    Row(Row),
}

/// Metadata returned by preparing a statement, used by the compile-time macros.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Describe {
    pub columns: Vec<Column>,
    pub params: Vec<TypeInfo>,
}

#[derive(Debug)]
pub(crate) struct StatementInfo {
    pub(crate) id: u32,
    pub(crate) params: Vec<ColumnDefinition>,
    pub(crate) columns: Arc<ResultColumns>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Expecting the first packet of a (possibly additional) result.
    Response,
    /// Expecting the EOF that follows the column definitions (servers without DEPRECATE_EOF).
    ColumnsEof,
    Rows,
    /// The stream position is unknown: a read or write was cancelled or failed half-way.
    Broken,
}

enum Msg {
    Done(OkPacket),
    Columns(Vec<ColumnDefinition>),
    Row(Vec<u8>),
}

struct StatementCache {
    capacity: usize,
    tick: u64,
    map: HashMap<String, (Arc<StatementInfo>, u64)>,
}

impl StatementCache {
    fn get(&mut self, sql: &str) -> Option<Arc<StatementInfo>> {
        self.tick += 1;
        let tick = self.tick;
        self.map.get_mut(sql).map(|(s, t)| {
            *t = tick;
            s.clone()
        })
    }

    /// Inserts and returns the evicted statement id, if any.
    fn insert(&mut self, sql: &str, stmt: Arc<StatementInfo>) -> Option<u32> {
        self.tick += 1;
        let mut evicted = None;
        if self.map.len() >= self.capacity
            && let Some(k) = self.map.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| k.clone())
        {
            evicted = self.map.remove(&k).map(|(s, _)| s.id);
        }
        self.map.insert(sql.to_owned(), (stmt, self.tick));
        evicted
    }
}

/// A connection to TiDB.
pub struct TiConnection {
    stream: PacketStream,
    capabilities: u32,
    pub(crate) status: u16,
    phase: Phase,
    server_version: String,
    tidb_version: Option<TiDbVersion>,
    connection_id: u32,
    /// The server's address (`None` over a Unix socket).
    server_addr: Option<std::net::SocketAddr>,
    cache: StatementCache,
    /// Statements to close before the next command.
    pending_close: Vec<u32>,
    pub(crate) transaction_depth: usize,
    /// Shallowest transaction level dropped without commit/rollback; rolled back before the next command.
    pub(crate) rollback_to: Option<usize>,
    /// Whether the open transaction has savepoints. A stale read has none: TiDB accepts
    /// `SAVEPOINT` there but creates nothing, and there is nothing to roll back.
    pub(crate) savepoints: bool,
    /// May hold `GET_LOCK` locks taken by tisqlx (the migrator), e.g. after a cancelled run.
    pub(crate) user_locks: bool,
    /// Session `time_zone`, for instant types.
    zone: SessionZone,
}

impl fmt::Debug for TiConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TiConnection")
            .field("server_version", &self.server_version)
            .field("connection_id", &self.connection_id)
            .field("transaction_depth", &self.transaction_depth)
            .finish()
    }
}

impl TiConnection {
    /// Connect using a URL such as `tidb://root@127.0.0.1:4000/test`.
    pub async fn connect(url: &str) -> Result<Self> {
        Self::connect_with(&url.parse()?).await
    }

    /// Connect with `options`. With several hosts, they are tried in order (see
    /// [`ConnectOptions::hosts`]).
    pub async fn connect_with(options: &ConnectOptions) -> Result<Self> {
        if options.socket.is_some() {
            check_connect_timeout(options)?;
            return tokio::time::timeout(options.connect_timeout, Self::establish(options, None))
                .await
                .map_err(|_| timed_out())?;
        }
        let targets: Vec<Target> = options
            .get_hosts()
            .into_iter()
            .map(|(host, port)| Target { host: host.to_owned(), port, addr: None })
            .collect();
        Ok(Self::connect_any(options, &targets, |_, _| {}).await?.0)
    }

    /// Try `targets` in order until one connects; each attempt gets an even share of
    /// `connect_timeout` (the last one, whatever is left). Only network errors and a server
    /// that is full (1040) move on to the next target: anything else (bad password, not
    /// TiDB...) would fail on every one of them.
    /// `report(i, ok)` is called after each attempt. Returns the connection and its target index.
    pub(crate) async fn connect_any(
        options: &ConnectOptions,
        targets: &[Target],
        mut report: impl FnMut(usize, bool),
    ) -> Result<(Self, usize)> {
        if targets.is_empty() {
            return Err(Error::config("no host to connect to"));
        }
        check_connect_timeout(options)?;
        let start = tokio::time::Instant::now();
        let share = options.connect_timeout / targets.len() as u32;
        let mut last = None;
        for (i, target) in targets.iter().enumerate() {
            let remaining = options.connect_timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            let budget = if i + 1 == targets.len() { remaining } else { share.min(remaining) };
            let error = match tokio::time::timeout(budget, Self::establish(options, Some(target))).await {
                Ok(Ok(conn)) => {
                    report(i, true);
                    return Ok((conn, i));
                }
                Ok(Err(e)) if is_host_specific(&e) => e,
                Ok(Err(e)) => return Err(e),
                Err(_) => timed_out(),
            };
            if targets.len() > 1 {
                log::debug!("cannot connect to {target}: {error}");
            }
            report(i, false);
            last = Some(error);
        }
        Err(last.unwrap_or_else(timed_out))
    }

    /// Open a connection to `target` (`None`: the Unix socket).
    async fn establish(options: &ConnectOptions, target: Option<&Target>) -> Result<Self> {
        let (socket, server_addr, tls_name) = match (target, &options.socket) {
            #[cfg(unix)]
            (None, Some(path)) => (Socket::Unix(tokio::net::UnixStream::connect(path).await?), None, None),
            #[cfg(not(unix))]
            (None, Some(_)) => return Err(Error::config("unix sockets are not supported on this platform")),
            (None, None) => return Err(Error::config("no host to connect to")),
            (Some(t), _) => {
                let tcp = match t.addr {
                    Some(addr) => TcpStream::connect(addr).await?,
                    None => TcpStream::connect((t.host.as_str(), t.port)).await?,
                };
                tcp.set_nodelay(true)?;
                let peer = tcp.peer_addr().ok();
                (Socket::Tcp(tcp), peer, Some(t.host.as_str()))
            }
        };
        let mut stream = PacketStream::new(socket);

        let handshake = Handshake::decode(&stream.recv_packet().await?)?;
        log::debug!(
            "server handshake: version={:?} capabilities={:#x} plugin={:?}",
            handshake.server_version,
            handshake.capabilities,
            handshake.auth_plugin
        );
        let tidb_version = TiDbVersion::parse(&handshake.server_version);
        if options.require_tidb {
            match tidb_version {
                None => return Err(Error::NotTiDb(handshake.server_version)),
                Some(v) if v < TiDbVersion::MINIMUM => return Err(Error::UnsupportedTiDb(v)),
                Some(_) => {}
            }
        }

        let mut capabilities = caps::LONG_PASSWORD
            | caps::FOUND_ROWS
            | caps::LONG_FLAG
            | caps::PROTOCOL_41
            | caps::TRANSACTIONS
            | caps::SECURE_CONNECTION
            | caps::MULTI_STATEMENTS
            | caps::MULTI_RESULTS
            | caps::PS_MULTI_RESULTS
            | caps::PLUGIN_AUTH
            | caps::PLUGIN_AUTH_LENENC_DATA
            | caps::CONNECT_ATTRS
            | caps::DEPRECATE_EOF;
        // TiDB (8.5) builds a `USE` statement from the handshake's database without escaping
        // backticks, and fails with a syntax error: such a name is selected after connecting.
        let handshake_db = options.database.as_deref().filter(|db| !db.contains('`'));
        if handshake_db.is_some() {
            capabilities |= caps::CONNECT_WITH_DB;
        }
        capabilities &= handshake.capabilities;

        let server_tls = handshake.capabilities & caps::SSL != 0;
        let want_tls = match options.ssl_mode {
            SslMode::Disabled => false,
            SslMode::Preferred => server_tls,
            _ if !server_tls => return Err(Error::Tls("server does not support TLS".into())),
            _ => true,
        };
        if want_tls {
            capabilities |= caps::SSL;
            stream.send_packet(&protocol::encode_ssl_request(capabilities)).await?;
            crate::tls::upgrade(&mut stream, options, tls_name.unwrap_or(options.get_host())).await?;
        }

        let password = options.password.as_deref().unwrap_or("");
        let secure = stream.is_tls || options.socket.is_some();
        // The handshake names the server's *default* plugin, not the account's: for an
        // unknown one (or clear text, which is never sent unprompted) answer with
        // native password and let the server switch plugins if needed.
        let mut plugin = match handshake.auth_plugin.as_deref().map(AuthPlugin::from_name) {
            Some(Ok(AuthPlugin::ClearPassword)) | Some(Err(_)) | None => AuthPlugin::NativePassword,
            Some(Ok(p)) => p,
        };
        let mut nonce = handshake.nonce.clone();
        let response = plugin.scramble(password, &nonce);
        stream
            .send_packet(&protocol::encode_handshake_response(
                capabilities,
                &options.username,
                &response,
                handshake_db,
                plugin.name(),
            ))
            .await?;

        loop {
            let packet = stream.recv_packet().await?;
            match packet.first() {
                Some(0x00) => break,
                Some(0xFF) => return Err(protocol::decode_err(&packet).into()),
                Some(0xFE) => {
                    // AuthSwitchRequest
                    let mut r = Reader::new(&packet[1..]);
                    plugin = AuthPlugin::from_name(&r.nul_str()?)?;
                    let data = r.rest();
                    nonce = protocol::buf::trim_nul(data).to_vec();
                    if plugin == AuthPlugin::ClearPassword && !secure {
                        return Err(Error::config(
                            "server requested clear-text password over an unencrypted connection",
                        ));
                    }
                    stream.send_packet(&plugin.scramble(password, &nonce)).await?;
                }
                Some(0x01) if plugin == AuthPlugin::CachingSha2 => match packet.get(1) {
                    Some(0x03) => {} // fast auth success; OK follows
                    Some(0x04) => {
                        // full authentication
                        // TiDB has no RSA key exchange: full authentication needs a
                        // secure channel (TLS or a Unix socket) to send the password.
                        if !secure {
                            return Err(Error::config(
                                "caching_sha2_password authentication against TiDB requires TLS or a Unix socket; \
                                 enable TLS (ssl-mode=preferred/required) or use mysql_native_password",
                            ));
                        }
                        stream.send_packet(&auth::to_asciz(password)).await?;
                    }
                    other => return Err(Error::protocol(format!("unexpected caching_sha2 response {other:?}"))),
                },
                other => {
                    return Err(Error::protocol(format!("unexpected packet during authentication: {other:?}")));
                }
            }
        }

        let mut conn = TiConnection {
            stream,
            capabilities,
            status: handshake.status,
            phase: Phase::Idle,
            server_version: handshake.server_version,
            tidb_version,
            connection_id: handshake.connection_id,
            server_addr,
            cache: StatementCache { capacity: options.statement_cache_capacity, tick: 0, map: HashMap::new() },
            pending_close: Vec::new(),
            transaction_depth: 0,
            rollback_to: None,
            savepoints: true,
            user_locks: false,
            zone: SessionZone::Utc,
        };
        conn.after_connect(options).await?;
        Ok(conn)
    }

    async fn after_connect(&mut self, options: &ConnectOptions) -> Result<()> {
        let mut sets = vec!["NAMES utf8mb4 COLLATE utf8mb4_bin".to_owned()];
        if let Some(tz) = &options.timezone {
            sets.push(format!("time_zone = {}", quote_str(tz)));
        }
        if let Some(mode) = options.txn_mode {
            sets.push(format!("tidb_txn_mode = '{}'", mode.as_str()));
        }
        for (name, value) in &options.session_vars {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
                return Err(Error::config(format!("invalid session variable name {name:?}")));
            }
            let is_plain = !value.is_empty()
                // a plain decimal literal (not `inf`/`NaN`, which `f64` would also accept)
                && ((value.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b)) && value.parse::<f64>().is_ok())
                    || matches!(&*value.to_ascii_uppercase(), "ON" | "OFF" | "TRUE" | "FALSE" | "DEFAULT"));
            let v = if is_plain { value.clone() } else { quote_str(value) };
            sets.push(format!("SESSION {name} = {v}"));
        }
        let mut sql = format!("SET {}", sets.join(", "));
        if let Some(db) = options.database.as_deref().filter(|db| db.contains('`')) {
            sql.push_str(&format!("; USE {}", quote_ident(db)));
        }
        if let Some(group) = &options.resource_group {
            sql.push_str(&format!("; SET RESOURCE GROUP {}", quote_ident(group)));
        }
        self.execute_raw(&sql).await?;
        if !options.timezone.as_deref().is_some_and(|tz| SessionZone::resolve(tz, None).is_utc()) {
            self.reload_time_zone().await?;
        }
        Ok(())
    }

    /// Re-read the session `time_zone`, which instant types (`DateTime<Utc>`,
    /// `DateTime<Local>`, ...) are converted to and from.
    ///
    /// It is read when the connection opens (and after a pool's `after_connect` hook). TiDB does
    /// not report later changes, so call this after changing it with a raw `SET time_zone`.
    pub async fn reload_time_zone(&mut self) -> Result<()> {
        let row = crate::query::raw_sql("SELECT @@time_zone, @@system_time_zone").fetch_one(&mut *self).await?;
        let (tz, system): (String, Option<String>) = (row.try_get(0)?, row.try_get(1)?);
        self.zone = SessionZone::resolve(&tz, system.as_deref());
        Ok(())
    }

    /// Server version string, e.g. `8.0.11-TiDB-v8.5.8`.
    pub fn server_version(&self) -> &str {
        &self.server_version
    }

    /// Parsed TiDB version (`None` if the server is not TiDB).
    pub fn tidb_version(&self) -> Option<TiDbVersion> {
        self.tidb_version
    }

    /// The address of the server this connection is to (`None` over a Unix socket): with
    /// several hosts, which one it landed on.
    pub fn server_addr(&self) -> Option<std::net::SocketAddr> {
        self.server_addr
    }

    /// Server-side connection id (usable with `KILL`).
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }

    /// `true` if the server reports an open transaction on this session.
    pub fn in_transaction(&self) -> bool {
        self.status & status::IN_TRANS != 0
    }

    /// `true` if the connection is unusable: an operation was cancelled while waiting for the
    /// server, or failed with an I/O or protocol error.
    pub fn is_broken(&self) -> bool {
        self.phase == Phase::Broken
    }

    pub async fn ping(&mut self) -> Result<()> {
        self.wait_until_ready().await?;
        self.command(&[cmd::PING]).await?;
        self.phase = Phase::Response;
        self.drain().await?.map_or(Ok(()), Err)
    }

    /// Gracefully close the connection.
    pub async fn close(mut self) -> Result<()> {
        if self.phase != Phase::Broken {
            self.stream.reset_seq();
            self.stream.send_packet(&[cmd::QUIT]).await?;
        }
        self.stream.shutdown().await
    }

    /// Run SQL over the text protocol (multiple `;`-separated statements allowed),
    /// discarding any rows.
    pub async fn execute_raw(&mut self, sql: &str) -> Result<QueryResult> {
        crate::query::raw_sql(sql).execute(self).await
    }

    /// Clear the prepared-statement cache, closing every statement on the server.
    pub async fn clear_cached_statements(&mut self) -> Result<()> {
        self.forget_statements();
        self.wait_until_ready().await?;
        // The closes are only buffered so far. Broken while flushing, as for any write.
        self.phase = Phase::Broken;
        self.stream.flush().await?;
        self.phase = Phase::Idle;
        Ok(())
    }

    pub fn cached_statements_size(&self) -> usize {
        self.cache.map.len()
    }

    /// Prepare `sql` and report its parameter and column metadata.
    pub async fn describe(&mut self, sql: &str) -> Result<Describe> {
        self.wait_until_ready().await?;
        let stmt = self.prepare_raw(sql).await?;
        self.pending_close.push(stmt.id);
        Ok(Describe {
            columns: stmt.columns.columns.clone(),
            params: stmt.params.iter().map(|p| p.type_info).collect(),
        })
    }

    // ---------------------------------------------------------------- internals

    /// Send a command whose payload is `cmd` followed by `sql`.
    async fn command_sql(&mut self, cmd: u8, sql: &str) -> Result<()> {
        let mut p = Vec::with_capacity(sql.len() + 1);
        p.push(cmd);
        p.extend_from_slice(sql.as_bytes());
        self.command(&p).await
    }

    /// An ERR packet ends the response.
    fn err_packet(&mut self, packet: &[u8]) -> Error {
        self.phase = Phase::Idle;
        protocol::decode_err(packet).into()
    }

    async fn command(&mut self, payload: &[u8]) -> Result<()> {
        self.stream.reset_seq();
        self.phase = Phase::Broken;
        self.stream.send_packet(payload).await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<Vec<u8>> {
        let prev = self.phase;
        self.phase = Phase::Broken;
        let packet = self.stream.recv_packet().await?;
        self.phase = prev;
        Ok(packet)
    }

    /// Empty the statement cache; the statements are closed before the next command.
    fn forget_statements(&mut self) {
        self.pending_close.extend(self.cache.map.drain().map(|(_, (s, _))| s.id));
    }

    /// Finish whatever a previous (possibly dropped) operation left behind.
    pub(crate) async fn wait_until_ready(&mut self) -> Result<()> {
        if self.phase == Phase::Broken {
            return Err(broken());
        }
        self.drain().await?;
        if !self.pending_close.is_empty() {
            // Broken while writing: a cancelled partial write would desynchronize the stream.
            self.phase = Phase::Broken;
            for id in std::mem::take(&mut self.pending_close) {
                let mut p = vec![cmd::STMT_CLOSE];
                p.put_u32(id);
                self.stream.reset_seq();
                // No response; buffered until the next command's flush.
                self.stream.write_packet(&p).await?;
            }
            self.phase = Phase::Idle;
        }
        if let Some(level) = self.rollback_to.take()
            && level <= self.transaction_depth
        {
            self.rollback_level(level).await?;
        }
        Ok(())
    }

    /// Roll back transaction level `level` and everything nested in it. If that fails
    /// (e.g. the savepoint no longer exists), the whole transaction is rolled back, so
    /// enclosing levels cannot commit work the caller believes is gone.
    pub(crate) async fn rollback_level(&mut self, level: usize) -> Result<()> {
        self.transaction_depth = level - 1;
        if level > 1 && !self.savepoints {
            return Ok(());
        }
        if level > 1 && !self.in_transaction() {
            return Err(self.transaction_ended_by_server());
        }
        match self.execute_text(&crate::transaction::rollback_sql(level)).await {
            Err(e @ Error::Database(_)) if level > 1 => {
                self.transaction_depth = 0;
                self.execute_text("ROLLBACK").await?;
                Err(e)
            }
            other => other,
        }
    }

    /// The error of a nested level that finds no transaction open on the server, so its
    /// savepoint is gone. Every level is forgotten.
    pub(crate) fn transaction_ended_by_server(&mut self) -> Error {
        self.transaction_depth = 0;
        Error::protocol(
            "the server already ended the transaction: a DDL statement commits it implicitly, \
             with everything before it",
        )
    }

    /// Give up on this connection: the session is in a state tisqlx cannot restore.
    pub(crate) fn mark_broken(&mut self) {
        self.phase = Phase::Broken;
    }

    /// Whether the session must be cleaned before reuse: an unread response (which
    /// may still open a transaction, e.g. `...; BEGIN`) or an open/abandoned transaction.
    pub(crate) fn needs_reset(&self) -> bool {
        self.phase != Phase::Idle
            || self.rollback_to.is_some()
            || self.transaction_depth > 0
            || self.in_transaction()
            || self.user_locks
    }

    /// Roll back anything left open so the connection can be reused (used by the pool).
    pub(crate) async fn reset_session(&mut self) -> Result<()> {
        self.wait_until_ready().await?;
        if self.transaction_depth > 0 || self.in_transaction() {
            self.transaction_depth = 0;
            self.execute_text("ROLLBACK").await?;
        }
        self.release_user_locks().await
    }

    /// Release every `GET_LOCK` lock if a cancelled operation may have left one held.
    pub(crate) async fn release_user_locks(&mut self) -> Result<()> {
        if self.user_locks {
            self.execute_text("SELECT RELEASE_ALL_LOCKS()").await?;
            self.user_locks = false;
        }
        Ok(())
    }

    /// Read the rest of the response, ignoring rows. Returns the first database error.
    async fn drain(&mut self) -> Result<Option<Error>> {
        let mut first_err = None;
        while self.phase != Phase::Idle {
            if self.phase == Phase::Broken {
                return Err(broken());
            }
            match self.next_message().await {
                Ok(_) => {}
                Err(e @ Error::Database(_)) => {
                    first_err.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(first_err)
    }

    async fn execute_text(&mut self, sql: &str) -> Result<()> {
        self.command_sql(cmd::QUERY, sql).await?;
        self.phase = Phase::Response;
        self.drain().await?.map_or(Ok(()), Err)
    }

    fn deprecate_eof(&self) -> bool {
        self.capabilities & caps::DEPRECATE_EOF != 0
    }

    fn end_result(&mut self, status: u16) {
        self.status = status;
        self.phase = if status & status::MORE_RESULTS_EXISTS != 0 { Phase::Response } else { Phase::Idle };
    }

    /// Read the next message of the current response. A database error ends the
    /// response cleanly; any other error (I/O, malformed packet) leaves the stream
    /// position unknown, so the connection is marked broken.
    async fn next_message(&mut self) -> Result<Msg> {
        let result = self.read_message().await;
        if let Err(e) = &result
            && !matches!(e, Error::Database(_))
        {
            self.phase = Phase::Broken;
        }
        result
    }

    async fn read_message(&mut self) -> Result<Msg> {
        loop {
            match self.phase {
                Phase::Idle => return Err(Error::protocol("no response pending")),
                Phase::Broken => return Err(broken()),
                Phase::Response => {
                    let packet = self.recv().await?;
                    match packet.first() {
                        Some(0x00) => {
                            let ok = OkPacket::decode(&packet)?;
                            self.end_result(ok.status);
                            return Ok(Msg::Done(ok));
                        }
                        Some(0xFF) => return Err(self.err_packet(&packet)),
                        Some(0xFB) => {
                            self.phase = Phase::Broken;
                            return Err(Error::protocol("LOAD DATA LOCAL INFILE is not supported"));
                        }
                        _ => {
                            let n = Reader::new(&packet).lenenc_int()? as usize;
                            if n > MAX_COLUMNS {
                                return Err(Error::protocol(format!("invalid column count {n}")));
                            }
                            let mut defs = Vec::with_capacity(n);
                            for _ in 0..n {
                                defs.push(ColumnDefinition::decode(&self.recv().await?)?);
                            }
                            self.phase = if self.deprecate_eof() { Phase::Rows } else { Phase::ColumnsEof };
                            return Ok(Msg::Columns(defs));
                        }
                    }
                }
                Phase::ColumnsEof => {
                    let p = self.recv().await?;
                    if !protocol::is_eof(&p) {
                        return Err(Error::protocol("expected EOF after column definitions"));
                    }
                    self.phase = Phase::Rows;
                }
                Phase::Rows => {
                    let packet = self.recv().await?;
                    match packet.first() {
                        Some(0xFF) => return Err(self.err_packet(&packet)),
                        Some(0xFE) if packet.len() < 0xFF_FFFF => {
                            let ok = if self.deprecate_eof() {
                                OkPacket::decode(&packet)?
                            } else {
                                OkPacket { status: protocol::decode_eof_status(&packet)?, ..Default::default() }
                            };
                            self.end_result(ok.status);
                            return Ok(Msg::Done(ok));
                        }
                        _ => return Ok(Msg::Row(packet)),
                    }
                }
            }
        }
    }

    async fn prepare_raw(&mut self, sql: &str) -> Result<StatementInfo> {
        self.command_sql(cmd::STMT_PREPARE, sql).await?;
        let packet = self.stream.recv_packet().await?;
        if packet.first() == Some(&0xFF) {
            return Err(self.err_packet(&packet));
        }
        let ok = PrepareOk::decode(&packet)?;
        let params = self.recv_definitions(ok.params).await?;
        let columns = self.recv_definitions(ok.columns).await?;
        self.phase = Phase::Idle;
        Ok(StatementInfo { id: ok.statement_id, params, columns: Arc::new(ResultColumns::new(columns)) })
    }

    /// `n` column definitions of a PREPARE response, then the EOF (unless DEPRECATE_EOF).
    async fn recv_definitions(&mut self, n: u16) -> Result<Vec<ColumnDefinition>> {
        let mut defs = Vec::with_capacity(n as usize);
        for _ in 0..n {
            defs.push(ColumnDefinition::decode(&self.stream.recv_packet().await?)?);
        }
        if n > 0 && !self.deprecate_eof() {
            self.stream.recv_packet().await?;
        }
        Ok(defs)
    }

    async fn prepare_cached(&mut self, sql: &str, use_cache: bool) -> Result<Arc<StatementInfo>> {
        if use_cache && let Some(s) = self.cache.get(sql) {
            return Ok(s);
        }
        let stmt = Arc::new(self.prepare_raw(sql).await?);
        if use_cache && let Some(evicted) = self.cache.insert(sql, stmt.clone()) {
            self.pending_close.push(evicted);
        }
        Ok(stmt)
    }

    async fn prepare_and_execute(
        &mut self,
        sql: &str,
        args: &Arguments,
        use_cache: bool,
    ) -> Result<Arc<StatementInfo>> {
        let stmt = self.prepare_cached(sql, use_cache).await?;
        if !use_cache {
            self.pending_close.push(stmt.id);
        }
        if stmt.params.len() != args.types.len() {
            return Err(Error::protocol(format!(
                "statement expects {} bind parameter(s) but {} were provided",
                stmt.params.len(),
                args.types.len()
            )));
        }
        self.send_execute(&stmt, args).await?;
        Ok(stmt)
    }

    async fn send_execute(&mut self, stmt: &StatementInfo, args: &Arguments) -> Result<()> {
        let mut p = Vec::with_capacity(16 + args.values.len());
        p.push(cmd::STMT_EXECUTE);
        p.put_u32(stmt.id);
        p.put_u8(0); // CURSOR_TYPE_NO_CURSOR
        p.put_u32(1); // iteration count
        if !args.types.is_empty() {
            p.extend_from_slice(&args.null_bitmap());
            p.put_u8(1); // new params bound
            for ty in &args.types {
                p.put_u8(ty.r#type as u8);
                p.put_u8(if ty.is_unsigned() { 0x80 } else { 0 });
            }
            let values = p.len();
            p.extend_from_slice(&args.values);
            if !self.zone.is_utc() {
                for &at in &args.instants {
                    let slot = &mut p[values + at..];
                    let local = crate::types::temporal::DateTimeParts::decode_binary(slot)
                        .and_then(|utc| self.zone.utc_to_local(utc))
                        .map_err(Error::Encode)?;
                    let mut buf = Vec::with_capacity(12);
                    local.encode_datetime(&mut buf);
                    // Same fractional seconds, so the same length (7 or 11 bytes + length byte).
                    slot[..buf.len()].copy_from_slice(&buf);
                }
            }
        }
        self.command(&p).await
    }
}

/// Stream every result set of `sql` on an owned or borrowed connection. With `args`, the
/// statement goes through the binary (prepared) protocol; without, through the text protocol.
pub(crate) fn run_on<'e, C>(
    conn: C,
    sql: Cow<'e, str>,
    args: Option<Arguments>,
    persistent: bool,
) -> BoxStream<'e, Result<Step>>
where
    C: DerefMut<Target = TiConnection> + Send + 'e,
{
    let use_cache = persistent && conn.cache.capacity > 0;
    let run = Run {
        conn,
        sql,
        args,
        use_cache,
        format: None,
        cached: None,
        columns: Arc::new(ResultColumns::new(Vec::new())),
        retried: false,
        yielded: false,
        finished: false,
    };
    Box::pin(futures_util::stream::try_unfold(run, |mut run| async move {
        Ok(run.next_step().await?.map(|step| (step, run)))
    }))
}

/// State of one [`run_on`] stream.
struct Run<'e, C> {
    conn: C,
    sql: Cow<'e, str>,
    args: Option<Arguments>,
    use_cache: bool,
    /// `None` until the command has been sent.
    format: Option<RowFormat>,
    /// Column metadata of the prepared statement, reused when the result matches it.
    cached: Option<Arc<ResultColumns>>,
    columns: Arc<ResultColumns>,
    retried: bool,
    yielded: bool,
    finished: bool,
}

impl<C: DerefMut<Target = TiConnection>> Run<'_, C> {
    /// Send the command (again, after a stale cached statement was evicted).
    async fn start(&mut self) -> Result<()> {
        let conn = &mut *self.conn;
        // A prepared statement keeps reading the database that was current when it was
        // prepared: after a `USE`, a cached one would silently query the old database.
        if !conn.cache.map.is_empty() && changes_database(&self.sql) {
            conn.forget_statements();
        }
        conn.wait_until_ready().await?;
        if let Some(e) = self.args.as_mut().and_then(|a| a.error.take()) {
            return Err(e);
        }
        let format = match &self.args {
            Some(args) => {
                let stmt = conn.prepare_and_execute(&self.sql, args, self.use_cache).await?;
                self.cached = Some(stmt.columns.clone());
                RowFormat::Binary
            }
            None => {
                conn.command_sql(cmd::QUERY, &self.sql).await?;
                RowFormat::Text
            }
        };
        conn.phase = Phase::Response;
        self.format = Some(format);
        Ok(())
    }

    async fn next_step(&mut self) -> Result<Option<Step>> {
        if self.finished {
            return Ok(None);
        }
        if self.format.is_none() {
            self.start().await?;
        }
        let format = self.format.expect("set by start");
        loop {
            let conn = &mut *self.conn;
            let msg = match conn.next_message().await {
                // TiDB: "schema changed after PREPARE" -- re-prepare once.
                Err(Error::Database(e))
                    if e.code == crate::error::codes::TIDB_SCHEMA_CHANGED_AFTER_PREPARE
                        && self.use_cache
                        && !self.retried
                        && !self.yielded
                        && conn.phase == Phase::Idle =>
                {
                    self.retried = true;
                    if let Some((stale, _)) = conn.cache.map.remove(&*self.sql) {
                        conn.pending_close.push(stale.id);
                    }
                    self.start().await?;
                    continue;
                }
                other => other?,
            };
            match msg {
                Msg::Done(ok) => {
                    self.finished = conn.phase == Phase::Idle;
                    return Ok(Some(Step::Done(QueryResult {
                        rows_affected: ok.affected_rows,
                        last_insert_id: ok.last_insert_id,
                    })));
                }
                Msg::Columns(defs) => {
                    self.columns = match &self.cached {
                        // Reuse only identical metadata: flags (UNSIGNED) and charset (binary) matter for decoding.
                        Some(c) if c.columns == defs => c.clone(),
                        _ => Arc::new(ResultColumns::new(defs)),
                    };
                }
                Msg::Row(payload) => {
                    let ranges = match format {
                        RowFormat::Binary => decode_binary_row(&payload, &self.columns.columns)?,
                        RowFormat::Text => decode_text_row(&payload, self.columns.columns.len())?,
                    };
                    self.yielded = true;
                    return Ok(Some(Step::Row(Row::new(
                        payload,
                        ranges,
                        format,
                        self.columns.clone(),
                        conn.zone.clone(),
                    ))));
                }
            }
        }
    }
}

/// Whether a statement of `sql` starts with `USE`, so changes the current database. An index
/// hint (`FROM t USE INDEX (i)`) doesn't count, or it would empty the cache on every run. A
/// false positive (a `;` inside a string or a comment) only costs preparing the cached
/// statements again.
fn changes_database(sql: &str) -> bool {
    sql.split(';').any(|statement| {
        let mut rest = statement.trim_start();
        // Comments before the statement.
        loop {
            let after = if let Some(comment) = rest.strip_prefix("/*") {
                comment.split_once("*/")
            } else if rest.starts_with("--") || rest.starts_with('#') {
                rest.split_once('\n')
            } else {
                break;
            };
            rest = after.map_or("", |(_, after)| after.trim_start());
        }
        let b = rest.as_bytes();
        b.len() >= 3
            && b[..3].eq_ignore_ascii_case(b"use")
            && !b.get(3).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
    })
}

/// One server to connect to: a host name and port, or one of its resolved addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    /// Also the TLS server name.
    pub host: String,
    pub port: u16,
    /// Connect to this address of `host` only, instead of trying each in turn.
    pub addr: Option<std::net::SocketAddr>,
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.addr {
            Some(addr) => write!(f, "{}:{} ({addr})", self.host, self.port),
            None => write!(f, "{}:{}", self.host, self.port),
        }
    }
}

/// Whether a connect error is about that server only, so the next one is worth trying.
fn is_host_specific(e: &Error) -> bool {
    match e {
        Error::Io(_) => true,
        Error::Database(e) => e.code == crate::error::codes::TOO_MANY_CONNECTIONS,
        _ => false,
    }
}

pub(crate) fn check_connect_timeout(options: &ConnectOptions) -> Result<()> {
    if options.connect_timeout.is_zero() {
        return Err(Error::config("connect_timeout must not be zero"));
    }
    Ok(())
}

pub(crate) fn timed_out() -> Error {
    Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out"))
}

/// TiDB (like MySQL) allows at most 4096 columns per table or result.
const MAX_COLUMNS: usize = 4096;

fn broken() -> Error {
    Error::protocol("connection is in an unusable state after a cancelled or failed operation")
}

#[cfg(test)]
mod tests {
    use super::changes_database;

    #[test]
    fn use_statements_are_spotted() {
        for sql in [
            "USE db",
            " use`db`",
            "SELECT 1;\nUse db",
            "/* x */ /* y */use db",
            "SELECT 1; USE",
            "-- to the other one\nUSE db",
            "# a\n/* b */\n-- c\n use db",
        ] {
            assert!(changes_database(sql), "{sql}");
        }
        for sql in [
            "",
            "us",
            "user",
            "use_",
            "SELECT 1 FROM t USE INDEX (i) WHERE a = ?",
            "SELECT used, `use` FROM house",
            "/* use db */ SELECT 1",
            "-- use db",
            "-- x\nSELECT 1 -- y\nuse",
        ] {
            assert!(!changes_database(sql), "{sql}");
        }
    }
}
