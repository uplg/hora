//! What operators and producers add on top of the checks: events,
//! announcements, pushed alerts and silences, plus the `meta` key-value table.

use super::Store;
use crate::config::Severity;

/// Read a value from the `meta` key-value store.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn meta_get(store: &Store, key: &str) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key = ?")
        .bind(key)
        .fetch_optional(store.sqlx())
        .await
}

/// Write (or overwrite) a value in the `meta` key-value store.
///
/// # Errors
///
/// Returns an error if the upsert fails.
pub async fn meta_set(store: &Store, key: &str, value: &str) -> sqlx::Result<()> {
    sqlx::query(upsert_sql!("meta", "key", "value"))
        .bind(key)
        .bind(value)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Remove a key from the `meta` key-value store (a no-op when absent).
pub(crate) async fn meta_delete(store: &Store, key: &str) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM meta WHERE key = ?")
        .bind(key)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// An ad-hoc public announcement, pinned as a status-page banner.
#[derive(Debug, sqlx::FromRow)]
pub struct Announcement {
    pub id: i64,
    pub title: String,
    pub body: String,
    /// Validated at insert; a row holding anything else reads as `info`
    /// rather than breaking the page that lists it.
    pub severity: Severity,
    /// Auto-expiry (unix epoch seconds); `None` = shown until cleared.
    pub until: Option<i64>,
    pub created_at: i64,
}

// An announcement's severity is stored as its word. Decoding is lenient: the
// insert only ever writes a valid word, and a hand-edited row must cost one
// banner its colour, not the whole status page.
impl sqlx::Type<sqlx::Sqlite> for Severity {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <str as sqlx::Type<sqlx::Sqlite>>::type_info()
    }

    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <str as sqlx::Type<sqlx::Sqlite>>::compatible(ty)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for Severity {
    fn decode(value: sqlx::sqlite::SqliteValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let word = <&str as sqlx::Decode<sqlx::Sqlite>>::decode(value)?;
        Ok(word.parse().unwrap_or_default())
    }
}

/// Pin an announcement.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_announcement(
    store: &Store,
    title: &str,
    body: &str,
    severity: Severity,
    until: Option<i64>,
) -> sqlx::Result<i64> {
    let result = sqlx::query(
        "INSERT INTO announcements (title, body, severity, until, created_at) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(title)
    .bind(body)
    .bind(severity.as_str())
    .bind(until)
    .bind(chrono::Utc::now().timestamp())
    .execute(store.sqlx())
    .await?;
    Ok(result.last_insert_rowid())
}

/// The announcements still pinned at `now`, newest first.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn active_announcements(store: &Store, now: i64) -> sqlx::Result<Vec<Announcement>> {
    sqlx::query_as::<_, Announcement>(concat!(
        "SELECT ",
        announcement_columns!(),
        " FROM announcements WHERE until IS NULL OR until > ? ORDER BY created_at DESC, id DESC"
    ))
    .bind(now)
    .fetch_all(store.sqlx())
    .await
}

/// Delete every announcement (pinned or expired), returning how many were
/// still pinned.
///
/// # Errors
///
/// Returns an error if the deletion fails.
pub async fn clear_announcements(store: &Store, now: i64) -> sqlx::Result<u64> {
    // One statement: the count describes exactly the rows deleted, even when
    // an announcement is pinned concurrently.
    let deleted: Vec<Option<i64>> = sqlx::query_scalar("DELETE FROM announcements RETURNING until")
        .fetch_all(store.sqlx())
        .await?;
    Ok(count_active(deleted.into_iter(), now))
}

/// How many of the deleted rows' expiries were still in force at `now`
/// (`None` = no expiry).
fn count_active(untils: impl Iterator<Item = Option<i64>>, now: i64) -> u64 {
    let active = untils.filter(|until| until.is_none_or(|until| until > now));
    u64::try_from(active.count()).unwrap_or(u64::MAX)
}

/// An operator-recorded event marker ("deploy api v2.3"): the "what changed?"
/// answer, drawn on the latency charts and correlated into incidents.
#[derive(Debug, sqlx::FromRow)]
pub struct EventMarker {
    pub id: i64,
    pub title: String,
    pub created_at: i64,
}

/// Record an event marker.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_event(store: &Store, title: &str) -> sqlx::Result<i64> {
    let result = sqlx::query("INSERT INTO events (title, created_at) VALUES (?, ?)")
        .bind(title)
        .bind(chrono::Utc::now().timestamp())
        .execute(store.sqlx())
        .await?;
    Ok(result.last_insert_rowid())
}

/// Recent event markers, newest first (`hora event list`, /history).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn recent_events(store: &Store, limit: i64) -> sqlx::Result<Vec<EventMarker>> {
    sqlx::query_as::<_, EventMarker>(concat!(
        "SELECT ",
        event_columns!(),
        " FROM events ORDER BY created_at DESC, id DESC LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(store.sqlx())
    .await
}

/// Event markers since `since`, oldest first - the sparkline overlay.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn events_since(store: &Store, since: i64) -> sqlx::Result<Vec<EventMarker>> {
    sqlx::query_as::<_, EventMarker>(concat!(
        "SELECT ",
        event_columns!(),
        " FROM events WHERE created_at >= ? ORDER BY created_at ASC, id ASC"
    ))
    .bind(since)
    .fetch_all(store.sqlx())
    .await
}

/// The most recent event within `window_secs` before `now`, if any - the
/// incident correlation ("down 3 min after deploy api v2.3").
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn latest_event_before(
    store: &Store,
    now: i64,
    window_secs: i64,
) -> sqlx::Result<Option<EventMarker>> {
    sqlx::query_as::<_, EventMarker>(concat!(
        "SELECT ",
        event_columns!(),
        " FROM events WHERE created_at <= ? AND created_at >= ? \
         ORDER BY created_at DESC, id DESC LIMIT 1"
    ))
    .bind(now)
    .bind(now - window_secs.max(0))
    .fetch_optional(store.sqlx())
    .await
}

/// Announcements created since `since` (expired ones included - the timeline
/// wants the history, unlike the banner query which wants the active set).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn announcements_since(store: &Store, since: i64) -> sqlx::Result<Vec<Announcement>> {
    sqlx::query_as::<_, Announcement>(concat!(
        "SELECT ",
        announcement_columns!(),
        " FROM announcements WHERE created_at >= ? ORDER BY created_at DESC, id DESC"
    ))
    .bind(since)
    .fetch_all(store.sqlx())
    .await
}

/// Silences created since `since` (expired ones included, for the timeline).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn silences_since(store: &Store, since: i64) -> sqlx::Result<Vec<Silence>> {
    sqlx::query_as::<_, Silence>(concat!(
        "SELECT ",
        silence_columns!(),
        " FROM silences WHERE created_at >= ? ORDER BY created_at DESC, id DESC"
    ))
    .bind(since)
    .fetch_all(store.sqlx())
    .await
}

/// An alert pushed by an external producer to a monitor
/// (`POST /api/monitors/{id}/alert`). A timeline annotation, never a probe
/// result: it carries no status and is read only by the history page.
#[derive(Debug, sqlx::FromRow)]
pub struct PushedAlert {
    pub id: i64,
    pub monitor_id: String,
    /// `info` | `warning` | `error` | `critical` (validated at the endpoint).
    pub severity: String,
    pub title: String,
    pub message: String,
    /// The coalescing key the producer sent, if any.
    pub dedup_key: Option<String>,
    pub created_at: i64,
}

/// Record a pushed alert. Returns the new row id.
///
/// # Errors
///
/// Returns an error if the insert fails.
pub async fn insert_pushed_alert(
    store: &Store,
    monitor_id: &str,
    severity: &str,
    title: &str,
    message: &str,
    dedup_key: Option<&str>,
) -> sqlx::Result<i64> {
    let result = sqlx::query(
        "INSERT INTO pushed_alerts (monitor_id, severity, title, message, dedup_key, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(monitor_id)
    .bind(severity)
    .bind(title)
    .bind(message)
    .bind(dedup_key)
    .bind(chrono::Utc::now().timestamp())
    .execute(store.sqlx())
    .await?;
    Ok(result.last_insert_rowid())
}

/// The most recent pushed alerts, newest first - the history-page timeline.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn recent_pushed_alerts(store: &Store, limit: i64) -> sqlx::Result<Vec<PushedAlert>> {
    sqlx::query_as::<_, PushedAlert>(
        "SELECT id, monitor_id, severity, title, message, dedup_key, created_at \
         FROM pushed_alerts ORDER BY created_at DESC, id DESC LIMIT ?",
    )
    .bind(limit)
    .fetch_all(store.sqlx())
    .await
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
    store: &Store,
    monitor_id: &str,
    until: i64,
    reason: Option<&str>,
) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO silences (monitor_id, until, reason, created_at) VALUES (?, ?, ?, ?)")
        .bind(monitor_id)
        .bind(until)
        .bind(reason)
        .bind(chrono::Utc::now().timestamp())
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Record one ad-hoc silence per id in `monitor_ids`, all expiring at
/// `until`, in a single transaction: a failure leaves none of them behind.
///
/// # Errors
///
/// Returns an error if an insert or the commit fails.
pub async fn insert_silences(
    store: &Store,
    monitor_ids: &[String],
    until: i64,
    reason: Option<&str>,
) -> sqlx::Result<()> {
    let now = chrono::Utc::now().timestamp();
    let mut tx = store.sqlx().begin().await?;
    for id in monitor_ids {
        sqlx::query(
            "INSERT INTO silences (monitor_id, until, reason, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(id)
        .bind(until)
        .bind(reason)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// Whether an active silence (its own or the `*` wildcard) covers `monitor_id`
/// at `now`.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn is_silenced(store: &Store, monitor_id: &str, now: i64) -> sqlx::Result<bool> {
    sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM silences \
         WHERE (monitor_id = ?1 OR monitor_id = '*') AND until > ?2)",
    )
    .bind(monitor_id)
    .bind(now)
    .fetch_one(store.sqlx())
    .await
}

/// All silences still active at `now`, soonest to expire first.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn active_silences(store: &Store, now: i64) -> sqlx::Result<Vec<Silence>> {
    sqlx::query_as::<_, Silence>(concat!(
        "SELECT ",
        silence_columns!(),
        " FROM silences WHERE until > ? ORDER BY until ASC"
    ))
    .bind(now)
    .fetch_all(store.sqlx())
    .await
}

/// Delete every silence (active or expired), returning how many were active.
///
/// # Errors
///
/// Returns an error if the deletion fails.
pub async fn clear_silences(store: &Store, now: i64) -> sqlx::Result<u64> {
    // One statement, so the count matches the rows actually deleted.
    let deleted: Vec<i64> = sqlx::query_scalar("DELETE FROM silences RETURNING until")
        .fetch_all(store.sqlx())
        .await?;
    Ok(count_active(deleted.into_iter().map(Some), now))
}
