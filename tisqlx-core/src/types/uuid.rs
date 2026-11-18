//! `uuid::Uuid` as `BINARY(16)` (the compact form `UUID_TO_BIN` produces), and
//! `uuid::fmt::Hyphenated` as `CHAR(36)` text.

use uuid::Uuid;
use uuid::fmt::Hyphenated;

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::row::ValueRef;
use crate::type_info::TypeInfo;
use crate::types::Type;

impl Type for Uuid {
    fn type_info() -> TypeInfo {
        <[u8] as Type>::type_info()
    }

    fn compatible(ty: &TypeInfo) -> bool {
        <[u8] as Type>::compatible(ty)
    }
}

impl Encode for Uuid {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        self.as_bytes()[..].encode(buf)
    }
}

/// Reads `BINARY(16)` as raw bytes, and text columns (`CHAR(36)`) as the textual form.
impl<'r> Decode<'r> for Uuid {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        if value.type_info().is_binary() {
            Ok(Uuid::from_slice(value.as_bytes()?)?)
        } else {
            Ok(Uuid::parse_str(value.as_str()?)?)
        }
    }
}

impl Type for Hyphenated {
    fn type_info() -> TypeInfo {
        <str as Type>::type_info()
    }

    fn compatible(ty: &TypeInfo) -> bool {
        <str as Type>::compatible(ty)
    }
}

impl Encode for Hyphenated {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        self.encode_lower(&mut Uuid::encode_buffer()).encode(buf)
    }
}

impl<'r> Decode<'r> for Hyphenated {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(Uuid::parse_str(value.as_str()?)?.hyphenated())
    }
}
