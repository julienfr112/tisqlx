//! The session time zone, for instant types (`DateTime<Utc>`, `DateTime<Local>`, ...).
//!
//! TiDB sends and receives DATETIME and TIMESTAMP values as wall-clock times in the session
//! `time_zone`. Instant types are converted between UTC and that zone on the client: when a
//! statement is executed (bind parameters) and when a value is decoded. The zone is resolved
//! when the connection opens; with the default `+00:00` both conversions are no-ops.
//!
//! Wall times are mapped to instants exactly as TiDB does, so a decoded value is the instant
//! TiDB stored. That is lossy around daylight-saving changes, on the server as much as here:
//! - a wall time that occurs twice (clocks going back) is read as one of the two instants, the
//!   one Go's `time.Date` picks (in Europe/Paris the later, in America/New_York the earlier);
//! - a wall time that does not exist (clocks going forward) is read as the transition instant.
//!
//! Use a UTC session (the default) to store instants losslessly.

use std::sync::Arc;

use crate::error::BoxDynError;
use crate::types::temporal::DateTimeParts;

/// The time zone wall-clock values are interpreted in on one connection.
#[derive(Debug, Clone, Default)]
pub(crate) enum SessionZone {
    #[default]
    Utc,
    Zone(jiff::tz::TimeZone),
    /// Not usable on the client (e.g. no time zone database); instant types fail with this reason.
    Unresolved(Arc<str>),
}

impl SessionZone {
    /// Resolve a `time_zone` value; `SYSTEM` stands for `system` (`@@system_time_zone`).
    pub fn resolve(name: &str, system: Option<&str>) -> Self {
        let name = match name {
            "SYSTEM" => match system {
                Some(s) => s,
                None => return Self::Unresolved("time_zone is SYSTEM but @@system_time_zone is unknown".into()),
            },
            n => n,
        };
        if let Some(seconds) = parse_offset(name) {
            return match seconds {
                0 => Self::Utc,
                s => Self::Zone(jiff::tz::TimeZone::fixed(jiff::tz::Offset::from_seconds(s).expect("|offset| < 24h"))),
            };
        }
        if matches!(name, "UTC" | "Etc/UTC" | "Etc/UCT" | "UCT" | "Zulu" | "Etc/Zulu" | "GMT" | "Etc/GMT") {
            return Self::Utc;
        }
        match jiff::tz::db().get(name) {
            Ok(tz) => Self::Zone(tz),
            Err(e) => Self::Unresolved(
                format!(
                    "session time zone {name:?} is not in the client's time zone database ({e}); \
                     install tzdata, or connect with `timezone=+00:00`"
                )
                .into(),
            ),
        }
    }

    pub fn is_utc(&self) -> bool {
        matches!(self, Self::Utc)
    }

    /// UTC wall time -> session wall time (for bind parameters).
    pub fn utc_to_local(&self, utc: DateTimeParts) -> Result<DateTimeParts, BoxDynError> {
        match self {
            Self::Utc => Ok(utc),
            Self::Zone(tz) => {
                Ok(from_civil(tz.to_datetime(to_civil(utc)?.to_zoned(jiff::tz::TimeZone::UTC)?.timestamp()))?)
            }
            Self::Unresolved(why) => Err(why.to_string().into()),
        }
    }

    /// Session wall time -> (UTC wall time, the session's UTC offset in seconds at that instant).
    pub fn local_to_utc(&self, local: DateTimeParts) -> Result<(DateTimeParts, i32), BoxDynError> {
        match self {
            Self::Utc => Ok((local, 0)),
            Self::Zone(tz) => {
                let ts = tidb_instant(tz, to_civil(local)?)?;
                let utc = from_civil(jiff::tz::TimeZone::UTC.to_datetime(ts))?;
                Ok((utc, tz.to_offset(ts).seconds()))
            }
            Self::Unresolved(why) => Err(why.to_string().into()),
        }
    }
}

/// The instant TiDB maps a session wall time to (see the module docs).
fn tidb_instant(tz: &jiff::tz::TimeZone, wall: jiff::civil::DateTime) -> Result<jiff::Timestamp, BoxDynError> {
    use jiff::tz::{AmbiguousOffset, Offset};
    match tz.to_ambiguous_timestamp(wall).offset() {
        AmbiguousOffset::Unambiguous { offset } => Ok(offset.to_timestamp(wall)?),
        AmbiguousOffset::Gap { before, .. } => {
            // Read with the old offset, the wall time lies after the transition.
            let past = before.to_timestamp(wall)?.checked_add(jiff::SignedDuration::from_nanos(1))?;
            Ok(tz.preceding(past).next().ok_or("missing time zone transition")?.timestamp())
        }
        AmbiguousOffset::Fold { .. } => {
            // Go's `time.Date`: take the offset in effect at the wall time read as UTC, and keep
            // it if the resulting instant still has that offset.
            let offset = tz.to_offset(Offset::UTC.to_timestamp(wall)?);
            let ts = offset.to_timestamp(wall)?;
            let actual = tz.to_offset(ts);
            Ok(if actual == offset { ts } else { actual.to_timestamp(wall)? })
        }
    }
}

/// `+HH:MM` / `-HH:MM` (as accepted by `SET time_zone`), in seconds.
fn parse_offset(s: &str) -> Option<i32> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    let digits = |p: &str| {
        (!p.is_empty() && p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse::<i32>().ok())
            .flatten()
    };
    let (h, m) = (digits(h)?, digits(m)?);
    (h < 24 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

fn to_civil(p: DateTimeParts) -> Result<jiff::civil::DateTime, BoxDynError> {
    Ok(jiff::civil::DateTime::new(
        p.year as i16,
        p.month as i8,
        p.day as i8,
        p.hour as i8,
        p.minute as i8,
        p.second as i8,
        p.micros as i32 * 1000,
    )?)
}

fn from_civil(dt: jiff::civil::DateTime) -> Result<DateTimeParts, BoxDynError> {
    Ok(DateTimeParts::date(dt.year().into(), dt.month() as u8, dt.day() as u8)?.with_time(
        dt.hour() as u8,
        dt.minute() as u8,
        dt.second() as u8,
        (dt.subsec_nanosecond() / 1000) as u32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(y: i32, mo: u8, d: u8, h: u8, mi: u8) -> DateTimeParts {
        DateTimeParts::date(y, mo, d).unwrap().with_time(h, mi, 0, 0)
    }

    #[test]
    fn resolves_offsets_and_names() {
        assert!(SessionZone::resolve("+00:00", None).is_utc());
        assert!(SessionZone::resolve("SYSTEM", Some("Etc/UTC")).is_utc());
        assert!(matches!(SessionZone::resolve("+05:30", None), SessionZone::Zone(_)));
        assert!(matches!(SessionZone::resolve("SYSTEM", None), SessionZone::Unresolved(_)));
        assert!(matches!(SessionZone::resolve("Not/AZone", None), SessionZone::Unresolved(_)));
        assert_eq!(parse_offset("-08:00"), Some(-8 * 3600));
        assert_eq!(parse_offset("+8:00"), Some(8 * 3600));
        assert_eq!(parse_offset("+24:00"), None);
        assert_eq!(parse_offset("08:00"), None);
    }

    #[test]
    fn fixed_offset_round_trip() {
        let z = SessionZone::resolve("+05:30", None);
        let local = z.utc_to_local(dt(2026, 1, 1, 0, 0)).unwrap();
        assert_eq!(local, dt(2026, 1, 1, 5, 30));
        assert_eq!(z.local_to_utc(local).unwrap(), (dt(2026, 1, 1, 0, 0), 5 * 3600 + 1800));
    }

    #[test]
    fn dst_fold_and_gap() {
        let Ok(tz) = jiff::tz::db().get("Europe/Paris") else { return }; // no tzdata on this machine
        let z = SessionZone::Zone(tz);
        // 2026-10-25 02:30 happens twice in Paris: TiDB reads it as the later instant (CET, +01:00).
        assert_eq!(z.local_to_utc(dt(2026, 10, 25, 2, 30)).unwrap(), (dt(2026, 10, 25, 1, 30), 3600));
        // 2026-03-29 02:30 does not exist: TiDB reads it as the transition (03:00 CEST).
        assert_eq!(z.local_to_utc(dt(2026, 3, 29, 2, 30)).unwrap(), (dt(2026, 3, 29, 1, 0), 7200));
        // In New York, Go (and so TiDB) reads the repeated hour as the earlier instant (EDT).
        let ny = SessionZone::Zone(jiff::tz::db().get("America/New_York").unwrap());
        assert_eq!(ny.local_to_utc(dt(2026, 11, 1, 1, 30)).unwrap(), (dt(2026, 11, 1, 5, 30), -4 * 3600));
        assert_eq!(z.utc_to_local(dt(2026, 7, 1, 12, 0)).unwrap(), dt(2026, 7, 1, 14, 0));
    }
}
