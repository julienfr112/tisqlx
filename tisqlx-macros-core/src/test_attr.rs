//! `#[tisqlx::test]`: run an async test against its own throwaway database.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{FnArg, ItemFn, LitStr};

/// Expand `#[tisqlx::test]` / `#[tisqlx::test(migrations = "dir")]` on `item`.
pub fn expand(args: TokenStream, item: ItemFn) -> syn::Result<TokenStream> {
    let mut migrations: Option<LitStr> = None;
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("migrations") {
            migrations = Some(meta.value()?.parse()?);
            Ok(())
        } else {
            Err(meta.error("unknown argument; expected `migrations = \"dir\"`"))
        }
    });
    syn::parse::Parser::parse2(parser, args)?;

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
                |__pool| #call,
            )
        }
    })
}
