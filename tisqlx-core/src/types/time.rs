//! `time` crate types. The wire format lives in [`super::temporal`].

use time::{Date, Duration, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

use super::temporal::{DateTimeParts, TimeParts, decode_instant, instant_param, is_date_or_datetime, is_datetime};
use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::row::ValueRef;
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

fn date_parts(d: &Date) -> Result<DateTimeParts, BoxDynError> {
    DateTimeParts::date(d.year(), d.month() as u8, d.day())
}

fn datetime_parts(dt: &PrimitiveDateTime) -> Result<DateTimeParts, BoxDynError> {
    Ok(date_parts(&dt.date())?.with_time(dt.hour(), dt.minute(), dt.second(), dt.microsecond()))
}

fn date(p: &DateTimeParts) -> Result<Date, BoxDynError> {
    Ok(Date::from_calendar_date(p.year as i32, Month::try_from(p.month)?, p.day)?)
}

fn primitive(value: &ValueRef<'_>) -> Result<PrimitiveDateTime, BoxDynError> {
    from_parts(&DateTimeParts::decode(value)?)
}

fn from_parts(p: &DateTimeParts) -> Result<PrimitiveDateTime, BoxDynError> {
    Ok(PrimitiveDateTime::new(date(p)?, Time::from_hms_micro(p.hour, p.minute, p.second, p.micros)?))
}

fn duration(value: &ValueRef<'_>) -> Result<Duration, BoxDynError> {
    Ok(Duration::microseconds(i64::try_from(TimeParts::decode(value)?.as_micros())?))
}

fn encode_duration(d: &Duration, buf: &mut Vec<u8>) -> Result<(), BoxDynError> {
    TimeParts::from_micros(d.whole_microseconds())?.encode(buf);
    Ok(())
}

impl Type for Date {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Date)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        is_date_or_datetime(ty)
    }
}

impl Encode for Date {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        date_parts(self)?.encode_date(buf);
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for Date {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        date(&DateTimeParts::decode(&value)?)
    }
}

impl Type for PrimitiveDateTime {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Datetime)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        is_date_or_datetime(ty)
    }
}

impl Encode for PrimitiveDateTime {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        datetime_parts(self)?.encode_datetime(buf);
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for PrimitiveDateTime {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        primitive(&value)
    }
}

/// An instant, like `chrono::DateTime<Utc>`: converted from and to the session `time_zone`
/// (see `TiConnection::reload_time_zone`). Decoded values carry the session's offset at that instant.
impl Type for OffsetDateTime {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Timestamp)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        is_datetime(ty)
    }
}

impl Encode for OffsetDateTime {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        let utc = self.to_offset(UtcOffset::UTC);
        datetime_parts(&PrimitiveDateTime::new(utc.date(), utc.time()))?.encode_datetime(buf);
        Ok(IsNull::No)
    }

    fn produces(&self) -> TypeInfo {
        instant_param()
    }
}

impl<'r> Decode<'r> for OffsetDateTime {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        let (utc, offset) = decode_instant(&value)?;
        Ok(from_parts(&utc)?.assume_utc().to_offset(UtcOffset::from_whole_seconds(offset)?))
    }
}

/// TIME within a day (00:00:00..24:00:00).
impl Type for Time {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Time)
    }
}

impl Encode for Time {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        encode_duration(&(*self - Time::MIDNIGHT), buf)?;
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for Time {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        let d = duration(&value)?;
        if d.is_negative() || d >= Duration::DAY {
            return Err(format!("TIME value {d} is outside 00:00:00..24:00:00; decode as time::Duration").into());
        }
        Ok(Time::MIDNIGHT + d)
    }
}

/// Any TIME value (it may be negative or exceed 24 hours).
impl Type for Duration {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Time)
    }
}

impl Encode for Duration {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        encode_duration(self, buf)?;
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for Duration {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        duration(&value)
    }
}
