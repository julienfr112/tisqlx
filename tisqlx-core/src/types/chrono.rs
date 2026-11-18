//! `chrono` types. The wire format lives in [`super::temporal`].

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeDelta, Timelike, Utc};

use super::temporal::{DateTimeParts, TimeParts, is_date_or_datetime, is_datetime};
use crate::decode::Decode;
use crate::encode::{Encode, IsNull};
use crate::error::BoxDynError;
use crate::row::ValueRef;
use crate::type_info::{ColumnType, TypeInfo};
use crate::types::Type;

fn date_parts(d: &NaiveDate) -> Result<DateTimeParts, BoxDynError> {
    DateTimeParts::date(d.year(), d.month() as u8, d.day() as u8)
}

fn datetime_parts(dt: &NaiveDateTime) -> Result<DateTimeParts, BoxDynError> {
    // `nanosecond()` exceeds 1e9 during a leap second; TiDB has none.
    let micros = (dt.nanosecond() % 1_000_000_000) / 1000;
    Ok(date_parts(&dt.date())?.with_time(dt.hour() as u8, dt.minute() as u8, dt.second() as u8, micros))
}

fn naive_date(p: &DateTimeParts) -> Result<NaiveDate, BoxDynError> {
    NaiveDate::from_ymd_opt(p.year as i32, p.month as u32, p.day as u32)
        .ok_or_else(|| format!("invalid date {:04}-{:02}-{:02}", p.year, p.month, p.day).into())
}

fn naive_datetime(value: &ValueRef<'_>) -> Result<NaiveDateTime, BoxDynError> {
    let p = DateTimeParts::decode(value)?;
    let time = NaiveTime::from_hms_micro_opt(p.hour as u32, p.minute as u32, p.second as u32, p.micros)
        .ok_or("invalid time of day")?;
    Ok(naive_date(&p)?.and_time(time))
}

fn time_delta(value: &ValueRef<'_>) -> Result<TimeDelta, BoxDynError> {
    let micros = TimeParts::decode(value)?.as_micros();
    Ok(TimeDelta::microseconds(i64::try_from(micros)?))
}

fn encode_time_delta(d: &TimeDelta, buf: &mut Vec<u8>) -> Result<(), BoxDynError> {
    let micros = d.num_microseconds().ok_or("TIME value out of range")?;
    TimeParts::from_micros(micros as i128)?.encode(buf);
    Ok(())
}

// ---- NaiveDate

impl Type for NaiveDate {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Date)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        is_date_or_datetime(ty)
    }
}

impl Encode for NaiveDate {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        date_parts(self)?.encode_date(buf);
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for NaiveDate {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        naive_date(&DateTimeParts::decode(&value)?)
    }
}

// ---- NaiveDateTime

impl Type for NaiveDateTime {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Datetime)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        is_date_or_datetime(ty)
    }
}

impl Encode for NaiveDateTime {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        datetime_parts(self)?.encode_datetime(buf);
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for NaiveDateTime {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        naive_datetime(&value)
    }
}

// ---- DateTime<Utc>
//
// TIMESTAMP values travel in the *session* time zone in both directions, so these
// impls are only correct while it is UTC: the default `timezone` connect option
// (`+00:00`). With another session zone, use `NaiveDateTime` and convert yourself.

impl Type for DateTime<Utc> {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Timestamp)
    }

    fn compatible(ty: &TypeInfo) -> bool {
        is_datetime(ty)
    }
}

impl Encode for DateTime<Utc> {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        datetime_parts(&self.naive_utc())?.encode_datetime(buf);
        Ok(IsNull::No)
    }

    fn produces(&self) -> TypeInfo {
        // A plain datetime literal; the server interprets it in the session zone (UTC).
        TypeInfo::new(ColumnType::Datetime)
    }
}

impl<'r> Decode<'r> for DateTime<Utc> {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        Ok(naive_datetime(&value)?.and_utc())
    }
}

// ---- NaiveTime (00:00..24:00) / TimeDelta (any TIME, which may be negative or exceed 24h)

impl Type for NaiveTime {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Time)
    }
}

impl Encode for NaiveTime {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        encode_time_delta(&(*self - NaiveTime::MIN), buf)?;
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for NaiveTime {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        let d = time_delta(&value)?;
        if d < TimeDelta::zero() || d >= TimeDelta::days(1) {
            return Err(format!("TIME value {d} is outside 00:00:00..24:00:00; decode as chrono::TimeDelta").into());
        }
        Ok(NaiveTime::MIN + d)
    }
}

impl Type for TimeDelta {
    fn type_info() -> TypeInfo {
        TypeInfo::new(ColumnType::Time)
    }
}

impl Encode for TimeDelta {
    fn encode(&self, buf: &mut Vec<u8>) -> Result<IsNull, BoxDynError> {
        encode_time_delta(self, buf)?;
        Ok(IsNull::No)
    }
}

impl<'r> Decode<'r> for TimeDelta {
    fn decode(value: ValueRef<'r>) -> Result<Self, BoxDynError> {
        time_delta(&value)
    }
}
