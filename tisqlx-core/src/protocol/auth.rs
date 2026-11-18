//! Authentication plugins supported by TiDB.

use sha1::{Digest, Sha1};
use sha2::Sha256;

use crate::error::{Error, Result};
use crate::protocol::buf::BufMutExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthPlugin {
    NativePassword,
    CachingSha2,
    ClearPassword,
}

impl AuthPlugin {
    pub(crate) fn from_name(name: &str) -> Result<Self> {
        match name {
            "mysql_native_password" => Ok(Self::NativePassword),
            "caching_sha2_password" => Ok(Self::CachingSha2),
            "mysql_clear_password" => Ok(Self::ClearPassword),
            other => Err(Error::protocol(format!("unsupported authentication plugin: {other}"))),
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::NativePassword => "mysql_native_password",
            Self::CachingSha2 => "caching_sha2_password",
            Self::ClearPassword => "mysql_clear_password",
        }
    }

    pub(crate) fn scramble(self, password: &str, nonce: &[u8]) -> Vec<u8> {
        match self {
            Self::NativePassword | Self::CachingSha2 if password.is_empty() => Vec::new(),
            Self::NativePassword => scramble_sha1(password, nonce),
            Self::CachingSha2 => scramble_sha256(password, nonce),
            Self::ClearPassword => to_asciz(password),
        }
    }
}

/// SHA1(password) XOR SHA1(nonce + SHA1(SHA1(password)))
fn scramble_sha1(password: &str, nonce: &[u8]) -> Vec<u8> {
    let stage1 = Sha1::digest(password.as_bytes());
    let stage2 = Sha1::digest(stage1);
    let mut h = Sha1::new();
    h.update(nonce);
    h.update(stage2);
    let pad = h.finalize();
    stage1.iter().zip(pad.iter()).map(|(a, b)| a ^ b).collect()
}

/// SHA256(password) XOR SHA256(SHA256(SHA256(password)) + nonce)
fn scramble_sha256(password: &str, nonce: &[u8]) -> Vec<u8> {
    let stage1 = Sha256::digest(password.as_bytes());
    let stage2 = Sha256::digest(stage1);
    let mut h = Sha256::new();
    h.update(stage2);
    h.update(nonce);
    let pad = h.finalize();
    stage1.iter().zip(pad.iter()).map(|(a, b)| a ^ b).collect()
}

pub(crate) fn to_asciz(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 1);
    v.put_nul_str(s);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_scramble_invertible() {
        let nonce = b"0123456789abcdefghij";
        let s = scramble_sha256("pw", nonce);
        let stage1 = Sha256::digest(b"pw");
        let stage2 = Sha256::digest(stage1);
        let mut h = Sha256::new();
        h.update(stage2);
        h.update(nonce);
        let pad = h.finalize();
        let back: Vec<u8> = s.iter().zip(pad.iter()).map(|(a, b)| a ^ b).collect();
        assert_eq!(back, stage1.to_vec());
    }
}
