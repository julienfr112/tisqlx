use rust_decimal::Decimal;

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::protocol::buf::BufMutExt;
use crate::row::ValueRef;
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

impl Type for Decimal {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::NewDecimal)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        matches!(ty.r#type, ColumnType::Decimal | ColumnType::NewDecimal) || ty.r#type.is_integer()
    }
}

impl Encode for Decimal {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        buf.put_lenenc_bytes(self.to_string().as_bytes());
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for Decimal {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        if value.type_info.r#type.is_integer() {
            return Ok(Decimal::from_i128_with_scale(super::int::decode_int(&value)?, 0));
        }
        // Exact: `from_str` would round a value with more digits than `Decimal` holds.
        Ok(Decimal::from_str_exact(value.as_str()?.trim())?)
    }
}
