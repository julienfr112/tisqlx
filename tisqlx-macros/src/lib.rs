//! Procedural macros for tisqlx. Use them through the `tisqlx` crate.

use proc_macro::TokenStream;
use syn::parse::Parser;
use tisqlx_macros_core::query::{Kind, QueryInput, Source, expand};

fn run(input: TokenStream, kind: Kind, source: Source) -> TokenStream {
    let parsed = match QueryInput::parser(kind).parse(input) {
        Ok(p) => p,
        Err(e) => return e.to_compile_error().into(),
    };
    expand(parsed, kind, source).unwrap_or_else(|e| e.to_compile_error()).into()
}

#[proc_macro]
pub fn query(input: TokenStream) -> TokenStream {
    run(input, Kind::Anonymous, Source::Inline)
}

#[proc_macro]
pub fn query_as(input: TokenStream) -> TokenStream {
    run(input, Kind::As, Source::Inline)
}

#[proc_macro]
pub fn query_scalar(input: TokenStream) -> TokenStream {
    run(input, Kind::Scalar, Source::Inline)
}

#[proc_macro]
pub fn query_file(input: TokenStream) -> TokenStream {
    run(input, Kind::Anonymous, Source::File)
}

#[proc_macro]
pub fn query_file_as(input: TokenStream) -> TokenStream {
    run(input, Kind::As, Source::File)
}

#[proc_macro]
pub fn query_file_scalar(input: TokenStream) -> TokenStream {
    run(input, Kind::Scalar, Source::File)
}

#[proc_macro]
pub fn migrate(input: TokenStream) -> TokenStream {
    let dir = if input.is_empty() {
        None
    } else {
        match syn::parse::<syn::LitStr>(input) {
            Ok(l) => Some(l),
            Err(e) => return e.to_compile_error().into(),
        }
    };
    tisqlx_macros_core::migrate::expand(dir).unwrap_or_else(|e| e.to_compile_error()).into()
}

#[proc_macro_derive(FromRow, attributes(tisqlx))]
pub fn derive_from_row(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    tisqlx_macros_core::derives::expand_derive_from_row(&input).unwrap_or_else(|e| e.to_compile_error()).into()
}

#[proc_macro_derive(Type, attributes(tisqlx))]
pub fn derive_type(input: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(input as syn::DeriveInput);
    tisqlx_macros_core::derives::expand_derive_type(&input).unwrap_or_else(|e| e.to_compile_error()).into()
}

/// Run an async test against its own database, created on the server named by
/// `DATABASE_URL` and dropped afterwards. The test may take a `tisqlx::Pool` on it.
///
/// ```ignore
/// #[tisqlx::test(migrations = "migrations")]
/// async fn creates_a_user(pool: tisqlx::Pool) {
///     tisqlx::query!("INSERT INTO users (name) VALUES (?)", "a").execute(&pool).await.unwrap();
/// }
/// ```
#[proc_macro_attribute]
pub fn test(args: TokenStream, input: TokenStream) -> TokenStream {
    let item = match syn::parse::<syn::ItemFn>(input) {
        Ok(i) => i,
        Err(e) => return e.to_compile_error().into(),
    };
    tisqlx_macros_core::test_attr::expand(args.into(), item).unwrap_or_else(|e| e.to_compile_error()).into()
}
