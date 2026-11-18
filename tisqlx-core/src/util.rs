//! SQL quoting helpers.

/// Quote an identifier with backticks.
pub fn quote_ident(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

/// Quote a string literal (assumes `NO_BACKSLASH_ESCAPES` is off, the TiDB default).
pub fn quote_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        match c {
            '\'' => out.push_str("''"),
            '\\' => out.push_str("\\\\"),
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("a`b"), "`a``b`");
        assert_eq!(quote_str("it's \\"), "'it''s \\\\'");
    }
}

/// Lowercase hexadecimal encoding.
pub fn hex(bytes: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;
    let bytes = bytes.as_ref();
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// A non-cryptographic random number (for retry jitter): std's randomly seeded
/// SipHash keys mixed with a per-process counter and the clock.
pub(crate) fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher, RandomState};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(d.as_nanos());
    }
    h.finish()
}

#[cfg(test)]
mod hex_tests {
    #[test]
    fn hex() {
        assert_eq!(super::hex([0x00, 0xab, 0x7f]), "00ab7f");
    }
}
