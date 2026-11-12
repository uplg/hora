//! Raw check rows: probe results, push heartbeats and missed heartbeats.

use super::Store;

use crate::probe::{FailureKind, Outcome};
use crate::status::{CheckStatus, MonitorState};

/// Latest stored check for a monitor.
#[derive(Debug, sqlx::FromRow)]
pub struct Latest {
    pub time: i64,
    pub latency_ms: Option<i64>,
    pub status: CheckStatus,
    /// Failure reason (or response snippet) when the check was not up.
    pub error: Option<String>,
    /// What kind of failure `error` describes; `None` on rows written before
    /// the kind was stored.
    pub reason: Option<FailureKind>,
}

/// Where a check row came from (the `checks.source` column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckSource {
    /// The scheduler probed (or ran) the monitor.
    Probe,
    /// An explicit heartbeat on `/api/push/{id}`.
    Push,
    /// The scheduler recorded an overdue heartbeat as down.
    Miss,
}

impl CheckSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Push => "push",
            Self::Miss => "miss",
        }
    }
}

/// One row of `checks`, as written by the scheduler or the push endpoint.
struct CheckRow<'a> {
    time: i64,
    monitor_id: &'a str,
    status: CheckStatus,
    latency_ms: Option<i64>,
    status_code: Option<i64>,
    error: Option<&'a str>,
    reason: Option<FailureKind>,
    source: CheckSource,
}

/// The single `checks` insert. `UNIQUE(monitor_id, time)` makes same-second
/// writes collide, and who wins depends on the source: an explicit push
/// overwrites whatever holds its second (a `status=up` then `status=down`
/// within one second keeps the failure; a push landing in the second of a
/// recorded miss replaces the miss, so the heartbeat that ends the streak is
/// not lost). Probes and misses never overwrite: a same-second duplicate probe
/// is a no-op, and a miss must not erase the push it raced with.
async fn insert_check_row(store: &Store, row: CheckRow<'_>) -> sqlx::Result<()> {
    let sql = if row.source == CheckSource::Push {
        "INSERT INTO checks \
            (time, monitor_id, status, latency_ms, status_code, error, reason, source) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(monitor_id, time) DO UPDATE SET \
            status = excluded.status, latency_ms = excluded.latency_ms, \
            status_code = excluded.status_code, error = excluded.error, \
            reason = excluded.reason, source = excluded.source"
    } else {
        "INSERT OR IGNORE INTO checks \
            (time, monitor_id, status, latency_ms, status_code, error, reason, source) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
    };
    sqlx::query(sql)
        .bind(row.time)
        .bind(row.monitor_id)
        .bind(row.status)
        .bind(row.latency_ms)
        .bind(row.status_code)
        .bind(row.error)
        .bind(row.reason.map(FailureKind::code))
        .bind(row.source.as_str())
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Insert one probe result.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_check(store: &Store, monitor_id: &str, outcome: &Outcome) -> sqlx::Result<()> {
    insert_check_row(
        store,
        CheckRow {
            time: chrono::Utc::now().timestamp(),
            monitor_id,
            status: outcome.status,
            latency_ms: outcome.latency_ms,
            status_code: outcome.status_code,
            error: outcome.error.as_deref(),
            reason: outcome.reason,
            source: CheckSource::Probe,
        },
    )
    .await
}

/// Record an overdue heartbeat (push monitor or watched peer) as a down check,
/// marked as a miss so it is never mistaken for an explicit `status=down` push
/// (see [`last_heartbeat`]). A push already holding this second wins.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_heartbeat_miss(
    store: &Store,
    monitor_id: &str,
    kind: FailureKind,
    reason: &str,
) -> sqlx::Result<()> {
    insert_heartbeat_miss_at(
        store,
        monitor_id,
        kind,
        reason,
        chrono::Utc::now().timestamp(),
    )
    .await
}

pub(super) async fn insert_heartbeat_miss_at(
    store: &Store,
    monitor_id: &str,
    kind: FailureKind,
    reason: &str,
    time: i64,
) -> sqlx::Result<()> {
    insert_check_row(
        store,
        CheckRow {
            time,
            monitor_id,
            status: CheckStatus::Down,
            latency_ms: None,
            status_code: None,
            error: Some(reason),
            reason: Some(kind),
            source: CheckSource::Miss,
        },
    )
    .await
}

/// Record a heartbeat pushed via the API.
/// The last push within a second wins over whatever held that second.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_push(
    store: &Store,
    monitor_id: &str,
    status: CheckStatus,
    latency_ms: Option<i64>,
    message: Option<&str>,
) -> sqlx::Result<()> {
    insert_push_at(
        store,
        monitor_id,
        status,
        latency_ms,
        message,
        chrono::Utc::now().timestamp(),
    )
    .await
}

pub(super) async fn insert_push_at(
    store: &Store,
    monitor_id: &str,
    status: CheckStatus,
    latency_ms: Option<i64>,
    message: Option<&str>,
    time: i64,
) -> sqlx::Result<()> {
    insert_check_row(
        store,
        CheckRow {
            time,
            monitor_id,
            status,
            latency_ms,
            status_code: None,
            error: message,
            // The job's own words: free text, never shown publicly.
            reason: message.map(|_| FailureKind::Pushed),
            source: CheckSource::Push,
        },
    )
    .await
}

/// The last `limit` checks for a monitor, newest first. One query serves both the
/// current status (from the statuses) and the latest sample (the first row).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn recent_checks(
    store: &Store,
    monitor_id: &str,
    limit: i64,
) -> sqlx::Result<Vec<Latest>> {
    sqlx::query_as::<_, Latest>(
        "SELECT time, latency_ms, status, error, reason FROM checks \
         WHERE monitor_id = ? ORDER BY time DESC LIMIT ?",
    )
    .bind(monitor_id)
    .bind(limit)
    .fetch_all(store.sqlx())
    .await
}

/// A stored check reduced to what `hora tune` replays: when it ran, its status
/// and the latency sample, if any.
#[derive(Debug, sqlx::FromRow)]
pub struct CheckSample {
    pub time: i64,
    pub status: CheckStatus,
    pub latency_ms: Option<i64>,
}

/// Every stored check for a monitor since `since`, oldest first - the raw
/// material [`crate::tune`] replays against alternative thresholds. Raw rows
/// survive the monitor's full retention window (default 90 days): the 7-day
/// downsampling only *copies* them into the hourly aggregate, so this reaches
/// as far back as retention even though [`latency_series`](super::latency_series) reads the buckets.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn check_samples(
    store: &Store,
    monitor_id: &str,
    since: i64,
) -> sqlx::Result<Vec<CheckSample>> {
    sqlx::query_as::<_, CheckSample>(
        "SELECT time, status, latency_ms FROM checks \
         WHERE monitor_id = ? AND time >= ? ORDER BY time ASC",
    )
    .bind(monitor_id)
    .bind(since)
    .fetch_all(store.sqlx())
    .await
}

/// The timestamp of a monitor's most recent check, if any (push staleness).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn last_check_time(store: &Store, monitor_id: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(time) FROM checks WHERE monitor_id = ?")
        .bind(monitor_id)
        .fetch_one(store.sqlx())
        .await
}

/// The most recent heartbeat of a push monitor or watched peer: an explicit
/// push of any status (an `up`, a `degraded`, or a `down` carrying the job's
/// own message), never a recorded miss.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Heartbeat {
    pub time: i64,
    /// As pushed.
    pub status: CheckStatus,
    pub latency_ms: Option<i64>,
    /// The pushed `msg`, if any.
    pub error: Option<String>,
}

/// A monitor's most recent heartbeat, ignoring recorded misses. Heartbeat
/// staleness must be measured from this, not from [`last_check_time`]: a
/// recorded miss has a fresh timestamp, so using the latter would reset the
/// staleness clock every tick and mask a continuing outage (the monitor would
/// flap down/up and never confirm down).
///
/// An explicit `status=down` push *is* a heartbeat - the job ran and said it
/// failed - so it keeps the clock fresh and its status and message drive the
/// alert. Rows written before the `source` column existed are read as they
/// always were: positive statuses are heartbeats, downs are misses.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn last_heartbeat(store: &Store, monitor_id: &str) -> sqlx::Result<Option<Heartbeat>> {
    sqlx::query_as::<_, Heartbeat>(
        "SELECT time, status, latency_ms, error FROM checks \
         WHERE monitor_id = ? AND (source = 'push' OR (source = 'probe' AND status != 0)) \
         ORDER BY time DESC LIMIT 1",
    )
    .bind(monitor_id)
    .fetch_optional(store.sqlx())
    .await
}

/// The timestamp of a monitor's most recent heartbeat (see [`last_heartbeat`]).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn last_heartbeat_time(store: &Store, monitor_id: &str) -> sqlx::Result<Option<i64>> {
    Ok(last_heartbeat(store, monitor_id)
        .await?
        .map(|beat| beat.time))
}

/// A monitor's current status from its recent checks (newest first): a single
/// failure only counts as `degraded` until `threshold` consecutive failures
/// confirm `down` - except a missed heartbeat, which is down at once, as the
/// scheduler alerts on it at once (see `FailureKind::is_missed_heartbeat`).
#[must_use]
pub fn derive_status(recent: &[Latest], threshold: i64) -> MonitorState {
    let missed = recent.first().is_some_and(|latest| {
        latest.status == CheckStatus::Down
            && latest.reason.is_some_and(FailureKind::is_missed_heartbeat)
    });
    if missed {
        MonitorState::Down
    } else {
        derive_status_by_threshold(recent, threshold)
    }
}

/// [`derive_status`] without the missed-heartbeat rule: every failure, a
/// miss included, needs `threshold` in a row. A watched peer's status: its
/// alert keeps the threshold (a peer's silence is weighed with the mesh).
#[must_use]
pub fn derive_status_by_threshold(recent: &[Latest], threshold: i64) -> MonitorState {
    let Some(latest) = recent.first() else {
        return MonitorState::Unknown;
    };
    match latest.status {
        CheckStatus::Up => MonitorState::Up,
        CheckStatus::Degraded => MonitorState::Degraded,
        CheckStatus::Down => {
            let needed = usize::try_from(threshold).unwrap_or(usize::MAX);
            if recent.len() >= needed
                && recent.iter().all(|check| check.status == CheckStatus::Down)
            {
                MonitorState::Down
            } else {
                MonitorState::Degraded
            }
        }
    }
}
