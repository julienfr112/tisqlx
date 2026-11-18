use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::row::{RowFormat, ValueRef};
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

fn float_compatible(ty: &TypeInfo) -> bool {
    matches!(ty.r#type, ColumnType::Float | ColumnType::Double | ColumnType::Decimal | ColumnType::NewDecimal)
}

fn decode_f64(value: &ValueRef<'_>) -> Result<f64, BoxDynError> {
    let b = value.as_bytes()?;
    if value.format == RowFormat::Binary {
        match value.type_info.r#type {
            ColumnType::Float => return Ok(f32::from_le_bytes(b.try_into()?) as f64),
            ColumnType::Double => return Ok(f64::from_le_bytes(b.try_into()?)),
            _ => {}
        }
    }
    Ok(std::str::from_utf8(b)?.trim().parse()?)
}

impl Type for f32 {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Float)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        float_compatible(ty)
    }
}

impl Type for f64 {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Double)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        float_compatible(ty)
    }
}

impl Encode for f32 {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        buf.extend_from_slice(&self.to_le_bytes());
        Ok(IsNull::No)
    }
}

impl Encode for f64 {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        buf.extend_from_slice(&self.to_le_bytes());
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for f64 {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        decode_f64(&value)
    }
}

impl<'r> Decode<'r> for f32 {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(decode_f64(&value)? as f32)
    }
}
