//! `SQLite` persistence layer (sqlx). All timestamps are unix epoch seconds (UTC).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
// Re-exported so the CLI can hold a pool without depending on sqlx directly.
pub use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use tokio::sync::watch;

use crate::SECONDS_PER_DAY;
use crate::config::{Config, Peer};
use crate::probe::Outcome;

const PRUNE_INTERVAL: Duration = Duration::from_hours(6);

/// Raw checks roll up into hourly buckets once older than this.
const DOWNSAMPLE_HOURLY_AFTER_DAYS: i64 = 7;
/// Hourly buckets roll up into daily ones (and are pruned) once older than this.
const DOWNSAMPLE_DAILY_AFTER_DAYS: i64 = 90;
/// Daily buckets and closed incidents are kept this long.
const AGGREGATE_RETENTION_DAYS: i64 = 365;

/// Latest stored check for a monitor.
#[derive(Debug, sqlx::FromRow)]
pub struct Latest {
    pub time: i64,
    pub latency_ms: Option<i64>,
    pub status: i64,
    /// Failure reason (or response snippet) when the check was not up.
    pub error: Option<String>,
}

/// Per-day aggregate of check statuses.
#[derive(Debug, sqlx::FromRow)]
pub struct DayRow {
    pub day: String,
    pub up: i64,
    pub down: i64,
    pub degraded: i64,
}

/// A single latency sample, also serialized directly by the latency API.
#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct Point {
    pub t: i64,
    pub latency_ms: i64,
}

/// The embedded database migrations.
#[must_use]
pub fn migrator() -> sqlx::migrate::Migrator {
    sqlx::migrate!("./migrations")
}

/// Open the pool (creating the file if needed, WAL mode) and run migrations.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or migrations fail.
pub async fn connect(database_path: &str) -> anyhow::Result<SqlitePool> {
    // The database holds failure snippets, push payloads and incident detail, so
    // create it private (0600) rather than at the process umask. SQLite then
    // mirrors that mode onto the -wal/-shm sidecars it spawns.
    precreate_private(database_path)?;
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        // NORMAL is the recommended durability level under WAL: writes fsync only
        // at checkpoint, not on every probe insert. Safe for this time series -
        // at worst a power loss drops the last few checks.
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        // Keep the hot index pages resident across the 5s summary rebuilds
        // (negative = KiB, so this is 16 MiB rather than the 2 MiB default).
        .pragma("cache_size", "-16000");

    // WAL lets readers run while one writer (the scheduler) inserts, so a roomy
    // pool keeps the parallel summary queries from queueing.
    let pool = SqlitePoolOptions::new()
        .max_connections(8)
        // Fail fast under contention instead of the 30s default; a slow monitor
        // query then degrades just that card, not the whole page.
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(options)
        .await?;

    migrator().run(&pool).await?;
    Ok(pool)
}

/// Create the database file with owner-only (0600) permissions before sqlx opens
/// it, so secrets in the time series aren't world-readable. A no-op for an
/// in-memory database, an existing file, or a non-Unix platform (where file
/// modes don't apply).
#[cfg(unix)]
fn precreate_private(database_path: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    // `file:` URIs are skipped wholesale: parsing them (query params, mode=memory)
    // isn't worth it for a spelling Hora never documents. An operator who points a
    // `file:` URI at a real on-disk database owns its permissions.
    if database_path == ":memory:" || database_path.starts_with("file:") {
        return Ok(());
    }
    let path = std::path::Path::new(database_path);
    if path.exists() {
        return Ok(());
    }
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(_) => Ok(()),
        // Lost a race to create it: another opener won, the file now exists.
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err).with_context(|| format!("creating database file {database_path}")),
    }
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)]
fn precreate_private(_database_path: &str) -> anyhow::Result<()> {
    Ok(())
}

/// Insert one probe result.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_check(
    pool: &SqlitePool,
    monitor_id: &str,
    status: i64,
    outcome: &Outcome,
) -> sqlx::Result<()> {
    let now = chrono::Utc::now().timestamp();
    // OR IGNORE: a same-second duplicate (UNIQUE monitor_id, time) is a no-op.
    sqlx::query(
        "INSERT OR IGNORE INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(now)
    .bind(monitor_id)
    .bind(status)
    .bind(outcome.latency_ms)
    .bind(outcome.status_code)
    .bind(outcome.error.as_deref())
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a heartbeat pushed via the API (status: 0 down, 1 up, 2 degraded).
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_push(
    pool: &SqlitePool,
    monitor_id: &str,
    status: i64,
    latency_ms: Option<i64>,
    message: Option<&str>,
) -> sqlx::Result<()> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT OR IGNORE INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
         VALUES (?, ?, ?, ?, NULL, ?)",
    )
    .bind(now)
    .bind(monitor_id)
    .bind(status)
    .bind(latency_ms)
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

/// The last `limit` checks for a monitor, newest first. One query serves both the
/// current status (from the statuses) and the latest sample (the first row).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn recent_checks(
    pool: &SqlitePool,
    monitor_id: &str,
    limit: i64,
) -> sqlx::Result<Vec<Latest>> {
    sqlx::query_as::<_, Latest>(
        "SELECT time, latency_ms, status, error FROM checks \
         WHERE monitor_id = ? ORDER BY time DESC LIMIT ?",
    )
    .bind(monitor_id)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// The timestamp of a monitor's most recent check, if any (push staleness).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn last_check_time(pool: &SqlitePool, monitor_id: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(time) FROM checks WHERE monitor_id = ?")
        .bind(monitor_id)
        .fetch_one(pool)
        .await
}

/// The timestamp of a monitor's most recent *positive* heartbeat (status != 0),
/// ignoring recorded misses. Heartbeat staleness must be measured from this, not
/// from [`last_check_time`]: a recorded "down" miss has a fresh timestamp, so
/// using the latter would reset the staleness clock every tick and mask a
/// continuing outage (the monitor would flap down/up and never confirm down).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn last_heartbeat_time(pool: &SqlitePool, monitor_id: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT MAX(time) FROM checks WHERE monitor_id = ? AND status != 0",
    )
    .bind(monitor_id)
    .fetch_one(pool)
    .await
}

/// `(available, total)` check counts since `since`. Available = up or degraded.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn availability(
    pool: &SqlitePool,
    monitor_id: &str,
    since: i64,
) -> sqlx::Result<(i64, i64)> {
    sqlx::query_as::<_, (i64, i64)>(
        "SELECT \
            CAST(COALESCE(SUM(CASE WHEN status IN (1, 2) THEN 1 ELSE 0 END), 0) AS INTEGER), \
            COUNT(*) \
         FROM checks WHERE monitor_id = ? AND time >= ?",
    )
    .bind(monitor_id)
    .bind(since)
    .fetch_one(pool)
    .await
}

/// Daily up/down/degraded aggregates since `since`, oldest day first.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn daily(pool: &SqlitePool, monitor_id: &str, since: i64) -> sqlx::Result<Vec<DayRow>> {
    sqlx::query_as::<_, DayRow>(
        "SELECT \
            strftime('%Y-%m-%d', time, 'unixepoch') AS day, \
            CAST(SUM(CASE WHEN status = 1 THEN 1 ELSE 0 END) AS INTEGER) AS up, \
            CAST(SUM(CASE WHEN status = 0 THEN 1 ELSE 0 END) AS INTEGER) AS down, \
            CAST(SUM(CASE WHEN status = 2 THEN 1 ELSE 0 END) AS INTEGER) AS degraded \
         FROM checks WHERE monitor_id = ? AND time >= ? \
         GROUP BY day ORDER BY day ASC",
    )
    .bind(monitor_id)
    .bind(since)
    .fetch_all(pool)
    .await
}

/// Latency samples since `since`, averaged into time buckets of `bucket_secs`
/// (must be `>= 1`), oldest first (rows without latency are skipped). Aggregating
/// in SQL caps the rows materialized at `window / bucket_secs` however dense the
/// checks are, so a wide window on a high-frequency monitor can't pull hundreds
/// of thousands of rows into memory. Buckets are anchored to the epoch, not to
/// `since`, so two consecutive requests group the same rows identically and an
/// auto-refreshing chart doesn't jitter. Ordering by the group key (`MIN(time)`
/// is monotone in it) lets `SQLite` reuse the GROUP BY sort instead of building
/// a second temp b-tree.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn latency_series(
    pool: &SqlitePool,
    monitor_id: &str,
    since: i64,
    bucket_secs: i64,
) -> sqlx::Result<Vec<Point>> {
    sqlx::query_as::<_, Point>(
        "SELECT MIN(time) AS t, CAST(AVG(latency_ms) AS INTEGER) AS latency_ms FROM checks \
         WHERE monitor_id = ?1 AND time >= ?2 AND latency_ms IS NOT NULL \
         GROUP BY time / ?3 ORDER BY time / ?3",
    )
    .bind(monitor_id)
    .bind(since)
    .bind(bucket_secs)
    .fetch_all(pool)
    .await
}

/// Store (or refresh) the latest known certificate expiry for a monitor.
///
/// # Errors
///
/// Returns an error if the upsert fails.
pub async fn upsert_cert(
    pool: &SqlitePool,
    monitor_id: &str,
    not_after: i64,
    checked_at: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO certs (monitor_id, not_after, checked_at) VALUES (?, ?, ?) \
         ON CONFLICT(monitor_id) DO UPDATE SET \
            not_after = excluded.not_after, checked_at = excluded.checked_at",
    )
    .bind(monitor_id)
    .bind(not_after)
    .bind(checked_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// The stored certificate `not_after` timestamp for a monitor, if known.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn cert_not_after(pool: &SqlitePool, monitor_id: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar::<_, i64>("SELECT not_after FROM certs WHERE monitor_id = ?")
        .bind(monitor_id)
        .fetch_optional(pool)
        .await
}

// --- Batch reads: one query for every monitor, used to build the summary ----
// The status page batches the 24h/90d aggregates here (keyed by `monitor_id`)
// instead of running them per monitor; the covering index serves every one.
// `recent_checks` stays per-monitor: an indexed `LIMIT N` is already minimal and,
// unlike a windowed batch, is correct for monitors checked less than once a day.

/// `(available, total)` per monitor since `since`. Available = up or degraded.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn availability_all(
    pool: &SqlitePool,
    since: i64,
) -> sqlx::Result<HashMap<String, (i64, i64)>> {
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT monitor_id, \
            CAST(COALESCE(SUM(CASE WHEN status IN (1, 2) THEN 1 ELSE 0 END), 0) AS INTEGER), \
            COUNT(*) \
         FROM checks WHERE time >= ? GROUP BY monitor_id",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, available, total)| (id, (available, total)))
        .collect())
}

/// Daily up/down/degraded aggregates per monitor since `since`, oldest first.
///
/// Reads the raw checks *and* the downsampled `checks_hourly` / `checks_daily`
/// buckets, so the daily bars extend beyond the raw retention window. For each
/// `(monitor, day)` the source with the most samples wins: raw is
/// authoritative while complete, and the aggregates take over for days whose
/// raw rows retention has already pruned (a partially pruned boundary day
/// resolves to whichever source still holds the full count).
///
/// # Errors
///
/// Returns an error if a query fails.
pub async fn daily_all(
    pool: &SqlitePool,
    since: i64,
) -> sqlx::Result<HashMap<String, Vec<DayRow>>> {
    let raw = sqlx::query_as::<_, (String, String, i64, i64, i64)>(
        "SELECT monitor_id, \
            strftime('%Y-%m-%d', time, 'unixepoch') AS day, \
            CAST(SUM(CASE WHEN status = 1 THEN 1 ELSE 0 END) AS INTEGER), \
            CAST(SUM(CASE WHEN status = 0 THEN 1 ELSE 0 END) AS INTEGER), \
            CAST(SUM(CASE WHEN status = 2 THEN 1 ELSE 0 END) AS INTEGER) \
         FROM checks WHERE time >= ? GROUP BY monitor_id, day",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    let hourly = sqlx::query_as::<_, (String, String, i64, i64, i64)>(
        "SELECT monitor_id, \
            strftime('%Y-%m-%d', hour, 'unixepoch') AS day, \
            SUM(up_count), SUM(down_count), SUM(degraded_count) \
         FROM checks_hourly WHERE hour >= ? GROUP BY monitor_id, day",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;
    let daily = sqlx::query_as::<_, (String, String, i64, i64, i64)>(
        "SELECT monitor_id, \
            strftime('%Y-%m-%d', day, 'unixepoch') AS day, \
            up_count, down_count, degraded_count \
         FROM checks_daily WHERE day >= ?",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    let mut best: HashMap<String, BTreeMap<String, (i64, i64, i64)>> = HashMap::new();
    for (id, day, up, down, degraded) in raw.into_iter().chain(hourly).chain(daily) {
        let slot = best.entry(id).or_default().entry(day).or_insert((0, 0, 0));
        if up + down + degraded > slot.0 + slot.1 + slot.2 {
            *slot = (up, down, degraded);
        }
    }
    // BTreeMap keys are ISO dates, so iteration order is oldest-first already.
    Ok(best
        .into_iter()
        .map(|(id, days)| {
            let rows = days
                .into_iter()
                .map(|(day, (up, down, degraded))| DayRow {
                    day,
                    up,
                    down,
                    degraded,
                })
                .collect();
            (id, rows)
        })
        .collect())
}

/// Latency samples per monitor since `since`, oldest first (NULLs skipped).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn latency_all(
    pool: &SqlitePool,
    since: i64,
) -> sqlx::Result<HashMap<String, Vec<Point>>> {
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT monitor_id, time, latency_ms FROM checks \
         WHERE time >= ? AND latency_ms IS NOT NULL ORDER BY monitor_id, time ASC",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    let mut map: HashMap<String, Vec<Point>> = HashMap::new();
    for (id, t, latency_ms) in rows {
        map.entry(id).or_default().push(Point { t, latency_ms });
    }
    Ok(map)
}

/// 24h latency percentiles (p50/p95/p99) per monitor, computed in SQL so the raw
/// samples never have to be pulled into memory. The nearest-rank rule mirrors the
/// former in-Rust computation: `rank = ceil(p * n / 100)`, clamped into `[1, n]`,
/// and the value at that rank (oldest-to-largest order) is returned.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn latency_percentiles_all(
    pool: &SqlitePool,
    since: i64,
) -> sqlx::Result<HashMap<String, (i64, i64, i64)>> {
    let rows = sqlx::query_as::<_, (String, i64, i64, i64)>(
        "WITH ranked AS ( \
             SELECT monitor_id, latency_ms, \
                    ROW_NUMBER() OVER (PARTITION BY monitor_id ORDER BY latency_ms) AS rn, \
                    COUNT(*)     OVER (PARTITION BY monitor_id)                     AS n \
             FROM checks \
             WHERE time >= ?1 AND latency_ms IS NOT NULL \
         ) \
         SELECT monitor_id, \
                MAX(CASE WHEN rn = MAX(MIN((50 * n + 99) / 100, n), 1) THEN latency_ms END), \
                MAX(CASE WHEN rn = MAX(MIN((95 * n + 99) / 100, n), 1) THEN latency_ms END), \
                MAX(CASE WHEN rn = MAX(MIN((99 * n + 99) / 100, n), 1) THEN latency_ms END) \
         FROM ranked \
         GROUP BY monitor_id",
    )
    .bind(since)
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, p50, p95, p99)| (id, (p50, p95, p99)))
        .collect())
}

/// Latency samples for the per-monitor sparkline, averaged into time buckets of
/// `bucket_secs` (must be `>= 1`) so the series stays small however dense the
/// checks are: one query, at most `window / bucket_secs` points per monitor,
/// oldest first. This caps both the memory held and the size of the rendered SVG.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn latency_sparkline_all(
    pool: &SqlitePool,
    since: i64,
    bucket_secs: i64,
) -> sqlx::Result<HashMap<String, Vec<Point>>> {
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT monitor_id, MIN(time) AS t, CAST(AVG(latency_ms) AS INTEGER) AS latency_ms \
         FROM checks \
         WHERE time >= ?1 AND latency_ms IS NOT NULL \
         GROUP BY monitor_id, (time - ?1) / ?2 \
         ORDER BY monitor_id, t ASC",
    )
    .bind(since)
    .bind(bucket_secs)
    .fetch_all(pool)
    .await?;

    let mut map: HashMap<String, Vec<Point>> = HashMap::new();
    for (id, t, latency_ms) in rows {
        map.entry(id).or_default().push(Point { t, latency_ms });
    }
    Ok(map)
}

/// Hourly average latency for one monitor since `since`: `(hour, avg_ms)`
/// pairs, hour-aligned, oldest first. Reads the raw checks *and* the
/// downsampled `checks_hourly` buckets so the series extends beyond the raw
/// retention window; where both cover an hour the raw average wins (it is
/// authoritative while complete). Feeds the latency heatmap.
///
/// # Errors
///
/// Returns an error if a query fails.
pub async fn latency_hourly(
    pool: &SqlitePool,
    monitor_id: &str,
    since: i64,
) -> sqlx::Result<Vec<(i64, i64)>> {
    let buckets = sqlx::query_as::<_, (i64, i64)>(
        "SELECT hour, avg_latency_ms FROM checks_hourly \
         WHERE monitor_id = ?1 AND hour >= ?2 AND avg_latency_ms IS NOT NULL",
    )
    .bind(monitor_id)
    .bind(since)
    .fetch_all(pool)
    .await?;
    let raw = sqlx::query_as::<_, (i64, i64)>(
        "SELECT (time / 3600) * 3600 AS hour, CAST(AVG(latency_ms) AS INTEGER) FROM checks \
         WHERE monitor_id = ?1 AND time >= ?2 AND latency_ms IS NOT NULL GROUP BY hour",
    )
    .bind(monitor_id)
    .bind(since)
    .fetch_all(pool)
    .await?;

    // Buckets first, raw second: on overlap the raw average overwrites.
    let merged: BTreeMap<i64, i64> = buckets.into_iter().chain(raw).collect();
    Ok(merged.into_iter().collect())
}

/// The stored certificate `not_after` for every monitor that has one.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn cert_all(pool: &SqlitePool) -> sqlx::Result<HashMap<String, i64>> {
    let rows = sqlx::query_as::<_, (String, i64)>("SELECT monitor_id, not_after FROM certs")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().collect())
}

/// Store (or refresh) a monitor's registered-domain expiration (RDAP).
///
/// # Errors
///
/// Returns an error if the upsert fails.
pub async fn upsert_domain_expiry(
    pool: &SqlitePool,
    monitor_id: &str,
    domain: &str,
    expires_at: i64,
    checked_at: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO domain_expiry (monitor_id, domain, expires_at, checked_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(monitor_id) DO UPDATE SET \
            domain = excluded.domain, expires_at = excluded.expires_at, \
            checked_at = excluded.checked_at",
    )
    .bind(monitor_id)
    .bind(domain)
    .bind(expires_at)
    .bind(checked_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// The stored `(domain, expires_at, checked_at)` for a monitor, if any. The
/// watcher uses `checked_at` to poll RDAP at most once a day per monitor, and
/// `domain` to re-query immediately when the configured domain changed.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn domain_expiry(
    pool: &SqlitePool,
    monitor_id: &str,
) -> sqlx::Result<Option<(String, i64, i64)>> {
    sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT domain, expires_at, checked_at FROM domain_expiry WHERE monitor_id = ?",
    )
    .bind(monitor_id)
    .fetch_optional(pool)
    .await
}

/// Store (or refresh) the certificate pin (SHA-256 fingerprint of the leaf public key).
///
/// # Errors
///
/// Returns an error if the upsert fails.
pub async fn upsert_cert_pin(
    pool: &SqlitePool,
    monitor_id: &str,
    fingerprint: &str,
    checked_at: i64,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO cert_pins (monitor_id, fingerprint, checked_at) VALUES (?, ?, ?) \
         ON CONFLICT(monitor_id) DO UPDATE SET \
            fingerprint = excluded.fingerprint, checked_at = excluded.checked_at",
    )
    .bind(monitor_id)
    .bind(fingerprint)
    .bind(checked_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// The stored certificate pin fingerprint for a monitor, if known.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn cert_pin_fingerprint(
    pool: &SqlitePool,
    monitor_id: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar::<_, String>("SELECT fingerprint FROM cert_pins WHERE monitor_id = ?")
        .bind(monitor_id)
        .fetch_optional(pool)
        .await
}

/// An automatically recorded incident from a down/up transition.
#[derive(Debug, sqlx::FromRow)]
pub struct Incident {
    pub id: i64,
    pub monitor_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_s: Option<i64>,
    pub cause: Option<String>,
    pub impacted: Option<String>,
    pub error: Option<String>,
    /// Operator-written annotation ("fiber cut"), set via `hora annotate`.
    pub note: Option<String>,
    /// What the service actually answered: the failing response's status line,
    /// headers and body start, captured (bounded) when the down was confirmed.
    pub snapshot: Option<String>,
    pub created_at: i64,
}

/// Record the start of an incident (monitor going down).
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_incident_start(
    pool: &SqlitePool,
    monitor_id: &str,
    error: Option<&str>,
    cause: Option<&str>,
    impacted: &[String],
    snapshot: Option<&str>,
) -> sqlx::Result<i64> {
    let now = chrono::Utc::now().timestamp();
    let impacted_json = serde_json::to_string(impacted).unwrap_or_else(|_| "[]".to_owned());
    let result = sqlx::query(
        "INSERT INTO incidents \
            (monitor_id, started_at, error, cause, impacted, snapshot, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(monitor_id)
    .bind(now)
    .bind(error)
    .bind(cause)
    .bind(&impacted_json)
    .bind(snapshot)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(result.last_insert_rowid())
}

/// Record the end of an incident (monitor recovering).
///
/// # Errors
///
/// Returns an error if the update fails.
pub async fn update_incident_end(pool: &SqlitePool, incident_id: i64) -> sqlx::Result<()> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query("UPDATE incidents SET ended_at = ?, duration_s = (? - started_at) WHERE id = ?")
        .bind(now)
        .bind(now)
        .bind(incident_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Find the most recent open incident for a monitor (one without `ended_at`).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn find_open_incident(pool: &SqlitePool, monitor_id: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM incidents WHERE monitor_id = ? AND ended_at IS NULL \
         ORDER BY started_at DESC LIMIT 1",
    )
    .bind(monitor_id)
    .fetch_optional(pool)
    .await
}

/// Fetch recent incidents for the history page/Atom feed.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn recent_incidents(pool: &SqlitePool, limit: i64) -> sqlx::Result<Vec<Incident>> {
    // The id tie-break keeps the order deterministic when incidents share a
    // start second (a cascade), and matches what [`latest_incident_id`] calls
    // "last" - so `hora annotate last` annotates the incident listed first.
    sqlx::query_as::<_, Incident>(
        "SELECT id, monitor_id, started_at, ended_at, duration_s, cause, impacted, error, note, \
            snapshot, created_at \
         FROM incidents ORDER BY started_at DESC, id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Set (or, with an empty string, clear) the operator note on an incident.
/// Returns whether the incident exists.
///
/// # Errors
///
/// Returns an error if the update fails.
pub async fn set_incident_note(pool: &SqlitePool, id: i64, note: &str) -> sqlx::Result<bool> {
    let note = (!note.is_empty()).then_some(note);
    let result = sqlx::query("UPDATE incidents SET note = ? WHERE id = ?")
        .bind(note)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// The id of the most recently started incident, if any (`hora annotate last`).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn latest_incident_id(pool: &SqlitePool) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM incidents ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
}

/// Read a value from the `meta` key-value store.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn meta_get(pool: &SqlitePool, key: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await
}

/// Write (or overwrite) a value in the `meta` key-value store.
///
/// # Errors
///
/// Returns an error if the upsert fails.
pub async fn meta_set(pool: &SqlitePool, key: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO meta (key, value) VALUES (?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// An ad-hoc alert silence, created via `hora silence` or `POST /api/silence`.
#[derive(Debug, sqlx::FromRow)]
pub struct Silence {
    pub id: i64,
    /// A monitor id, or `*` for every monitor.
    pub monitor_id: String,
    pub until: i64,
    pub reason: Option<String>,
    pub created_at: i64,
}

/// Record an ad-hoc silence: alerts for `monitor_id` (or `*` for all) are
/// muted until `until`.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_silence(
    pool: &SqlitePool,
    monitor_id: &str,
    until: i64,
    reason: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO silences (monitor_id, until, reason, created_at) VALUES (?, ?, ?, ?)")
        .bind(monitor_id)
        .bind(until)
        .bind(reason)
        .bind(chrono::Utc::now().timestamp())
        .execute(pool)
        .await?;
    Ok(())
}

/// Whether an active silence (its own or the `*` wildcard) covers `monitor_id`
/// at `now`.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn is_silenced(pool: &SqlitePool, monitor_id: &str, now: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM silences \
         WHERE (monitor_id = ?1 OR monitor_id = '*') AND until > ?2)",
    )
    .bind(monitor_id)
    .bind(now)
    .fetch_one(pool)
    .await
}

/// All silences still active at `now`, soonest to expire first.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn active_silences(pool: &SqlitePool, now: i64) -> sqlx::Result<Vec<Silence>> {
    sqlx::query_as::<_, Silence>(
        "SELECT id, monitor_id, until, reason, created_at FROM silences \
         WHERE until > ? ORDER BY until ASC",
    )
    .bind(now)
    .fetch_all(pool)
    .await
}

/// Delete every silence (active or expired), returning how many were active.
///
/// # Errors
///
/// Returns an error if the deletion fails.
pub async fn clear_silences(pool: &SqlitePool, now: i64) -> sqlx::Result<u64> {
    let active = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM silences WHERE until > ?")
        .bind(now)
        .fetch_one(pool)
        .await?;
    sqlx::query("DELETE FROM silences").execute(pool).await?;
    Ok(u64::try_from(active).unwrap_or(0))
}

/// Drop silences that have expired before `cutoff`.
async fn prune_silences(pool: &SqlitePool, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM silences WHERE until < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(())
}

/// Copy the database into `dest` with `VACUUM INTO`: a consistent, compacted
/// snapshot taken through `SQLite` itself, safe while the daemon is writing
/// (a WAL reader does not block the writer). The source is opened read-only,
/// so a backup never creates or migrates a database.
///
/// # Errors
///
/// Returns an error if `dest` already exists, the source cannot be opened, or
/// the copy fails.
pub async fn backup_into(database_path: &str, dest: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;

    // VACUUM INTO requires a fresh file; checking first gives a clearer error
    // than SQLite's "output file already exists".
    anyhow::ensure!(
        !std::path::Path::new(dest).exists(),
        "destination {dest} already exists; refusing to overwrite a backup"
    );
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .read_only(true)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .with_context(|| format!("opening {database_path} read-only"))?;
    sqlx::query("VACUUM INTO ?")
        .bind(dest)
        .execute(&pool)
        .await
        .with_context(|| format!("copying into {dest}"))?;
    pool.close().await;

    // The live database is created 0600 (it holds failure snippets and incident
    // detail); the snapshot deserves the same, but SQLite creates it at the
    // process umask - tighten it after the fact.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("setting permissions on {dest}"))?;
    }
    Ok(())
}

/// Aggregate raw checks into hourly buckets for long-term storage.
///
/// Only hours that lie *entirely* below `cutoff` are aggregated, and a bucket
/// is written once (`INSERT OR IGNORE`): a complete hour aggregated from
/// still-complete raw data is final. Recomputing it later (`OR REPLACE`) could
/// silently shrink it once retention starts eating the raw rows it came from.
///
/// # Errors
///
/// Returns an error if the aggregation fails.
pub async fn downsample_hourly(pool: &SqlitePool, cutoff: i64) -> sqlx::Result<()> {
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
         WHERE time < ? \
         GROUP BY monitor_id, hour \
         HAVING hour + 3600 <= ?",
    )
    .bind(cutoff)
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(())
}

/// Aggregate hourly buckets into daily ones for even longer-term storage.
///
/// Same write-once rule as [`downsample_hourly`]: only days entirely below
/// `cutoff`, inserted once. The latency average is weighted by each hour's
/// sample count, so a quiet hour does not skew the day.
///
/// # Errors
///
/// Returns an error if the aggregation fails.
pub async fn downsample_daily(pool: &SqlitePool, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO checks_daily \
            (monitor_id, day, up_count, down_count, degraded_count, avg_latency_ms) \
         SELECT \
           monitor_id, \
           (hour / 86400) * 86400 AS day, \
           SUM(up_count), \
           SUM(down_count), \
           SUM(degraded_count), \
           CAST(SUM(avg_latency_ms * (up_count + degraded_count)) \
                / NULLIF(SUM(CASE WHEN avg_latency_ms IS NOT NULL \
                              THEN up_count + degraded_count END), 0) AS INTEGER) \
         FROM checks_hourly \
         WHERE hour < ? \
         GROUP BY monitor_id, day \
         HAVING day + 86400 <= ?",
    )
    .bind(cutoff)
    .bind(cutoff)
    .execute(pool)
    .await?;
    Ok(())
}

/// Prune old hourly aggregates beyond the retention period.
///
/// # Errors
///
/// Returns an error if the deletion fails.
pub async fn prune_hourly(pool: &SqlitePool, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM checks_hourly WHERE hour < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(())
}

/// Prune old daily aggregates beyond the retention period.
///
/// # Errors
///
/// Returns an error if the deletion fails.
pub async fn prune_daily(pool: &SqlitePool, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM checks_daily WHERE day < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(())
}

/// Current status from the recent checks (newest first): a single failure only
/// counts as `degraded` until `threshold` consecutive failures confirm `down`.
#[must_use]
pub fn derive_status(recent: &[Latest], threshold: i64) -> &'static str {
    let Some(latest) = recent.first() else {
        return "unknown";
    };
    match latest.status {
        1 => "up",
        2 => "degraded",
        _ => {
            let needed = usize::try_from(threshold).unwrap_or(usize::MAX);
            if recent.len() >= needed && recent.iter().all(|check| check.status == 0) {
                "down"
            } else {
                "degraded"
            }
        }
    }
}

/// Background task: periodically prune each monitor's history to its retention,
/// and drop any data left behind by monitors removed from the config. A shutdown
/// signal lets it stop between ticks instead of being aborted.
#[must_use]
pub fn spawn_pruner(
    pool: &SqlitePool,
    config: watch::Receiver<Arc<Config>>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let pool = pool.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PRUNE_INTERVAL);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            let config = config.borrow().clone();
            if let Err(err) = prune(&pool, &config).await {
                tracing::warn!("pruning failed: {err}");
            }
        }
    })
}

/// Downsample old history and age the aggregates out. Failures are logged and
/// non-fatal: the retention pruning in [`prune`] still runs.
async fn roll_up_history(pool: &SqlitePool, now: i64) {
    // Downsample before any deletion: raw checks older than 7 days roll up
    // into hourly buckets, hourly buckets older than 90 days into daily ones.
    // Each bucket is written exactly once (see `downsample_hourly`), so the
    // aggregates survive after retention prunes the raw rows they came from.
    let hourly_cutoff = now - DOWNSAMPLE_HOURLY_AFTER_DAYS * SECONDS_PER_DAY;
    if let Err(err) = downsample_hourly(pool, hourly_cutoff).await {
        tracing::warn!("hourly downsampling failed: {err}");
    }
    let daily_cutoff = now - DOWNSAMPLE_DAILY_AFTER_DAYS * SECONDS_PER_DAY;
    if let Err(err) = downsample_daily(pool, daily_cutoff).await {
        tracing::warn!("daily downsampling failed: {err}");
    }

    // Prune aggregates: hourly beyond 90 days (rounded down to a whole day, so
    // only hours already rolled up into a *complete* daily bucket are dropped),
    // daily beyond a year.
    let hourly_prune_cutoff = (daily_cutoff / 86400) * 86400;
    if let Err(err) = prune_hourly(pool, hourly_prune_cutoff).await {
        tracing::warn!("hourly prune failed: {err}");
    }
    let yearly_cutoff = now - AGGREGATE_RETENTION_DAYS * SECONDS_PER_DAY;
    if let Err(err) = prune_daily(pool, yearly_cutoff).await {
        tracing::warn!("daily prune failed: {err}");
    }
    // Closed incidents age out with the daily aggregates; open ones are kept
    // (they are still being displayed, and close on the next healthy tick).
    if let Err(err) = prune_incidents(pool, yearly_cutoff).await {
        tracing::warn!("incident prune failed: {err}");
    }
    // Expired silences are dead weight the moment they lapse.
    if let Err(err) = prune_silences(pool, now).await {
        tracing::warn!("silence prune failed: {err}");
    }
}

async fn prune(pool: &SqlitePool, config: &Config) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();

    roll_up_history(pool, now).await;

    // Trim each monitor's history to its retention window. Monitors are grouped
    // by cutoff (most share the default), so this is one DELETE per distinct
    // retention rather than one per monitor.
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
        sqlx::query(
            "DELETE FROM checks WHERE time < ? \
             AND monitor_id IN (SELECT value FROM json_each(?))",
        )
        .bind(cutoff)
        .bind(&ids)
        .execute(pool)
        .await?;
    }

    // Drop everything left behind by monitors removed from the config. The ids
    // to keep travel as a single JSON array, expanded by SQLite's `json_each`
    // and matched with a `NOT EXISTS` anti-join - one static statement per
    // table, with no `IN`-list size limit.
    // Keep both monitor ids and watched peers' listen ids, so the orphan sweep
    // never deletes a peer's heartbeat history.
    let keep: Vec<&str> = config
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
    let keep = serde_json::to_string(&keep)?;
    sqlx::query(
        "DELETE FROM checks WHERE NOT EXISTS \
         (SELECT 1 FROM json_each(?) AS keep WHERE keep.value = checks.monitor_id)",
    )
    .bind(&keep)
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM certs WHERE NOT EXISTS \
         (SELECT 1 FROM json_each(?) AS keep WHERE keep.value = certs.monitor_id)",
    )
    .bind(&keep)
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM cert_pins WHERE NOT EXISTS \
         (SELECT 1 FROM json_each(?) AS keep WHERE keep.value = cert_pins.monitor_id)",
    )
    .bind(&keep)
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM domain_expiry WHERE NOT EXISTS \
         (SELECT 1 FROM json_each(?) AS keep WHERE keep.value = domain_expiry.monitor_id)",
    )
    .bind(&keep)
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM incidents WHERE NOT EXISTS \
         (SELECT 1 FROM json_each(?) AS keep WHERE keep.value = incidents.monitor_id)",
    )
    .bind(&keep)
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM checks_hourly WHERE NOT EXISTS \
         (SELECT 1 FROM json_each(?) AS keep WHERE keep.value = checks_hourly.monitor_id)",
    )
    .bind(&keep)
    .execute(pool)
    .await?;
    sqlx::query(
        "DELETE FROM checks_daily WHERE NOT EXISTS \
         (SELECT 1 FROM json_each(?) AS keep WHERE keep.value = checks_daily.monitor_id)",
    )
    .bind(&keep)
    .execute(pool)
    .await?;
    Ok(())
}

/// Drop closed incidents older than `cutoff` (by start time). Open incidents
/// are never pruned here.
async fn prune_incidents(pool: &SqlitePool, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM incidents WHERE ended_at IS NOT NULL AND started_at < ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn memory_pool() -> SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect in-memory");
        migrator().run(&pool).await.expect("run migrations");
        pool
    }

    async fn insert(pool: &SqlitePool, id: &str, time: i64, status: i64, latency: Option<i64>) {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
             VALUES (?, ?, ?, ?, NULL, NULL)",
        )
        .bind(time)
        .bind(id)
        .bind(status)
        .bind(latency)
        .execute(pool)
        .await
        .expect("insert check");
    }

    #[tokio::test]
    async fn availability_latest_and_series() {
        let pool = memory_pool().await;
        insert(&pool, "m", 100, 1, Some(10)).await;
        insert(&pool, "m", 200, 0, None).await;
        insert(&pool, "m", 300, 1, Some(20)).await;

        assert_eq!(availability(&pool, "m", 0).await.unwrap(), (2, 3));

        let recent = recent_checks(&pool, "m", 2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(
            (recent[0].time, recent[0].status, recent[0].latency_ms),
            (300, 1, Some(20))
        );
        assert_eq!(
            recent.iter().map(|c| c.status).collect::<Vec<_>>(),
            vec![1, 0]
        );

        let series = latency_series(&pool, "m", 0, 1).await.unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].latency_ms, 10);
    }

    #[tokio::test]
    async fn last_heartbeat_ignores_recorded_misses() {
        let pool = memory_pool().await;
        insert(&pool, "m", 100, 1, Some(5)).await; // positive heartbeat
        insert(&pool, "m", 200, 0, None).await; // recorded miss (fresher)

        // last_check_time sees the miss; last_heartbeat_time skips it, so staleness
        // is measured from the real heartbeat and an outage can't reset the clock.
        assert_eq!(last_check_time(&pool, "m").await.unwrap(), Some(200));
        assert_eq!(last_heartbeat_time(&pool, "m").await.unwrap(), Some(100));

        // A monitor with only misses has no positive heartbeat at all.
        insert(&pool, "only-miss", 50, 0, None).await;
        assert_eq!(last_heartbeat_time(&pool, "only-miss").await.unwrap(), None);
    }

    #[tokio::test]
    async fn latency_percentiles_match_nearest_rank() {
        let pool = memory_pool().await;
        // Latencies 10,20,..,100 (n=10) for "x"; a down check (no latency) is ignored.
        let latencies = [10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        for (i, &latency) in latencies.iter().enumerate() {
            insert(
                &pool,
                "x",
                1000 + i64::try_from(i).unwrap(),
                1,
                Some(latency),
            )
            .await;
        }
        insert(&pool, "x", 2000, 0, None).await;
        insert(&pool, "y", 1000, 1, Some(42)).await; // single sample

        let pcts = latency_percentiles_all(&pool, 0).await.unwrap();
        // rank = ceil(p*n/100): p50 -> 5th = 50, p95 & p99 -> 10th = 100.
        assert_eq!(pcts["x"], (50, 100, 100));
        // n = 1: every percentile is the only value.
        assert_eq!(pcts["y"], (42, 42, 42));
    }

    #[tokio::test]
    async fn latency_sparkline_buckets_and_averages() {
        let pool = memory_pool().await;
        insert(&pool, "m", 0, 1, Some(10)).await;
        insert(&pool, "m", 50, 1, Some(30)).await; // same 100s bucket as t=0 -> avg 20
        insert(&pool, "m", 150, 1, Some(80)).await; // next bucket
        insert(&pool, "m", 120, 0, None).await; // no latency -> ignored

        let spark = latency_sparkline_all(&pool, 0, 100).await.unwrap();
        let points = &spark["m"];
        assert_eq!(points.len(), 2);
        assert_eq!(points[0].latency_ms, 20); // (10 + 30) / 2
        assert_eq!(points[1].latency_ms, 80);
    }

    #[tokio::test]
    async fn daily_aggregates_by_utc_day() {
        let pool = memory_pool().await;
        let day0 = 1_609_459_200; // 2021-01-01 00:00:00 UTC
        insert(&pool, "m", day0, 1, Some(5)).await;
        insert(&pool, "m", day0 + 10, 0, None).await;
        insert(&pool, "m", day0 + SECONDS_PER_DAY, 1, Some(7)).await;

        let rows = daily(&pool, "m", 0).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (rows[0].day.as_str(), rows[0].up, rows[0].down),
            ("2021-01-01", 1, 1)
        );
        assert_eq!((rows[1].day.as_str(), rows[1].up), ("2021-01-02", 1));
    }

    #[tokio::test]
    async fn latency_hourly_merges_raw_and_buckets() {
        let pool = memory_pool().await;
        let hour0 = 10 * 86_400;
        let hour1 = hour0 + 3600;

        // hour0 exists in both sources with different averages; hour1 only as
        // raw checks; an older hour only as a downsampled bucket.
        sqlx::query(
            "INSERT INTO checks_hourly \
                (monitor_id, hour, up_count, down_count, degraded_count, avg_latency_ms) \
             VALUES ('m', ?, 10, 0, 0, 999), ('m', ?, 5, 0, 0, 50)",
        )
        .bind(hour0)
        .bind(hour0 - 3600)
        .execute(&pool)
        .await
        .unwrap();
        insert(&pool, "m", hour0 + 10, 1, Some(100)).await;
        insert(&pool, "m", hour0 + 20, 1, Some(200)).await;
        insert(&pool, "m", hour1 + 10, 1, Some(300)).await;

        let cells = latency_hourly(&pool, "m", 0).await.unwrap();
        // Oldest first; on the hour0 overlap the raw average (150) wins.
        assert_eq!(cells, vec![(hour0 - 3600, 50), (hour0, 150), (hour1, 300)]);

        // `since` trims the window.
        let recent = latency_hourly(&pool, "m", hour0).await.unwrap();
        assert_eq!(recent.len(), 2);
    }

    #[tokio::test]
    async fn cert_upsert_roundtrip() {
        let pool = memory_pool().await;
        assert_eq!(cert_not_after(&pool, "m").await.unwrap(), None);
        upsert_cert(&pool, "m", 1000, 1).await.unwrap();
        upsert_cert(&pool, "m", 2000, 2).await.unwrap();
        assert_eq!(cert_not_after(&pool, "m").await.unwrap(), Some(2000));
    }

    #[tokio::test]
    async fn batch_reads_group_by_monitor() {
        let pool = memory_pool().await;
        insert(&pool, "a", 100, 1, Some(10)).await;
        insert(&pool, "a", 200, 0, None).await; // down, no latency
        insert(&pool, "b", 150, 1, Some(20)).await;
        upsert_cert(&pool, "a", 5000, 1).await.unwrap();

        let availability = availability_all(&pool, 0).await.unwrap();
        assert_eq!(availability.get("a"), Some(&(1, 2))); // 1 up of 2
        assert_eq!(availability.get("b"), Some(&(1, 1)));

        let latency = latency_all(&pool, 0).await.unwrap();
        assert_eq!(latency["a"].len(), 1); // the down check has no latency
        assert_eq!(latency["b"][0].latency_ms, 20);

        let daily = daily_all(&pool, 0).await.unwrap();
        assert!(daily.contains_key("a") && daily.contains_key("b"));

        let certs = cert_all(&pool).await.unwrap();
        assert_eq!(certs.get("a"), Some(&5000));
        assert!(!certs.contains_key("b"));
    }

    #[tokio::test]
    async fn prune_removes_orphans_and_expired_rows() {
        let pool = memory_pool().await;
        let now = chrono::Utc::now().timestamp();

        insert(&pool, "keep", now - 10, 1, Some(5)).await; // recent, retained
        insert(&pool, "keep", now - 200 * SECONDS_PER_DAY, 1, Some(5)).await; // beyond retention
        insert(&pool, "gone", now - 10, 1, Some(5)).await; // monitor removed from config
        upsert_cert(&pool, "gone", now + 1000, now).await.unwrap();

        let config: Config = toml::from_str(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "keep"
            name = "Keep"
            target = "https://example.com"
            interval_secs = 60
        "#,
        )
        .unwrap();

        prune(&pool, &config).await.unwrap();

        // The orphan monitor's data is gone entirely.
        assert!(recent_checks(&pool, "gone", 10).await.unwrap().is_empty());
        assert_eq!(cert_not_after(&pool, "gone").await.unwrap(), None);

        // The retained monitor keeps only its recent row.
        assert_eq!(latency_series(&pool, "keep", 0, 1).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn downsampling_is_write_once_and_only_buckets_complete_hours() {
        let pool = memory_pool().await;
        let hour0 = 10 * 86400; // aligned on both an hour and a day
        let hour1 = hour0 + 3600;

        insert(&pool, "m", hour0 + 10, 1, Some(100)).await;
        insert(&pool, "m", hour0 + 20, 1, Some(200)).await;
        insert(&pool, "m", hour0 + 30, 0, None).await;
        insert(&pool, "m", hour1 + 10, 1, Some(300)).await;

        // Cutoff inside hour1: only hour0 is entirely below it, so only hour0
        // is bucketed.
        downsample_hourly(&pool, hour1 + 1800).await.unwrap();
        let buckets = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            "SELECT hour, up_count, down_count, avg_latency_ms FROM checks_hourly ORDER BY hour",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(buckets, vec![(hour0, 2, 1, 150)]);

        // Write-once: a late raw row never rewrites an existing bucket.
        insert(&pool, "m", hour0 + 40, 0, None).await;
        downsample_hourly(&pool, hour1 + 3600).await.unwrap();
        let buckets = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            "SELECT hour, up_count, down_count, avg_latency_ms FROM checks_hourly ORDER BY hour",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(buckets, vec![(hour0, 2, 1, 150), (hour1, 1, 0, 300)]);

        // Daily roll-up weights the latency average by sample count:
        // (150 * 2 + 300 * 1) / 3 = 200.
        downsample_daily(&pool, hour0 + 86400).await.unwrap();
        let days = sqlx::query_as::<_, (i64, i64, i64, i64)>(
            "SELECT day, up_count, down_count, avg_latency_ms FROM checks_daily",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(days, vec![(hour0, 3, 1, 200)]);
    }

    #[tokio::test]
    async fn daily_all_reads_aggregates_once_raw_is_pruned() {
        let pool = memory_pool().await;
        let hour0 = 10 * 86400;

        insert(&pool, "m", hour0 + 10, 1, Some(100)).await;
        insert(&pool, "m", hour0 + 20, 0, None).await;
        downsample_hourly(&pool, hour0 + 3600).await.unwrap();

        // Raw still present: counts come from it (and agree with the bucket).
        let day_key = "1970-01-11";
        let bars = daily_all(&pool, 0).await.unwrap();
        assert_eq!(bars["m"][0].day, day_key);
        assert_eq!((bars["m"][0].up, bars["m"][0].down), (1, 1));

        // Raw pruned: the hourly bucket transparently takes over.
        sqlx::query("DELETE FROM checks")
            .execute(&pool)
            .await
            .unwrap();
        let bars = daily_all(&pool, 0).await.unwrap();
        assert_eq!(bars["m"][0].day, day_key);
        assert_eq!((bars["m"][0].up, bars["m"][0].down), (1, 1));
    }

    #[tokio::test]
    async fn recent_checks_carry_the_failure_reason() {
        let pool = memory_pool().await;
        insert(&pool, "m", 100, 1, Some(10)).await;
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
             VALUES (200, 'm', 0, NULL, 522, 'HTTP 522: origin timeout')",
        )
        .execute(&pool)
        .await
        .unwrap();

        let recent = recent_checks(&pool, "m", 2).await.unwrap();
        assert_eq!(recent[0].error.as_deref(), Some("HTTP 522: origin timeout"));
        assert_eq!(recent[1].error, None);
    }

    #[tokio::test]
    async fn incidents_open_close_and_prune() {
        let pool = memory_pool().await;

        let id = insert_incident_start(
            &pool,
            "m",
            Some("boom"),
            None,
            &["a".to_owned()],
            Some("HTTP/2 503\n\n<html>maintenance</html>"),
        )
        .await
        .unwrap();
        assert_eq!(find_open_incident(&pool, "m").await.unwrap(), Some(id));

        update_incident_end(&pool, id).await.unwrap();
        assert_eq!(find_open_incident(&pool, "m").await.unwrap(), None);

        let incidents = recent_incidents(&pool, 10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].error.as_deref(), Some("boom"));
        assert_eq!(
            incidents[0].snapshot.as_deref(),
            Some("HTTP/2 503\n\n<html>maintenance</html>")
        );
        assert!(incidents[0].duration_s.is_some());

        // Closed incidents prune by age; open ones never do.
        let open = insert_incident_start(&pool, "m", None, None, &[], None)
            .await
            .unwrap();
        prune_incidents(&pool, chrono::Utc::now().timestamp() + 1000)
            .await
            .unwrap();
        let incidents = recent_incidents(&pool, 10).await.unwrap();
        assert_eq!(incidents.len(), 1);
        assert_eq!(incidents[0].id, open);
    }

    #[tokio::test]
    async fn incident_notes_set_clear_and_resolve_last() {
        let pool = memory_pool().await;
        let first = insert_incident_start(&pool, "m", None, None, &[], None)
            .await
            .unwrap();
        let second = insert_incident_start(&pool, "m", None, None, &[], None)
            .await
            .unwrap();

        // `last` resolves to the most recently started incident.
        assert_eq!(latest_incident_id(&pool).await.unwrap(), Some(second));

        assert!(set_incident_note(&pool, first, "fiber cut").await.unwrap());
        let incidents = recent_incidents(&pool, 10).await.unwrap();
        let annotated = incidents.iter().find(|i| i.id == first).unwrap();
        assert_eq!(annotated.note.as_deref(), Some("fiber cut"));

        // An empty note clears; an unknown id reports "not found".
        assert!(set_incident_note(&pool, first, "").await.unwrap());
        let incidents = recent_incidents(&pool, 10).await.unwrap();
        assert_eq!(incidents.iter().find(|i| i.id == first).unwrap().note, None);
        assert!(!set_incident_note(&pool, 999, "nope").await.unwrap());
    }

    #[tokio::test]
    async fn silences_cover_wildcard_expire_and_clear() {
        let pool = memory_pool().await;
        let now = 1000;

        insert_silence(&pool, "api", now + 600, Some("deploying"))
            .await
            .unwrap();
        assert!(is_silenced(&pool, "api", now).await.unwrap());
        assert!(!is_silenced(&pool, "web", now).await.unwrap());
        // Expiry is a hard edge: at `until` the silence is over.
        assert!(!is_silenced(&pool, "api", now + 600).await.unwrap());

        // The wildcard covers every monitor.
        insert_silence(&pool, "*", now + 300, None).await.unwrap();
        assert!(is_silenced(&pool, "web", now).await.unwrap());

        let active = active_silences(&pool, now).await.unwrap();
        assert_eq!(active.len(), 2);
        // Soonest to expire first.
        assert_eq!(active[0].monitor_id, "*");
        assert_eq!(active[1].reason.as_deref(), Some("deploying"));

        // The expired row is swept by the pruner; the active ones survive.
        prune_silences(&pool, now + 450).await.unwrap();
        assert_eq!(active_silences(&pool, 0).await.unwrap().len(), 1);

        assert_eq!(clear_silences(&pool, now).await.unwrap(), 1);
        assert!(!is_silenced(&pool, "api", now).await.unwrap());
    }

    #[tokio::test]
    async fn backup_into_snapshots_and_refuses_overwrite() {
        let dir = std::env::temp_dir();
        let src = dir.join(format!("hora-test-backup-src-{}.db", std::process::id()));
        let dest = dir.join(format!("hora-test-backup-dest-{}.db", std::process::id()));
        for path in [&src, &dest] {
            let _ = std::fs::remove_file(path);
        }
        let src_s = src.to_str().unwrap();
        let dest_s = dest.to_str().unwrap();

        let pool = connect(src_s).await.unwrap();
        insert(&pool, "m", 100, 1, Some(10)).await;
        backup_into(src_s, dest_s).await.unwrap();

        // The snapshot is a self-sufficient database with the data.
        let copy = connect(dest_s).await.unwrap();
        assert_eq!(recent_checks(&copy, "m", 10).await.unwrap().len(), 1);
        copy.close().await;

        // An existing destination is never overwritten.
        let err = backup_into(src_s, dest_s).await.unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        pool.close().await;
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{src_s}{suffix}"));
            let _ = std::fs::remove_file(format!("{dest_s}{suffix}"));
        }
    }
}
