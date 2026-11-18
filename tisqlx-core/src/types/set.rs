//! TiDB `SET` columns as a list of members.

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::row::ValueRef;
use crate::type_info::TypeInfo;
use crate::types::Type;

/// The members of a `SET('a','b',...)` value, in the server's order. TiDB sends and
/// accepts sets as comma-separated text (`'a,b'`); members cannot contain commas.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Set(pub Vec<String>);

impl Set {
    pub fn contains(&self, member: &str) -> bool {
        self.0.iter().any(|m| m == member)
    }
}

impl<S: Into<String>> FromIterator<S> for Set {
    fn from_iter<I: IntoIterator<Item = S>>(iter: I) -> Self {
        Self(iter.into_iter().map(Into::into).collect())
    }
}

impl Type for Set {
    fn type_info() -> TypeInfo {
        <str as Type>::type_info()
    }

    fn compatible(ty: &TypeInfo) -> bool {
        <str as Type>::compatible(ty)
    }
}

impl Encode for Set {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        if let Some(bad) = self.0.iter().find(|m| m.contains(',')) {
            return Err(format!("SET member {bad:?} contains a comma").into());
        }
        self.0.join(",").encode(buf)
    }
}

impl<'r> Decode<'r> for Set {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        let s = value.as_str()?;
        Ok(if s.is_empty() { Self::default() } else { s.split(',').collect() })
    }
}
