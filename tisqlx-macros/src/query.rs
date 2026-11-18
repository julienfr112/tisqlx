//! `query!`, `query_as!`, `query_scalar!` and their `_file` variants.

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote, quote_spanned};
use syn::parse::ParseStream;
use syn::spanned::Spanned;
use syn::{Expr, LitStr, Path, Token, Type};

use crate::describe::describe;
use crate::rust_types::{bind_column_kind, rust_type_for};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Anonymous,
    As,
    Scalar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Inline,
    File,
}

#[derive(Debug)]
pub struct QueryInput {
    record: Option<Path>,
    sql: LitStr,
    args: Vec<Arg>,
}

#[derive(Debug)]
struct Arg {
    expr: Expr,
    /// `false` for `value as _`: bound without the compile-time type check.
    checked: bool,
}

impl QueryInput {
    pub fn parser(kind: Kind) -> impl FnOnce(ParseStream<'_>) -> syn::Result<Self> {
        move |input| {
            let record = if kind == Kind::As {
                let p: Path = input.parse()?;
                input.parse::<Token![,]>()?;
                Some(p)
            } else {
                None
            };
            let sql: LitStr = input.parse()?;
            let mut args = Vec::new();
            while !input.is_empty() {
                input.parse::<Token![,]>()?;
                if input.is_empty() {
                    break;
                }
                args.push(match input.parse()? {
                    // `value as _` cannot compile as a bind argument otherwise, so it is free to
                    // mean "don't check this argument's type".
                    Expr::Cast(c) if matches!(*c.ty, Type::Infer(_)) => Arg { expr: *c.expr, checked: false },
                    expr => Arg { expr, checked: true },
                });
            }
            Ok(Self { record, sql, args })
        }
    }
}

/// `5`, `-2.5`: a literal whose type is still an inference variable in the generated check.
fn is_unsuffixed_number(expr: &Expr) -> bool {
    match expr {
        Expr::Lit(l) => match &l.lit {
            syn::Lit::Int(i) => i.suffix().is_empty(),
            syn::Lit::Float(f) => f.suffix().is_empty(),
            _ => false,
        },
        Expr::Unary(u) if matches!(u.op, syn::UnOp::Neg(_)) => is_unsuffixed_number(&u.expr),
        Expr::Paren(p) => is_unsuffixed_number(&p.expr),
        _ => false,
    }
}

enum Nullability {
    Inferred,
    NonNull,
    Nullable,
}

struct ColumnSpec {
    name: String,
    ty: Option<TokenStream>, // None => `_` (infer)
    /// Inferred from the column type: decode with the compatibility check. An
    /// override is the user's assertion and decodes unchecked.
    checked: bool,
}

/// Split a column name `col`, `col!`, `col?`, `col: Type` (or `col!: Type`) into the field name
/// and its overrides.
fn parse_override(name: &str) -> (String, Nullability, Option<String>) {
    let (left, ty) = match name.split_once(':') {
        Some((l, t)) => (l.trim(), Some(t.trim().to_owned())),
        None => (name.trim(), None),
    };
    let (base, null) = if let Some(b) = left.strip_suffix('!') {
        (b, Nullability::NonNull)
    } else if let Some(b) = left.strip_suffix('?') {
        (b, Nullability::Nullable)
    } else {
        (left, Nullability::Inferred)
    };
    // The name of an unaliased expression (`DATE_FORMAT(d, '%H:%i')`, `id + ?`) is not an override.
    if base.is_empty() || !base.chars().all(|c| c.is_alphanumeric() || c == '_') {
        return (name.to_owned(), Nullability::Inferred, None);
    }
    (base.to_owned(), null, ty)
}

fn field_ident(name: &str, span: Span) -> syn::Result<syn::Ident> {
    if matches!(name, "self" | "Self" | "super" | "crate" | "_") {
        return Err(syn::Error::new(span, format!("column name {name:?} cannot be a Rust field name; alias it")));
    }
    // A plain identifier, else a keyword as a raw identifier (`type` => `r#type`).
    syn::parse_str::<syn::Ident>(name)
        .or_else(|_| syn::parse_str::<syn::Ident>(&format!("r#{name}")))
        .map(|mut id| {
            id.set_span(span);
            id
        })
        .map_err(|_| {
            syn::Error::new(
                span,
                format!("column name {name:?} is not a valid Rust identifier; give it an alias, e.g. `... AS my_col`"),
            )
        })
}

pub fn expand(input: QueryInput, kind: Kind, source: Source) -> syn::Result<TokenStream> {
    let span = input.sql.span();
    let (sql, include) = match source {
        Source::Inline => (input.sql.value(), quote!()),
        Source::File => {
            let path = crate::describe::manifest_dir().map_err(|e| syn::Error::new(span, e))?.join(input.sql.value());
            let sql = std::fs::read_to_string(&path)
                .map_err(|e| syn::Error::new(span, format!("failed to read {}: {e}", path.display())))?
                // Same query (and offline file) whether git checked it out with CRLF or LF.
                .replace("\r\n", "\n");
            let p = path.to_string_lossy().into_owned();
            (
                sql,
                quote!(
                    const _: &str = ::std::include_str!(#p);
                ),
            )
        }
    };

    let described = describe(&sql).map_err(|e| syn::Error::new(span, e))?;
    let desc = described.describe;
    // Re-expand when the offline data or `.env` change, or when these variables do.
    let inputs = described.inputs.iter().map(|p| p.to_string_lossy().into_owned());
    let track = quote! {
        #( const _: &str = ::std::include_str!(#inputs); )*
        const _: ::std::option::Option<&str> = ::std::option_env!("DATABASE_URL");
        const _: ::std::option::Option<&str> = ::std::option_env!("TISQLX_OFFLINE");
    };

    if desc.params.len() != input.args.len() {
        return Err(syn::Error::new(
            span,
            format!("expected {} bind argument(s) for this query, got {}", desc.params.len(), input.args.len()),
        ));
    }

    let mut columns = Vec::with_capacity(desc.columns.len());
    for (i, col) in desc.columns.iter().enumerate() {
        let (name, null, ty_override) = parse_override(col.name());
        let base = match ty_override.as_deref() {
            Some("_") if kind == Kind::As => None,
            Some("_") => return Err(syn::Error::new(span, "`col: _` is only allowed in `query_as!`")),
            Some(t) => {
                let t: Type = syn::parse_str(t)
                    .map_err(|e| syn::Error::new(span, format!("invalid type override {t:?}: {e}")))?;
                Some(quote!(#t))
            }
            None => Some(
                rust_type_for(col.type_info()).map_err(|e| syn::Error::new(span, format!("column {name:?}: {e}")))?,
            ),
        };
        let nullable = match null {
            Nullability::NonNull => false,
            Nullability::Nullable => true,
            Nullability::Inferred => !desc.columns[i].type_info.is_not_null(),
        };
        let ty = base.map(|b| if nullable { quote!(::std::option::Option<#b>) } else { b });
        columns.push(ColumnSpec { name, ty, checked: ty_override.is_none() });
    }

    // Evaluate every argument (in order, once) before any generated binding exists,
    // so user expressions cannot see or be shadowed by the macro's names.
    let arg_names: Vec<_> = (0..input.args.len()).map(|i| format_ident!("__arg{}", i)).collect();
    let args = input.args.iter().map(|a| &a.expr);
    // Each argument whose parameter type was inferred must be of a compatible class (see
    // `tisqlx_core::bind_check`). Only references are taken: nothing is evaluated twice.
    let checks = if described.param_types.len() == input.args.len() {
        input
            .args
            .iter()
            .zip(&described.param_types)
            .zip(&arg_names)
            .filter(|((arg, _), _)| arg.checked)
            .filter_map(|((arg, ty), name)| {
                let col = bind_column_kind(ty.as_ref()?)?;
                let check = quote!(::tisqlx::bind_check);
                if is_unsuffixed_number(&arg.expr) {
                    return Some(quote_spanned! {arg.expr.span()=>
                        let () = #check::accept::<#check::col::#col, _>(#check::Class::number());
                    });
                }
                Some(quote_spanned! {arg.expr.span()=>
                    // The `&&` picks `Known` over `Unknown` (autoref dispatch).
                    #[allow(clippy::needless_borrow)]
                    let () = #check::accept::<#check::col::#col, _>((&&#check::Probe::new(#name)).class());
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let checks = (!checks.is_empty()).then(|| {
        quote! {
            #[allow(unused_imports)]
            use ::tisqlx::bind_check::{Known as _, Unknown as _};
            #( #checks )*
        }
    });
    let bind = quote! {
        let __args = match (#( &(#args), )*) {
            (#( #arg_names, )*) => {
                #checks
                let mut __args = ::tisqlx::Arguments::new();
                #( __args.add(#arg_names); )*
                __args
            }
        };
    };
    let get = |i: usize, c: &ColumnSpec| match (&c.ty, c.checked) {
        (Some(ty), true) => quote!(__row.try_get::<#ty, _>(#i)),
        (Some(ty), false) => quote!(__row.try_get_unchecked::<#ty, _>(#i)),
        (None, _) => quote!(__row.try_get_unchecked(#i)),
    };
    let ident = |c: &ColumnSpec| field_ident(&c.name, span);

    let mapper = match kind {
        Kind::Anonymous => {
            let idents = columns.iter().map(ident).collect::<syn::Result<Vec<_>>>()?;
            let tys: Vec<_> =
                columns.iter().map(|c| c.ty.clone().expect("`_` is rejected outside query_as!")).collect();
            let gets: Vec<_> = columns.iter().enumerate().map(|(i, c)| get(i, c)).collect();
            // In its own block: the struct must not be visible to the argument expressions.
            quote! {
                let __map = {
                    #[derive(Debug)]
                    #[allow(non_snake_case)]
                    struct Record { #( pub #idents: #tys, )* }
                    |__row: ::tisqlx::Row| -> ::tisqlx::Result<Record> {
                        ::std::result::Result::Ok(Record { #( #idents: #gets?, )* })
                    }
                };
            }
        }
        Kind::As => {
            let record = input.record.as_ref().unwrap();
            let fields = columns
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let id = ident(c)?;
                    let g = get(i, c);
                    Ok(quote!(#id: #g?))
                })
                .collect::<syn::Result<Vec<_>>>()?;
            // Through an alias: `#record` may be a generic path such as `Page<User>`.
            quote! {
                type __Record = #record;
                let __map = |__row: ::tisqlx::Row| -> ::tisqlx::Result<__Record> {
                    ::std::result::Result::Ok(__Record { #( #fields, )* })
                };
            }
        }
        Kind::Scalar => {
            if columns.len() != 1 {
                return Err(syn::Error::new(
                    span,
                    format!("query_scalar! expects exactly 1 column, query returns {}", columns.len()),
                ));
            }
            let g = get(0, &columns[0]);
            quote! {
                let __map = |__row: ::tisqlx::Row| #g;
            }
        }
    };

    Ok(quote! {{
        #include
        #track
        #[allow(clippy::all)]
        #bind
        #mapper
        ::tisqlx::query_with(#sql, __args).try_map(__map)
    }})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overrides() {
        let (n, null, t) = parse_override("id!: i64");
        assert_eq!(n, "id");
        assert!(matches!(null, Nullability::NonNull));
        assert_eq!(t.as_deref(), Some("i64"));
        assert!(matches!(parse_override("x?").1, Nullability::Nullable));
        // An unaliased expression is not an override, whatever it contains.
        for name in ["DATE_FORMAT(created_at, '%H:%i')", "id + ?", "a != b", "?"] {
            let (n, null, t) = parse_override(name);
            assert_eq!(n, name);
            assert!(matches!(null, Nullability::Inferred));
            assert_eq!(t, None);
        }
        assert_eq!(field_ident("type", Span::call_site()).unwrap().to_string(), "r#type");
        assert!(field_ident("COUNT(*)", Span::call_site()).is_err());
    }
}
