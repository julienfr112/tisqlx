//! `migrate!`: embed a migrations directory at compile time.

use proc_macro2::TokenStream;
use quote::quote;
use syn::LitStr;
use tisqlx_core::migrate::{MigrationType, resolve_dir};

pub fn expand(dir: Option<LitStr>) -> syn::Result<TokenStream> {
    let rel = dir.as_ref().map_or_else(|| "migrations".to_owned(), LitStr::value);
    let span = dir.as_ref().map_or_else(proc_macro2::Span::call_site, LitStr::span);
    let path = crate::describe::manifest_dir().map_err(|e| syn::Error::new(span, e))?.join(&rel);
    let migrations = resolve_dir(&path).map_err(|e| syn::Error::new(span, e.to_string()))?;

    let items = migrations.iter().map(|(m, file)| {
        let version = m.version;
        let description = &*m.description;
        let ty = match m.migration_type {
            MigrationType::Simple => quote!(Simple),
            MigrationType::ReversibleUp => quote!(ReversibleUp),
            MigrationType::ReversibleDown => quote!(ReversibleDown),
        };
        let file = file.to_string_lossy().into_owned();
        let checksum = m.checksum.iter();
        quote! {
            ::tisqlx::migrate::Migration {
                version: #version,
                description: ::std::borrow::Cow::Borrowed(#description),
                migration_type: ::tisqlx::migrate::MigrationType::#ty,
                sql: ::std::borrow::Cow::Borrowed(::std::include_str!(#file)),
                checksum: ::std::borrow::Cow::Borrowed(&[#(#checksum),*]),
            }
        }
    });
    Ok(quote! {{
        const __MIGRATIONS: &[::tisqlx::migrate::Migration] = &[#(#items),*];
        ::tisqlx::migrate::Migrator::from_static(__MIGRATIONS)
    }})
}
