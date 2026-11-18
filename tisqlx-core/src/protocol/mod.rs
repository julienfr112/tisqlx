//! The subset of the MySQL client/server protocol that TiDB speaks.

pub(crate) mod auth;
pub(crate) mod buf;
pub(crate) mod row;
pub(crate) mod stream;

use crate::error::{DatabaseError, Error, Result};
use crate::type_info::{ColumnFlags, ColumnType, TypeInfo};
use buf::{BufMutExt, Reader};

pub(crate) mod caps {
    pub const LONG_PASSWORD: u32 = 1;
    pub const FOUND_ROWS: u32 = 2;
    pub const LONG_FLAG: u32 = 4;
    pub const CONNECT_WITH_DB: u32 = 8;
    pub const PROTOCOL_41: u32 = 512;
    pub const SSL: u32 = 2048;
    pub const TRANSACTIONS: u32 = 8192;
    pub const SECURE_CONNECTION: u32 = 32768;
    pub const MULTI_STATEMENTS: u32 = 1 << 16;
    pub const MULTI_RESULTS: u32 = 1 << 17;
    pub const PS_MULTI_RESULTS: u32 = 1 << 18;
    pub const PLUGIN_AUTH: u32 = 1 << 19;
    pub const CONNECT_ATTRS: u32 = 1 << 20;
    pub const PLUGIN_AUTH_LENENC_DATA: u32 = 1 << 21;
    pub const DEPRECATE_EOF: u32 = 1 << 24;
}

pub(crate) mod status {
    pub const IN_TRANS: u16 = 0x0001;
    pub const MORE_RESULTS_EXISTS: u16 = 0x0008;
}

pub(crate) mod cmd {
    pub const QUIT: u8 = 0x01;
    pub const QUERY: u8 = 0x03;
    pub const PING: u8 = 0x0E;
    pub const STMT_PREPARE: u8 = 0x16;
    pub const STMT_EXECUTE: u8 = 0x17;
    pub const STMT_CLOSE: u8 = 0x19;
}

/// Initial handshake (protocol v10) sent by the server.
#[derive(Debug)]
pub(crate) struct Handshake {
    pub server_version: String,
    pub connection_id: u32,
    pub capabilities: u32,
    pub status: u16,
    pub auth_plugin: Option<String>,
    pub nonce: Vec<u8>,
}

impl Handshake {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = Reader::new(payload);
        let version = r.u8()?;
        if version == 0xFF {
            return Err(decode_err(payload).into());
        }
        if version != 10 {
            return Err(Error::protocol(format!("unsupported handshake protocol version {version}")));
        }
        let server_version = r.nul_str()?;
        let connection_id = r.u32()?;
        let mut nonce = r.bytes(8)?.to_vec();
        r.skip(1)?; // filler
        let mut capabilities = r.u16()? as u32;
        let _charset = r.u8()?;
        let status = r.u16()?;
        capabilities |= (r.u16()? as u32) << 16;
        let auth_data_len = r.u8()?;
        r.skip(10)?; // reserved
        if capabilities & caps::SECURE_CONNECTION != 0 {
            let len = (auth_data_len as isize - 8).max(13) as usize;
            let part2 = r.bytes(len)?;
            // part 2 is NUL-terminated
            nonce.extend_from_slice(buf::trim_nul(part2));
        }
        let auth_plugin = if capabilities & caps::PLUGIN_AUTH != 0 && !r.is_empty() {
            Some(String::from_utf8_lossy(buf::trim_nul(r.rest())).into_owned())
        } else {
            None
        };
        Ok(Self { server_version, connection_id, capabilities, status, auth_plugin, nonce })
    }
}

pub(crate) const UTF8MB4_GENERAL_CI: u8 = 45;

pub(crate) fn encode_ssl_request(capabilities: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.put_u32(capabilities);
    buf.put_u32(16 * 1024 * 1024);
    buf.put_u8(UTF8MB4_GENERAL_CI);
    buf.extend_from_slice(&[0; 23]);
    buf
}

pub(crate) fn encode_handshake_response(
    capabilities: u32,
    username: &str,
    auth_response: &[u8],
    database: Option<&str>,
    auth_plugin: &str,
) -> Vec<u8> {
    let mut buf = encode_ssl_request(capabilities);
    buf.put_nul_str(username);
    if capabilities & caps::PLUGIN_AUTH_LENENC_DATA != 0 {
        buf.put_lenenc_bytes(auth_response);
    } else {
        buf.put_u8(auth_response.len() as u8);
        buf.extend_from_slice(auth_response);
    }
    if let Some(db) = database
        && capabilities & caps::CONNECT_WITH_DB != 0
    {
        buf.put_nul_str(db);
    }
    if capabilities & caps::PLUGIN_AUTH != 0 {
        buf.put_nul_str(auth_plugin);
    }
    if capabilities & caps::CONNECT_ATTRS != 0 {
        let mut attrs = Vec::new();
        for (k, v) in [("_client_name", "tisqlx"), ("_client_version", env!("CARGO_PKG_VERSION"))] {
            attrs.put_lenenc_bytes(k.as_bytes());
            attrs.put_lenenc_bytes(v.as_bytes());
        }
        buf.put_lenenc_bytes(&attrs);
    }
    buf
}

/// Decoded OK packet (also used for the EOF-replacing OK when DEPRECATE_EOF is set).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct OkPacket {
    pub affected_rows: u64,
    pub last_insert_id: u64,
    pub status: u16,
}

impl OkPacket {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = Reader::new(payload);
        let header = r.u8()?;
        if header != 0x00 && header != 0xFE {
            return Err(Error::protocol(format!("expected OK packet, got header 0x{header:02x}")));
        }
        let affected_rows = r.lenenc_int()?;
        let last_insert_id = r.lenenc_int()?;
        let status = r.u16()?;
        Ok(Self { affected_rows, last_insert_id, status })
    }
}

/// Legacy EOF packet: `0xFE warnings status` with payload length < 9.
pub(crate) fn decode_eof_status(payload: &[u8]) -> Result<u16> {
    let mut r = Reader::new(payload);
    r.skip(1)?;
    let _warnings = r.u16()?;
    r.u16()
}

pub(crate) fn is_eof(payload: &[u8]) -> bool {
    payload.first() == Some(&0xFE) && payload.len() < 9
}

pub(crate) fn decode_err(payload: &[u8]) -> Box<DatabaseError> {
    let parse = || -> Result<DatabaseError> {
        let mut r = Reader::new(payload);
        r.skip(1)?;
        let code = r.u16()?;
        let sql_state = if r.remaining() > 0 && r.peek_u8()? == b'#' {
            r.skip(1)?;
            Some(String::from_utf8_lossy(r.bytes(5)?).into_owned())
        } else {
            None
        };
        let message = String::from_utf8_lossy(r.rest()).into_owned();
        Ok(DatabaseError { code, sql_state, message })
    };
    Box::new(parse().unwrap_or_else(|e| DatabaseError {
        code: 0,
        sql_state: None,
        message: format!("malformed error packet: {e}"),
    }))
}

/// A column definition (Protocol::ColumnDefinition41).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ColumnDefinition {
    pub schema: String,
    pub table: String,
    pub org_table: String,
    pub name: String,
    pub org_name: String,
    pub type_info: TypeInfo,
    pub decimals: u8,
}

impl ColumnDefinition {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = Reader::new(payload);
        let _catalog = r.lenenc_bytes()?;
        let schema = r.lenenc_str()?;
        let table = r.lenenc_str()?;
        let org_table = r.lenenc_str()?;
        let name = r.lenenc_str()?;
        let org_name = r.lenenc_str()?;
        let _fixed_len = r.lenenc_int()?;
        let charset = r.u16()?;
        let max_size = r.u32()?;
        let ty = r.u8()?;
        let flags = r.u16()?;
        let decimals = r.u8()?;
        // A type this driver does not know (e.g. from a newer TiDB): MySQL-protocol
        // servers send such values length-encoded, so read them as strings/bytes
        // rather than failing (which would break every prepared query on it).
        let r#type = ColumnType::try_from_u8(ty).unwrap_or_else(|| {
            log::warn!("unknown column type 0x{ty:02x} for `{name}`; reading it as a string");
            ColumnType::VarString
        });
        Ok(Self {
            schema,
            table,
            org_table,
            name,
            org_name,
            type_info: TypeInfo { r#type, flags: ColumnFlags(flags), charset, max_size },
            decimals,
        })
    }
}

/// COM_STMT_PREPARE_OK.
#[derive(Debug)]
pub(crate) struct PrepareOk {
    pub statement_id: u32,
    pub columns: u16,
    pub params: u16,
}

impl PrepareOk {
    pub(crate) fn decode(payload: &[u8]) -> Result<Self> {
        let mut r = Reader::new(payload);
        let header = r.u8()?;
        if header != 0x00 {
            return Err(Error::protocol(format!("expected PREPARE_OK, got 0x{header:02x}")));
        }
        let statement_id = r.u32()?;
        let columns = r.u16()?;
        let params = r.u16()?;
        Ok(Self { statement_id, columns, params })
    }
}
