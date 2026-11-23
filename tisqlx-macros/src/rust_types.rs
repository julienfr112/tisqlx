//! Map TiDB column types to Rust types for generated code.

use proc_macro2::TokenStream;
use quote::quote;
use tisqlx_core::{ColumnFlags, ColumnType, TypeInfo};

pub fn rust_type_for(ty: &TypeInfo) -> Result<TokenStream, String> {
    use ColumnType::*;
    let u = ty.is_unsigned();
    Ok(match ty.r#type {
        Tiny if ty.is_bool() => quote!(bool),
        Tiny if u => quote!(u8),
        Tiny => quote!(i8),
        Short if u => quote!(u16),
        Short => quote!(i16),
        Int24 | Long if u => quote!(u32),
        Int24 | Long => quote!(i32),
        LongLong if u => quote!(u64),
        LongLong => quote!(i64),
        Year => quote!(u16),
        Bit if ty.max_size == 1 => quote!(bool),
        Bit => quote!(u64),
        Float => quote!(f32),
        Double => quote!(f64),
        // rust_decimal holds ~28 digits: wider columns (display width = precision + sign
        // [+ point]) get BigDecimal.
        Decimal | NewDecimal if ty.max_size > 29 => quote!(::tisqlx::types::BigDecimal),
        Decimal | NewDecimal => quote!(::tisqlx::types::Decimal),
        Json => quote!(::tisqlx::types::JsonValue),
        Date => quote!(::tisqlx::types::chrono::NaiveDate),
        Datetime => quote!(::tisqlx::types::chrono::NaiveDateTime),
        Timestamp => quote!(::tisqlx::types::chrono::DateTime<::tisqlx::types::chrono::Utc>),
        Time => quote!(::tisqlx::types::chrono::TimeDelta),
        Enum => quote!(::std::string::String),
        Set => quote!(::tisqlx::types::Set),
        String if ty.flags.contains(ColumnFlags::SET) => quote!(::tisqlx::types::Set),
        String if ty.flags.contains(ColumnFlags::ENUM) => quote!(::std::string::String),
        // TiDB has no UUID type: a fixed `BINARY(16)` column is taken to hold one (the form
        // `UUID_TO_BIN` produces). Other 16-byte values need an override such as `col: Vec<u8>`.
        String if ty.is_binary() && ty.max_size == 16 => quote!(::tisqlx::types::Uuid),
        VarChar | VarString | String | TinyBlob | MediumBlob | LongBlob | Blob if ty.is_binary() => {
            quote!(::std::vec::Vec<u8>)
        }
        VarChar | VarString | String | TinyBlob | MediumBlob | LongBlob | Blob => quote!(::std::string::String),
        Geometry => quote!(::std::vec::Vec<u8>),
        Null => return Err("column type is NULL; add a type override such as ``AS `col?: String` ``".into()),
    })
}

/// The `tisqlx::bind_check::col` kind a bind argument for a parameter matched against a column
/// of this type must be compatible with; `None` leaves it unchecked.
pub fn bind_column_kind(ty: &TypeInfo) -> Option<proc_macro2::Ident> {
    use ColumnType::*;
    let kind = match ty.r#type {
        Tiny | Short | Long | Int24 | LongLong | Year | Bit | Float | Double | Decimal | NewDecimal => "Numeric",
        Json => "Json",
        Date => "Date",
        Datetime | Timestamp => "DateTime",
        Time => "Time",
        Enum | Set => "Text",
        String if ty.flags.contains(ColumnFlags::SET) || ty.flags.contains(ColumnFlags::ENUM) => "Text",
        VarChar | VarString | String | TinyBlob | MediumBlob | LongBlob | Blob if ty.is_binary() => "Bytes",
        VarChar | VarString | String | TinyBlob | MediumBlob | LongBlob | Blob => "Text",
        Geometry | Null => return None,
    };
    Some(proc_macro2::Ident::new(kind, proc_macro2::Span::call_site()))
}

#[cfg(test)]
mod tests {
    //! The macros pick a Rust type per column type; the runtime then checks
    //! `<T as Type>::compatible` (with `try_get`). Both tables must agree.

    use tisqlx_core::type_info::BINARY_CHARSET;
    use tisqlx_core::{ColumnFlags, ColumnType, Type, TypeInfo};

    use super::rust_type_for;

    const TEXT: u16 = 46; // utf8mb4_bin

    fn ti(r#type: ColumnType, flags: u16, charset: u16, max_size: u32) -> TypeInfo {
        TypeInfo { r#type, flags: ColumnFlags(flags), charset, max_size }
    }

    fn mapped(ty: &TypeInfo) -> String {
        rust_type_for(ty).unwrap().to_string().replace(' ', "")
    }

    macro_rules! case {
        ($ti:expr, $expected:literal, $rust:ty) => {{
            let ty = $ti;
            assert_eq!(mapped(&ty), $expected, "{}", ty.name());
            assert!(<$rust as Type>::compatible(&ty), "{} not decodable as {}", ty.name(), stringify!($rust));
        }};
    }

    #[test]
    fn mapping_agrees_with_runtime_compatibility() {
        use ColumnType::*;
        let u = ColumnFlags::UNSIGNED;
        case!(ti(Tiny, 0, BINARY_CHARSET, 1), "bool", bool);
        case!(ti(Tiny, 0, BINARY_CHARSET, 4), "i8", i8);
        case!(ti(Tiny, u, BINARY_CHARSET, 3), "u8", u8);
        case!(ti(Short, 0, BINARY_CHARSET, 6), "i16", i16);
        case!(ti(Short, u, BINARY_CHARSET, 5), "u16", u16);
        case!(ti(Int24, 0, BINARY_CHARSET, 9), "i32", i32);
        case!(ti(Long, 0, BINARY_CHARSET, 11), "i32", i32);
        case!(ti(Long, u, BINARY_CHARSET, 10), "u32", u32);
        case!(ti(LongLong, 0, BINARY_CHARSET, 20), "i64", i64);
        case!(ti(LongLong, u, BINARY_CHARSET, 20), "u64", u64);
        case!(ti(Year, u, BINARY_CHARSET, 4), "u16", u16);
        case!(ti(Bit, u, BINARY_CHARSET, 8), "u64", u64);
        case!(ti(Bit, u, BINARY_CHARSET, 1), "bool", bool);
        case!(ti(Float, 0, BINARY_CHARSET, 12), "f32", f32);
        case!(ti(Double, 0, BINARY_CHARSET, 22), "f64", f64);
        case!(ti(VarString, 0, TEXT, 255), "::std::string::String", std::string::String);
        case!(ti(String, 0, TEXT, 10), "::std::string::String", std::string::String);
        case!(ti(Blob, 0, TEXT, 65535), "::std::string::String", std::string::String);
        case!(ti(String, ColumnFlags::ENUM, TEXT, 6), "::std::string::String", std::string::String);
        case!(ti(String, ColumnFlags::SET, TEXT, 6), "::tisqlx::types::Set", tisqlx_core::types::Set);
        case!(ti(VarString, 0, BINARY_CHARSET, 255), "::std::vec::Vec<u8>", Vec<u8>);
        case!(ti(Blob, 0, BINARY_CHARSET, 65535), "::std::vec::Vec<u8>", Vec<u8>);
        case!(ti(String, 0, BINARY_CHARSET, 16), "::tisqlx::types::Uuid", uuid::Uuid);
        case!(ti(String, 0, BINARY_CHARSET, 20), "::std::vec::Vec<u8>", Vec<u8>);
        case!(ti(VarString, 0, BINARY_CHARSET, 16), "::std::vec::Vec<u8>", Vec<u8>);
        case!(ti(Json, 0, BINARY_CHARSET, 0), "::tisqlx::types::JsonValue", serde_json::Value);
        case!(ti(Geometry, 0, BINARY_CHARSET, 0), "::std::vec::Vec<u8>", Vec<u8>);
        assert!(rust_type_for(&ti(Null, 0, BINARY_CHARSET, 0)).is_err());
    }

    #[test]
    fn decimal_mapping() {
        case!(ti(ColumnType::NewDecimal, 0, BINARY_CHARSET, 67), "::tisqlx::types::BigDecimal", bigdecimal::BigDecimal);
        case!(ti(ColumnType::NewDecimal, 0, BINARY_CHARSET, 12), "::tisqlx::types::Decimal", rust_decimal::Decimal);
    }

    #[test]
    fn chrono_mapping() {
        use ColumnType::*;
        use chrono::{DateTime, NaiveDate, NaiveDateTime, TimeDelta, Utc};
        case!(ti(Date, 0, BINARY_CHARSET, 10), "::tisqlx::types::chrono::NaiveDate", NaiveDate);
        case!(ti(Datetime, 0, BINARY_CHARSET, 19), "::tisqlx::types::chrono::NaiveDateTime", NaiveDateTime);
        case!(
            ti(Timestamp, 0, BINARY_CHARSET, 19),
            "::tisqlx::types::chrono::DateTime<::tisqlx::types::chrono::Utc>",
            DateTime<Utc>
        );
        case!(ti(Time, 0, BINARY_CHARSET, 10), "::tisqlx::types::chrono::TimeDelta", TimeDelta);
    }
}
