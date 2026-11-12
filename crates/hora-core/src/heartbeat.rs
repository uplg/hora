//! Heartbeat evaluation, shared by push monitors (the scheduler) and watched
//! peers (the mesh): when a heartbeat is due, and what the stored ones say.

use crate::db::Store;
use crate::status::CheckStatus;
use tracing::error;

use crate::config::{Monitor, PushSpec};
use crate::db;
use crate::probe::{FailureKind, Outcome};

/// How often a heartbeat (a push monitor's, or a watched peer's) is expected.
pub(crate) enum Cadence {
    /// At least one heartbeat every this many seconds.
    Every(u64),
    /// One per scheduled run of a cron expression, late by at most `grace_secs`.
    Cron {
        cron: Box<croner::Cron>,
        grace_secs: u64,
    },
}

impl Cadence {
    /// Why a heartbeat last seen at `since` is overdue at `now`, or `None`
    /// while it is on time. `seen` says whether `since` is a real heartbeat
    /// (or only when one was first expected): only a real one may count for
    /// the run it slightly precedes (see [`cron_due`]).
    fn overdue(&self, since: i64, seen: bool, now: i64) -> Option<(FailureKind, String)> {
        match self {
            Self::Every(interval_secs) => {
                let max_gap = i64::try_from(*interval_secs).unwrap_or(i64::MAX);
                (now - since > max_gap).then(|| {
                    (
                        FailureKind::MissingHeartbeat,
                        "missing heartbeat".to_owned(),
                    )
                })
            }
            Self::Cron { cron, grace_secs } => {
                let early_ok = seen.then_some(*grace_secs);
                let due = cron_missed(cron, since, *grace_secs, early_ok, now)?;
                let due_label = chrono::DateTime::from_timestamp(due, 0)
                    .map_or_else(|| due.to_string(), |dt| dt.format("%H:%M UTC").to_string());
                Some((
                    FailureKind::MissedSchedule,
                    format!(
                        "missed scheduled heartbeat (was due {due_label} + {} grace)",
                        crate::fmt::period(*grace_secs)
                    ),
                ))
            }
        }
    }
}

/// A push monitor's heartbeat expectations, resolved once per task.
pub(crate) struct HeartbeatWatch {
    pub(crate) cadence: Cadence,
    /// When a heartbeat was first expected (persisted, see
    /// [`heartbeat_expected_since`]).
    pub(crate) expected_since: i64,
}

impl HeartbeatWatch {
    /// The watch for a push monitor: every `interval_secs`, or per cron run
    /// when it has a schedule (parsed at config load).
    pub(crate) async fn for_monitor(store: &Store, monitor: &Monitor, push: &PushSpec) -> Self {
        let cadence = match &push.schedule {
            None => Cadence::Every(monitor.interval_secs),
            Some(schedule) => Cadence::Cron {
                cron: Box::new((*schedule.cron).clone()),
                grace_secs: schedule.grace_secs,
            },
        };
        let now = chrono::Utc::now().timestamp();
        Self {
            cadence,
            expected_since: heartbeat_expected_since(store, &monitor.id, now).await,
        }
    }
}

/// When a heartbeat was first expected for `id`: the first time a task ever
/// watched it, persisted in `meta` so a restart does not push the deadline
/// back. A job that never pings (broken from day one, a typo in its push URL)
/// is then judged against this instead of staying unknown forever. A database
/// error falls back to `now`, unpersisted: the watch still works, just from
/// this start.
pub(crate) async fn heartbeat_expected_since(store: &Store, id: &str, now: i64) -> i64 {
    let key = format!("{}{id}", db::HEARTBEAT_EXPECTED_META_PREFIX);
    match db::meta_get(store, &key).await {
        Ok(Some(stored)) => {
            if let Ok(since) = stored.parse::<i64>() {
                return since;
            }
        }
        Ok(None) => {}
        Err(err) => {
            error!(monitor = %id, "failed to read the heartbeat start: {err:#}");
            return now;
        }
    }
    if let Err(err) = db::meta_set(store, &key, &now.to_string()).await {
        error!(monitor = %id, "failed to record the heartbeat start: {err:#}");
    }
    now
}

/// With a cron schedule, the heartbeat is overdue once `now` passes the
/// scheduled run it should have answered (see [`cron_due`]) plus the grace.
/// Returns that due time when missed (for the alert message), `None` while on
/// time. A schedule with no computable next occurrence never alerts rather
/// than alerting forever.
fn cron_missed(
    cron: &croner::Cron,
    last: i64,
    grace_secs: u64,
    early_secs: Option<u64>,
    now: i64,
) -> Option<i64> {
    let due = cron_due(cron, last, early_secs)?;
    let deadline = due.saturating_add(i64::try_from(grace_secs).unwrap_or(i64::MAX));
    (now > deadline).then_some(due)
}

/// The scheduled run the next heartbeat after `last` must answer: normally the
/// first run after `last`. A heartbeat arriving slightly *before* a run (clock
/// skew between the job's host and this one, a job that pings as it starts)
/// belongs to that run, so the one after it is due instead - otherwise a ping
/// at 02:59:59 for a `0 3 * * *` job reads as "missed 03:00". "Slightly" is
/// the grace (`early_secs`), capped at half the gap between runs so a frequent
/// schedule can never skip a real run this way.
fn cron_due(cron: &croner::Cron, last: i64, early_secs: Option<u64>) -> Option<i64> {
    let last_at = chrono::DateTime::from_timestamp(last, 0)?;
    let next = cron.find_next_occurrence(&last_at, false).ok()?;
    let Some(early_secs) = early_secs else {
        return Some(next.timestamp());
    };
    let Ok(after) = cron.find_next_occurrence(&next, false) else {
        return Some(next.timestamp());
    };
    let (next, after) = (next.timestamp(), after.timestamp());
    let early = i64::try_from(early_secs)
        .unwrap_or(i64::MAX)
        .min((after - next) / 2);
    Some(if next - last <= early { after } else { next })
}

/// What the stored heartbeats say about a push monitor or peer right now.
#[derive(Debug)]
enum HeartbeatVerdict {
    /// Never pinged, and the first expected heartbeat is not late yet.
    Unknown,
    /// The heartbeat is overdue, for this reason.
    Overdue(FailureKind, String),
    /// On time: the outcome of the latest heartbeat, as pushed.
    OnTime(Outcome),
}

/// Judge the latest heartbeat (`None`: never pinged) against the cadence at
/// `now`. A monitor that never pinged is judged from when a heartbeat was
/// first expected. An on-time heartbeat yields what the job pushed: an
/// explicit `status=down` is down with the job's own message, `degraded` is
/// degraded - the job's verdict, not just its liveness, drives the alert.
fn judge_heartbeat(
    cadence: &Cadence,
    last: Option<&db::Heartbeat>,
    expected_since: i64,
    now: i64,
) -> HeartbeatVerdict {
    let since = last.map_or(expected_since, |beat| beat.time);
    if let Some((kind, reason)) = cadence.overdue(since, last.is_some(), now) {
        return if last.is_some() {
            HeartbeatVerdict::Overdue(kind, reason)
        } else {
            HeartbeatVerdict::Overdue(
                FailureKind::NoHeartbeatYet,
                "no heartbeat received yet".to_owned(),
            )
        };
    }
    let Some(beat) = last else {
        return HeartbeatVerdict::Unknown;
    };
    HeartbeatVerdict::OnTime(match beat.status {
        CheckStatus::Down => Outcome::down(
            FailureKind::Pushed,
            beat.error
                .clone()
                .unwrap_or_else(|| "push reported down".to_owned()),
        ),
        status => Outcome {
            status,
            latency_ms: beat.latency_ms,
            status_code: None,
            // A degraded push keeps the job's words for the degraded alert;
            // an up one has nothing to say.
            error: if status == CheckStatus::Degraded {
                beat.error.clone()
            } else {
                None
            },
            reason: (status == CheckStatus::Degraded && beat.error.is_some())
                .then_some(FailureKind::Pushed),
            snapshot: None,
        },
    })
}

/// Evaluate a heartbeat-driven monitor (push monitor or peer watch) from the
/// stored pings for `id`: down (and recorded as a miss, once per
/// `record_every` seconds; 0 records every time) when overdue, else the
/// latest heartbeat's own status. `None` means nothing to react to yet (never
/// pinged and not yet late, or a read error).
///
/// Staleness is measured from the last *heartbeat*, not the last check: the
/// misses recorded here carry a fresh timestamp, so measuring from them would
/// reset the clock each tick and the monitor would flap instead of confirming
/// down (see [`db::last_heartbeat`]).
pub(crate) async fn heartbeat_outcome_for(
    store: &Store,
    id: &str,
    cadence: &Cadence,
    expected_since: i64,
    record_every: i64,
    now: i64,
) -> Option<Outcome> {
    let last = match db::last_heartbeat(store, id).await {
        Ok(last) => last,
        Err(err) => {
            error!(monitor = %id, "failed to read last heartbeat: {err:#}");
            return None;
        }
    };
    match judge_heartbeat(cadence, last.as_ref(), expected_since, now) {
        HeartbeatVerdict::Unknown => None,
        HeartbeatVerdict::OnTime(outcome) => Some(outcome),
        HeartbeatVerdict::Overdue(kind, reason) => {
            // Recorded so the page and history show it; marked as a miss so it
            // never reads as a heartbeat (or as the job's own down). At most
            // one row per `record_every`: the monitor is looked at far more
            // often than its heartbeat is due, and availability counts rows -
            // a nightly job that missed one night must weigh one check, not
            // the 1 440 looks of that day.
            let recorded_lately = record_every > 0
                && db::last_check_time(store, id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|last| now - last < record_every);
            if !recorded_lately
                && let Err(err) = db::insert_heartbeat_miss(store, id, kind, &reason).await
            {
                error!(monitor = %id, "failed to record heartbeat miss: {err:#}");
            }
            Some(Outcome::down(kind, reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_grace_reads_as_configured() {
        assert_eq!(crate::fmt::period(20), "20 s");
        assert_eq!(crate::fmt::period(90), "1 min 30 s");
        assert_eq!(crate::fmt::period(1_800), "30 min");
        assert_eq!(crate::fmt::period(5_400), "1 h 30 min");
        assert_eq!(crate::fmt::period(7_200), "2 h");
    }

    fn beat(time: i64, status: CheckStatus, error: Option<&str>) -> db::Heartbeat {
        db::Heartbeat {
            time,
            status,
            latency_ms: None,
            error: error.map(str::to_owned),
        }
    }

    fn nightly() -> Cadence {
        Cadence::Cron {
            cron: Box::new("0 3 * * *".parse().expect("valid cron")),
            grace_secs: 1800,
        }
    }

    #[test]
    fn cron_missed_only_after_due_plus_grace() {
        // Nightly at 03:00 UTC; epoch day boundaries make the math readable.
        let cron: croner::Cron = "0 3 * * *".parse().expect("valid cron");
        let day = 86_400;
        let last = day + 3 * 3600 + 120; // pinged 03:02, day 2
        let due = 2 * day + 3 * 3600; // next run: 03:00, day 3
        let grace: u64 = 1800;
        let deadline = due + 1800; // due + grace, as a timestamp

        for early in [None, Some(grace)] {
            // Before the next run, and within the grace window: on time.
            assert_eq!(cron_missed(&cron, last, grace, early, due - 3600), None);
            assert_eq!(cron_missed(&cron, last, grace, early, deadline), None);
            // Past due + grace: missed, reporting the due time.
            assert_eq!(
                cron_missed(&cron, last, grace, early, deadline + 1),
                Some(due)
            );
            // A heartbeat long dead stays missed until a fresh ping moves `last`.
            assert_eq!(
                cron_missed(&cron, last, grace, early, due + 30 * day),
                Some(due)
            );
        }
    }

    #[test]
    fn early_cron_heartbeat_counts_for_the_run_it_precedes() {
        let cron: croner::Cron = "0 3 * * *".parse().expect("valid cron");
        let day = 86_400;
        let run = day + 3 * 3600; // 03:00, day 2
        let grace: u64 = 1800;
        // Pinged at 02:59:59 (clock skew): that is the 03:00 run, so 03:30
        // today is *not* a miss - tomorrow's run is the one due.
        let last = run - 1;
        assert_eq!(
            cron_missed(&cron, last, grace, Some(grace), run + 1801),
            None
        );
        assert_eq!(
            cron_missed(&cron, last, grace, Some(grace), run + day + 1801),
            Some(run + day)
        );
        // Without the early allowance (only a "first expected" time, not a
        // real ping) the 03:00 run is still due.
        assert_eq!(cron_missed(&cron, last, grace, None, run + 1801), Some(run));
        // Far ahead of the run (more than the grace) is not "slightly early".
        let last = run - 3600;
        assert_eq!(
            cron_missed(&cron, last, grace, Some(grace), run + 1801),
            Some(run)
        );
    }

    #[test]
    fn early_allowance_never_skips_a_run_of_a_frequent_schedule() {
        // Every 15 minutes with a 30-minute grace: a ping right after 03:00
        // must not be taken as the 03:15 run (half the gap caps the allowance).
        let cron: croner::Cron = "*/15 * * * *".parse().expect("valid cron");
        let run = 86_400 + 3 * 3600; // 03:00
        assert_eq!(cron_due(&cron, run + 5, Some(1800)), Some(run + 900));
        // 02:59:00 is within half the gap of 03:00: it answers 03:00.
        assert_eq!(cron_due(&cron, run - 60, Some(1800)), Some(run + 900));
    }

    #[test]
    fn explicit_down_and_degraded_pushes_drive_the_outcome() {
        let every = Cadence::Every(60);
        let now = 1_000;

        // A fresh `status=down&msg=backup failed`: down, with the job's words.
        let HeartbeatVerdict::OnTime(outcome) = judge_heartbeat(
            &every,
            Some(&beat(990, CheckStatus::Down, Some("backup failed"))),
            0,
            now,
        ) else {
            panic!("on time");
        };
        assert!(!outcome.is_up());
        assert_eq!(outcome.error.as_deref(), Some("backup failed"));

        // Without a message it still reads as the job's own verdict.
        let HeartbeatVerdict::OnTime(outcome) =
            judge_heartbeat(&every, Some(&beat(990, CheckStatus::Down, None)), 0, now)
        else {
            panic!("on time");
        };
        assert_eq!(outcome.error.as_deref(), Some("push reported down"));

        // Degraded is up-but-degraded, so `alert_on_degraded` applies; the
        // job's message rides along for the degraded alert.
        let HeartbeatVerdict::OnTime(outcome) = judge_heartbeat(
            &every,
            Some(&beat(990, CheckStatus::Degraded, Some("disk 91% full"))),
            0,
            now,
        ) else {
            panic!("on time");
        };
        assert!(outcome.is_up() && outcome.is_degraded());
        assert_eq!(outcome.error.as_deref(), Some("disk 91% full"));

        // Up is up.
        let HeartbeatVerdict::OnTime(outcome) =
            judge_heartbeat(&every, Some(&beat(990, CheckStatus::Up, None)), 0, now)
        else {
            panic!("on time");
        };
        assert!(outcome.is_up() && !outcome.is_degraded());

        // Any of them, once stale, is a missing heartbeat.
        assert!(matches!(
            judge_heartbeat(&every, Some(&beat(900, CheckStatus::Down, Some("x"))), 0, now),
            HeartbeatVerdict::Overdue(_, reason) if reason == "missing heartbeat"
        ));
    }

    #[test]
    fn a_monitor_that_never_pinged_alerts_after_its_first_window() {
        let every = Cadence::Every(60);
        // Expected since t=1000: unknown within the interval...
        assert!(matches!(
            judge_heartbeat(&every, None, 1000, 1060),
            HeartbeatVerdict::Unknown
        ));
        // ...then overdue, saying why.
        assert!(matches!(
            judge_heartbeat(&every, None, 1000, 1061),
            HeartbeatVerdict::Overdue(_, reason) if reason == "no heartbeat received yet"
        ));

        // Scheduled: the first run after it was expected, plus the grace.
        let day = 86_400;
        let since = day + 3600; // 01:00, day 2
        let run = day + 3 * 3600; // 03:00, day 2
        assert!(matches!(
            judge_heartbeat(&nightly(), None, since, run + 1800),
            HeartbeatVerdict::Unknown
        ));
        assert!(matches!(
            judge_heartbeat(&nightly(), None, since, run + 1801),
            HeartbeatVerdict::Overdue(..)
        ));
    }

    #[tokio::test]
    async fn heartbeat_expectation_survives_restarts() {
        let store = Store::in_memory().await;
        assert_eq!(heartbeat_expected_since(&store, "job", 1000).await, 1000);
        // A later task start (daemon restart) keeps the original deadline.
        assert_eq!(heartbeat_expected_since(&store, "job", 5000).await, 1000);
        // Per id.
        assert_eq!(heartbeat_expected_since(&store, "other", 5000).await, 5000);
    }

    #[tokio::test]
    async fn explicit_down_push_is_evaluated_and_never_recorded_as_a_miss() {
        let store = Store::in_memory().await;
        let now = chrono::Utc::now().timestamp();
        db::insert_push(
            &store,
            "job",
            CheckStatus::Down,
            None,
            Some("backup failed"),
        )
        .await
        .unwrap();
        let outcome = heartbeat_outcome_for(&store, "job", &Cadence::Every(60), now, 0, now)
            .await
            .expect("an outcome");
        assert!(!outcome.is_up());
        assert_eq!(outcome.error.as_deref(), Some("backup failed"));
        // Only the push itself is stored: an on-time down records no miss.
        assert_eq!(db::recent_checks(&store, "job", 10).await.unwrap().len(), 1);

        // Never pinged and past its first window: a recorded miss.
        let outcome =
            heartbeat_outcome_for(&store, "silent", &Cadence::Every(60), now - 120, 0, now)
                .await
                .expect("an outcome");
        assert_eq!(outcome.error.as_deref(), Some("no heartbeat received yet"));
        let rows = db::recent_checks(&store, "silent", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, CheckStatus::Down);
        // And that miss is not a heartbeat: still never pinged.
        assert_eq!(db::last_heartbeat(&store, "silent").await.unwrap(), None);
    }

    /// A push monitor is looked at every minute but its miss weighs one check
    /// per interval: a nightly job that skipped one night must not fill the
    /// day with down rows and sink its availability.
    #[tokio::test]
    async fn an_overdue_heartbeat_is_recorded_once_per_interval() {
        let store = Store::in_memory().await;
        let now = chrono::Utc::now().timestamp();
        let rows = || async {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM checks WHERE monitor_id = 'nightly'")
                .fetch_one(store.sqlx())
                .await
                .unwrap()
        };
        let day = 86_400;
        let cadence = Cadence::Every(90_000);
        // Never pinged, expected for two days: overdue at every look...
        for _ in 0..5 {
            let outcome =
                heartbeat_outcome_for(&store, "nightly", &cadence, now - 2 * day, day, now)
                    .await
                    .expect("an outcome");
            assert!(!outcome.is_up());
            assert!(outcome.reason.is_some_and(FailureKind::is_missed_heartbeat));
        }
        // ...and one row says so for the whole interval.
        assert_eq!(rows().await, 1);

        // The interval is counted from the last row, to the second. That
        // row moved 5 000 s into the past: within an interval of 5 001 s
        // nothing is added, at 5 000 s the next miss is due.
        sqlx::query("UPDATE checks SET time = ? WHERE monitor_id = 'nightly'")
            .bind(now - 5000)
            .execute(store.sqlx())
            .await
            .unwrap();
        heartbeat_outcome_for(&store, "nightly", &cadence, now - 2 * day, 5001, now).await;
        assert_eq!(rows().await, 1, "recorded again inside the interval");
        heartbeat_outcome_for(&store, "nightly", &cadence, now - 2 * day, 5000, now).await;
        assert_eq!(rows().await, 2, "not recorded once the interval had passed");

        // Recording every time is still what a peer watch asks for: a row
        // one second old is no reason to skip.
        sqlx::query("UPDATE checks SET time = time - 1 WHERE monitor_id = 'nightly'")
            .execute(store.sqlx())
            .await
            .unwrap();
        heartbeat_outcome_for(&store, "nightly", &cadence, now - 2 * day, 0, now).await;
        assert_eq!(rows().await, 3);
    }
}
