use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields};

use super::attrs::{container_attrs, variant_rename};

/// `#[derive(Type)]`:
/// - on a single-field tuple struct: transparent newtype over the inner type;
/// - on a fieldless enum: stored as a string (`VARCHAR`/`ENUM`), with
///   `#[tisqlx(rename_all = "...")]` and per-variant `#[tisqlx(rename = "...")]`.
pub fn expand_derive_type(input: &DeriveInput) -> syn::Result<TokenStream> {
    let ident = &input.ident;
    let container = container_attrs(&input.attrs)?;
    match &input.data {
        Data::Struct(s) => {
            let Fields::Unnamed(f) = &s.fields else {
                return Err(syn::Error::new_spanned(
                    input,
                    "Type can be derived for single-field tuple structs or fieldless enums",
                ));
            };
            if f.unnamed.len() != 1 {
                return Err(syn::Error::new_spanned(
                    input,
                    "Type can only be derived for a newtype (one field) or a fieldless enum",
                ));
            }
            if container.rename_all.is_some() {
                return Err(syn::Error::new_spanned(
                    ident,
                    "`rename_all` has no effect on a newtype: it renames the variants of an enum",
                ));
            }
            let inner = &f.unnamed[0].ty;
            let bounded = |bound: TokenStream| {
                let mut g = input.generics.clone();
                g.make_where_clause().predicates.push(syn::parse_quote!(#inner: #bound));
                g
            };
            let type_generics = bounded(quote!(::tisqlx::Type));
            let (t_impl, t_ty, t_where) = type_generics.split_for_impl();
            let encode_generics = bounded(quote!(::tisqlx::Encode));
            let (e_impl, e_ty, e_where) = encode_generics.split_for_impl();
            // Under a binder, the bound is not "trivial" (checked eagerly) for a non-generic
            // newtype: over a type tisqlx cannot classify, the impl just doesn't apply, and the
            // newtype is not checked either (instead of the derive failing to compile).
            let mut class_generics = input.generics.clone();
            class_generics
                .make_where_clause()
                .predicates
                .push(syn::parse_quote!(for<'__tisqlx> #inner: ::tisqlx::bind_check::BindClass));
            let (c_impl, c_ty, c_where) = class_generics.split_for_impl();
            // Decode borrows from the row: reuse the newtype's lifetime, else add `'r`.
            let mut decode_generics = input.generics.clone();
            let lt = match input.generics.lifetimes().next() {
                Some(existing) => existing.lifetime.clone(),
                None => {
                    let lt: syn::Lifetime = syn::parse_quote!('r);
                    decode_generics.params.insert(0, syn::GenericParam::Lifetime(syn::LifetimeParam::new(lt.clone())));
                    lt
                }
            };
            decode_generics.make_where_clause().predicates.push(syn::parse_quote!(#inner: ::tisqlx::Decode<#lt>));
            let (d_impl, _, d_where) = decode_generics.split_for_impl();
            let (_, d_ty, _) = input.generics.split_for_impl();
            Ok(quote! {
                #[automatically_derived]
                impl #t_impl ::tisqlx::Type for #ident #t_ty #t_where {
                    fn type_info() -> ::tisqlx::TypeInfo { <#inner as ::tisqlx::Type>::type_info() }
                    fn compatible(ty: &::tisqlx::TypeInfo) -> bool { <#inner as ::tisqlx::Type>::compatible(ty) }
                }
                #[automatically_derived]
                impl #e_impl ::tisqlx::Encode for #ident #e_ty #e_where {
                    fn encode(&self, buf: &mut ::std::vec::Vec<u8>) -> ::std::result::Result<::tisqlx::IsNull, ::tisqlx::error::BoxDynError> {
                        ::tisqlx::Encode::encode(&self.0, buf)
                    }
                    fn produces(&self) -> ::tisqlx::TypeInfo { ::tisqlx::Encode::produces(&self.0) }
                }
                #[automatically_derived]
                impl #c_impl ::tisqlx::bind_check::BindClass for #ident #c_ty #c_where {
                    type Class = <#inner as ::tisqlx::bind_check::BindClass>::Class;
                }
                #[automatically_derived]
                impl #d_impl ::tisqlx::Decode<#lt> for #ident #d_ty #d_where {
                    fn decode(value: ::tisqlx::ValueRef<#lt>) -> ::std::result::Result<Self, ::tisqlx::error::BoxDynError> {
                        <#inner as ::tisqlx::Decode<#lt>>::decode(value).map(Self)
                    }
                }
            })
        }
        Data::Enum(e) => {
            if !input.generics.params.is_empty() {
                return Err(syn::Error::new_spanned(&input.generics, "Type cannot be derived for a generic enum"));
            }
            let mut to_str = Vec::new();
            let mut from_str = Vec::new();
            for v in &e.variants {
                if !matches!(v.fields, Fields::Unit) {
                    return Err(syn::Error::new_spanned(v, "Type enums must be fieldless"));
                }
                let vid = &v.ident;
                let name = variant_rename(&v.attrs)?
                    .unwrap_or_else(|| container.rename_all.map_or(vid.to_string(), |r| r.apply(&vid.to_string())));
                to_str.push(quote!(Self::#vid => #name));
                from_str.push(quote!(#name => Self::#vid));
            }
            Ok(quote! {
                #[automatically_derived]
                impl ::tisqlx::Type for #ident {
                    fn type_info() -> ::tisqlx::TypeInfo { <str as ::tisqlx::Type>::type_info() }
                    fn compatible(ty: &::tisqlx::TypeInfo) -> bool { <str as ::tisqlx::Type>::compatible(ty) }
                }
                #[automatically_derived]
                impl ::tisqlx::Encode for #ident {
                    fn encode(&self, buf: &mut ::std::vec::Vec<u8>) -> ::std::result::Result<::tisqlx::IsNull, ::tisqlx::error::BoxDynError> {
                        let s: &str = match self { #(#to_str,)* };
                        ::tisqlx::Encode::encode(s, buf)
                    }
                }
                #[automatically_derived]
                impl ::tisqlx::bind_check::BindClass for #ident {
                    type Class = ::tisqlx::bind_check::class::Text;
                }
                #[automatically_derived]
                impl<'r> ::tisqlx::Decode<'r> for #ident {
                    fn decode(value: ::tisqlx::ValueRef<'r>) -> ::std::result::Result<Self, ::tisqlx::error::BoxDynError> {
                        let s = value.as_str()?;
                        ::std::result::Result::Ok(match s {
                            #(#from_str,)*
                            other => return ::std::result::Result::Err(::std::format!("invalid value {:?} for enum {}", other, ::std::stringify!(#ident)).into()),
                        })
                    }
                }
            })
        }
        Data::Union(_) => Err(syn::Error::new_spanned(input, "Type cannot be derived for unions")),
    }
}
