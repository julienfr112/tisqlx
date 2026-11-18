use std::ops::{Deref, DerefMut};

use serde::{Deserialize, Serialize};

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::protocol::buf::BufMutExt;
use crate::row::ValueRef;
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

/// Wrapper to (de)serialize any serde type as a `JSON` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Json<T>(pub T);

impl<T> Deref for Json<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

fn json_compatible(ty: &TypeInfo) -> bool {
    ty.r#type.is_textual()
}

fn encode_json<T: Serialize + ?Sized>(v: &T, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
    buf.put_lenenc_bytes(&serde_json::to_vec(v)?);
    Ok(IsNull::No)
}

// JSON is sent as a string; TiDB casts it on insert/compare.
fn json_param_type() -> TypeInfo {
    TypeInfo::text(ColumnType::VarString)
}

impl<T> Type for Json<T> {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Json)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        json_compatible(ty)
    }
}

impl<T: Serialize> Encode for Json<T> {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        encode_json(&self.0, buf)
    }

    fn produces(&self) -> TypeInfo {
        json_param_type()
    }
}

impl<'r, T: Deserialize<'r>> Decode<'r> for Json<T> {
    /// SQL `NULL` is decoded as JSON `null`, so `Json<Option<T>>` yields `None`.
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        let bytes = value.bytes.unwrap_or(b"null");
        Ok(Json(serde_json::from_slice(bytes)?))
    }
}

impl Type for serde_json::Value {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Json)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        json_compatible(ty)
    }
}

impl Encode for serde_json::Value {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        encode_json(self, buf)
    }

    fn produces(&self) -> TypeInfo {
        json_param_type()
    }
}

impl<'r> Decode<'r> for serde_json::Value {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(serde_json::from_slice(value.as_bytes()?)?)
    }
}

// ---- &RawValue: the JSON text as-is, without parsing it into a tree

impl Type for serde_json::value::RawValue {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Json)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        json_compatible(ty)
    }
}

impl Encode for serde_json::value::RawValue {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        buf.put_lenenc_bytes(self.get().as_bytes());
        Ok(IsNull::No)
    }

    fn produces(&self) -> TypeInfo {
        json_param_type()
    }
}

impl<'r> Decode<'r> for &'r serde_json::value::RawValue {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(serde_json::from_str(value.as_str()?)?)
    }
}
