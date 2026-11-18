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
        parse_exact(value.as_str()?.trim())
    }
}

/// The value of `s` exactly, or an error: `from_str` would round a value with more digits than
/// `Decimal` holds. Trailing zeros are not such digits: a `DECIMAL(40, 30)` column sends `1.5` as
/// `1.500000000000000000000000000000`, beyond `Decimal`'s scale of 28.
fn parse_exact(s: &str) -> Result<Decimal, BoxDynError> {
    Decimal::from_str_exact(s).or_else(|e| {
        let significant = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s };
        if significant.len() == s.len() { Err(e.into()) } else { Ok(Decimal::from_str_exact(significant)?) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_means_the_value_not_the_digits() {
        let d = |s: &str| parse_exact(s).map(|d| d.to_string()).map_err(|e| e.to_string());
        assert_eq!(d("1.50").unwrap(), "1.50", "the scale is kept when it fits");
        assert_eq!(d("1.500000000000000000000000000000").unwrap(), "1.5");
        assert_eq!(d("-0.000000000000000000000000000000").unwrap(), "0");
        assert_eq!(d("79228162514264337593543950335.00").unwrap(), "79228162514264337593543950335");
        assert_eq!(d("0.0000000000000000000000000001000").unwrap(), "0.0000000000000000000000000001");
        // Digits that would be rounded away, and values out of range.
        assert!(d("1.123456789012345678901234567891").is_err());
        assert!(d("0.000000000000000000000000000001").is_err());
        assert!(d("79228162514264337593543950336").is_err());
        assert!(d("792281625142643375935439503350").is_err(), "an integer's zeros are significant");
    }
}
