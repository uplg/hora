//! Incidents: one row per outage, opened and closed by the scheduler.

use std::collections::HashMap;

use super::Store;
use crate::probe::FailureKind;

/// An automatically recorded incident from a down/up transition.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Incident {
    pub id: i64,
    pub monitor_id: String,
    pub started_at: i64,
    pub ended_at: Option<i64>,
    pub duration_s: Option<i64>,
    pub cause: Option<String>,
    pub impacted: Option<String>,
    pub error: Option<String>,
    /// What kind of failure `error` describes; `None` on incidents recorded
    /// before the kind was stored.
    pub reason: Option<FailureKind>,
    /// Operator-written annotation ("fiber cut"), set via `hora annotate`.
    pub note: Option<String>,
    /// What the service actually answered: the failing response's status line,
    /// headers and body start, captured (bounded) when the down was confirmed.
    pub snapshot: Option<String>,
    /// The correlated event phrase ("deploy api v2.3, 3 min before"), resolved
    /// when the down was confirmed - the "what changed?" answer.
    pub event: Option<String>,
    /// The multi-vantage verdict recorded once the peers answered; `None` when
    /// no peers were asked.
    pub vantage: Option<String>,
    pub created_at: i64,
}

/// Record the start of an incident (monitor going down).
///
/// # Errors
///
/// Returns an error if the insert fails.
#[expect(
    clippy::too_many_arguments,
    reason = "one argument per column of the row it writes"
)]
pub async fn insert_incident_start(
    store: &Store,
    monitor_id: &str,
    error: Option<&str>,
    reason: Option<FailureKind>,
    cause: Option<&str>,
    impacted: &[String],
    snapshot: Option<&str>,
    event: Option<&str>,
) -> sqlx::Result<i64> {
    let now = chrono::Utc::now().timestamp();
    let impacted_json = serde_json::to_string(impacted).unwrap_or_else(|_| "[]".to_owned());
    let result = sqlx::query(
        "INSERT INTO incidents \
            (monitor_id, started_at, error, reason, cause, impacted, snapshot, event, \
             created_at, notified) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0)",
    )
    .bind(monitor_id)
    .bind(now)
    .bind(error)
    .bind(reason.map(FailureKind::code))
    .bind(cause)
    .bind(&impacted_json)
    .bind(snapshot)
    .bind(event)
    .bind(now)
    .execute(store.sqlx())
    .await?;
    Ok(result.last_insert_rowid())
}

/// Record how an incident's down was routed: `true` while it is local-only
/// (sent to the quiet channels only), `false` once the peers confirmed it and
/// the usual down went out too. Never set for an ordinary down.
///
/// # Errors
///
/// Returns an error if the update fails.
pub async fn set_incident_local_only(
    store: &Store,
    incident_id: i64,
    local_only: bool,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE incidents SET local_only = ? WHERE id = ?")
        .bind(local_only)
        .bind(incident_id)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Record that an incident's down alert was announced, or settled without a
/// notification by design (folded into its root cause, local-only with nowhere
/// to go). Until then a daemon start that finds the incident still open
/// confirms and announces the down instead of taking it for already sent.
///
/// # Errors
///
/// Returns an error if the update fails.
pub async fn mark_incident_notified(store: &Store, incident_id: i64) -> sqlx::Result<()> {
    sqlx::query("UPDATE incidents SET notified = 1 WHERE id = ? AND notified = 0")
        .bind(incident_id)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Whether an incident's down alert was announced (see
/// [`mark_incident_notified`]). An incident that no longer exists reads as
/// announced: there is nothing left to send for it.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn incident_notified(store: &Store, incident_id: i64) -> sqlx::Result<bool> {
    let notified: Option<bool> = sqlx::query_scalar("SELECT notified FROM incidents WHERE id = ?")
        .bind(incident_id)
        .fetch_optional(store.sqlx())
        .await?;
    Ok(notified.unwrap_or(true))
}

/// How an incident's down was routed (see [`set_incident_local_only`]):
/// `None` for an ordinary down, or an incident that no longer exists.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn incident_local_only(store: &Store, incident_id: i64) -> sqlx::Result<Option<bool>> {
    let row: Option<(Option<bool>,)> =
        sqlx::query_as("SELECT local_only FROM incidents WHERE id = ?")
            .bind(incident_id)
            .fetch_optional(store.sqlx())
            .await?;
    Ok(row.and_then(|(local_only,)| local_only))
}

/// Record the multi-vantage verdict on an incident, once the peers answered
/// (the incident row is written *before* the peers are consulted, so the
/// history never waits on the network).
///
/// # Errors
///
/// Returns an error if the update fails.
pub async fn update_incident_vantage(
    store: &Store,
    incident_id: i64,
    vantage: &str,
) -> sqlx::Result<()> {
    sqlx::query("UPDATE incidents SET vantage = ? WHERE id = ?")
        .bind(vantage)
        .bind(incident_id)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Record the end of an incident (monitor recovering).
///
/// # Errors
///
/// Returns an error if the update fails.
pub async fn update_incident_end(store: &Store, incident_id: i64) -> sqlx::Result<()> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query("UPDATE incidents SET ended_at = ?, duration_s = (? - started_at) WHERE id = ?")
        .bind(now)
        .bind(now)
        .bind(incident_id)
        .execute(store.sqlx())
        .await?;
    Ok(())
}

/// Record the end of every incident still open for a monitor. A monitor has
/// one outage at a time, so more than one open row is a leftover (an earlier
/// close that failed, an incident opened while the open one could not be
/// read): ending only the one held in memory would leave the others open
/// forever, and "down since" reading from the oldest of them.
///
/// # Errors
///
/// Returns an error if the update fails.
pub async fn end_open_incidents(store: &Store, monitor_id: &str) -> sqlx::Result<()> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "UPDATE incidents SET ended_at = ?, duration_s = (? - started_at) \
         WHERE monitor_id = ? AND ended_at IS NULL",
    )
    .bind(now)
    .bind(now)
    .bind(monitor_id)
    .execute(store.sqlx())
    .await?;
    Ok(())
}

/// Find the most recent open incident for a monitor (one without `ended_at`).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn find_open_incident(store: &Store, monitor_id: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM incidents WHERE monitor_id = ? AND ended_at IS NULL \
         ORDER BY started_at DESC LIMIT 1",
    )
    .bind(monitor_id)
    .fetch_optional(store.sqlx())
    .await
}

/// Fetch recent incidents for the history page/Atom feed.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn recent_incidents(store: &Store, limit: i64) -> sqlx::Result<Vec<Incident>> {
    // The id tie-break keeps the order deterministic when incidents share a
    // start second (a cascade), and matches what [`latest_incident_id`] calls
    // "last" - so `hora annotate last` annotates the incident listed first.
    sqlx::query_as::<_, Incident>(concat!(
        "SELECT ",
        incident_columns!(),
        " FROM incidents ORDER BY started_at DESC, id DESC LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(store.sqlx())
    .await
}

/// One monitor's incidents started since `since`, newest first, at most
/// `limit` (its own page: an indexed read of its rows only).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn monitor_incidents(
    store: &Store,
    monitor_id: &str,
    since: i64,
    limit: i64,
) -> sqlx::Result<Vec<Incident>> {
    sqlx::query_as::<_, Incident>(concat!(
        "SELECT ",
        incident_columns!(),
        " FROM incidents WHERE monitor_id = ? AND started_at >= ? \
         ORDER BY started_at DESC, id DESC LIMIT ?"
    ))
    .bind(monitor_id)
    .bind(since)
    .bind(limit)
    .fetch_all(store.sqlx())
    .await
}

/// Where a monitor stands in its incident log: when its last finished
/// incident ended, and since when it is down if an incident is open.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncidentMarks {
    pub last_end: Option<i64>,
    pub open_since: Option<i64>,
}

/// Every monitor's [`IncidentMarks`], in one grouped pass over the incident
/// log, for the status page's "down since" and "no incident in N days". The
/// page reads them through an [`IncidentMarksCache`] instead, which does not
/// rescan the log on every build; this one-shot read is its reference.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn incident_marks(store: &Store) -> sqlx::Result<HashMap<String, IncidentMarks>> {
    let rows = sqlx::query_as::<_, (String, Option<i64>, Option<i64>)>(
        "SELECT monitor_id, MAX(ended_at), \
            MIN(CASE WHEN ended_at IS NULL THEN started_at END) \
         FROM incidents GROUP BY monitor_id",
    )
    .fetch_all(store.sqlx())
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, last_end, open_since)| {
            (
                id,
                IncidentMarks {
                    last_end,
                    open_since,
                },
            )
        })
        .collect())
}

/// [`incident_marks`], kept between status-page builds: the first refresh
/// reads the incident log once, every later one only the incidents added
/// since (by id, the primary key) and the ones it knows are open (by id), so
/// a build costs the same whether the log holds a hundred incidents or a
/// million.
///
/// The marks follow every incident the cache has seen. Retention deletes
/// closed incidents a year after they started; one that ages out while the
/// daemon runs keeps its mark until the next restart, after which the
/// monitor reads as having no finished incident at all. "No incident in N
/// days" stays true either way: without a mark it counts from the oldest day
/// of data shown, never more than the real number of quiet days.
#[derive(Debug, Default)]
pub struct IncidentMarksCache {
    /// The newest incident id read; `None` before the first refresh.
    last_id: Option<i64>,
    /// The open incidents: id -> (monitor, started at).
    open: HashMap<i64, (String, i64)>,
    /// Per monitor, the latest end among the finished incidents read.
    last_end: HashMap<String, i64>,
}

impl IncidentMarksCache {
    /// Every monitor's [`IncidentMarks`], as [`incident_marks`] reads them.
    ///
    /// # Errors
    ///
    /// Returns an error if a query fails; the cache then starts over.
    pub async fn refresh(&mut self, store: &Store) -> sqlx::Result<HashMap<String, IncidentMarks>> {
        let result = self.refresh_inner(store).await;
        if result.is_err() {
            *self = Self::default();
        }
        result
    }

    async fn refresh_inner(
        &mut self,
        store: &Store,
    ) -> sqlx::Result<HashMap<String, IncidentMarks>> {
        // Closed (or deleted) since the last refresh: re-read by id.
        if !self.open.is_empty() {
            let ids = serde_json::to_string(&self.open.keys().collect::<Vec<_>>())
                .unwrap_or_else(|_| "[]".to_owned());
            let rows: Vec<(i64, Option<i64>)> = sqlx::query_as(
                "SELECT id, ended_at FROM incidents \
                 WHERE id IN (SELECT value FROM json_each(?))",
            )
            .bind(ids)
            .fetch_all(store.sqlx())
            .await?;
            let still: HashMap<i64, Option<i64>> = rows.into_iter().collect();
            self.open.retain(|id, (monitor, _)| match still.get(id) {
                Some(None) => true,
                Some(Some(ended)) => {
                    let end = self.last_end.entry(monitor.clone()).or_insert(*ended);
                    *end = (*end).max(*ended);
                    false
                }
                None => false,
            });
        }
        let rows: Vec<(i64, String, i64, Option<i64>)> = sqlx::query_as(
            "SELECT id, monitor_id, started_at, ended_at FROM incidents \
             WHERE id > ? ORDER BY id",
        )
        .bind(self.last_id.unwrap_or(i64::MIN))
        .fetch_all(store.sqlx())
        .await?;
        for (id, monitor, started_at, ended_at) in rows {
            self.last_id = Some(self.last_id.map_or(id, |last| last.max(id)));
            match ended_at {
                None => {
                    self.open.insert(id, (monitor, started_at));
                }
                Some(ended) => {
                    let end = self.last_end.entry(monitor).or_insert(ended);
                    *end = (*end).max(ended);
                }
            }
        }
        // The first refresh of an empty log still counts as read.
        self.last_id.get_or_insert(i64::MIN);

        let mut marks: HashMap<String, IncidentMarks> = self
            .last_end
            .iter()
            .map(|(monitor, &end)| {
                (
                    monitor.clone(),
                    IncidentMarks {
                        last_end: Some(end),
                        open_since: None,
                    },
                )
            })
            .collect();
        for (monitor, started_at) in self.open.values() {
            let mark = marks.entry(monitor.clone()).or_default();
            mark.open_since = Some(
                mark.open_since
                    .map_or(*started_at, |since| since.min(*started_at)),
            );
        }
        Ok(marks)
    }
}

/// Every incident overlapping `[since, until)`: started before `until` and
/// still open or ended after `since`. Newest first, unbounded - the monthly
/// report and the digest must count them all, not the latest N.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn incidents_between(
    store: &Store,
    since: i64,
    until: i64,
) -> sqlx::Result<Vec<Incident>> {
    sqlx::query_as::<_, Incident>(concat!(
        "SELECT ",
        incident_columns!(),
        " FROM incidents WHERE started_at < ? AND (ended_at IS NULL OR ended_at > ?) \
         ORDER BY started_at DESC, id DESC"
    ))
    .bind(until)
    .bind(since)
    .fetch_all(store.sqlx())
    .await
}

/// Fetch one incident by id, for the post-mortem page and CLI.
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn incident_by_id(store: &Store, id: i64) -> sqlx::Result<Option<Incident>> {
    sqlx::query_as::<_, Incident>(concat!(
        "SELECT ",
        incident_columns!(),
        " FROM incidents WHERE id = ?"
    ))
    .bind(id)
    .fetch_optional(store.sqlx())
    .await
}

/// Set (or, with an empty string, clear) the operator note on an incident.
/// Returns whether the incident exists.
///
/// # Errors
///
/// Returns an error if the update fails.
pub async fn set_incident_note(store: &Store, id: i64, note: &str) -> sqlx::Result<bool> {
    let note = (!note.is_empty()).then_some(note);
    let result = sqlx::query("UPDATE incidents SET note = ? WHERE id = ?")
        .bind(note)
        .bind(id)
        .execute(store.sqlx())
        .await?;
    Ok(result.rows_affected() > 0)
}

/// The id of the most recently started incident, if any (`hora annotate last`).
///
/// # Errors
///
/// Returns an error if the query fails.
pub async fn latest_incident_id(store: &Store) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar::<_, i64>(
        "SELECT id FROM incidents ORDER BY started_at DESC, id DESC LIMIT 1",
    )
    .fetch_optional(store.sqlx())
    .await
}
