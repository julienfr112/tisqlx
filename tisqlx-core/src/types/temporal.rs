//! Wire format of DATE/DATETIME/TIMESTAMP and TIME values, independent of any
//! date-time crate. `chrono` and `time` impls convert to and from these parts;
//! `std::time::Duration` is implemented here directly.

// Date helpers are only used by the `chrono` and `time` impls.
#![cfg_attr(not(any(feature = "chrono", feature = "time")), allow(dead_code))]

use std::time::Duration;

use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::protocol::buf::Reader;
use crate::row::{RowFormat, ValueRef};
use crate::type_info::{ColumnFlags, ColumnType, TypeInfo};
use crate::types::Type;

pub(crate) fn is_datetime(ty: &TypeInfo) -> bool {
    matches!(ty.r#type, ColumnType::Datetime | ColumnType::Timestamp)
}

pub(crate) fn is_date_or_datetime(ty: &TypeInfo) -> bool {
    ty.r#type == ColumnType::Date || is_datetime(ty)
}

/// The parameter type of instant types (`DateTime<Utc>`, `OffsetDateTime`, ...): sent as a
/// DATETIME, tagged so the connection converts it from UTC to the session zone at execute time.
pub(crate) const fn instant_param() -> TypeInfo {
    let mut ty = TypeInfo::new(ColumnType::Datetime);
    ty.flags = ColumnFlags(ColumnFlags::TIMESTAMP);
    ty
}

/// Whether a bind parameter holds a UTC instant (see [`instant_param`]); a wrapper that does
/// not forward `produces` still declares the TIMESTAMP type of its inner instant type.
pub(crate) fn is_instant_param(ty: &TypeInfo) -> bool {
    ty.r#type == ColumnType::Timestamp
        || (ty.r#type == ColumnType::Datetime && ty.flags.contains(ColumnFlags::TIMESTAMP))
}

/// Decode a DATETIME/TIMESTAMP as an instant: (UTC wall time, session UTC offset in seconds).
pub(crate) fn decode_instant(value: &ValueRef<'_>) -> Result<(DateTimeParts, i32), BoxDynError> {
    value.zone.local_to_utc(DateTimeParts::decode(value)?)
}

/// A calendar date and time of day, as TiDB sends and receives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct DateTimeParts {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
    pub micros: u32,
}

impl DateTimeParts {
    pub fn date(year: i32, month: u8, day: u8) -> Result<Self, BoxDynError> {
        let year = u16::try_from(year)
            .ok()
            .filter(|&y| y <= 9999)
            .ok_or_else(|| format!("year {year} out of range for TiDB"))?;
        Ok(Self { year, month, day, ..Self::default() })
    }

    pub fn with_time(self, hour: u8, minute: u8, second: u8, micros: u32) -> Self {
        Self { hour, minute, second, micros, ..self }
    }

    pub fn encode_date(&self, buf: &mut Vec<u8>) {
        buf.push(4);
        self.put_date(buf);
    }

    pub fn encode_datetime(&self, buf: &mut Vec<u8>) {
        buf.push(if self.micros > 0 { 11 } else { 7 });
        self.put_date(buf);
        buf.extend_from_slice(&[self.hour, self.minute, self.second]);
        if self.micros > 0 {
            buf.extend_from_slice(&self.micros.to_le_bytes());
        }
    }

    fn put_date(&self, buf: &mut Vec<u8>) {
        buf.extend_from_slice(&self.year.to_le_bytes());
        buf.extend_from_slice(&[self.month, self.day]);
    }

    pub fn decode(value: &ValueRef<'_>) -> Result<Self, BoxDynError> {
        let parts = match value.format {
            RowFormat::Binary => Self::decode_binary(value.as_bytes()?)?,
            RowFormat::Text => Self::parse(value.as_str()?)?,
        };
        if parts.year == 0 && parts.month == 0 && parts.day == 0 {
            return Err("zero date '0000-00-00' cannot be represented".into());
        }
        Ok(parts)
    }

    pub fn decode_binary(b: &[u8]) -> Result<Self, BoxDynError> {
        let mut r = Reader::new(b);
        let len = r.u8()?;
        let mut p = Self::default();
        if len >= 4 {
            (p.year, p.month, p.day) = (r.u16()?, r.u8()?, r.u8()?);
        }
        if len >= 7 {
            (p.hour, p.minute, p.second) = (r.u8()?, r.u8()?, r.u8()?);
        }
        if len >= 11 {
            p.micros = r.u32()?;
        }
        if p.micros >= 1_000_000 {
            return Err(format!("invalid fractional seconds {} in a binary date/time", p.micros).into());
        }
        Ok(p)
    }

    /// `YYYY-MM-DD[ HH:MM:SS[.ffffff]]`
    pub fn parse(s: &str) -> Result<Self, BoxDynError> {
        let bad = || format!("invalid date/time {s:?}");
        let (date, time) = s.trim().split_once(' ').unwrap_or((s.trim(), ""));
        let mut d = date.split('-');
        let num = |it: &mut std::str::Split<'_, char>| -> Result<u32, BoxDynError> {
            Ok(it.next().ok_or_else(bad)?.parse::<u32>().map_err(|_| bad())?)
        };
        let mut p =
            Self { year: num(&mut d)? as u16, month: num(&mut d)? as u8, day: num(&mut d)? as u8, ..Self::default() };
        if !time.is_empty() {
            let t = TimeParts::parse(time)?;
            if t.negative || t.micros >= 86_400_000_000 {
                return Err(bad().into());
            }
            let (h, m, s, us) = t.hms();
            p = p.with_time(h as u8, m, s, us);
        }
        Ok(p)
    }
}

/// A TIME value: a signed duration that may exceed 24 hours (up to 838:59:59).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TimeParts {
    pub negative: bool,
    /// Magnitude in microseconds.
    pub micros: u64,
}

impl TimeParts {
    pub fn from_micros(micros: i128) -> Result<Self, BoxDynError> {
        let magnitude = u64::try_from(micros.unsigned_abs()).map_err(|_| "TIME value out of range")?;
        Ok(Self { negative: micros < 0, micros: magnitude })
    }

    pub fn as_micros(&self) -> i128 {
        if self.negative { -(self.micros as i128) } else { self.micros as i128 }
    }

    /// (hours, minutes, seconds, microseconds) of the magnitude.
    fn hms(&self) -> (u64, u8, u8, u32) {
        let secs = self.micros / 1_000_000;
        (secs / 3600, ((secs / 60) % 60) as u8, (secs % 60) as u8, (self.micros % 1_000_000) as u32)
    }

    pub fn encode(&self, buf: &mut Vec<u8>) {
        let (hours, m, s, us) = self.hms();
        buf.push(if us > 0 { 12 } else { 8 });
        buf.push(self.negative as u8);
        buf.extend_from_slice(&((hours / 24) as u32).to_le_bytes());
        buf.extend_from_slice(&[(hours % 24) as u8, m, s]);
        if us > 0 {
            buf.extend_from_slice(&us.to_le_bytes());
        }
    }

    pub fn decode(value: &ValueRef<'_>) -> Result<Self, BoxDynError> {
        match value.format {
            RowFormat::Binary => {
                let mut r = Reader::new(value.as_bytes()?);
                let len = r.u8()?;
                if len == 0 {
                    return Ok(Self::default());
                }
                let negative = r.u8()? != 0;
                let days = r.u32()? as u64;
                let (h, m, s) = (r.u8()? as u64, r.u8()? as u64, r.u8()? as u64);
                let us = if len >= 12 { r.u32()? as u64 } else { 0 };
                Ok(Self { negative, micros: micros_of(days * 24 + h, m, s, us).ok_or("TIME value out of range")? })
            }
            RowFormat::Text => Self::parse(value.as_str()?),
        }
    }

    /// `[-]H:MM:SS[.ffffff]` (hours may exceed 24)
    pub fn parse(s: &str) -> Result<Self, BoxDynError> {
        let bad = || format!("invalid TIME {s:?}");
        let (negative, rest) = s.trim().strip_prefix('-').map_or((false, s.trim()), |r| (true, r));
        let (hms, frac) = rest.split_once('.').unwrap_or((rest, ""));
        let mut parts = hms.split(':').map(str::parse::<u64>);
        let mut next = || parts.next().ok_or_else(bad)?.map_err(|_| bad());
        let (h, m, sec) = (next()?, next()?, next()?);
        if m >= 60 || sec >= 60 || frac.len() > 6 || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad().into());
        }
        let us = if frac.is_empty() { 0 } else { format!("{frac:0<6}").parse::<u64>()? };
        Ok(Self { negative, micros: micros_of(h, m, sec, us).ok_or_else(bad)? })
    }
}

/// `h:m:s.us` in microseconds, or `None` on overflow (from hostile or corrupt input).
fn micros_of(h: u64, m: u64, s: u64, us: u64) -> Option<u64> {
    h.checked_mul(3600)?.checked_add(m * 60 + s)?.checked_mul(1_000_000)?.checked_add(us)
}

// ---- std::time::Duration <-> TIME (non-negative)

impl Type for Duration {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Time)
    }
}

impl Encode for Duration {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        TimeParts::from_micros(self.as_micros() as i128)?.encode(buf);
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for Duration {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        let t = TimeParts::decode(&value)?;
        if t.negative && t.micros > 0 {
            return Err("negative TIME value cannot be a std::time::Duration".into());
        }
        Ok(Duration::from_micros(t.micros))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_datetime_text() {
        let p = DateTimeParts::parse("2026-11-18 09:05:07.25").unwrap();
        assert_eq!(p, DateTimeParts::date(2026, 11, 18).unwrap().with_time(9, 5, 7, 250_000));
        assert_eq!(DateTimeParts::parse("2026-01-02").unwrap(), DateTimeParts::date(2026, 1, 2).unwrap());
        assert!(DateTimeParts::parse("2026-01").is_err());
        assert!(DateTimeParts::parse("2026-01-02 25:00:00").is_err());
        assert!(DateTimeParts::date(10_000, 1, 1).is_err());
    }

    #[test]
    fn time_round_trip() {
        for (text, micros) in [("838:59:59", 3_020_399_000_000i128), ("-01:02:03.5", -3_723_500_000), ("00:00:00", 0)] {
            let t = TimeParts::parse(text).unwrap();
            assert_eq!(t.as_micros(), micros, "{text}");
            assert_eq!(TimeParts::from_micros(micros).unwrap(), t);
            let mut buf = Vec::new();
            t.encode(&mut buf);
            assert_eq!(buf[0] as usize, buf.len() - 1, "length prefix");
        }
        assert!(TimeParts::parse("1:60:00").is_err());
        assert!(TimeParts::parse("1:00:00.1234567").is_err());
        // Overflowed (debug: panicked; release: wrapped to a wrong value) before.
        assert!(TimeParts::parse("99999999999999:00:00").is_err());
    }
}
