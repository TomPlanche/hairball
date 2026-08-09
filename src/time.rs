//! Time helpers: TID decoding, choice of the event time, day buckets.
//!
//! The database stores UTC unix seconds. Days exist only at export time,
//! in the configured time zone.

use std::fmt;

use jiff::{Timestamp, civil::Date, tz::TimeZone};

/// Base32-sortable alphabet of the TIDs.
const TID_ALPHABET: &[u8; 32] = b"234567abcdefghijklmnopqrstuvwxyz";

/// The oldest accepted event time (2022-01-01T00:00:00Z): ATProto records
/// did not exist before.
pub const MIN_EVENT_TIME: Timestamp = Timestamp::constant(1_640_995_200, 0);

/// An event time can be this far in the future (clock drift between servers).
const MAX_AHEAD_SECONDS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TidError {
    #[error("a TID has 13 characters, not {0}")]
    Length(usize),

    #[error("`{0}` is not a TID character")]
    Character(char),

    #[error("the first character of a TID must be one of `234567abcdefghij`")]
    FirstCharacter,

    #[error("the time of the TID is out of range")]
    OutOfRange,
}

/// Decodes the time of a TID (Timestamp IDentifier), the usual ATProto record key.
///
/// A TID is 13 characters of the alphabet `234567abcdefghijklmnopqrstuvwxyz`,
/// 5 bits each. The 53 bits after the top bit are microseconds since the Unix
/// epoch. The last 10 bits are a clock id: they are ignored here.
pub fn decode_tid(tid: &str) -> Result<Timestamp, TidError> {
    let length = tid.chars().count();
    if length != 13 {
        return Err(TidError::Length(length));
    }

    let mut value: u64 = 0;
    for (i, c) in tid.chars().enumerate() {
        let digit = TID_ALPHABET
            .iter()
            .position(|&a| char::from(a) == c)
            .ok_or(TidError::Character(c))?;
        // The first character holds only 4 bits: 4 + 12 * 5 = 64.
        if i == 0 && digit >= 16 {
            return Err(TidError::FirstCharacter);
        }
        // `digit` < 32, and the value stays below 2^64 thanks to the check above.
        value = (value << 5) | digit as u64;
    }

    let micros = i64::try_from(value >> 10).map_err(|_| TidError::OutOfRange)?;
    Timestamp::from_microsecond(micros).map_err(|_| TidError::OutOfRange)
}

/// Where the time of an event comes from, by order of preference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeSource {
    /// The `createdAt` field. The client writes it: it can be wrong or backdated.
    CreatedAt,
    /// The time decoded from the record key, if it is a TID.
    RecordKey,
    /// The first time the CLI saw the record. Always accepted.
    FirstSeen,
}

impl fmt::Display for TimeSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::CreatedAt => "createdAt",
            Self::RecordKey => "record key",
            Self::FirstSeen => "first seen",
        })
    }
}

/// Why a candidate time was not used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Invalid(String),
    TooOld(Timestamp),
    InFuture(Timestamp),
}

/// A candidate time that was not used. The caller logs it as a warning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub source: TimeSource,
    pub reason: Reason,
}

impl fmt::Display for Rejected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.reason {
            Reason::Invalid(error) => write!(f, "{} is invalid: {error}", self.source),
            Reason::TooOld(at) => write!(f, "{} {at} is before {MIN_EVENT_TIME}", self.source),
            Reason::InFuture(at) => write!(f, "{} {at} is in the future", self.source),
        }
    }
}

/// The chosen time of an event, and the candidates that were rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventTime {
    pub at: Timestamp,
    pub source: TimeSource,
    pub rejected: Vec<Rejected>,
}

/// Chooses the time of an event: `createdAt`, then the record key if it is a
/// TID, then `first_seen`.
///
/// A candidate is rejected if it cannot be read, if it is before
/// [`MIN_EVENT_TIME`], or if it is more than 1 day after `now`.
///
/// A record key that is not a TID (`self`, a custom key) is normal: it is not
/// a candidate, and not a rejection.
pub fn event_time(
    created_at: Option<&str>,
    record_key: Option<&str>,
    first_seen: Timestamp,
    now: Timestamp,
) -> EventTime {
    let candidates = [
        (
            TimeSource::CreatedAt,
            created_at.map(|text| text.parse::<Timestamp>().map_err(|e| e.to_string())),
        ),
        (
            TimeSource::RecordKey,
            record_key.and_then(|key| decode_tid(key).ok()).map(Ok),
        ),
    ];

    let mut rejected = Vec::new();
    for (source, candidate) in candidates {
        let reason = match candidate {
            None => continue,
            Some(Err(error)) => Reason::Invalid(error),
            Some(Ok(at)) if at < MIN_EVENT_TIME => Reason::TooOld(at),
            Some(Ok(at)) if at.as_second() > now.as_second() + MAX_AHEAD_SECONDS => {
                Reason::InFuture(at)
            }
            Some(Ok(at)) => {
                return EventTime {
                    at,
                    source,
                    rejected,
                };
            }
        };
        rejected.push(Rejected { source, reason });
    }

    EventTime {
        at: first_seen,
        source: TimeSource::FirstSeen,
        rejected,
    }
}

/// The calendar day of a time, in a time zone.
pub fn day(at: Timestamp, tz: &TimeZone) -> Date {
    tz.to_datetime(at).date()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn paris() -> TimeZone {
        TimeZone::get("Europe/Paris").unwrap()
    }

    /// A post of `tomplanche.com` on Eurosky: rkey and `createdAt` of the record.
    const REAL_TID: &str = "3mvgdyaeyt22l";
    const REAL_CREATED_AT: &str = "2026-09-13T19:40:12.208Z";

    const NOW: &str = "2026-11-12T12:00:00Z";

    #[test]
    fn decodes_known_tids() {
        // Example of the ATProto specification.
        assert_eq!(
            decode_tid("3jzfcijpj2z2a").unwrap(),
            ts("2023-06-30T15:03:01.887007Z")
        );
        // A real record: the TID time is a few milliseconds after `createdAt`.
        assert_eq!(
            decode_tid(REAL_TID).unwrap(),
            ts("2026-09-13T19:40:12.212Z")
        );
        assert_eq!(decode_tid("2222222222222").unwrap(), Timestamp::UNIX_EPOCH);
    }

    #[test]
    fn rejects_bad_tids() {
        assert_eq!(decode_tid("self"), Err(TidError::Length(4)));
        assert_eq!(decode_tid("3jzfcijpj2z2"), Err(TidError::Length(12)));
        assert_eq!(decode_tid("3jzfcijpj2z21"), Err(TidError::Character('1')));
        assert_eq!(decode_tid("3JZFCIJPJ2Z2A"), Err(TidError::Character('J')));
        assert_eq!(decode_tid("kzzzzzzzzzzzz"), Err(TidError::FirstCharacter));
    }

    #[test]
    fn valid_created_at_wins() {
        let time = event_time(Some(REAL_CREATED_AT), Some(REAL_TID), ts(NOW), ts(NOW));

        assert_eq!(time.at, ts(REAL_CREATED_AT));
        assert_eq!(time.source, TimeSource::CreatedAt);
        assert!(time.rejected.is_empty());
    }

    #[test]
    fn created_at_with_an_offset_is_read() {
        let time = event_time(Some("2026-09-13T21:40:12+02:00"), None, ts(NOW), ts(NOW));

        assert_eq!(time.at, ts("2026-09-13T19:40:12Z"));
    }

    #[test]
    fn bad_created_at_falls_back_to_the_tid() {
        let time = event_time(Some("yesterday"), Some(REAL_TID), ts(NOW), ts(NOW));

        assert_eq!(time.source, TimeSource::RecordKey);
        assert_eq!(time.at, ts("2026-09-13T19:40:12.212Z"));
        assert_eq!(time.rejected.len(), 1);
        assert_eq!(time.rejected[0].source, TimeSource::CreatedAt);
        assert!(matches!(time.rejected[0].reason, Reason::Invalid(_)));
    }

    #[test]
    fn future_created_at_falls_back_to_the_tid() {
        let time = event_time(
            Some("2030-01-01T00:00:00Z"),
            Some(REAL_TID),
            ts(NOW),
            ts(NOW),
        );

        assert_eq!(time.source, TimeSource::RecordKey);
        assert_eq!(
            time.rejected[0].to_string(),
            "createdAt 2030-01-01T00:00:00Z is in the future"
        );
    }

    #[test]
    fn up_to_one_day_ahead_is_accepted() {
        let time = event_time(Some("2026-11-13T11:59:59Z"), None, ts(NOW), ts(NOW));

        assert_eq!(time.source, TimeSource::CreatedAt);
    }

    #[test]
    fn backdated_created_at_falls_back_to_the_tid() {
        let time = event_time(
            Some("2001-09-11T00:00:00Z"),
            Some(REAL_TID),
            ts(NOW),
            ts(NOW),
        );

        assert_eq!(time.source, TimeSource::RecordKey);
        assert_eq!(
            time.rejected[0].reason,
            Reason::TooOld(ts("2001-09-11T00:00:00Z"))
        );
    }

    #[test]
    fn no_valid_candidate_falls_back_to_first_seen() {
        let first_seen = ts("2026-11-01T08:00:00Z");
        // `self` is the record key of a profile: not a TID.
        let time = event_time(Some("not a date"), Some("self"), first_seen, ts(NOW));

        assert_eq!(time.at, first_seen);
        assert_eq!(time.source, TimeSource::FirstSeen);
        // Only `createdAt` is a rejection: a key that is not a TID is normal.
        assert_eq!(
            time.rejected.iter().map(|r| r.source).collect::<Vec<_>>(),
            [TimeSource::CreatedAt]
        );
    }

    #[test]
    fn tid_out_of_range_is_a_rejection() {
        let first_seen = ts("2026-11-01T08:00:00Z");
        // A valid TID with the time 0 (1970).
        let time = event_time(None, Some("2222222222222"), first_seen, ts(NOW));

        assert_eq!(time.source, TimeSource::FirstSeen);
        assert_eq!(
            time.rejected[0].reason,
            Reason::TooOld(Timestamp::UNIX_EPOCH)
        );
    }

    #[test]
    fn late_evening_utc_is_the_next_day_in_paris_in_winter() {
        // Winter: Paris is UTC+1.
        let at = ts("2026-01-15T23:30:00Z");

        assert_eq!(day(at, &paris()), jiff::civil::date(2026, 1, 16));
        assert_eq!(day(at, &TimeZone::UTC), jiff::civil::date(2026, 1, 15));
        assert_eq!(
            day(ts("2026-01-15T22:59:59Z"), &paris()),
            jiff::civil::date(2026, 1, 15)
        );
    }

    #[test]
    fn day_boundary_moves_in_summer() {
        // Summer: Paris is UTC+2, so the day starts at 22:00 UTC.
        assert_eq!(
            day(ts("2026-07-15T21:59:59Z"), &paris()),
            jiff::civil::date(2026, 7, 15)
        );
        assert_eq!(
            day(ts("2026-07-15T22:00:00Z"), &paris()),
            jiff::civil::date(2026, 7, 16)
        );
    }
}
