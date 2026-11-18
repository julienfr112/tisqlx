//! Column type metadata.

use std::fmt;

/// Wire-level column types used by TiDB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum ColumnType {
    Decimal = 0x00,
    Tiny = 0x01,
    Short = 0x02,
    Long = 0x03,
    Float = 0x04,
    Double = 0x05,
    Null = 0x06,
    Timestamp = 0x07,
    LongLong = 0x08,
    Int24 = 0x09,
    Date = 0x0A,
    Time = 0x0B,
    Datetime = 0x0C,
    Year = 0x0D,
    VarChar = 0x0F,
    Bit = 0x10,
    Json = 0xF5,
    NewDecimal = 0xF6,
    Enum = 0xF7,
    Set = 0xF8,
    TinyBlob = 0xF9,
    MediumBlob = 0xFA,
    LongBlob = 0xFB,
    Blob = 0xFC,
    VarString = 0xFD,
    String = 0xFE,
    Geometry = 0xFF,
}

impl ColumnType {
    pub fn try_from_u8(v: u8) -> Option<Self> {
        use ColumnType::*;
        Some(match v {
            0x00 => Decimal,
            0x01 => Tiny,
            0x02 => Short,
            0x03 => Long,
            0x04 => Float,
            0x05 => Double,
            0x06 => Null,
            0x07 => Timestamp,
            0x08 => LongLong,
            0x09 => Int24,
            0x0A => Date,
            0x0B => Time,
            0x0C => Datetime,
            0x0D => Year,
            0x0F => VarChar,
            0x10 => Bit,
            0xF5 => Json,
            0xF6 => NewDecimal,
            0xF7 => Enum,
            0xF8 => Set,
            0xF9 => TinyBlob,
            0xFA => MediumBlob,
            0xFB => LongBlob,
            0xFC => Blob,
            0xFD => VarString,
            0xFE => String,
            0xFF => Geometry,
            _ => return None,
        })
    }

    pub fn is_integer(self) -> bool {
        matches!(self, Self::Tiny | Self::Short | Self::Long | Self::Int24 | Self::LongLong | Self::Year)
    }

    pub fn is_textual(self) -> bool {
        matches!(
            self,
            Self::VarChar
                | Self::VarString
                | Self::String
                | Self::TinyBlob
                | Self::MediumBlob
                | Self::LongBlob
                | Self::Blob
                | Self::Enum
                | Self::Set
                | Self::Json
                | Self::Decimal
                | Self::NewDecimal
        )
    }
}

/// Column definition flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ColumnFlags(pub u16);

impl ColumnFlags {
    pub const NOT_NULL: u16 = 1;
    pub const PRIMARY_KEY: u16 = 2;
    pub const UNIQUE_KEY: u16 = 4;
    pub const MULTIPLE_KEY: u16 = 8;
    pub const BLOB: u16 = 16;
    pub const UNSIGNED: u16 = 32;
    pub const ZEROFILL: u16 = 64;
    pub const BINARY: u16 = 128;
    pub const ENUM: u16 = 256;
    pub const AUTO_INCREMENT: u16 = 512;
    pub const TIMESTAMP: u16 = 1024;
    pub const SET: u16 = 2048;
    pub const NO_DEFAULT_VALUE: u16 = 4096;
    pub const ON_UPDATE_NOW: u16 = 8192;
    pub const NUM: u16 = 32768;

    pub fn contains(self, flag: u16) -> bool {
        self.0 & flag == flag
    }
}

/// The binary collation id; strings with this charset are raw bytes.
pub const BINARY_CHARSET: u16 = 63;

/// Type information for a column or a bind parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct TypeInfo {
    pub r#type: ColumnType,
    pub flags: ColumnFlags,
    pub charset: u16,
    /// Display width (e.g. `1` for `TINYINT(1)`).
    pub max_size: u32,
}

impl TypeInfo {
    pub const fn new(r#type: ColumnType) -> Self {
        Self { r#type, flags: ColumnFlags(0), charset: BINARY_CHARSET, max_size: 0 }
    }

    pub const fn unsigned(r#type: ColumnType) -> Self {
        Self { r#type, flags: ColumnFlags(ColumnFlags::UNSIGNED), charset: BINARY_CHARSET, max_size: 0 }
    }

    pub const fn text(r#type: ColumnType) -> Self {
        Self { r#type, flags: ColumnFlags(0), charset: crate::protocol::UTF8MB4_GENERAL_CI as u16, max_size: 0 }
    }

    pub fn is_unsigned(&self) -> bool {
        self.flags.contains(ColumnFlags::UNSIGNED)
    }

    pub fn is_not_null(&self) -> bool {
        self.flags.contains(ColumnFlags::NOT_NULL)
    }

    pub fn is_binary(&self) -> bool {
        self.charset == BINARY_CHARSET
    }

    pub fn is_null(&self) -> bool {
        self.r#type == ColumnType::Null
    }

    /// `TINYINT(1)`, conventionally a boolean.
    pub fn is_bool(&self) -> bool {
        self.r#type == ColumnType::Tiny && self.max_size == 1
    }

    /// SQL-ish name for display and error messages.
    pub fn name(&self) -> &'static str {
        use ColumnType::*;
        let u = self.is_unsigned();
        match self.r#type {
            Tiny if self.is_bool() => "BOOLEAN",
            Tiny if u => "TINYINT UNSIGNED",
            Tiny => "TINYINT",
            Short if u => "SMALLINT UNSIGNED",
            Short => "SMALLINT",
            Int24 if u => "MEDIUMINT UNSIGNED",
            Int24 => "MEDIUMINT",
            Long if u => "INT UNSIGNED",
            Long => "INT",
            LongLong if u => "BIGINT UNSIGNED",
            LongLong => "BIGINT",
            Float => "FLOAT",
            Double => "DOUBLE",
            Null => "NULL",
            Timestamp => "TIMESTAMP",
            Date => "DATE",
            Time => "TIME",
            Datetime => "DATETIME",
            Year => "YEAR",
            Bit => "BIT",
            Json => "JSON",
            Decimal | NewDecimal => "DECIMAL",
            Enum => "ENUM",
            Set => "SET",
            TinyBlob | MediumBlob | LongBlob | Blob if self.is_binary() => "BLOB",
            TinyBlob | MediumBlob | LongBlob | Blob => "TEXT",
            VarChar | VarString if self.is_binary() => "VARBINARY",
            VarChar | VarString => "VARCHAR",
            String if self.flags.contains(ColumnFlags::ENUM) => "ENUM",
            String if self.flags.contains(ColumnFlags::SET) => "SET",
            String if self.is_binary() => "BINARY",
            String => "CHAR",
            Geometry => "GEOMETRY",
        }
    }
}

impl fmt::Display for TypeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
