//! TiDB's `VECTOR` / `VECTOR(n)` type (FLOAT32 elements, TiDB 8.4+).
//!
//! TiDB sends vectors as text (`[1,2.5,3]`) in both protocols, and accepts the same
//! text for inserts and for functions such as `VEC_COSINE_DISTANCE(col, ?)`.

use std::fmt::Write;

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::row::ValueRef;
use crate::type_info::TypeInfo;
use crate::types::Type;

/// A TiDB vector of 32-bit floats.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Vector(pub Vec<f32>);

impl Vector {
    /// The text form TiDB accepts, e.g. `[1,2.5,3]`.
    pub fn to_text(&self) -> Result<String, BoxDynError> {
        let mut s = String::with_capacity(2 + self.0.len() * 8);
        s.push('[');
        for (i, x) in self.0.iter().enumerate() {
            if !x.is_finite() {
                return Err(format!("vector element {x} is not finite; TiDB rejects NaN and infinity").into());
            }
            if i > 0 {
                s.push(',');
            }
            write!(s, "{x}")?;
        }
        s.push(']');
        Ok(s)
    }

    /// Parse TiDB's text form.
    pub fn parse(s: &str) -> Result<Self, BoxDynError> {
        let inner = s
            .trim()
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .ok_or_else(|| format!("invalid vector {s:?}"))?
            .trim();
        if inner.is_empty() {
            return Ok(Self(Vec::new()));
        }
        Ok(Self(inner.split(',').map(|x| x.trim().parse::<f32>()).collect::<Result<_, _>>()?))
    }
}

impl From<Vec<f32>> for Vector {
    fn from(v: Vec<f32>) -> Self {
        Self(v)
    }
}

impl From<Vector> for Vec<f32> {
    fn from(v: Vector) -> Self {
        v.0
    }
}

impl Type for Vector {
    fn type_info() -> TypeInfo {
        <str as Type>::type_info()
    }

    fn compatible(ty: &TypeInfo) -> bool {
        <str as Type>::compatible(ty)
    }
}

impl Encode for Vector {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        self.to_text()?.encode(buf)
    }
}

impl<'r> Decode<'r> for Vector {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        Self::parse(value.as_str()?)
    }
}

#[cfg(test)]
mod tests {
    use super::Vector;

    #[test]
    fn text_round_trip() {
        let v = Vector(vec![1.0, 2.5, -3.25e-3]);
        let text = v.to_text().unwrap();
        assert_eq!(text, "[1,2.5,-0.00325]");
        assert_eq!(Vector::parse(&text).unwrap(), v);
        assert_eq!(Vector::parse(" [ ] ").unwrap(), Vector::default());
        assert!(Vector::parse("1,2").is_err());
        assert!(Vector(vec![f32::NAN]).to_text().is_err());
    }
}
