use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields};

use super::attrs::{container_attrs, field_attrs};

pub fn expand_derive_from_row(input: &DeriveInput) -> syn::Result<TokenStream> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(input, "FromRow can only be derived for structs"));
    };
    let container = container_attrs(&input.attrs)?;
    let ident = &input.ident;

    // The row lifetime: the struct's own first lifetime (`struct User<'a> { name: &'a str }`
    // borrows from the row), else a new `'r`.
    let mut generics = input.generics.clone();
    let lt = match input.generics.lifetimes().next() {
        Some(existing) => existing.lifetime.clone(),
        None => {
            let lt: syn::Lifetime = syn::parse_quote!('r);
            generics.params.insert(0, syn::GenericParam::Lifetime(syn::LifetimeParam::new(lt.clone())));
            lt
        }
    };
    let (impl_generics, _, _) = generics.split_for_impl();
    let (_, ty_generics, where_clause) = input.generics.split_for_impl();
    let mut predicates: Vec<TokenStream> =
        where_clause.map(|w| w.predicates.iter().map(|p| quote!(#p)).collect()).unwrap_or_default();

    let build = |fields: &Fields, predicates: &mut Vec<TokenStream>| -> syn::Result<Vec<TokenStream>> {
        let mut out = Vec::new();
        for (i, field) in fields.iter().enumerate() {
            let attrs = field_attrs(&field.attrs)?;
            let ty = &field.ty;
            let column = match (&field.ident, &attrs.rename) {
                (_, Some(r)) => r.clone(),
                (Some(id), None) => {
                    let name = id.to_string().trim_start_matches("r#").to_owned();
                    container.rename_all.map_or(name.clone(), |r| r.apply(&name))
                }
                (None, None) => String::new(),
            };
            let index = if field.ident.is_some() { quote!(#column) } else { quote!(#i) };
            let combined = [
                ("skip", attrs.skip),
                ("flatten", attrs.flatten),
                ("try_from", attrs.try_from.is_some()),
                ("json", attrs.json),
            ];
            let kinds: Vec<_> = combined.iter().filter(|(_, on)| *on).map(|(name, _)| *name).collect();
            if kinds.len() > 1 || (attrs.default && (attrs.skip || attrs.flatten || attrs.try_from.is_some())) {
                return Err(syn::Error::new_spanned(
                    field,
                    "unsupported combination of #[tisqlx(...)] attributes: use at most one of skip, flatten, \
                     try_from, json, and `default` only alone or with json",
                ));
            }
            let expr = if attrs.skip {
                quote!(::std::default::Default::default())
            } else if attrs.flatten {
                predicates.push(quote!(#ty: ::tisqlx::FromRow<#lt>));
                quote!(<#ty as ::tisqlx::FromRow<#lt>>::from_row(__row)?)
            } else if let Some(src) = &attrs.try_from {
                predicates.push(quote!(#src: ::tisqlx::Decode<#lt> + ::tisqlx::Type));
                quote!({
                    let v: #src = __row.try_get(#index)?;
                    <#ty as ::std::convert::TryFrom<#src>>::try_from(v)
                        .map_err(|e| ::tisqlx::Error::ColumnDecode { index: ::std::string::ToString::to_string(&#index), source: ::std::convert::Into::into(e) })?
                })
            } else {
                let get = if attrs.json {
                    predicates.push(quote!(#ty: ::tisqlx::__rt::serde::de::DeserializeOwned));
                    quote!(__row.try_get::<::tisqlx::types::Json<#ty>, _>(#index).map(|v| v.0))
                } else {
                    predicates.push(quote!(#ty: ::tisqlx::Decode<#lt> + ::tisqlx::Type));
                    quote!(__row.try_get::<#ty, _>(#index))
                };
                if attrs.default {
                    quote!(match #get {
                        Ok(v) => v,
                        Err(::tisqlx::Error::ColumnNotFound(_)) => ::std::default::Default::default(),
                        Err(e) => return Err(e),
                    })
                } else {
                    quote!(#get?)
                }
            };
            out.push(match &field.ident {
                Some(id) => quote!(#id: #expr),
                None => expr,
            });
        }
        Ok(out)
    };

    let body = match &data.fields {
        Fields::Named(_) => {
            let fields = build(&data.fields, &mut predicates)?;
            quote!(#ident { #(#fields,)* })
        }
        Fields::Unnamed(_) => {
            let fields = build(&data.fields, &mut predicates)?;
            quote!(#ident ( #(#fields,)* ))
        }
        Fields::Unit => quote!(#ident),
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::tisqlx::FromRow<#lt> for #ident #ty_generics where #(#predicates,)* {
            fn from_row(__row: &#lt ::tisqlx::Row) -> ::tisqlx::Result<Self> {
                ::std::result::Result::Ok(#body)
            }
        }
    })
}
