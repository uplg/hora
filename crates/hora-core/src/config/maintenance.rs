//! The `[[maintenance]]` windows of the config: fixed (`start` / `end`) or
//! recurring (`cron` in a `timezone`, for `duration_secs`).

use jiff::Timestamp;
use serde::Deserialize;

use super::{Compiled, parse_cron};
use crate::clock::LocalCronTime;

/// A scheduled maintenance window: alerts for the affected monitors are
/// muted while it lasts. Either fixed, between `start` and `end` (RFC 3339
/// timestamps, quoted in TOML), or recurring: from every run of a five-field
/// `cron`, read on the wall clock of `timezone` (IANA, default UTC), for
/// `duration_secs`.
#[derive(Debug, Clone, Deserialize)]
#[serde(try_from = "RawMaintenance")]
pub struct Maintenance {
    pub title: String,
    /// Monitors covered by the window; empty = all monitors.
    pub monitors: Vec<String>,
    pub time: MaintenanceTime,
}

/// When a configured window applies.
#[derive(Debug, Clone)]
pub enum MaintenanceTime {
    /// Once, both bounds included.
    Fixed { start: Timestamp, end: Timestamp },
    /// On every run of a schedule.
    Recurring(Box<Recurrence>),
}

/// `cron` / `timezone` / `duration_secs`.
#[derive(Debug, Clone)]
pub struct Recurrence {
    /// Five fields, on the wall clock of `timezone`.
    pub cron: Compiled<croner::Cron>,
    /// Where the cron's clock hangs: daylight saving time moves the windows
    /// with the local time they were written in.
    pub timezone: jiff::tz::TimeZone,
    /// How long each run lasts, at most [`MAX_DURATION_SECS`].
    pub duration_secs: u32,
}

/// The longest a recurring window lasts: a week, the longest window the
/// API plans.
pub const MAX_DURATION_SECS: u32 = 7 * 24 * 3600;

/// The most runs of one recurring window a schedule lists. A daily window
/// over the longest history a page shows is a few hundred; past this, a
/// cron that fires every minute would make a schedule of thousands.
const MAX_OCCURRENCES: usize = 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawMaintenance {
    #[serde(default)]
    title: String,
    #[serde(default, deserialize_with = "super::optional_instant")]
    start: Option<Timestamp>,
    #[serde(default, deserialize_with = "super::optional_instant")]
    end: Option<Timestamp>,
    #[serde(default)]
    cron: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    duration_secs: Option<u32>,
    #[serde(default)]
    monitors: Vec<String>,
}

impl TryFrom<RawMaintenance> for Maintenance {
    type Error = String;

    fn try_from(raw: RawMaintenance) -> Result<Self, Self::Error> {
        let name = &raw.title;
        let time = match (raw.start, raw.end, raw.cron) {
            (Some(start), Some(end), None)
                if raw.timezone.is_none() && raw.duration_secs.is_none() =>
            {
                MaintenanceTime::Fixed { start, end }
            }
            (None, None, Some(cron)) => {
                let schedule = parse_cron(&cron)
                    .map_err(|err| format!("maintenance window {name:?}: invalid cron: {err}"))?;
                let timezone = match raw.timezone.as_deref() {
                    None => jiff::tz::TimeZone::UTC,
                    Some(zone) => jiff::tz::TimeZone::get(zone).map_err(|_| {
                        format!(
                            "maintenance window {name:?}: unknown timezone {zone:?} \
                             (an IANA name like \"America/New_York\")"
                        )
                    })?,
                };
                let duration_secs = raw
                    .duration_secs
                    .filter(|secs| (1..=MAX_DURATION_SECS).contains(secs))
                    .ok_or_else(|| {
                        format!(
                            "maintenance window {name:?}: a cron needs duration_secs, \
                             1 to {MAX_DURATION_SECS} (a week)"
                        )
                    })?;
                MaintenanceTime::Recurring(Box::new(Recurrence {
                    cron: Compiled::from_parsed(cron, schedule),
                    timezone,
                    duration_secs,
                }))
            }
            _ => {
                return Err(format!(
                    "maintenance window {name:?}: give start and end, or a cron with \
                     duration_secs (and optionally timezone) - not both"
                ));
            }
        };
        Ok(Self {
            title: raw.title,
            monitors: raw.monitors,
            time,
        })
    }
}

impl Maintenance {
    /// The window's spans, as `(start, end)`, that a schedule of the unix
    /// seconds `from` to `to` lists: a fixed window's one span, whatever the
    /// range (as it always was), and each run of a recurring window that
    /// overlaps the range, at most [`MAX_OCCURRENCES`] of them.
    #[must_use]
    pub fn spans(&self, from: i64, to: i64) -> Vec<(Timestamp, Timestamp)> {
        match &self.time {
            MaintenanceTime::Fixed { start, end } => vec![(*start, *end)],
            MaintenanceTime::Recurring(recurrence) => recurrence.spans(from, to),
        }
    }
}

impl Recurrence {
    /// Every run whose span overlaps `[from, to]`: from the first one that
    /// is still going at `from` (it started at most `duration_secs` before)
    /// to the last that starts by `to`.
    fn spans(&self, from: i64, to: i64) -> Vec<(Timestamp, Timestamp)> {
        let duration = i64::from(self.duration_secs);
        let mut cursor = LocalCronTime {
            at: from.saturating_sub(duration).saturating_sub(1),
            tz: self.timezone.clone(),
        };
        let mut spans = Vec::new();
        while spans.len() < MAX_OCCURRENCES {
            let Ok(next) = self.cron.find_next_occurrence(&cursor, false) else {
                break;
            };
            if next.at > to {
                break;
            }
            if let (Some(start), Some(end)) = (
                crate::clock::at(next.at),
                crate::clock::at(next.at.saturating_add(duration)),
            ) {
                spans.push((start, end));
            }
            cursor = next;
        }
        spans
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> i64 {
        text.parse::<Timestamp>().expect("a time").as_second()
    }

    fn window(fields: &str) -> Result<Maintenance, toml::de::Error> {
        toml::from_str(fields)
    }

    /// The span of `window` running at `now`, if any.
    fn active(window: &Maintenance, now: i64) -> Option<(i64, i64)> {
        window
            .spans(now, now)
            .into_iter()
            .map(|(start, end)| (start.as_second(), end.as_second()))
            .find(|(start, end)| (*start..=*end).contains(&now))
    }

    /// 03:00 in New York is 08:00 UTC in winter and 07:00 in summer, across
    /// both changes of the clock; each run lasts exactly `duration_secs`.
    #[test]
    fn a_recurring_window_follows_its_local_clock() {
        let daily = window(
            "cron = '0 3 * * *'\ntimezone = 'America/New_York'\nduration_secs = 7200\nmonitors = ['job']",
        )
        .expect("valid");
        for (before, start, end) in [
            (
                "2026-01-01T07:59:59Z",
                "2026-01-01T08:00:00Z",
                "2026-01-01T10:00:00Z",
            ),
            (
                "2026-07-01T06:59:59Z",
                "2026-07-01T07:00:00Z",
                "2026-07-01T09:00:00Z",
            ),
            (
                "2026-03-08T06:59:59Z",
                "2026-03-08T07:00:00Z",
                "2026-03-08T09:00:00Z",
            ),
            (
                "2026-11-01T07:59:59Z",
                "2026-11-01T08:00:00Z",
                "2026-11-01T10:00:00Z",
            ),
        ] {
            assert_eq!(active(&daily, at(before)), None, "{before}");
            assert_eq!(
                active(&daily, at(start)),
                Some((at(start), at(end))),
                "{start}"
            );
            assert!(active(&daily, at(end)).is_some(), "the end is included");
            assert_eq!(active(&daily, at(end) + 1), None, "{end}");
        }
        // A day's history: one run per day, the one under way at its start
        // included.
        let spans = daily.spans(at("2026-07-01T00:00:00Z"), at("2026-07-03T23:59:59Z"));
        assert_eq!(spans.len(), 3);
        let spans = daily.spans(at("2026-07-01T08:10:00Z"), at("2026-07-01T08:10:00Z"));
        assert_eq!(spans.len(), 1);
    }

    /// 02:30 in New York never happens on the second Sunday of March: that
    /// run starts when the clocks jump, at 03:00. 01:30 happens twice on the
    /// first Sunday of November: one run, at the first.
    #[test]
    fn a_skipped_wall_time_runs_at_the_jump_and_a_repeated_one_once() {
        let starts = |cron: &str, date: &str| {
            let daily = window(&format!(
                "cron = '{cron}'\ntimezone = 'America/New_York'\nduration_secs = 600"
            ))
            .expect("valid");
            let from = at(&format!("{date}T00:00:00Z"));
            daily
                .spans(from, at(&format!("{date}T23:59:59Z")))
                .into_iter()
                .map(|(start, _)| start.as_second())
                .filter(|start| *start >= from)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            starts("30 2 * * *", "2026-03-07"),
            [at("2026-03-07T07:30:00Z")]
        );
        assert_eq!(
            starts("30 2 * * *", "2026-03-08"),
            [at("2026-03-08T07:00:00Z")]
        );
        assert_eq!(
            starts("30 2 * * *", "2026-03-09"),
            [at("2026-03-09T06:30:00Z")]
        );
        assert_eq!(
            starts("30 1 * * *", "2026-11-01"),
            [at("2026-11-01T05:30:00Z")]
        );
    }

    /// A weekly window: Sundays 03:00 New York, for two hours.
    #[test]
    fn a_weekly_window_covers_its_monitors_on_its_day_only() {
        let config = crate::config::parse(
            "[page]\n[server]\n[[monitors]]\nid='job'\nname='Job'\nkind='push'\n\
             interval_secs=60\nallow_unauthenticated=true\n[[maintenance]]\n\
             title='Backup'\ncron='0 3 * * 0'\ntimezone='America/New_York'\n\
             duration_secs=7200\nmonitors=['job']",
        )
        .expect("valid");
        let schedule = |now: i64| crate::planned::Schedule::merged(&config, &[], now, now);
        let sunday = "2026-07-05T07:10:00Z".parse().expect("a time");
        let monday = "2026-07-06T07:10:00Z".parse().expect("a time");
        assert!(schedule(at("2026-07-05T07:10:00Z")).in_maintenance("job", sunday));
        assert!(!schedule(at("2026-07-05T07:10:00Z")).in_maintenance("other", sunday));
        assert!(!schedule(at("2026-07-06T07:10:00Z")).in_maintenance("job", monday));
    }

    #[test]
    fn a_window_is_fixed_or_recurring_and_says_what_is_wrong() {
        for (fields, says) in [
            ("cron = 'bad'\nduration_secs = 60", "invalid cron"),
            ("cron = '0 0 * * *'", "duration_secs"),
            ("cron = '0 0 * * *'\nduration_secs = 0", "duration_secs"),
            (
                "cron = '0 0 * * *'\nduration_secs = 604801",
                "duration_secs",
            ),
            (
                "cron = '0 0 * * *'\nduration_secs = 60\ntimezone = 'Mars/Olympus'",
                "unknown timezone",
            ),
            (
                "start = '2026-01-01T00:00:00Z'\nend = '2026-01-02T00:00:00Z'\n\
                 cron = '0 0 * * *'\nduration_secs = 60",
                "not both",
            ),
            ("start = '2026-01-01T00:00:00Z'", "not both"),
            (
                "start = '2026-01-01T00:00:00Z'\nend = '2026-01-02T00:00:00Z'\nduration_secs = 60",
                "not both",
            ),
        ] {
            let err = window(fields).expect_err(fields).to_string();
            assert!(err.contains(says), "{fields}: {err}");
        }
        // UTC by default.
        let utc = window("cron = '0 3 * * *'\nduration_secs = 60").expect("valid");
        assert_eq!(
            active(&utc, at("2026-07-01T03:00:30Z")),
            Some((at("2026-07-01T03:00:00Z"), at("2026-07-01T03:01:00Z")))
        );
    }

    /// A cron that fires every minute is listed a bounded number of times.
    #[test]
    fn a_schedule_lists_a_bounded_number_of_runs() {
        let busy = window("cron = '* * * * *'\nduration_secs = 30").expect("valid");
        assert_eq!(
            busy.spans(0, at("2026-01-01T00:00:00Z")).len(),
            MAX_OCCURRENCES
        );
    }
}
