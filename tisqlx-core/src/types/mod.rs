//! Rust ↔ TiDB type mapping.
//!
//! | TiDB | Rust |
//! |------|------|
//! | `BOOLEAN` / `TINYINT(1)`, `BIT(1)` | `bool` |
//! | `TINYINT`..`BIGINT` (`UNSIGNED`) | `i8`..`i64` (`u8`..`u64`) |
//! | `BIT(n)` | `u64`, `Vec<u8>` |
//! | `YEAR` | `u16` |
//! | `FLOAT`, `DOUBLE` | `f32`, `f64` |
//! | `DECIMAL` | `rust_decimal::Decimal` (`decimal`, ≤ 28 digits), `bigdecimal::BigDecimal` (`bigdecimal`, any precision), `i128`/`u128` (integral values), `f32`/`f64` (rounded) |
//! | `CHAR`, `VARCHAR`, `TEXT` | `String`, `&str`, `Cow<str>`, `Box<str>`, `Arc<str>` |
//! | `BINARY`, `VARBINARY`, `BLOB` | `Vec<u8>`, `&[u8]`, `[u8; N]`, `Box<[u8]>`, `Arc<[u8]>` |
//! | `ENUM` | `String`, or a `#[derive(Type)]` enum |
//! | `SET` | [`Set`], `String` |
//! | `JSON` | `serde_json::Value`, [`Json<T>`], `&serde_json::value::RawValue` |
//! | `VECTOR` | [`Vector`] |
//! | `DATE` | `chrono::NaiveDate`, `time::Date` (also a `DATETIME`/`TIMESTAMP` value at midnight; another time of day is an error) |
//! | `DATETIME`, `TIMESTAMP` | `chrono::NaiveDateTime`, `time::PrimitiveDateTime` (the wall time in the session zone) |
//! | `TIMESTAMP`, `DATETIME` as an instant | `chrono::DateTime<Utc>`, `DateTime<FixedOffset>`, `DateTime<Local>`, `time::OffsetDateTime` (converted from and to the session zone) |
//! | `TIME` | `chrono::TimeDelta`, `time::Duration`, `std::time::Duration` (≥ 0), `chrono::NaiveTime`, `time::Time` (< 24h) |
//! | `BINARY(16)`, `CHAR(36)` | `uuid::Uuid` (bytes or text), `uuid::fmt::Hyphenated` (text) (`uuid`) |
//! | `BIGINT UNSIGNED` | [`Tso`](crate::tidb::Tso) (TiDB timestamp oracle value) |
//!
//! Every type also decodes as `Option<T>` for nullable columns.
//!
//! A conversion that would lose something is a decode error, not a silent change: a fractional
//! `DECIMAL` into an integer, a `DATETIME` with a time of day into a date type, a negative `TIME`
//! into `std::time::Duration`. So is the zero date `0000-00-00` (which a non-strict `sql_mode`
//! stores), for every date and datetime type and their `Option`: it is not NULL.

use crate::type_info::TypeInfo;

/// Implement [`Type`] and [`Encode`](crate::Encode) for owned/borrowed forms by forwarding to `$target`.
macro_rules! forward_type {
    ($target:ty => $($t:ty),+) => {$(
        impl $crate::types::Type for $t {
            fn type_info() -> $crate::type_info::TypeInfo {
                <$target as $crate::types::Type>::type_info()
            }

            fn compatible(ty: &$crate::type_info::TypeInfo) -> bool {
                <$target as $crate::types::Type>::compatible(ty)
            }
        }

        impl $crate::encode::Encode for $t {
            fn encode(&self, buf: &mut Vec<u8>) -> Result<$crate::encode::IsNull, $crate::error::BoxDynError> {
                <$target as $crate::encode::Encode>::encode(::std::convert::AsRef::<$target>::as_ref(self), buf)
            }
        }
    )+};
}

#[cfg(feature = "bigdecimal")]
mod bigdecimal;
mod bytes;
#[cfg(feature = "chrono")]
mod chrono;
#[cfg(feature = "decimal")]
mod decimal;
mod float;
mod int;
mod json;
mod set;
mod str;
pub(crate) mod temporal;
#[cfg(feature = "time")]
mod time;
#[cfg(feature = "uuid")]
mod uuid;
mod vector;

pub use json::Json;
pub use set::Set;
pub use vector::Vector;

/// Associates a Rust type with a TiDB type.
pub trait Type {
    /// The type declared when sending this value as a parameter.
    fn type_info() -> TypeInfo;

    /// Whether a column of type `ty` can be decoded into this type.
    fn compatible(ty: &TypeInfo) -> bool {
        ty.r#type == Self::type_info().r#type
    }
}

impl<T: Type + ?Sized> Type for &T {
    fn type_info() -> TypeInfo {
        T::type_info()
    }

    fn compatible(ty: &TypeInfo) -> bool {
        T::compatible(ty)
    }
}

impl<T: Type> Type for Option<T> {
    fn type_info() -> TypeInfo {
        T::type_info()
    }

    fn compatible(ty: &TypeInfo) -> bool {
        ty.is_null() || T::compatible(ty)
    }
}

/// `Box<T>` and `Arc<T>` (including unsized `str`, `[u8]`, `RawValue`) bind and
/// type-check like `T`; decoding is implemented per type.
macro_rules! smart_pointer {
    ($($p:ident)::+) => {
        impl<T: Type + ?Sized> Type for $($p)::+<T> {
            fn type_info() -> TypeInfo {
                T::type_info()
            }

            fn compatible(ty: &TypeInfo) -> bool {
                T::compatible(ty)
            }
        }

        impl<T: crate::encode::Encode + ?Sized> crate::encode::Encode for $($p)::+<T> {
            fn encode(&self, buf: &mut Vec<u8>) -> Result<crate::encode::IsNull, crate::error::BoxDynError> {
                (**self).encode(buf)
            }

            fn produces(&self) -> TypeInfo {
                (**self).produces()
            }
        }
    };
}

smart_pointer!(Box);
smart_pointer!(std::sync::Arc);
