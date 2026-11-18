//! A scripted, single-connection MySQL-protocol server that speaks just enough
//! of TiDB's dialect to drive the client through exact packet sequences:
//! cancellation, multi-results, re-prepares, malformed packets.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const CLIENT_PROTOCOL_41: u32 = 1 << 9;
pub const SECURE_CONNECTION: u32 = 1 << 15;
pub const MULTI_STATEMENTS: u32 = 1 << 16;
pub const MULTI_RESULTS: u32 = 1 << 17;
pub const PS_MULTI_RESULTS: u32 = 1 << 18;
pub const PLUGIN_AUTH: u32 = 1 << 19;
pub const PLUGIN_AUTH_LENENC: u32 = 1 << 21;
pub const DEPRECATE_EOF: u32 = 1 << 24;
pub const CONNECT_WITH_DB: u32 = 1 << 3;

pub const DEFAULT_CAPS: u32 = CLIENT_PROTOCOL_41
    | SECURE_CONNECTION
    | MULTI_STATEMENTS
    | MULTI_RESULTS
    | PS_MULTI_RESULTS
    | PLUGIN_AUTH
    | PLUGIN_AUTH_LENENC
    | DEPRECATE_EOF
    | CONNECT_WITH_DB;

pub const AUTOCOMMIT: u16 = 0x0002;
pub const IN_TRANS: u16 = 0x0001;
pub const MORE_RESULTS: u16 = 0x0008;

pub const QUERY: u8 = 0x03;
pub const PING: u8 = 0x0E;
pub const PREPARE: u8 = 0x16;
pub const EXECUTE: u8 = 0x17;
pub const CLOSE: u8 = 0x19;

pub const LONGLONG: u8 = 0x08;
pub const VAR_STRING: u8 = 0xFD;
pub const UNSIGNED: u16 = 32;

/// Every command packet the server received, in order.
pub type Log = Arc<Mutex<Vec<Vec<u8>>>>;

pub struct Mock {
    pub url: String,
    pub log: Log,
    task: tokio::task::JoinHandle<()>,
}

impl Mock {
    /// Run `script` against the first connection. The URL disables TLS (the mock has none).
    pub async fn start<F, Fut>(script: F) -> Self
    where
        F: FnOnce(Conn) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("tidb://root@{}/db?ssl-mode=disabled", listener.local_addr().unwrap());
        let log = Log::default();
        let conn_log = log.clone();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            script(Conn { socket, seq: 0, log: conn_log }).await;
        });
        Self { url, log, task }
    }

    /// Wait for the script to finish (propagating its panics).
    pub async fn finish(self) -> Vec<Vec<u8>> {
        self.task.await.expect("mock server script panicked");
        Arc::try_unwrap(self.log).unwrap().into_inner().unwrap()
    }
}

pub struct Conn {
    socket: TcpStream,
    seq: u8,
    log: Log,
}

pub fn lenenc_int(buf: &mut Vec<u8>, v: u64) {
    match v {
        0..=250 => buf.push(v as u8),
        251..=0xFFFF => {
            buf.push(0xFC);
            buf.extend_from_slice(&(v as u16).to_le_bytes());
        }
        0x1_0000..=0xFF_FFFF => {
            buf.push(0xFD);
            buf.extend_from_slice(&(v as u32).to_le_bytes()[..3]);
        }
        _ => {
            buf.push(0xFE);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
}

pub fn lenenc_str(buf: &mut Vec<u8>, s: &[u8]) {
    lenenc_int(buf, s.len() as u64);
    buf.extend_from_slice(s);
}

impl Conn {
    pub async fn send(&mut self, payload: &[u8]) {
        let len = payload.len() as u32;
        let mut frame = len.to_le_bytes()[..3].to_vec();
        frame.push(self.seq);
        frame.extend_from_slice(payload);
        self.seq = self.seq.wrapping_add(1);
        self.socket.write_all(&frame).await.unwrap();
        self.socket.flush().await.unwrap();
    }

    /// Read one packet; `None` on EOF.
    pub async fn recv(&mut self) -> Option<Vec<u8>> {
        let mut header = [0u8; 4];
        self.socket.read_exact(&mut header).await.ok()?;
        let len = u32::from_le_bytes([header[0], header[1], header[2], 0]) as usize;
        self.seq = header[3].wrapping_add(1);
        let mut payload = vec![0; len];
        self.socket.read_exact(&mut payload).await.ok()?;
        Some(payload)
    }

    /// Read the next command (logged), panicking on EOF.
    pub async fn command(&mut self) -> Vec<u8> {
        let p = self.recv().await.expect("client closed the connection");
        self.log.lock().unwrap().push(p.clone());
        p
    }

    /// Read the next command and check it is `cmd` (and, for COM_QUERY, its SQL).
    pub async fn expect(&mut self, cmd: u8, sql: Option<&str>) -> Vec<u8> {
        let p = self.command().await;
        assert_eq!(p[0], cmd, "unexpected command {:?}", String::from_utf8_lossy(&p));
        if let Some(sql) = sql {
            assert_eq!(std::str::from_utf8(&p[1..]).unwrap(), sql);
        }
        p
    }

    pub async fn handshake(&mut self, plugin: &str, caps: u32) {
        let mut p = vec![10u8];
        p.extend_from_slice(b"8.0.11-TiDB-v8.5.8\0");
        p.extend_from_slice(&7u32.to_le_bytes());
        p.extend_from_slice(b"abcdefgh\0");
        p.extend_from_slice(&(caps as u16).to_le_bytes());
        p.push(46);
        p.extend_from_slice(&AUTOCOMMIT.to_le_bytes());
        p.extend_from_slice(&((caps >> 16) as u16).to_le_bytes());
        p.push(21);
        p.extend_from_slice(&[0; 10]);
        p.extend_from_slice(b"ijklmnopqrst\0");
        p.extend_from_slice(plugin.as_bytes());
        p.push(0);
        self.seq = 0;
        self.send(&p).await;
    }

    /// Handshake (native password), accept any credentials, answer the session `SET`.
    pub async fn accept(&mut self) {
        self.handshake("mysql_native_password", DEFAULT_CAPS).await;
        self.recv().await.expect("handshake response");
        self.ok(0, 0, AUTOCOMMIT).await;
        let set = self.recv().await.expect("session SET");
        assert!(set.starts_with(b"\x03SET NAMES"), "{:?}", String::from_utf8_lossy(&set));
        self.ok(0, 0, AUTOCOMMIT).await;
    }

    pub async fn ok(&mut self, affected: u64, last_id: u64, status: u16) {
        let mut p = vec![0x00];
        lenenc_int(&mut p, affected);
        lenenc_int(&mut p, last_id);
        p.extend_from_slice(&status.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes());
        self.send(&p).await;
    }

    /// End of a result set (DEPRECATE_EOF: an OK packet with a 0xFE header).
    pub async fn end_rows(&mut self, status: u16) {
        let mut p = vec![0xFE, 0, 0];
        p.extend_from_slice(&status.to_le_bytes());
        p.extend_from_slice(&0u16.to_le_bytes());
        self.send(&p).await;
    }

    pub async fn err(&mut self, code: u16, msg: &str) {
        let mut p = vec![0xFF];
        p.extend_from_slice(&code.to_le_bytes());
        p.extend_from_slice(b"#HY000");
        p.extend_from_slice(msg.as_bytes());
        self.send(&p).await;
    }

    pub async fn column(&mut self, name: &str, ty: u8, flags: u16) {
        let mut p = Vec::new();
        for s in ["def", "db", "t", "t", name, name] {
            lenenc_str(&mut p, s.as_bytes());
        }
        p.push(0x0c);
        p.extend_from_slice(&(if ty == VAR_STRING { 46u16 } else { 63 }).to_le_bytes());
        p.extend_from_slice(&20u32.to_le_bytes());
        p.push(ty);
        p.extend_from_slice(&flags.to_le_bytes());
        p.push(0);
        p.extend_from_slice(&[0, 0]);
        self.send(&p).await;
    }

    /// Column count and definitions of a result set.
    pub async fn columns(&mut self, cols: &[(&str, u8, u16)]) {
        let mut n = Vec::new();
        lenenc_int(&mut n, cols.len() as u64);
        self.send(&n).await;
        for &(name, ty, flags) in cols {
            self.column(name, ty, flags).await;
        }
    }

    pub async fn text_row(&mut self, values: &[Option<&str>]) {
        let mut p = Vec::new();
        for v in values {
            match v {
                Some(s) => lenenc_str(&mut p, s.as_bytes()),
                None => p.push(0xFB),
            }
        }
        self.send(&p).await;
    }

    /// A binary-protocol row of BIGINT values.
    pub async fn binary_row_i64(&mut self, values: &[u64]) {
        let mut p = vec![0x00];
        p.extend(std::iter::repeat_n(0u8, (values.len() + 9) / 8)); // NULL bitmap, offset 2
        for v in values {
            p.extend_from_slice(&v.to_le_bytes());
        }
        self.send(&p).await;
    }

    /// COM_STMT_PREPARE response with BIGINT params and the given columns.
    pub async fn prepare_ok(&mut self, id: u32, params: u16, cols: &[(&str, u8, u16)]) {
        let mut p = vec![0x00];
        p.extend_from_slice(&id.to_le_bytes());
        p.extend_from_slice(&(cols.len() as u16).to_le_bytes());
        p.extend_from_slice(&params.to_le_bytes());
        p.push(0);
        p.extend_from_slice(&0u16.to_le_bytes());
        self.send(&p).await;
        for i in 0..params {
            self.column(&format!("?{i}"), LONGLONG, 0).await;
        }
        for &(name, ty, flags) in cols {
            self.column(name, ty, flags).await;
        }
    }
}

/// The statement id of a COM_STMT_EXECUTE / COM_STMT_CLOSE packet.
pub fn stmt_id(packet: &[u8]) -> u32 {
    u32::from_le_bytes(packet[1..5].try_into().unwrap())
}
