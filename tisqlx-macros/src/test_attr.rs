//! `#[tisqlx::test]`: run an async test against its own throwaway database.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{FnArg, ItemFn, LitStr};

/// Expand `#[tisqlx::test]` on `item`. Arguments, both optional:
/// - `migrations = "dir"`: apply these migrations first;
/// - `fixtures("a.sql", ...)`: then run these SQL files, in order.
///
/// Paths are relative to the crate root (`CARGO_MANIFEST_DIR`).
pub fn expand(args: TokenStream, item: ItemFn) -> syn::Result<TokenStream> {
    let mut migrations: Option<LitStr> = None;
    let mut fixtures: Vec<LitStr> = Vec::new();
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("migrations") {
            if migrations.is_some() {
                return Err(meta.error("`migrations` given twice: a test takes one migrations directory"));
            }
            migrations = Some(meta.value()?.parse()?);
            Ok(())
        } else if meta.path.is_ident("fixtures") {
            let content;
            syn::parenthesized!(content in meta.input);
            let files = content.parse_terminated(<LitStr as syn::parse::Parse>::parse, syn::Token![,])?;
            fixtures.extend(files);
            Ok(())
        } else {
            Err(meta.error("unknown argument; expected `migrations = \"dir\"` or `fixtures(\"file.sql\", ...)`"))
        }
    });
    syn::parse::Parser::parse2(parser, args)?;
    let root = crate::describe::manifest_dir().map_err(|e| syn::Error::new(proc_macro2::Span::call_site(), e))?;
    let mut fixture_paths = Vec::with_capacity(fixtures.len());
    for f in &fixtures {
        let path = root.join(f.value());
        if !path.is_file() {
            return Err(syn::Error::new(f.span(), format!("fixture {} not found", path.display())));
        }
        fixture_paths.push(path.to_string_lossy().into_owned());
    }

    let ItemFn { attrs, vis, sig, block } = item;
    if sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(sig.fn_token, "#[tisqlx::test] functions must be async"));
    }
    let pass_pool = match sig.inputs.len() {
        0 => false,
        1 if matches!(sig.inputs[0], FnArg::Typed(_)) => true,
        _ => {
            return Err(syn::Error::new_spanned(
                &sig.inputs,
                "#[tisqlx::test] functions take no argument or one `tisqlx::Pool`",
            ));
        }
    };
    let name = &sig.ident;
    let output = &sig.output;
    let inputs = &sig.inputs;
    let call = if pass_pool {
        quote!(__inner(__pool))
    } else {
        quote!({
            drop(__pool);
            __inner()
        })
    };
    let migrator = match migrations {
        Some(dir) => quote!(::std::option::Option::Some({
            static __MIGRATOR: ::tisqlx::migrate::Migrator = ::tisqlx::migrate!(#dir);
            &__MIGRATOR
        })),
        None => quote!(::std::option::Option::None),
    };
    Ok(quote! {
        #[::core::prelude::v1::test]
        #(#attrs)*
        #vis fn #name() #output {
            async fn __inner(#inputs) #output #block
            ::tisqlx::testing::run(
                ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#name)),
                #migrator,
                &[#( ::std::include_str!(#fixture_paths) ),*],
                |__pool| #call,
            )
        }
    })
}
