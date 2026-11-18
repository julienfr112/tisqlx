use syn::{Attribute, LitStr, Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameAll {
    Lower,
    Upper,
    Snake,
    ScreamingSnake,
    Kebab,
    Camel,
    Pascal,
}

impl RenameAll {
    fn parse(s: &LitStr) -> syn::Result<Self> {
        Ok(match s.value().as_str() {
            "lowercase" => Self::Lower,
            "UPPERCASE" => Self::Upper,
            "snake_case" => Self::Snake,
            "SCREAMING_SNAKE_CASE" => Self::ScreamingSnake,
            "kebab-case" => Self::Kebab,
            "camelCase" => Self::Camel,
            "PascalCase" => Self::Pascal,
            _ => return Err(syn::Error::new_spanned(s, "unknown rename_all value")),
        })
    }

    pub fn apply(self, ident: &str) -> String {
        let words = split_words(ident);
        match self {
            Self::Lower => ident.to_lowercase(),
            Self::Upper => ident.to_uppercase(),
            Self::Snake => words.join("_").to_lowercase(),
            Self::ScreamingSnake => words.join("_").to_uppercase(),
            Self::Kebab => words.join("-").to_lowercase(),
            Self::Camel | Self::Pascal => {
                let mut out = String::new();
                for (i, w) in words.iter().enumerate() {
                    let w = w.to_lowercase();
                    if i == 0 && self == Self::Camel {
                        out.push_str(&w);
                    } else {
                        let mut c = w.chars();
                        if let Some(f) = c.next() {
                            out.extend(f.to_uppercase());
                            out.push_str(c.as_str());
                        }
                    }
                }
                out
            }
        }
    }
}

fn split_words(ident: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    for c in ident.trim_start_matches("r#").chars() {
        if c == '_' || c == '-' {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
        } else if c.is_uppercase() && !cur.is_empty() && !cur.chars().last().unwrap().is_uppercase() {
            words.push(std::mem::take(&mut cur));
            cur.push(c);
        } else {
            cur.push(c);
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

#[derive(Default)]
pub struct ContainerAttrs {
    pub rename_all: Option<RenameAll>,
}

#[derive(Default)]
pub struct FieldAttrs {
    pub rename: Option<String>,
    pub default: bool,
    pub flatten: bool,
    pub skip: bool,
    pub json: bool,
    pub try_from: Option<Path>,
}

pub fn container_attrs(attrs: &[Attribute]) -> syn::Result<ContainerAttrs> {
    let mut out = ContainerAttrs::default();
    for attr in attrs.iter().filter(|a| a.path().is_ident("tisqlx")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                out.rename_all = Some(RenameAll::parse(&meta.value()?.parse()?)?);
            } else {
                return Err(meta.error("unknown tisqlx container attribute"));
            }
            Ok(())
        })?;
    }
    Ok(out)
}

/// Variant attributes of `#[derive(Type)]` enums: only `rename`.
pub fn variant_rename(attrs: &[Attribute]) -> syn::Result<Option<String>> {
    let mut rename = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("tisqlx")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                rename = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else {
                Err(meta.error("unknown tisqlx variant attribute (expected `rename`)"))
            }
        })?;
    }
    Ok(rename)
}

pub fn field_attrs(attrs: &[Attribute]) -> syn::Result<FieldAttrs> {
    let mut out = FieldAttrs::default();
    for attr in attrs.iter().filter(|a| a.path().is_ident("tisqlx")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                out.rename = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("default") {
                out.default = true;
            } else if meta.path.is_ident("flatten") {
                out.flatten = true;
            } else if meta.path.is_ident("skip") {
                out.skip = true;
            } else if meta.path.is_ident("json") {
                out.json = true;
            } else if meta.path.is_ident("try_from") {
                out.try_from = Some(meta.value()?.parse::<LitStr>()?.parse()?);
            } else {
                return Err(meta.error("unknown tisqlx field attribute"));
            }
            Ok(())
        })?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename() {
        assert_eq!(RenameAll::Snake.apply("userId"), "user_id");
        assert_eq!(RenameAll::Camel.apply("user_id"), "userId");
        assert_eq!(RenameAll::Pascal.apply("user_id"), "UserId");
        assert_eq!(RenameAll::ScreamingSnake.apply("InProgress"), "IN_PROGRESS");
        assert_eq!(RenameAll::Kebab.apply("InProgress"), "in-progress");
    }
}
