//! Compile-time checks of bind arguments against the column a parameter is compared with or
//! assigned to, used by the query macros. Not a public API.
//!
//! Every Rust type tisqlx knows has a value class ([`BindClass`]); every inferred parameter has a
//! column kind (`col::*`). A known type whose class the column kind does not [`Accept`] is a
//! compile error. Types tisqlx does not know (hand-written `Encode` impls, generic arguments)
//! are not checked: the macros pick between the two cases with autoref dispatch.
//!
//! ```text
//! use ::tisqlx::bind_check::{Known as _, Unknown as _};
//! ::tisqlx::bind_check::accept::<col::Numeric, _>((&&Probe::new(arg)).class());
//! ```

use std::marker::PhantomData;

/// Kinds of columns a parameter can be inferred to match.
pub mod col {
    macro_rules! kinds {
        ($($(#[$m:meta])* $name:ident),*) => {$(
            $(#[$m])*
            #[derive(Debug)]
            pub struct $name;
        )*};
    }
    kinds!(
        /// Integer, DECIMAL, FLOAT/DOUBLE, BIT, YEAR (and BOOLEAN, i.e. `TINYINT(1)`)
        Numeric,
        /// CHAR, VARCHAR, TEXT, ENUM, SET (and VECTOR, which TiDB reports as text)
        Text,
        /// BINARY, VARBINARY, BLOB
        Bytes,
        Json,
        Date,
        /// DATETIME or TIMESTAMP
        DateTime,
        Time
    );
}

/// Classes of Rust values.
pub mod class {
    macro_rules! classes {
        ($($name:ident),*) => {$(
            #[derive(Debug)]
            pub struct $name;
        )*};
    }
    classes!(Number, Text, Bytes, Json, Date, DateTime, Time, Unknown);
}

/// The class of a Rust type that can be bound.
pub trait BindClass {
    type Class;
}

/// Whether values of this class may be bound where TiDB expects `Col`.
#[diagnostic::on_unimplemented(
    message = "this argument's type is not compatible with the column it is compared with or assigned to",
    label = "a `{Self}` value, but the parameter expects a `{Col}` value",
    note = "the query macros infer the parameter's type from the column next to its `?`",
    note = "if the conversion is intended, bind `value as _` to skip this check"
)]
pub trait Accept<Col> {}

macro_rules! accept {
    ($col:ident <- $($class:ident),*) => {$(
        // Listing the other accepted pairs in errors only adds noise.
        #[diagnostic::do_not_recommend]
        impl Accept<col::$col> for class::$class {}
    )*};
}
accept!(Numeric <- Number);
accept!(Text <- Text, Json);
accept!(Bytes <- Bytes, Text, Json);
accept!(Json <- Json, Text);
accept!(Date <- Date, DateTime);
accept!(DateTime <- DateTime, Date);
accept!(Time <- Time);
#[diagnostic::do_not_recommend]
impl<Col> Accept<Col> for class::Unknown {}

/// Wraps a bind argument for the autoref dispatch between [`Known`] and [`Unknown`].
#[derive(Debug)]
pub struct Probe<'a, T: ?Sized>(PhantomData<&'a T>);

impl<'a, T: ?Sized> Probe<'a, T> {
    pub fn new(_: &'a T) -> Self {
        Self(PhantomData)
    }
}

/// The class of a value, carried to [`accept`].
#[derive(Debug)]
pub struct Class<C>(PhantomData<C>);

impl Class<class::Number> {
    /// For an unsuffixed numeric literal, whose type is still being inferred when the dispatch
    /// would run (and would then fall back to [`Unknown`]).
    pub fn number() -> Self {
        Class(PhantomData)
    }
}

/// Chosen (by autoref) when the argument's type has a [`BindClass`].
pub trait Known {
    type Class;
    fn class(&self) -> Class<Self::Class> {
        Class(PhantomData)
    }
}

impl<T: BindClass + ?Sized> Known for &Probe<'_, T> {
    type Class = T::Class;
}

/// Fallback for types tisqlx does not know: never rejected.
pub trait Unknown {
    fn class(&self) -> Class<class::Unknown> {
        Class(PhantomData)
    }
}

impl<T: ?Sized> Unknown for Probe<'_, T> {}

/// Compiles only if `C` is accepted for a `Col` parameter.
pub fn accept<Col, C: Accept<Col>>(_: Class<C>) {}

// ---- classes of the types tisqlx encodes

macro_rules! classify {
    ($class:ident: $($t:ty),* $(,)?) => {$( impl BindClass for $t { type Class = class::$class; } )*};
}

classify!(Number: i8, i16, i32, i64, i128, u8, u16, u32, u64, u128, f32, f64, bool);
classify!(Text: str, String, crate::types::Set, crate::types::Vector);
classify!(Bytes: [u8], Vec<u8>);
classify!(Json: serde_json::Value, serde_json::value::RawValue);
classify!(Time: std::time::Duration);

impl<const N: usize> BindClass for [u8; N] {
    type Class = class::Bytes;
}

impl BindClass for std::borrow::Cow<'_, str> {
    type Class = class::Text;
}

impl BindClass for std::borrow::Cow<'_, [u8]> {
    type Class = class::Bytes;
}

impl<T> BindClass for crate::types::Json<T> {
    type Class = class::Json;
}

impl<T: BindClass + ?Sized> BindClass for &T {
    type Class = T::Class;
}

impl<T: BindClass> BindClass for Option<T> {
    type Class = T::Class;
}

impl<T: BindClass + ?Sized> BindClass for Box<T> {
    type Class = T::Class;
}

impl<T: BindClass + ?Sized> BindClass for std::sync::Arc<T> {
    type Class = T::Class;
}

classify!(Number: rust_decimal::Decimal);
classify!(Number: bigdecimal::BigDecimal);
classify!(Bytes: uuid::Uuid);
classify!(Text: uuid::fmt::Hyphenated);
classify!(Date: chrono::NaiveDate);
classify!(DateTime: chrono::NaiveDateTime);
classify!(Time: chrono::NaiveTime, chrono::TimeDelta);
impl<Tz: chrono::TimeZone> BindClass for chrono::DateTime<Tz> {
    type Class = class::DateTime;
}

#[cfg(test)]
mod tests {
    use super::{Known as _, Unknown as _, *};

    struct Mine;

    #[test]
    #[allow(clippy::needless_borrow)] // the `&&` is the dispatch
    fn dispatch_compiles_for_known_and_unknown_types() {
        accept::<col::Numeric, _>((&&Probe::new(&5i64)).class());
        accept::<col::Numeric, _>((&&Probe::new(&Some(1.5f64))).class());
        accept::<col::Text, _>((&&Probe::new("x")).class());
        accept::<col::Bytes, _>((&&Probe::new(&"x".to_owned())).class());
        accept::<col::Json, _>((&&Probe::new(&&*Box::<str>::from("{}"))).class());
        // A binary column takes everything a text column does.
        accept::<col::Bytes, _>((&&Probe::new(&serde_json::Value::Null)).class());
        // Not classified: accepted anywhere.
        accept::<col::Numeric, _>((&&Probe::new(&Mine)).class());
        accept::<col::Text, _>((&&Probe::new(&Some(Mine))).class());
    }
}
