use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::protocol::buf::BufMutExt;
use crate::row::ValueRef;
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

fn bytes_compatible(ty: &TypeInfo) -> bool {
    ty.r#type.is_textual() || matches!(ty.r#type, ColumnType::Bit | ColumnType::Geometry)
}

impl Type for [u8] {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Blob)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        bytes_compatible(ty)
    }
}

forward_type!([u8] => Vec<u8>, std::borrow::Cow<'_, [u8]>);

impl<const N: usize> Type for [u8; N] {
    fn type_info() -> TypeInfo {
        <[u8] as Type>::type_info()
    }

    fn compatible(ty: &TypeInfo) -> bool {
        bytes_compatible(ty)
    }
}

impl Encode for [u8] {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        buf.put_lenenc_bytes(self);
        Ok(IsNull::No)
    }
}

impl<const N: usize> Encode for [u8; N] {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        self.as_slice().encode(buf)
    }
}

impl<'r> Decode<'r> for &'r [u8] {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_bytes()
    }
}

impl<'r> Decode<'r> for Vec<u8> {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_bytes().map(<[u8]>::to_vec)
    }
}

impl<'r, const N: usize> Decode<'r> for [u8; N] {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(value.as_bytes()?.try_into()?)
    }
}

impl<'r> Decode<'r> for Box<[u8]> {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_bytes().map(Box::from)
    }
}

impl<'r> Decode<'r> for std::sync::Arc<[u8]> {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_bytes().map(std::sync::Arc::from)
    }
}
