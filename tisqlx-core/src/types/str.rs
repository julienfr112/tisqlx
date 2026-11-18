use std::borrow::Cow;
use std::sync::Arc;

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::protocol::buf::BufMutExt;
use crate::row::ValueRef;
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

pub(crate) fn text_compatible(ty: &TypeInfo) -> bool {
    ty.r#type.is_textual()
}

impl Type for str {
    fn type_info() -> TypeInfo {
        TypeInfo::text(ColumnType::VarString)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        text_compatible(ty)
    }
}

forward_type!(str => String, Cow<'_, str>);

impl Encode for str {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        buf.put_lenenc_bytes(self.as_bytes());
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for &'r str {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_str()
    }
}

impl<'r> Decode<'r> for String {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_str().map(str::to_owned)
    }
}

impl<'r> Decode<'r> for Cow<'r, str> {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_str().map(Cow::Borrowed)
    }
}

impl<'r> Decode<'r> for Box<str> {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_str().map(Box::from)
    }
}

impl<'r> Decode<'r> for Arc<str> {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        value.as_str().map(Arc::from)
    }
}
