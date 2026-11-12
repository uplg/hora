//! Retention: downsampling the raw checks into hourly and daily buckets, aging
//! every table out, and sweeping the rows of monitors removed from the config.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use super::Store;

use crate::SECONDS_PER_DAY;
use crate::config::{Config, Peer};
use crate::histogram::LatencyHistogram;

use super::annotations::{meta_delete, meta_set};
use super::maintenance::{maintenance_selectors, prune_maintenance_windows};
use super::{CERT_PIN_AGAINST_META_PREFIX, HEARTBEAT_EXPECTED_META_PREFIX};

/// Raw checks roll up into hourly buckets once the hour ended this long ago.
/// Checks are stamped with the time they are recorded, so an ended hour only
/// needs a margin for an insert whose timestamp was taken just before the
/// boundary and committed just after it.
const HOURLY_ROLLUP_LAG_SECS: i64 = 300;
/// Hourly buckets roll up into daily ones (and are pruned) once older than this.
const DOWNSAMPLE_DAILY_AFTER_DAYS: i64 = 90;
/// Daily buckets and closed incidents are kept this long.
pub const AGGREGATE_RETENTION_DAYS: i64 = 365;

/// How long a monitor id missing from the config keeps its history before the
/// pruner deletes it. A rename, a monitor commented out for an afternoon or a
/// bad merge must not cost a year of aggregates and incidents on the next
/// prune tick; a week is long enough to notice and restore it.
pub(super) const ORPHAN_GRACE_SECS: i64 = 7 * SECONDS_PER_DAY;
/// `meta` key prefix recording when an id was first seen missing from the
/// config (`orphan_since:<id>` = unix seconds).
const ORPHAN_META_PREFIX: &str = "orphan_since:";
/// Every `meta` key prefix that holds per-id state, swept with the id's rows
/// (see [`delete_orphans`]).
const PER_ID_META_PREFIXES: &[&str] = &[
    HEARTBEAT_EXPECTED_META_PREFIX,
    CERT_PIN_AGAINST_META_PREFIX,
    crate::alert_once::META_PREFIXES[0],
    crate::alert_once::META_PREFIXES[1],
    crate::alert_once::META_PREFIXES[2],
];

/// Drop announcements whose expiry passed before `cutoff`.
pub(super) async fn prune_announcements(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM announcements WHERE until IS NOT NULL AND until < ?")
        .bind(cutoff)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Drop event markers older than `cutoff` (they age out with the closed
/// incidents they may have been correlated into).
pub(super) async fn prune_events(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM events WHERE created_at < ?")
        .bind(cutoff)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Drop pushed alerts older than `cutoff` (by `created_at`).
async fn prune_pushed_alerts(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pushed_alerts WHERE created_at < ?")
        .bind(cutoff)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Drop silences that have expired before `cutoff`.
pub(super) async fn prune_silences(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM silences WHERE until < ?")
        .bind(cutoff)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Aggregate raw checks into hourly buckets for long-term storage.
///
/// Only hours that lie *entirely* below `cutoff` are aggregated, and a bucket
/// is written once (`INSERT OR IGNORE`): a complete hour aggregated from
/// still-complete raw data is final. Recomputing it later (`OR REPLACE`) could
/// silently shrink it once retention starts eating the raw rows it came from.
///
/// Incremental: because buckets are write-once, everything at or below the
/// newest bucket is already rolled up, so only raw rows above it are scanned
/// (an indexed range of roughly one prune interval). The old full scan held
/// the write lock for seconds on a months-old database - long enough for the
/// scheduler's inserts to time out ("database is locked"). Checks are always
/// recorded at the current time, so no raw row can appear below a bucket that
/// was complete when written.
///
/// # Errors
///
/// Returns an error if the aggregation fails.
pub async fn downsample_hourly(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    let newest: Option<i64> = sqlx::query_scalar("SELECT MAX(hour) FROM checks_hourly")
        .fetch_one(store.sqlx())
        .await?;
    let floor = newest.map_or(i64::MIN, |hour| hour + 3600);
    // Only whole hours below the cutoff: with none ended since the newest
    // bucket (most ticks), there is nothing to scan at all.
    let end = cutoff.div_euclid(3600) * 3600;
    if end <= floor {
        return Ok(());
    }
    sqlx::query(
        "INSERT OR IGNORE INTO checks_hourly \
            (monitor_id, hour, up_count, down_count, degraded_count, avg_latency_ms) \
         SELECT \
           monitor_id, \
           (time / 3600) * 3600 AS hour, \
           SUM(CASE WHEN status = 1 THEN 1 ELSE 0 END), \
           SUM(CASE WHEN status = 0 THEN 1 ELSE 0 END), \
           SUM(CASE WHEN status = 2 THEN 1 ELSE 0 END), \
           CAST(AVG(latency_ms) AS INTEGER) \
         FROM checks \
         WHERE time >= ? AND time < ? \
         GROUP BY monitor_id, hour",
    )
    .bind(floor)
    .bind(end)
    .execute(store.sqlx())
    .await?;
    Ok(())
}

/// Hours whose latency histograms one [`fill_latency_histograms`] call
/// computes at most: a day and a bit, what the 24h window reads.
const MAX_HISTOGRAM_HOURS_PER_FILL: usize = 26;

/// Compute the latency histogram (see [`crate::histogram`]) of every hourly
/// bucket at or after `since` that lacks one: the hours just rolled up, and
/// after an upgrade the last day's buckets written before histograms existed.
/// One hour of raw checks is read per bucket hour (outside any write lock);
/// the hour's histograms are then written in one short transaction. Monitors
/// without a latency sample that hour get the empty histogram, so "not
/// computed" (NULL) never lingers.
///
/// # Errors
///
/// Returns an error if a query fails.
pub async fn fill_latency_histograms(store: &Store, since: i64) -> sqlx::Result<()> {
    let hours: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT hour FROM checks_hourly \
         WHERE hour >= ? AND latency_hist IS NULL ORDER BY hour DESC LIMIT ?",
    )
    .bind(since)
    .bind(i64::try_from(MAX_HISTOGRAM_HOURS_PER_FILL).unwrap_or(i64::MAX))
    .fetch_all(store.sqlx())
    .await?;
    for hour in hours {
        let rows = sqlx::query_as::<_, (String, i64, i64)>(
            "SELECT monitor_id, latency_ms, COUNT(*) FROM checks \
             WHERE time >= ? AND time < ? AND latency_ms IS NOT NULL \
             GROUP BY monitor_id, latency_ms",
        )
        .bind(hour)
        .bind(hour + 3600)
        .fetch_all(store.sqlx())
        .await?;
        let mut histograms: HashMap<String, LatencyHistogram> = HashMap::new();
        for (id, latency_ms, count) in rows {
            histograms
                .entry(id)
                .or_default()
                .record(latency_ms, u64::try_from(count).unwrap_or(0));
        }
        let mut tx = store.sqlx().begin().await?;
        for (id, histogram) in &histograms {
            sqlx::query(
                "UPDATE checks_hourly SET latency_hist = ? \
                 WHERE monitor_id = ? AND hour = ? AND latency_hist IS NULL",
            )
            .bind(histogram.encode())
            .bind(id)
            .bind(hour)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "UPDATE checks_hourly SET latency_hist = ? WHERE hour = ? AND latency_hist IS NULL",
        )
        .bind(LatencyHistogram::default().encode())
        .bind(hour)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok(())
}

/// Roll every ended hour up (counts, then latency histograms). Cheap when
/// no hour ended since the last call, so the maintenance task runs it every
/// few minutes: the status page reads ended hours from the buckets and only
/// the raw rows above them, so a fresh frontier keeps that raw tail short.
///
/// # Errors
///
/// Returns an error if a query fails.
pub async fn roll_up_recent(store: &Store, now: i64) -> sqlx::Result<()> {
    downsample_hourly(store, now - HOURLY_ROLLUP_LAG_SECS).await?;
    // The 24h window's hours, plus the one it starts in.
    fill_latency_histograms(store, now - SECONDS_PER_DAY - 3600).await
}

/// Aggregate hourly buckets into daily ones for even longer-term storage.
///
/// Same write-once rule (and the same incremental floor) as
/// [`downsample_hourly`]: only days entirely below `cutoff`, inserted once,
/// scanning only the hourly buckets above the newest daily one. The latency
/// average is weighted by each hour's sample count, so a quiet hour does not
/// skew the day. It is summed in floating point: an integer `SUM` that
/// overflows (hours of pushed latencies near the top of `i64`) is an error
/// in `SQLite`, and one such day would stop the roll-up of every monitor.
///
/// # Errors
///
/// Returns an error if the aggregation fails.
pub async fn downsample_daily(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    let newest: Option<i64> = sqlx::query_scalar("SELECT MAX(day) FROM checks_daily")
        .fetch_one(store.sqlx())
        .await?;
    let floor = newest.map_or(i64::MIN, |day| day + 86400);
    sqlx::query(
        "INSERT OR IGNORE INTO checks_daily \
            (monitor_id, day, up_count, down_count, degraded_count, avg_latency_ms) \
         SELECT \
           monitor_id, \
           (hour / 86400) * 86400 AS day, \
           SUM(up_count), \
           SUM(down_count), \
           SUM(degraded_count), \
           CAST(SUM(CAST(avg_latency_ms AS REAL) * (up_count + degraded_count)) \
                / NULLIF(SUM(CASE WHEN avg_latency_ms IS NOT NULL \
                              THEN up_count + degraded_count END), 0) AS INTEGER) \
         FROM checks_hourly \
         WHERE hour >= ? AND hour < ? \
         GROUP BY monitor_id, day \
         HAVING day + 86400 <= ?",
    )
    .bind(floor)
    .bind(cutoff)
    .bind(cutoff)
    .execute(store.sqlx())
    .await?;
    Ok(())
}

/// Prune old hourly aggregates beyond the retention period, an hour of
/// buckets per statement (see [`delete_sliced`]).
///
/// # Errors
///
/// Returns an error if the deletion fails.
pub(super) async fn prune_hourly(store: &Store, cutoff: i64, pace: &Pace) -> sqlx::Result<()> {
    delete_sliced(store, HOURLY, None, cutoff, 3600, pace).await
}

/// Prune old daily aggregates beyond the retention period.
///
/// # Errors
///
/// Returns an error if the deletion fails.
pub async fn prune_daily(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM checks_daily WHERE day < ?")
        .bind(cutoff)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Downsample old history and age the aggregates out. Failures are logged and
/// non-fatal: the retention pruning in [`prune`] still runs.
pub(super) async fn roll_up_history(store: &Store, now: i64, pace: &Pace) {
    // Downsample before any deletion: every ended hour rolls up into an hourly
    // bucket, hourly buckets older than 90 days into daily ones. Each bucket is
    // written exactly once (see `downsample_hourly`), so the aggregates survive
    // after retention prunes the raw rows they came from.
    if let Err(err) = roll_up_recent(store, now).await {
        tracing::warn!("hourly downsampling failed: {err}");
    }
    let daily_cutoff = now - DOWNSAMPLE_DAILY_AFTER_DAYS * SECONDS_PER_DAY;
    let daily_rolled = match downsample_daily(store, daily_cutoff).await {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!("daily downsampling failed: {err}");
            false
        }
    };

    // Prune aggregates: hourly beyond 90 days (rounded down to a whole day, so
    // only hours already rolled up into a *complete* daily bucket are dropped),
    // daily beyond a year. The hourly rows are the daily roll-up's only
    // source: when it failed (a passing "database is locked"), they stay for
    // the next pass rather than taking that day out of the history for good.
    let hourly_prune_cutoff = (daily_cutoff / 86400) * 86400;
    if !daily_rolled {
        tracing::warn!("hourly prune skipped: its daily roll-up did not complete");
    } else if let Err(err) = prune_hourly(store, hourly_prune_cutoff, pace).await {
        tracing::warn!("hourly prune failed: {err}");
    }
    let yearly_cutoff = now - AGGREGATE_RETENTION_DAYS * SECONDS_PER_DAY;
    if let Err(err) = prune_daily(store, yearly_cutoff).await {
        tracing::warn!("daily prune failed: {err}");
    }
    // Closed incidents age out with the daily aggregates; open ones are kept
    // (they are still being displayed, and close on the next healthy tick).
    if let Err(err) = prune_incidents(store, yearly_cutoff).await {
        tracing::warn!("incident prune failed: {err}");
    }
    // Expired silences are dead weight the moment they lapse.
    if let Err(err) = prune_silences(store, now).await {
        tracing::warn!("silence prune failed: {err}");
    }
    // Planned maintenance windows age out with the closed incidents: a
    // window that ended still marks its days on the status page's bars and
    // in the monthly reports, which reach a year back.
    if let Err(err) = prune_maintenance_windows(store, yearly_cutoff).await {
        tracing::warn!("maintenance-window prune failed: {err}");
    }
    // Same for expired announcements (the active query already hides them).
    if let Err(err) = prune_announcements(store, now).await {
        tracing::warn!("announcement prune failed: {err}");
    }
    // Pushed alerts age out with the closed incidents (a year of timeline).
    if let Err(err) = prune_pushed_alerts(store, yearly_cutoff).await {
        tracing::warn!("pushed-alert prune failed: {err}");
    }
    // Event markers too: a year covers any incident they could correlate with.
    if let Err(err) = prune_events(store, yearly_cutoff).await {
        tracing::warn!("event prune failed: {err}");
    }
}

/// The daemon's prune tick, paced and budgeted (see [`Pace::online`]).
/// Returns `false` when the budget ran out with rows still past their
/// retention: the caller comes back for them soon rather than in six hours.
pub(crate) async fn prune(store: &Store, config: &Config) -> anyhow::Result<bool> {
    let pace = Pace::online();
    prune_with(store, config, false, &pace).await?;
    Ok(pace.finished())
}

/// [`prune`], deleting the rows of removed monitors at once when
/// `purge_removed` (the explicit `hora compact --purge-removed`) instead of
/// after their grace period. `pace` paces the sliced deletes (see
/// [`delete_sliced`]): no pause and no deadline offline, where no writer waits.
pub(super) async fn prune_with(
    store: &Store,
    config: &Config,
    purge_removed: bool,
    pace: &Pace,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();

    roll_up_history(store, now, pace).await;

    // Trim each monitor's history to its retention window. Monitors are grouped
    // by cutoff (most share the default), so this is one sliced delete per
    // distinct retention rather than one per monitor.
    let mut ids_by_cutoff: HashMap<i64, Vec<&str>> = HashMap::new();
    for monitor in &config.monitors {
        let retention = i64::from(monitor.retention_days(config.alerts.default_retention_days));
        let cutoff = now - retention * SECONDS_PER_DAY;
        ids_by_cutoff
            .entry(cutoff)
            .or_default()
            .push(monitor.id.as_str());
    }
    // Watched peers store their heartbeats under `listen_id`; trim them to the
    // default retention like any other check series.
    let peer_cutoff = now - i64::from(config.alerts.default_retention_days) * SECONDS_PER_DAY;
    for peer in config.peers.iter().filter(|peer| peer.is_watched()) {
        ids_by_cutoff
            .entry(peer_cutoff)
            .or_default()
            .push(peer.listen_id());
    }
    for (cutoff, ids) in ids_by_cutoff {
        let ids = serde_json::to_string(&ids)?;
        delete_sliced(store, CHECKS, Some(&ids), cutoff, PRUNE_SLICE_SECS, pace).await?;
    }

    delete_orphans(store, config, now, purge_removed, pace).await?;

    // Keep the planner statistics current as the tables grow and the prunes
    // reshape them - same rationale as the call in [`connect`]; cheap unless
    // the data actually drifted.
    sqlx::query("PRAGMA optimize").execute(store.sqlx()).await?;
    Ok(())
}

/// The time span one retention `DELETE` on `checks` covers. A prune tick
/// deletes six hours of checks once the retention is reached (1.5M rows at
/// 700 monitors every 10 s): in one statement that held the write lock for
/// 5-10 s, past the 5 s busy timeout, so the scheduler's inserts failed with
/// "database is locked" and those checks were lost. Five minutes is ~21k
/// rows there, a fraction of a second.
pub(super) const PRUNE_SLICE_SECS: i64 = 300;
/// The span of one orphan-sweep `DELETE` on `checks`: a single monitor's
/// rows, so a day of them (8,640 at a 10 s interval) weighs about what five
/// minutes of a whole fleet does.
const ORPHAN_SLICE_SECS: i64 = SECONDS_PER_DAY;
/// The daemon's breather between two slices that deleted rows. It outlasts
/// SQLite's longest busy-retry sleep (100 ms), so an insert waiting on the
/// lock gets it before the next slice takes it again.
const ONLINE_SLICE_PAUSE: Duration = Duration::from_millis(100);
/// How long one daemon prune tick may spend deleting before it hands the
/// maintenance loop back (roll-ups, shutdown). A routine tick needs seconds;
/// only a backlog - retention lowered from a year to a month, say - runs into
/// it, and the loop then comes back for the rest a few minutes later instead
/// of deleting for hours in one go.
const ONLINE_PRUNE_BUDGET: Duration = Duration::from_secs(120);

/// How a prune paces its sliced deletes: the breather between two slices,
/// and an optional deadline after which it stops between slices and records
/// that rows are left for a later tick.
#[derive(Debug)]
pub(super) struct Pace {
    pause: Duration,
    deadline: Option<tokio::time::Instant>,
    ran_out: std::sync::atomic::AtomicBool,
}

impl Pace {
    /// The daemon's pace: [`ONLINE_SLICE_PAUSE`] between slices, within
    /// [`ONLINE_PRUNE_BUDGET`].
    fn online() -> Self {
        Self {
            pause: ONLINE_SLICE_PAUSE,
            deadline: Some(tokio::time::Instant::now() + ONLINE_PRUNE_BUDGET),
            ran_out: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// `hora compact`'s pace: no other writer waits, so no pause and no
    /// deadline.
    pub(super) fn offline() -> Self {
        Self {
            pause: Duration::ZERO,
            deadline: None,
            ran_out: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// A pace whose deadline has already passed: deletes nothing.
    #[cfg(test)]
    pub(super) fn expired() -> Self {
        Self {
            pause: Duration::ZERO,
            deadline: Some(tokio::time::Instant::now()),
            ran_out: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Whether the deadline has passed; remembers it for [`Self::finished`].
    fn out_of_time(&self) -> bool {
        let late = self
            .deadline
            .is_some_and(|deadline| tokio::time::Instant::now() >= deadline);
        if late {
            self.ran_out
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
        late
    }

    /// Whether every sliced delete ran to its cutoff.
    pub(super) fn finished(&self) -> bool {
        !self.ran_out.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// A time-keyed table whose old rows are deleted a slice at a time.
#[derive(Clone, Copy)]
pub(super) struct Sliced {
    table: &'static str,
    time: &'static str,
}

pub(super) const CHECKS: Sliced = Sliced {
    table: "checks",
    time: "time",
};
const HOURLY: Sliced = Sliced {
    table: "checks_hourly",
    time: "hour",
};

/// Delete the rows of `target` older than `cutoff` - of the monitors in `ids`
/// (a JSON array), or of all of them with `None` - oldest first, one
/// `slice_secs` span per statement (see [`delete_oldest_slice`]), sleeping
/// the pace's pause after each slice that deleted rows so the scheduler's
/// inserts get the write lock in between. One big `DELETE` held it for
/// seconds. Stops between slices once the pace's deadline has passed.
pub(super) async fn delete_sliced(
    store: &Store,
    target: Sliced,
    ids: Option<&str>,
    cutoff: i64,
    slice_secs: i64,
    pace: &Pace,
) -> sqlx::Result<()> {
    while !pace.out_of_time() {
        let Some(deleted) = delete_oldest_slice(store, target, ids, cutoff, slice_secs).await?
        else {
            break;
        };
        if deleted > 0 && !pace.pause.is_zero() {
            tokio::time::sleep(pace.pause).await;
        }
    }
    Ok(())
}

/// Delete one slice: the rows in `[oldest, oldest + slice_secs)` (capped at
/// `cutoff`), where `oldest` is the oldest row of the monitors concerned.
/// Returns how many rows went, or `None` once nothing is left below `cutoff`.
/// Seeking the oldest row each time (an index seek per monitor) skips the
/// gaps, so a sparse history never costs a statement per empty slice.
pub(super) async fn delete_oldest_slice(
    store: &Store,
    target: Sliced,
    ids: Option<&str>,
    cutoff: i64,
    slice_secs: i64,
) -> sqlx::Result<Option<u64>> {
    let Sliced { table, time } = target;
    // The interpolated names come from the `Sliced` consts, not input.
    let oldest: Option<i64> = match ids {
        Some(ids) => {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT MIN((SELECT MIN({time}) FROM {table} WHERE monitor_id = ids.value)) \
             FROM json_each(?) AS ids"
            )))
            .bind(ids)
            .fetch_one(store.sqlx())
            .await?
        }
        None => {
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT MIN({time}) FROM {table}"
            )))
            .fetch_one(store.sqlx())
            .await?
        }
    };
    let Some(from) = oldest.filter(|&oldest| oldest < cutoff) else {
        return Ok(None);
    };
    let to = from.saturating_add(slice_secs.max(1)).min(cutoff);
    let deleted = match ids {
        Some(ids) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE {time} >= ? AND {time} < ? \
             AND monitor_id IN (SELECT value FROM json_each(?))"
            )))
            .bind(from)
            .bind(to)
            .bind(ids)
            .execute(store.sqlx())
            .await?
        }
        None => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DELETE FROM {table} WHERE {time} >= ? AND {time} < ?"
            )))
            .bind(from)
            .bind(to)
            .execute(store.sqlx())
            .await?
        }
    };
    Ok(Some(deleted.rows_affected()))
}

/// The tables swept for rows left behind by removed monitors, all keyed by a
/// `monitor_id` column.
const ORPHAN_TABLES: [&str; 9] = [
    "checks",
    "certs",
    "cert_pins",
    "domain_expiry",
    "release_watch",
    "incidents",
    "pushed_alerts",
    "checks_hourly",
    "checks_daily",
];

/// Drop everything left behind by monitors removed from the config - once they
/// have been gone for [`ORPHAN_GRACE_SECS`]. Both monitor ids and watched
/// peers' listen ids are kept, so the sweep never deletes a peer's heartbeat
/// history.
///
/// The first sweep that finds an id missing records when (`orphan_since:<id>`
/// in `meta`) and logs a warning naming the ids and the deletion date; an id
/// that comes back before then simply loses its mark. Only a mark older than
/// the grace period deletes anything, so a rename or a monitor commented out
/// for a while keeps its year of history. The id's per-id `meta` state
/// ([`PER_ID_META_PREFIXES`]) goes with its rows.
///
/// The sweep first *reads* which ids each table actually holds and only then
/// deletes the orphaned ones, one targeted `DELETE` per id. The previous
/// `NOT EXISTS` anti-join was one statement per table but a full table scan
/// inside a write transaction - on `checks` that held the write lock for
/// seconds every prune tick, timing out the scheduler's inserts, all to
/// usually delete nothing. Reads don't block writers under WAL, and the
/// targeted deletes only run once a removed monitor's grace has run out; a
/// monitor's `checks` (months of them) go a day at a time, paced
/// (see [`delete_sliced`]), the other tables hold little per monitor.
pub(super) async fn delete_orphans(
    store: &Store,
    config: &Config,
    now: i64,
    purge: bool,
    pace: &Pace,
) -> anyhow::Result<()> {
    let keep: std::collections::HashSet<&str> = config
        .monitors
        .iter()
        .map(|m| m.id.as_str())
        .chain(
            config
                .peers
                .iter()
                .filter(|peer| peer.is_watched())
                .map(Peer::listen_id),
        )
        .collect();

    // Orphaned id -> the tables still holding its rows.
    let mut orphans: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    for table in ORPHAN_TABLES {
        // A plain DISTINCT read: on `checks` it walks the covering index
        // (fractions of a second even at millions of rows) and, being a read,
        // never holds the write lock. (A recursive-CTE "skip scan" would seek
        // instead of scanning, but its correlated MIN subquery loops forever
        // under some query plans - the UNIQUE autoindex triggers it - so the
        // boring query wins.)
        // The interpolated name comes from the ORPHAN_TABLES const, not input.
        let present: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT monitor_id FROM {table}"
        )))
        .fetch_all(store.sqlx())
        .await?;
        for id in present {
            if !keep.contains(id.as_str()) {
                orphans.entry(id).or_default().push(table);
            }
        }
    }

    // When each orphan was first seen missing. Marks of ids that came back
    // (or whose rows are gone anyway) are dropped, so a later removal starts
    // a fresh grace period.
    let marks: Vec<(String, String)> =
        sqlx::query_as("SELECT key, value FROM meta WHERE substr(key, 1, length(?1)) = ?1")
            .bind(ORPHAN_META_PREFIX)
            .fetch_all(store.sqlx())
            .await?;
    let mut missing_since: HashMap<String, i64> = HashMap::new();
    for (key, value) in marks {
        let id = &key[ORPHAN_META_PREFIX.len()..];
        match value.parse::<i64>() {
            Ok(since) if orphans.contains_key(id) => {
                missing_since.insert(id.to_owned(), since);
            }
            _ => meta_delete(store, &key).await?,
        }
    }

    let mut scheduled = Vec::new();
    for (id, tables) in &orphans {
        let key = format!("{ORPHAN_META_PREFIX}{id}");
        let since = missing_since.get(id).copied();
        let expired = since.is_some_and(|since| now - since >= ORPHAN_GRACE_SECS);
        if purge || expired {
            for &table in tables {
                if table == CHECKS.table {
                    let ids = serde_json::to_string(&[id])?;
                    let all = i64::MAX;
                    delete_sliced(store, CHECKS, Some(&ids), all, ORPHAN_SLICE_SECS, pace).await?;
                    continue;
                }
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM {table} WHERE monitor_id = ?"
                )))
                .bind(id)
                .execute(store.sqlx())
                .await?;
            }
            meta_delete(store, &key).await?;
            for prefix in PER_ID_META_PREFIXES {
                meta_delete(store, &format!("{prefix}{id}")).await?;
            }
            tracing::warn!(
                monitor = %id,
                "deleted the history of a monitor missing from the config since {}",
                crate::fmt::utc(since.unwrap_or(now))
            );
        } else if since.is_none() {
            meta_set(store, &key, &now.to_string()).await?;
            scheduled.push(id.as_str());
        }
    }
    // Per-id state of an id that is gone and has no rows to wait for (a push
    // monitor removed before its first heartbeat): nothing to restore, so no
    // grace - otherwise re-adding it months later would start "overdue".
    for prefix in PER_ID_META_PREFIXES {
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT key FROM meta WHERE substr(key, 1, length(?1)) = ?1")
                .bind(prefix)
                .fetch_all(store.sqlx())
                .await?;
        for key in keys {
            let id = &key[prefix.len()..];
            if !keep.contains(id) && !orphans.contains_key(id) {
                meta_delete(store, &key).await?;
            }
        }
    }
    delete_orphan_windows(store, &keep).await?;
    if !scheduled.is_empty() {
        tracing::warn!(
            "monitors no longer in the config: {}; their history will be deleted after {} \
             unless they are restored",
            scheduled.join(", "),
            crate::fmt::utc(now + ORPHAN_GRACE_SECS)
        );
    }
    Ok(())
}

/// Drop the planned maintenance windows that name monitors (`ids:a,b`) of
/// which none is left in the config: they cover nothing any more, and
/// nothing can bring them back - a window is never edited. No grace period:
/// unlike a monitor's history, there is nothing here worth restoring. A
/// window for a host, for everything or announced by a peer names no id and
/// is left to age out.
async fn delete_orphan_windows(
    store: &Store,
    keep: &std::collections::HashSet<&str>,
) -> sqlx::Result<()> {
    for (id, selector) in maintenance_selectors(store).await? {
        let Some(ids) = selector.strip_prefix("ids:") else {
            continue;
        };
        if !ids.split(',').any(|id| keep.contains(id)) {
            super::maintenance::delete_maintenance_window(store, id).await?;
        }
    }
    Ok(())
}

/// Drop closed incidents older than `cutoff` (by start time). Open incidents
/// are never pruned here.
pub(super) async fn prune_incidents(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM incidents WHERE ended_at IS NOT NULL AND started_at < ?")
        .bind(cutoff)
        .execute(store.sqlx())
        .await?;
    Ok(())
}
