//! Encoding bind parameters (binary protocol).

use crate::error::BoxDynError;
use crate::type_info::TypeInfo;
use crate::types::Type;

/// Whether an encoded value is NULL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsNull {
    Yes,
    No,
}

/// A type that can be sent as a bind parameter.
///
/// `encode` writes the value in the MySQL binary-protocol format matching
/// [`produces`](Encode::produces).
pub trait Encode: Type {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError>;

    /// The wire type to declare for this value (defaults to [`Type::type_info`]).
    fn produces(&self) -> TypeInfo {
        Self::type_info()
    }
}

impl<T: Encode + ?Sized> Encode for &T {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        (**self).encode(buf)
    }

    fn produces(&self) -> TypeInfo {
        (**self).produces()
    }
}

impl<T: Encode> Encode for Option<T> {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        match self {
            Some(v) => v.encode(buf),
            None => Ok(IsNull::Yes),
        }
    }

    fn produces(&self) -> TypeInfo {
        match self {
            Some(v) => v.produces(),
            None => T::type_info(),
        }
    }
}
