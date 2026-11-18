//! Hostile-input tests: every decoder of server data must return an error, never panic, on
//! arbitrary bytes. A deterministic stand-in for `cargo fuzz` (which needs nightly): random
//! inputs plus truncations and bit flips of valid packets.
//!
//! `TISQLX_FUZZ_ITERS=1000000 cargo test -p tisqlx-core --all-features robustness` runs longer.

use crate::protocol::row::{decode_binary_row, decode_text_row};
use crate::protocol::{ColumnDefinition, Handshake, OkPacket, PrepareOk, decode_eof_status, decode_err};
use crate::row::{RowFormat, ValueRef};
use crate::type_info::{BINARY_CHARSET, ColumnFlags, ColumnType, TypeInfo};
use crate::tz::SessionZone;

/// xorshift64*: deterministic, so a failure reproduces.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let len = self.below(max + 1);
        let mut v: Vec<u8> = (0..len).map(|_| self.next() as u8).collect();
        // Length prefixes are where decoders go wrong: make the special ones common.
        for b in v.iter_mut() {
            if self.below(8) == 0 {
                *b = [0x00, 0xFB, 0xFC, 0xFD, 0xFE, 0xFF][self.below(6)];
            }
        }
        v
    }

    /// A valid sample, truncated and/or with a few bytes changed.
    fn mutate(&mut self, valid: &[u8]) -> Vec<u8> {
        let mut v = valid[..self.below(valid.len() + 1)].to_vec();
        for _ in 0..self.below(4) {
            if !v.is_empty() {
                let i = self.below(v.len());
                v[i] = self.next() as u8;
            }
        }
        v
    }
}

fn iterations() -> usize {
    std::env::var("TISQLX_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(20_000)
}

fn lenenc_str(out: &mut Vec<u8>, s: &[u8]) {
    out.push(s.len() as u8);
    out.extend_from_slice(s);
}

fn column_definition(r#type: u8) -> Vec<u8> {
    let mut p = Vec::new();
    for s in [&b"def"[..], b"db", b"t", b"t", b"c", b"c"] {
        lenenc_str(&mut p, s);
    }
    p.push(0x0C);
    p.extend_from_slice(&46u16.to_le_bytes());
    p.extend_from_slice(&20u32.to_le_bytes());
    p.push(r#type);
    p.extend_from_slice(&0u16.to_le_bytes());
    p.push(0);
    p.extend_from_slice(&[0, 0]);
    p
}

fn handshake() -> Vec<u8> {
    let mut p = vec![10];
    p.extend_from_slice(b"8.0.11-TiDB-v8.5.8\0");
    p.extend_from_slice(&7u32.to_le_bytes());
    p.extend_from_slice(b"abcdefgh\0");
    p.extend_from_slice(&0xF7FFu16.to_le_bytes());
    p.push(46);
    p.extend_from_slice(&2u16.to_le_bytes());
    p.extend_from_slice(&0x81FFu16.to_le_bytes());
    p.push(21);
    p.extend_from_slice(&[0; 10]);
    p.extend_from_slice(b"ijklmnopqrst\0");
    p.extend_from_slice(b"mysql_native_password\0");
    p
}

#[test]
fn packet_decoders_never_panic() {
    let mut rng = Rng(0x7153_514C);
    let samples: Vec<Vec<u8>> = vec![
        handshake(),
        column_definition(0x08),
        vec![0x00, 0x01, 0x02, 0x02, 0x00, 0x00, 0x00],
        vec![0x00, 0x05, 0x00, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00],
        vec![0xFF, 0x48, 0x04, b'#', b'4', b'2', b'0', b'0', b'0', b'b', b'a', b'd'],
    ];
    for _ in 0..iterations() {
        let input = if rng.below(2) == 0 {
            rng.bytes(64)
        } else {
            let sample = &samples[rng.below(samples.len())];
            rng.mutate(sample)
        };
        let _ = Handshake::decode(&input);
        let _ = OkPacket::decode(&input);
        let _ = PrepareOk::decode(&input);
        let _ = ColumnDefinition::decode(&input);
        let _ = decode_eof_status(&input);
        let _ = decode_err(&input);
    }
}

#[test]
fn row_and_value_decoders_never_panic() {
    let mut rng = Rng(0x00DE_C0DE);
    let types: Vec<ColumnType> = (0..=255u8).filter_map(ColumnType::try_from_u8).collect();
    let column = |r#type: ColumnType, unsigned: bool, binary: bool| {
        let mut c = ColumnDefinition::decode(&column_definition(0x08)).unwrap();
        c.type_info.r#type = r#type;
        c.type_info.flags = ColumnFlags(if unsigned { ColumnFlags::UNSIGNED } else { 0 });
        c.type_info.charset = if binary { BINARY_CHARSET } else { 46 };
        c
    };
    let zones = [SessionZone::Utc, SessionZone::resolve("+05:30", None), SessionZone::resolve("Europe/Paris", None)];
    for _ in 0..iterations() {
        let columns: Vec<ColumnDefinition> = (0..rng.below(5))
            .map(|_| column(types[rng.below(types.len())], rng.below(2) == 0, rng.below(2) == 0))
            .collect();
        let payload = rng.bytes(48);
        let _ = decode_text_row(&payload, columns.len());
        let _ = decode_binary_row(&payload, &columns);

        // Every value decoder, on arbitrary bytes, for every column type, in both formats.
        let ti = TypeInfo {
            r#type: types[rng.below(types.len())],
            flags: ColumnFlags(if rng.below(2) == 0 { ColumnFlags::UNSIGNED } else { 0 }),
            charset: if rng.below(2) == 0 { BINARY_CHARSET } else { 46 },
            max_size: rng.below(70) as u32,
        };
        let bytes = rng.bytes(24);
        let value = ValueRef {
            bytes: Some(&bytes),
            format: if rng.below(2) == 0 { RowFormat::Binary } else { RowFormat::Text },
            type_info: &ti,
            zone: &zones[rng.below(zones.len())],
        };
        decode_all(value);
    }
}

fn decode_all(v: ValueRef<'_>) {
    use crate::decode::Decode;
    macro_rules! each {
        ($($t:ty),* $(,)?) => {$( let _ = <$t as Decode>::decode(v); )*};
    }
    each!(
        bool,
        i8,
        i16,
        i32,
        i64,
        i128,
        u8,
        u16,
        u32,
        u64,
        u128,
        f32,
        f64,
        String,
        &str,
        Vec<u8>,
        &[u8],
        [u8; 4],
        serde_json::Value,
        crate::types::Json<Vec<i32>>,
        &serde_json::value::RawValue,
        crate::types::Set,
        crate::types::Vector,
        std::time::Duration,
    );
    #[cfg(feature = "decimal")]
    each!(rust_decimal::Decimal);
    #[cfg(feature = "bigdecimal")]
    each!(bigdecimal::BigDecimal);
    #[cfg(feature = "uuid")]
    each!(uuid::Uuid, uuid::fmt::Hyphenated);
    #[cfg(feature = "chrono")]
    each!(
        chrono::NaiveDate,
        chrono::NaiveDateTime,
        chrono::NaiveTime,
        chrono::TimeDelta,
        chrono::DateTime<chrono::Utc>,
        chrono::DateTime<chrono::FixedOffset>,
        chrono::DateTime<chrono::Local>,
    );
    #[cfg(feature = "time")]
    each!(time::Date, time::PrimitiveDateTime, time::OffsetDateTime, time::Time, time::Duration);
}
