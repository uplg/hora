//! The conversions between what Hora stores (unix seconds) and what a
//! calendar question needs (an instant, a UTC wall clock). Everything here is
//! UTC: Hora never reads the host's time zone.

use jiff::civil::{Date, DateTime};
use jiff::tz::Offset;
use jiff::{SignedDuration, Timestamp};

/// The instant `seconds` after the Unix epoch. `None` outside the years
/// -9999 to 9999, which no stored time is.
#[must_use]
pub fn at(seconds: i64) -> Option<Timestamp> {
    Timestamp::from_second(seconds).ok()
}

/// The UTC wall clock at `at`.
#[must_use]
pub fn civil(at: Timestamp) -> DateTime {
    Offset::UTC.to_datetime(at)
}

/// The UTC calendar day `at` falls on.
#[must_use]
pub fn date(at: Timestamp) -> Date {
    civil(at).date()
}

/// Unix seconds of a UTC wall clock.
#[must_use]
pub fn unix(civil: DateTime) -> Option<i64> {
    Offset::UTC
        .to_timestamp(civil)
        .ok()
        .map(Timestamp::as_second)
}

/// An instant in unix seconds, for a cron schedule to search from: croner
/// reads the calendar through this, on the UTC wall clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct CronTime(pub i64);

impl croner::CronDateTime for CronTime {
    fn to_civil(&self) -> croner::CivilDateTime {
        // A second outside the calendar reads as its nearest end: croner
        // then finds nothing past it, which is the answer.
        let at = at(self.0).unwrap_or(if self.0 < 0 {
            Timestamp::MIN
        } else {
            Timestamp::MAX
        });
        let civil = civil(at);
        let part = |value: i8| u32::from(value.unsigned_abs());
        croner::CivilDateTime::new(
            croner::CivilDate::from_parts_unchecked(
                i32::from(civil.year()),
                part(civil.month()),
                part(civil.day()),
            ),
            croner::CivilTime::from_parts_unchecked(
                part(civil.hour()),
                part(civil.minute()),
                part(civil.second()),
            ),
        )
    }

    fn civil_weekday(&self) -> croner::Weekday {
        // 1970-01-01 was a Thursday, four days from Sunday.
        let days = self.0.div_euclid(crate::SECONDS_PER_DAY);
        let from_sunday = (days.rem_euclid(7) + 4) % 7;
        croner::Weekday::from_days_from_sunday(u32::try_from(from_sunday).unwrap_or(0))
    }

    fn resolve_civil(
        &self,
        civil: croner::CivilDateTime,
    ) -> Result<croner::Resolution<Self>, croner::errors::CronError> {
        let part = |value: u32| i8::try_from(value).ok();
        let resolved = (|| {
            let wall = DateTime::new(
                i16::try_from(civil.year()).ok()?,
                part(civil.month())?,
                part(civil.day())?,
                part(civil.hour())?,
                part(civil.minute())?,
                part(civil.second())?,
                0,
            )
            .ok()?;
            unix(wall)
        })();
        resolved
            .map(|seconds| croner::Resolution::Single(Self(seconds)))
            .ok_or(croner::errors::CronError::InvalidDate)
    }

    fn checked_add_seconds(&self, seconds: i64) -> Option<Self> {
        self.0.checked_add(seconds).map(Self)
    }

    fn cmp_instant(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

/// An instant in unix seconds read on the wall clock of `tz`, for a cron
/// schedule written in local time (a recurring maintenance window): croner
/// then follows daylight saving time - a wall clock time that happens twice
/// is one occurrence (the first), one that never happens falls on the
/// moment the clocks jump.
#[derive(Debug, Clone)]
pub struct LocalCronTime {
    pub at: i64,
    pub tz: jiff::tz::TimeZone,
}

impl LocalCronTime {
    fn wall(&self) -> DateTime {
        let at = at(self.at).unwrap_or(if self.at < 0 {
            Timestamp::MIN
        } else {
            Timestamp::MAX
        });
        self.tz.to_datetime(at)
    }
}

impl croner::CronDateTime for LocalCronTime {
    fn to_civil(&self) -> croner::CivilDateTime {
        let wall = self.wall();
        let part = |value: i8| u32::from(value.unsigned_abs());
        croner::CivilDateTime::new(
            croner::CivilDate::from_parts_unchecked(
                i32::from(wall.year()),
                part(wall.month()),
                part(wall.day()),
            ),
            croner::CivilTime::from_parts_unchecked(
                part(wall.hour()),
                part(wall.minute()),
                part(wall.second()),
            ),
        )
    }

    fn civil_weekday(&self) -> croner::Weekday {
        let from_sunday = self.wall().weekday().to_sunday_zero_offset();
        croner::Weekday::from_days_from_sunday(u32::from(from_sunday.unsigned_abs()))
    }

    fn resolve_civil(
        &self,
        civil: croner::CivilDateTime,
    ) -> Result<croner::Resolution<Self>, croner::errors::CronError> {
        use jiff::tz::AmbiguousOffset;

        let part = |value: u32| i8::try_from(value).ok();
        let wall = (|| {
            DateTime::new(
                i16::try_from(civil.year()).ok()?,
                part(civil.month())?,
                part(civil.day())?,
                part(civil.hour())?,
                part(civil.minute())?,
                part(civil.second())?,
                0,
            )
            .ok()
        })()
        .ok_or(croner::errors::CronError::InvalidDate)?;
        let ambiguous = self.tz.to_ambiguous_timestamp(wall);
        let local = |at: Result<Timestamp, jiff::Error>| {
            at.map(|at| Self {
                at: at.as_second(),
                tz: self.tz.clone(),
            })
            .map_err(|_| croner::errors::CronError::InvalidTime)
        };
        Ok(match ambiguous.offset() {
            AmbiguousOffset::Unambiguous { .. } => {
                croner::Resolution::Single(local(ambiguous.unambiguous())?)
            }
            AmbiguousOffset::Fold { .. } => croner::Resolution::Ambiguous(
                local(ambiguous.earlier())?,
                local(ambiguous.later())?,
            ),
            AmbiguousOffset::Gap { .. } => croner::Resolution::Gap,
        })
    }

    fn checked_add_seconds(&self, seconds: i64) -> Option<Self> {
        self.at.checked_add(seconds).map(|at| Self {
            at,
            tz: self.tz.clone(),
        })
    }

    fn cmp_instant(&self, other: &Self) -> std::cmp::Ordering {
        self.at.cmp(&other.at)
    }
}

/// `at` moved by `seconds`, stopping at the ends of the supported range
/// instead of overflowing.
#[must_use]
pub fn shift(at: Timestamp, seconds: i64) -> Timestamp {
    at.saturating_add(SignedDuration::from_secs(seconds))
        .unwrap_or(at)
}

/// `at` moved by whole days of 86 400 seconds (UTC has no others).
#[must_use]
pub fn shift_days(at: Timestamp, days: i64) -> Timestamp {
    shift(at, days.saturating_mul(crate::SECONDS_PER_DAY))
}

/// Unix seconds of an RFC 3339 date-time (`2026-05-14T08:03:12Z`, or with a
/// numeric offset). `None` for anything else.
#[must_use]
pub fn parse_rfc3339(raw: &str) -> Option<i64> {
    raw.parse::<Timestamp>().ok().map(Timestamp::as_second)
}

/// An instant as a config file writes one. RFC 3339, plus the looser
/// spellings every release so far has loaded: spaces around the value, a
/// space before the offset, and `UTC` for `Z` (`2026-05-14 08:03:12 UTC`).
#[must_use]
pub fn parse_config(raw: &str) -> Option<Timestamp> {
    let raw = raw.trim();
    if let Ok(at) = raw.parse() {
        return Some(at);
    }
    let (clock, offset) = raw.rsplit_once(' ')?;
    let offset = if offset.eq_ignore_ascii_case("utc") {
        "Z"
    } else {
        offset
    };
    format!("{}{offset}", clock.trim_end()).parse().ok()
}

/// [`parse_config`] for a `serde` field.
///
/// # Errors
/// The value is not a string, or not a date-time with its offset.
pub fn deserialize_config<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Timestamp, D::Error> {
    let raw = <std::borrow::Cow<'de, str> as serde::Deserialize>::deserialize(deserializer)?;
    parse_config(&raw).ok_or_else(|| {
        serde::de::Error::custom(format!(
            "`{raw}` is not an RFC 3339 date-time with its offset (2026-05-14T08:03:12Z)"
        ))
    })
}

/// `"2026-05-14T08:03:12Z"`: RFC 3339 to the second.
#[must_use]
pub fn rfc3339(at: Timestamp) -> String {
    at.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `"2026-05-14T08:03:12+00:00"`: the same instant with the offset spelled
/// out and any fraction of a second kept, as the JSON API and the Atom feed
/// have always written their dates.
#[must_use]
pub fn rfc3339_offset(at: Timestamp) -> String {
    at.strftime("%Y-%m-%dT%H:%M:%S%.f+00:00").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trips_to_the_second() {
        let seconds = parse_rfc3339("2026-05-14T10:03:12+02:00").expect("a date");
        assert_eq!(seconds, 1_778_745_792);
        assert_eq!(
            rfc3339(at(seconds).expect("in range")),
            "2026-05-14T08:03:12Z"
        );
        assert_eq!(
            rfc3339_offset(at(seconds).expect("in range")),
            "2026-05-14T08:03:12+00:00"
        );
        let precise: Timestamp = "2026-05-14T08:03:12.25Z".parse().expect("a date");
        assert_eq!(rfc3339_offset(precise), "2026-05-14T08:03:12.25+00:00");
        assert_eq!(parse_rfc3339(&rfc3339_offset(precise)), Some(seconds));
        assert_eq!(parse_rfc3339("2026-05-14"), None);
        assert_eq!(parse_rfc3339("yesterday"), None);
    }

    #[test]
    fn a_config_date_keeps_its_looser_spellings() {
        let expected = at(1_780_880_400);
        for raw in [
            "2026-06-08T01:00:00Z",
            "2026-06-08 01:00:00Z",
            "2026-06-08T03:00:00+02:00",
            "2026-06-08T03:00:00 +02:00",
            "2026-06-08T01:00:00 UTC",
            "2026-06-08 01:00:00 utc",
            " 2026-06-08T01:00:00Z ",
        ] {
            assert_eq!(parse_config(raw), expected, "{raw}");
        }
        // No offset is no instant: Hora does not guess a time zone.
        for raw in [
            "2026-06-08T01:00:00",
            "2026-06-08",
            "2026-06-08 01:00:00",
            "",
        ] {
            assert_eq!(parse_config(raw), None, "{raw}");
        }
    }

    #[test]
    fn a_cron_schedule_reads_the_utc_calendar() {
        use croner::CronDateTime as _;
        // 2026-05-14 was a Thursday; the epoch too, and the day before it a
        // Wednesday.
        assert_eq!(
            CronTime(1_778_745_792).civil_weekday(),
            croner::Weekday::Thursday
        );
        assert_eq!(CronTime(0).civil_weekday(), croner::Weekday::Thursday);
        assert_eq!(CronTime(-1).civil_weekday(), croner::Weekday::Wednesday);
        let civil = CronTime(1_778_745_792).to_civil();
        assert_eq!((civil.year(), civil.month(), civil.day()), (2026, 5, 14));
        assert_eq!((civil.hour(), civil.minute(), civil.second()), (8, 3, 12));
        assert_eq!(
            CronTime(0).resolve_civil(civil).ok(),
            Some(croner::Resolution::Single(CronTime(1_778_745_792)))
        );
        // Fridays at 18:30, from that Thursday morning: the next day.
        let cron: croner::Cron = "30 18 * * FRI".parse().expect("valid cron");
        let next = cron
            .find_next_occurrence(&CronTime(1_778_745_792), false)
            .expect("next");
        assert_eq!(Some(next.0), parse_rfc3339("2026-05-15T18:30:00Z"));
        // The last day of February in a leap year, across the year's end.
        let cron: croner::Cron = "0 0 L 2 *".parse().expect("valid cron");
        let from = parse_rfc3339("2027-12-31T23:59:59Z").expect("a date");
        let next = cron
            .find_next_occurrence(&CronTime(from), false)
            .expect("next");
        assert_eq!(Some(next.0), parse_rfc3339("2028-02-29T00:00:00Z"));
        // Past the end of the calendar there is no next run, and no panic.
        assert!(
            cron.find_next_occurrence(&CronTime(i64::MAX), false)
                .is_err()
        );
    }

    #[test]
    fn a_wall_clock_is_the_utc_one() {
        let civil = civil(at(1_778_745_792).expect("in range"));
        assert_eq!(civil.to_string(), "2026-05-14T08:03:12");
        assert_eq!(unix(civil), Some(1_778_745_792));
    }

    #[test]
    fn a_shift_stops_at_the_end_of_time() {
        assert_eq!(shift(Timestamp::MAX, 10), Timestamp::MAX);
        assert_eq!(shift_days(Timestamp::UNIX_EPOCH, 1).as_second(), 86_400);
        assert_eq!(at(i64::MAX), None);
    }
}
