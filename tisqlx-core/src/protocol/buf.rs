//! Low-level MySQL wire encoding helpers.

use crate::error::{Error, Result};

/// A cursor over a received packet payload.
pub(crate) struct Reader<'a> {
    pub(crate) buf: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    fn need(&self, n: usize) -> Result<()> {
        if self.remaining() < n {
            return Err(Error::protocol(format!(
                "unexpected end of packet: need {n} bytes, have {}",
                self.remaining()
            )));
        }
        Ok(())
    }

    pub(crate) fn peek_u8(&self) -> Result<u8> {
        self.need(1)?;
        Ok(self.buf[self.pos])
    }

    pub(crate) fn u8(&mut self) -> Result<u8> {
        self.need(1)?;
        let v = self.buf[self.pos];
        self.pos += 1;
        Ok(v)
    }

    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        self.need(n)?;
        let v = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(v)
    }

    pub(crate) fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }

    pub(crate) fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.bytes(2)?.try_into().unwrap()))
    }

    pub(crate) fn u24(&mut self) -> Result<u32> {
        let b = self.bytes(3)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], 0]))
    }

    pub(crate) fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }

    pub(crate) fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    /// Length-encoded integer. Returns `None` for the NULL marker (0xFB).
    pub(crate) fn lenenc_int_opt(&mut self) -> Result<Option<u64>> {
        Ok(match self.u8()? {
            0xFB => None,
            0xFC => Some(self.u16()? as u64),
            0xFD => Some(self.u24()? as u64),
            0xFE => Some(self.u64()?),
            v => Some(v as u64),
        })
    }

    pub(crate) fn lenenc_int(&mut self) -> Result<u64> {
        self.lenenc_int_opt()?.ok_or_else(|| Error::protocol("unexpected NULL length-encoded integer"))
    }

    pub(crate) fn lenenc_bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.lenenc_int()? as usize;
        self.bytes(len)
    }

    pub(crate) fn lenenc_str(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.lenenc_bytes()?).into_owned())
    }

    pub(crate) fn nul_bytes(&mut self) -> Result<&'a [u8]> {
        let rest = &self.buf[self.pos..];
        let end = rest.iter().position(|&b| b == 0).ok_or_else(|| Error::protocol("missing NUL terminator"))?;
        self.pos += end + 1;
        Ok(&rest[..end])
    }

    pub(crate) fn nul_str(&mut self) -> Result<String> {
        Ok(String::from_utf8_lossy(self.nul_bytes()?).into_owned())
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let v = &self.buf[self.pos..];
        self.pos = self.buf.len();
        v
    }
}

/// Writing helpers for outgoing payloads.
pub(crate) trait BufMutExt {
    fn put_u8(&mut self, v: u8);
    fn put_u16(&mut self, v: u16);
    fn put_u32(&mut self, v: u32);
    fn put_u64(&mut self, v: u64);
    fn put_lenenc_int(&mut self, v: u64);
    fn put_lenenc_bytes(&mut self, v: &[u8]);
    fn put_nul_str(&mut self, v: &str);
}

impl BufMutExt for Vec<u8> {
    fn put_u8(&mut self, v: u8) {
        self.push(v);
    }

    fn put_u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn put_u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn put_u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_le_bytes());
    }

    fn put_lenenc_int(&mut self, v: u64) {
        if v < 0xFB {
            self.push(v as u8);
        } else if v <= 0xFFFF {
            self.push(0xFC);
            self.put_u16(v as u16);
        } else if v <= 0xFF_FFFF {
            self.push(0xFD);
            self.extend_from_slice(&(v as u32).to_le_bytes()[..3]);
        } else {
            self.push(0xFE);
            self.put_u64(v);
        }
    }

    fn put_lenenc_bytes(&mut self, v: &[u8]) {
        self.put_lenenc_int(v.len() as u64);
        self.extend_from_slice(v);
    }

    fn put_nul_str(&mut self, v: &str) {
        self.extend_from_slice(v.as_bytes());
        self.push(0);
    }
}

/// Strip one trailing NUL byte, if present.
pub(crate) fn trim_nul(b: &[u8]) -> &[u8] {
    b.strip_suffix(&[0]).unwrap_or(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lenenc_roundtrip() {
        for v in [0u64, 250, 251, 0xFFFF, 0x10000, 0xFF_FFFF, 0x100_0000, u64::MAX] {
            let mut buf = Vec::new();
            buf.put_lenenc_int(v);
            assert_eq!(Reader::new(&buf).lenenc_int().unwrap(), v, "value {v}");
        }
    }
}
