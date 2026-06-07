//! `SQLite` persistence layer (sqlx). All timestamps are unix epoch seconds (UTC).

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use tokio::sync::watch;

use crate::SECONDS_PER_DAY;
use crate::config::Config;
use crate::probe::Outcome;

const PRUNE_INTERVAL: Duration = Duration::from_hours(6);

/// Latest stored check for a monitor.
#[derive(Debug, sqlx::FromRow)]
pub struct Latest {
    pub time: i64,
    pub latency_ms: Option<i64>,
    pub status: i64,
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
    sqlx::query(
        "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
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
        "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
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
        "SELECT time, latency_ms, status FROM checks \
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

/// Latency samples since `since`, oldest first (rows without latency are skipped).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn latency_series(
    pool: &SqlitePool,
    monitor_id: &str,
    since: i64,
) -> sqlx::Result<Vec<Point>> {
    sqlx::query_as::<_, Point>(
        "SELECT time AS t, latency_ms FROM checks \
         WHERE monitor_id = ? AND time >= ? AND latency_ms IS NOT NULL ORDER BY time ASC",
    )
    .bind(monitor_id)
    .bind(since)
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

/// Background task: periodically prune each monitor's history to its retention,
/// and drop any data left behind by monitors removed from the config.
pub fn spawn_pruner(pool: &SqlitePool, config: watch::Receiver<Arc<Config>>) {
    let pool = pool.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(PRUNE_INTERVAL);
        loop {
            ticker.tick().await;
            let config = config.borrow().clone();
            if let Err(err) = prune(&pool, &config).await {
                tracing::warn!("pruning failed: {err}");
            }
        }
    });
}

async fn prune(pool: &SqlitePool, config: &Config) -> anyhow::Result<()> {
    let now = chrono::Utc::now().timestamp();

    // Trim each configured monitor's history to its retention window.
    for monitor in &config.monitors {
        let retention = i64::from(monitor.retention_days(config.alerts.default_retention_days));
        let cutoff = now - retention * SECONDS_PER_DAY;
        sqlx::query("DELETE FROM checks WHERE monitor_id = ? AND time < ?")
            .bind(&monitor.id)
            .bind(cutoff)
            .execute(pool)
            .await?;
    }

    // Drop everything left behind by monitors removed from the config. The ids
    // to keep travel as a single JSON array, expanded by SQLite's `json_each`
    // and matched with a `NOT EXISTS` anti-join - one static statement per
    // table, with no `IN`-list size limit.
    let keep: Vec<&str> = config.monitors.iter().map(|m| m.id.as_str()).collect();
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

        let series = latency_series(&pool, "m", 0).await.unwrap();
        assert_eq!(series.len(), 2);
        assert_eq!(series[0].latency_ms, 10);
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
    async fn cert_upsert_roundtrip() {
        let pool = memory_pool().await;
        assert_eq!(cert_not_after(&pool, "m").await.unwrap(), None);
        upsert_cert(&pool, "m", 1000, 1).await.unwrap();
        upsert_cert(&pool, "m", 2000, 2).await.unwrap();
        assert_eq!(cert_not_after(&pool, "m").await.unwrap(), Some(2000));
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
        assert_eq!(latency_series(&pool, "keep", 0).await.unwrap().len(), 1);
    }
}
