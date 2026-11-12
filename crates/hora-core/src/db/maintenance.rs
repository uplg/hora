//! Maintenance windows planned at runtime (the `maintenance_windows` table):
//! the operator's, through the API or the CLI, and the ones a peer announced.
//! What a row means - which monitors it covers, whether it still applies -
//! is decided in [`crate::planned`]; this is storage only.

use super::Store;

/// A stored maintenance window.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct MaintenanceWindow {
    pub id: i64,
    /// Unix epoch seconds (UTC).
    pub starts_at: i64,
    /// Unix epoch seconds (UTC).
    pub ends_at: i64,
    pub reason: String,
    /// `*`, `ids:a,b`, `host:<name>` or `peer` (see
    /// [`crate::planned::Selector`]).
    pub selector: String,
    /// `api`, or `peer:<peer id>`.
    pub origin: String,
    /// The announcing peer's own id for the window.
    pub remote_id: Option<i64>,
    pub created_at: i64,
}

/// The `MaintenanceWindow` columns, in struct order.
macro_rules! maintenance_columns {
    () => {
        "id, starts_at, ends_at, reason, selector, origin, remote_id, created_at"
    };
}

/// A window to store.
#[derive(Debug, Clone, Copy)]
pub struct NewMaintenanceWindow<'a> {
    pub starts_at: i64,
    pub ends_at: i64,
    pub reason: &'a str,
    pub selector: &'a str,
    pub origin: &'a str,
    pub remote_id: Option<i64>,
}

/// Store a window, or find the one already stored for the same origin,
/// selector, start and end: the second value is `true` when this call
/// created it. One statement decides, so two identical requests arriving
/// together still leave one row.
///
/// # Errors
///
/// Returns an error if the insert or the read fails.
pub async fn insert_maintenance_window(
    store: &Store,
    window: NewMaintenanceWindow<'_>,
) -> sqlx::Result<(MaintenanceWindow, bool)> {
    let inserted = sqlx::query(
        "INSERT INTO maintenance_windows \
            (starts_at, ends_at, reason, selector, origin, remote_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (origin, selector, starts_at, ends_at) DO NOTHING",
    )
    .bind(window.starts_at)
    .bind(window.ends_at)
    .bind(window.reason)
    .bind(window.selector)
    .bind(window.origin)
    .bind(window.remote_id)
    .bind(chrono::Utc::now().timestamp())
    .execute(store.sqlx())
    .await?
    .rows_affected()
        > 0;
    let stored = sqlx::query_as::<_, MaintenanceWindow>(concat!(
        "SELECT ",
        maintenance_columns!(),
        " FROM maintenance_windows \
         WHERE origin = ? AND selector = ? AND starts_at = ? AND ends_at = ?"
    ))
    .bind(window.origin)
    .bind(window.selector)
    .bind(window.starts_at)
    .bind(window.ends_at)
    .fetch_one(store.sqlx())
    .await?;
    Ok((stored, inserted))
}

/// The windows that overlap `[from, to]` (bounds included, like a config
/// window's), earliest start first.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn maintenance_windows_between(
    store: &Store,
    from: i64,
    to: i64,
) -> sqlx::Result<Vec<MaintenanceWindow>> {
    sqlx::query_as::<_, MaintenanceWindow>(concat!(
        "SELECT ",
        maintenance_columns!(),
        " FROM maintenance_windows WHERE ends_at >= ? AND starts_at <= ? \
         ORDER BY starts_at ASC, id ASC"
    ))
    .bind(from)
    .bind(to)
    .fetch_all(store.sqlx())
    .await
}

/// The windows that have not ended at `now` - active and planned - earliest
/// start first.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn pending_maintenance_windows(
    store: &Store,
    now: i64,
) -> sqlx::Result<Vec<MaintenanceWindow>> {
    maintenance_windows_between(store, now, i64::MAX).await
}

/// Windows created since `since` (ended ones included), for the timeline.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn maintenance_windows_since(
    store: &Store,
    since: i64,
) -> sqlx::Result<Vec<MaintenanceWindow>> {
    sqlx::query_as::<_, MaintenanceWindow>(concat!(
        "SELECT ",
        maintenance_columns!(),
        " FROM maintenance_windows WHERE created_at >= ? ORDER BY created_at DESC, id DESC"
    ))
    .bind(since)
    .fetch_all(store.sqlx())
    .await
}

/// One window by id.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn maintenance_window(store: &Store, id: i64) -> sqlx::Result<Option<MaintenanceWindow>> {
    sqlx::query_as::<_, MaintenanceWindow>(concat!(
        "SELECT ",
        maintenance_columns!(),
        " FROM maintenance_windows WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(store.sqlx())
    .await
}

/// Delete one window; `false` when there was none with this id.
///
/// # Errors
///
/// Returns an error if the deletion fails.
pub async fn delete_maintenance_window(store: &Store, id: i64) -> sqlx::Result<bool> {
    let deleted = sqlx::query("DELETE FROM maintenance_windows WHERE id = ?")
        .bind(id)
        .execute(store.sqlx())
        .await?;
    Ok(deleted.rows_affected() > 0)
}

/// Drop the windows that ended before `cutoff`.
pub(super) async fn prune_maintenance_windows(store: &Store, cutoff: i64) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM maintenance_windows WHERE ends_at < ?")
        .bind(cutoff)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Every stored window as `(id, selector)`, for the sweep of the ones whose
/// monitors are gone.
pub(super) async fn maintenance_selectors(store: &Store) -> sqlx::Result<Vec<(i64, String)>> {
    sqlx::query_as("SELECT id, selector FROM maintenance_windows")
        .fetch_all(store.sqlx())
        .await
}
