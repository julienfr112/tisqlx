//! `bigdecimal::BigDecimal` for DECIMAL values of any precision (TiDB allows 65
//! digits; `rust_decimal::Decimal` holds about 28).

use std::str::FromStr;

use bigdecimal::BigDecimal;

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::protocol::buf::BufMutExt;
use crate::row::ValueRef;
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

impl Type for BigDecimal {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::NewDecimal)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        matches!(ty.r#type, ColumnType::Decimal | ColumnType::NewDecimal) || ty.r#type.is_integer()
    }
}

impl Encode for BigDecimal {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        // Plain notation: TiDB does not parse exponents in DECIMAL literals sent as strings.
        buf.put_lenenc_bytes(self.to_plain_string().as_bytes());
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for BigDecimal {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        if value.type_info().r#type.is_integer() {
            return Ok(BigDecimal::from(super::int::decode_int(&value)?));
        }
        Ok(BigDecimal::from_str(value.as_str()?.trim())?)
    }
}
