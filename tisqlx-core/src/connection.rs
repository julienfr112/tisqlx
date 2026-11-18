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
use crate::util::{quote_ident, quote_str};

/// The result of executing a statement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueryResult {
    pub(crate) rows_affected: u64,
    pub(crate) last_insert_id: u64,
}

impl QueryResult {
    pub fn rows_affected(&self) -> u64 {
        self.rows_affected
    }

    /// Last generated `AUTO_INCREMENT` or `AUTO_RANDOM` id (0 if none).
    pub fn last_insert_id(&self) -> u64 {
        self.last_insert_id
    }
}

impl QueryResult {
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
    Columns {
        remaining: usize,
    },
    ColumnsEof,
    Rows,
    /// A cancelled operation left the stream mid-packet or mid-handshake.
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
    cache: StatementCache,
    /// Statements to close before the next command.
    pending_close: Vec<u32>,
    pub(crate) transaction_depth: usize,
    /// Shallowest transaction level dropped without commit/rollback; rolled back before the next command.
    pub(crate) rollback_to: Option<usize>,
    /// May hold `GET_LOCK` locks taken by tisqlx (the migrator), e.g. after a cancelled run.
    pub(crate) user_locks: bool,
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

    pub async fn connect_with(options: &ConnectOptions) -> Result<Self> {
        tokio::time::timeout(options.connect_timeout, Self::establish(options))
            .await
            .map_err(|_| Error::Io(std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timed out")))?
    }

    async fn establish(options: &ConnectOptions) -> Result<Self> {
        let socket = match &options.socket {
            #[cfg(unix)]
            Some(path) => Socket::Unix(tokio::net::UnixStream::connect(path).await?),
            #[cfg(not(unix))]
            Some(_) => return Err(Error::config("unix sockets are not supported on this platform")),
            None => {
                let tcp = TcpStream::connect((options.host.as_str(), options.port)).await?;
                tcp.set_nodelay(true)?;
                Socket::Tcp(tcp)
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
        if tidb_version.is_none() && options.require_tidb {
            return Err(Error::NotTiDb(handshake.server_version));
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
        if options.database.is_some() {
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
            crate::tls::upgrade(&mut stream, options).await?;
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
                options.database.as_deref(),
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
            cache: StatementCache { capacity: options.statement_cache_capacity, tick: 0, map: HashMap::new() },
            pending_close: Vec::new(),
            transaction_depth: 0,
            rollback_to: None,
            user_locks: false,
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
            if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
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
        if let Some(group) = &options.resource_group {
            sql.push_str(&format!("; SET RESOURCE GROUP {}", quote_ident(group)));
        }
        self.execute_raw(&sql).await?;
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

    /// Server-side connection id (usable with `KILL`).
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }

    /// `true` if the server reports an open transaction on this session.
    pub fn in_transaction(&self) -> bool {
        self.status & status::IN_TRANS != 0
    }

    /// `true` if the connection was left unusable by a cancelled operation.
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

    /// Clear the prepared-statement cache (closing every statement server-side).
    pub async fn clear_cached_statements(&mut self) -> Result<()> {
        let ids: Vec<u32> = self.cache.map.drain().map(|(_, (s, _))| s.id).collect();
        self.pending_close.extend(ids);
        self.wait_until_ready().await
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
        match self.execute_text(&crate::transaction::rollback_sql(level)).await {
            Err(e @ Error::Database(_)) if level > 1 => {
                self.transaction_depth = 0;
                self.execute_text("ROLLBACK").await?;
                Err(e)
            }
            other => other,
        }
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

    /// Read the next column definition packet, or `None` after the last one.
    async fn next_column_packet(&mut self) -> Result<Option<Vec<u8>>> {
        let Phase::Columns { remaining } = self.phase else { return Ok(None) };
        if remaining == 0 {
            self.phase = if self.deprecate_eof() { Phase::Rows } else { Phase::ColumnsEof };
            return Ok(None);
        }
        let p = self.recv().await?;
        self.phase = Phase::Columns { remaining: remaining - 1 };
        Ok(Some(p))
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
                Phase::Broken => return Err(Error::protocol("connection is broken")),
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
                            self.phase = Phase::Columns { remaining: n };
                            let mut defs = Vec::with_capacity(n);
                            while let Some(p) = self.next_column_packet().await? {
                                defs.push(ColumnDefinition::decode(&p)?);
                            }
                            return Ok(Msg::Columns(defs));
                        }
                    }
                }
                Phase::Columns { .. } => {
                    // Only reachable when draining after a cancelled read.
                    while self.next_column_packet().await?.is_some() {}
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
            p.extend_from_slice(&args.values);
        }
        self.command(&p).await
    }

    /// Execute `sql`, streaming every result set.
    ///
    /// With `args`, the statement goes through the binary (prepared) protocol;
    /// without, through the text protocol.
    pub(crate) fn run<'e>(
        &'e mut self,
        sql: impl Into<Cow<'e, str>>,
        args: Option<Arguments>,
        persistent: bool,
    ) -> BoxStream<'e, Result<Step>> {
        run_on(self, sql.into(), args, persistent)
    }
}

/// Stream the results of `sql` on any owned or borrowed connection.
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
                    return Ok(Some(Step::Row(Row::new(payload, ranges, format, self.columns.clone()))));
                }
            }
        }
    }
}

/// TiDB (like MySQL) allows at most 4096 columns per table or result.
const MAX_COLUMNS: usize = 4096;

fn broken() -> Error {
    Error::protocol("connection is in an unusable state after a cancelled operation")
}
