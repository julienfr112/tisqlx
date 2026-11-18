use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::protocol::buf::BufMutExt;
use crate::row::{RowFormat, ValueRef};
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

/// Read any integer-like column into an i128.
pub(crate) fn decode_int(value: &ValueRef<'_>) -> Result<i128, BoxDynError> {
    let b = value.as_bytes()?;
    let ty = value.type_info;
    if ty.r#type == ColumnType::Bit {
        if b.len() > 8 {
            return Err("BIT value wider than 64 bits".into());
        }
        return Ok(b.iter().fold(0u64, |acc, &x| (acc << 8) | x as u64) as i128);
    }
    if value.format == RowFormat::Binary {
        let u = ty.is_unsigned();
        let v = match ty.r#type {
            ColumnType::Tiny if u => u8::from_le_bytes(b.try_into()?) as i128,
            ColumnType::Tiny => i8::from_le_bytes(b.try_into()?) as i128,
            ColumnType::Short | ColumnType::Year if u || ty.r#type == ColumnType::Year => {
                u16::from_le_bytes(b.try_into()?) as i128
            }
            ColumnType::Short => i16::from_le_bytes(b.try_into()?) as i128,
            ColumnType::Long | ColumnType::Int24 if u => u32::from_le_bytes(b.try_into()?) as i128,
            ColumnType::Long | ColumnType::Int24 => i32::from_le_bytes(b.try_into()?) as i128,
            ColumnType::LongLong if u => u64::from_le_bytes(b.try_into()?) as i128,
            ColumnType::LongLong => i64::from_le_bytes(b.try_into()?) as i128,
            _ => std::str::from_utf8(b)?.trim().parse()?,
        };
        return Ok(v);
    }
    Ok(std::str::from_utf8(b)?.trim().parse()?)
}

fn int_compatible(ty: &TypeInfo) -> bool {
    ty.r#type.is_integer() || ty.r#type == ColumnType::Bit
}

macro_rules! impl_int {
    ($t:ty, $col:ident, $unsigned:expr) => {
        impl Type for $t {
            fn type_info() -> TypeInfo {
                if $unsigned { TypeInfo::unsigned(ColumnType::$col) } else { TypeInfo::new(ColumnType::$col) }
            }

            fn compatible(ty: &TypeInfo) -> bool {
                int_compatible(ty)
            }
        }

        impl Encode for $t {
            fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
                buf.extend_from_slice(&self.to_le_bytes());
                Ok(IsNull::No)
            }
        }

        impl<'r> Decode<'r> for $t {
            fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
                let v = decode_int(&value)?;
                <$t>::try_from(v).map_err(|_| format!("value {v} out of range for {}", stringify!($t)).into())
            }
        }
    };
}

impl_int!(i8, Tiny, false);
impl_int!(i16, Short, false);
impl_int!(i32, Long, false);
impl_int!(i64, LongLong, false);
impl_int!(u8, Tiny, true);
impl_int!(u16, Short, true);
impl_int!(u32, Long, true);
impl_int!(u64, LongLong, true);

impl Type for bool {
    fn type_info() -> TypeInfo {
        TypeInfo { max_size: 1, ..TypeInfo::new(ColumnType::Tiny) }
    }

    fn compatible(ty: &TypeInfo) -> bool {
        int_compatible(ty)
    }
}

impl Encode for bool {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        buf.push(*self as u8);
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for bool {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(decode_int(&value)? != 0)
    }
}

// ---- i128 / u128: integer columns and integral DECIMAL(38, 0)-style values

macro_rules! impl_wide_int {
    ($t:ty) => {
        impl Type for $t {
            fn type_info() -> TypeInfo {
                TypeInfo::new(ColumnType::NewDecimal)
            }

            fn compatible(ty: &TypeInfo) -> bool {
                int_compatible(ty) || matches!(ty.r#type, ColumnType::Decimal | ColumnType::NewDecimal)
            }
        }

        impl Encode for $t {
            fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
                // Sent as a DECIMAL literal: no integer wire type is this wide.
                buf.put_lenenc_bytes(self.to_string().as_bytes());
                Ok(IsNull::No)
            }
        }

        impl<'r> Decode<'r> for $t {
            fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
                if int_compatible(value.type_info) {
                    let v = decode_int(&value)?;
                    return <$t>::try_from(v)
                        .map_err(|_| format!("value {v} out of range for {}", stringify!($t)).into());
                }
                // DECIMAL text: integral values only ("12.000" is fine, "12.5" is not).
                let s = value.as_str()?.trim();
                let (int, frac) = s.split_once('.').unwrap_or((s, ""));
                if frac.bytes().any(|b| b != b'0') {
                    return Err(format!("DECIMAL value {s} has a fractional part").into());
                }
                Ok(int.parse()?)
            }
        }
    };
}

impl_wide_int!(i128);
impl_wide_int!(u128);
