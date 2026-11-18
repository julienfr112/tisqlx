//! Decoding values from rows.

use crate::error::BoxDynError;
use crate::row::ValueRef;

/// A type that can be decoded from a column value.
pub trait Decode<'r>: Sized {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError>;
}

impl<'r, T: Decode<'r>> Decode<'r> for Option<T> {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        if value.is_null() { Ok(None) } else { T::decode(value).map(Some) }
    }
}
