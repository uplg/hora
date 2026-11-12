//! Planned maintenance at runtime: `POST`, `GET` and `DELETE /api/maintenance`.
//! The rules (what a window covers, what is told to the peers) live in
//! `hora_core::planned`; this is the HTTP door.

use axum::Json;
use axum::extract::{Path, Query, State};
use chrono::Utc;
use serde::Deserialize;

use hora_core::config::Config;
use hora_core::db::{self, MaintenanceWindow};
use hora_core::planned::{self, CancelError, Plan, PlanError};

use crate::AppState;
use crate::auth::Operator;
use crate::error::AppError;

#[derive(Debug, Deserialize)]
pub(crate) struct MaintenanceQuery {
    /// Comma-separated monitor ids, or `all`. Exactly one of this and `host`.
    #[serde(default)]
    monitors: Option<String>,
    /// A host name: the monitors labelled `host = "<name>"`.
    #[serde(default)]
    host: Option<String>,
    /// RFC 3339 or unix seconds; absent = now.
    #[serde(default)]
    starts_at: Option<String>,
    /// How long, e.g. `10m`, `1h30m`. Capped at 7 days.
    duration: String,
    /// Why: shown on the status page.
    reason: String,
}

/// A maintenance window planned at runtime, as the API answers it.
#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct PlannedWindow {
    pub(crate) id: i64,
    /// Unix epoch seconds (UTC).
    pub(crate) starts_at: i64,
    /// Unix epoch seconds (UTC).
    pub(crate) ends_at: i64,
    pub(crate) reason: String,
    /// `planned` (not started), `active`, or `ignored` (announced by a peer
    /// this node no longer accepts maintenance from).
    pub(crate) state: &'static str,
    /// `api` (planned on this node) or `peer:<peer id>` (announced by it).
    pub(crate) origin: String,
    /// The host it was planned for, when it was planned for one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) host: Option<String>,
    /// It covers every monitor and every peer watch of this node.
    pub(crate) all: bool,
    /// Otherwise, what it covers on this node: monitor ids, and the listen
    /// ids of the watched peers it mutes.
    pub(crate) monitors: Vec<String>,
    /// Whether this node tells its peers about it on its heartbeats.
    pub(crate) announced: bool,
    /// On `POST`: `false` when the same window was already planned and is
    /// returned as it was.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) created: Option<bool>,
}

impl PlannedWindow {
    fn new(config: &Config, row: MaintenanceWindow, now: i64) -> Self {
        let resolved = planned::resolve(config, &row);
        let state = match &resolved {
            None => "ignored",
            Some(_) if row.starts_at > now => "planned",
            Some(_) => "active",
        };
        Self {
            id: row.id,
            starts_at: row.starts_at,
            ends_at: resolved
                .as_ref()
                .map_or(row.ends_at, |window| window.end.timestamp()),
            state,
            announced: planned::is_announced(config, &row),
            host: resolved.as_ref().and_then(|window| window.host.clone()),
            all: resolved.as_ref().is_some_and(|window| window.all),
            monitors: resolved.map_or_default(|window| window.monitors),
            reason: row.reason,
            origin: row.origin,
            created: None,
        }
    }
}

#[utoipa::path(
    post,
    path = "/api/maintenance",
    params(
        ("monitors" = Option<String>, Query, description = "Comma-separated monitor ids (or watched peers' listen ids), or `all`. Exactly one of `monitors` and `host`"),
        ("host" = Option<String>, Query, description = "A host name: covers the monitors labelled `host = \"<name>\"`. When it is this node's own `[health] host`, the peers are told"),
        ("starts_at" = Option<String>, Query, description = "When it starts: RFC 3339 (2026-11-13T05:00:00Z) or unix seconds; default now"),
        ("duration" = String, Query, description = "How long it lasts (e.g. 10m, 1h30m; max 7d)"),
        ("reason" = String, Query, description = "Why, shown on the status page")
    ),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The window, planned - or the one already planned for the same selector, start and end (`created: false`)", body = PlannedWindow),
        (status = 400, description = "No selector or two, unparseable start or duration, empty reason, or a window already over"),
        (status = 401, description = "Missing or wrong Bearer token, no auth_token configured, or a `?token=` in the URL (refused on writes)"),
        (status = 404, description = "Unknown monitor id or host")
    )
)]
pub(crate) async fn plan(
    State(state): State<AppState>,
    // A window mutes alerts, here and (for this node's host) on the peers:
    // an operator action, like a silence.
    Operator { config }: Operator,
    Query(query): Query<MaintenanceQuery>,
) -> Result<Json<PlannedWindow>, AppError> {
    let now = Utc::now().timestamp();
    let planned = planned::plan(
        &state.core,
        &config,
        Plan {
            monitors: query.monitors.as_deref(),
            host: query.host.as_deref(),
            starts_at: query.starts_at.as_deref(),
            duration: &query.duration,
            reason: &query.reason,
        },
        now,
    )
    .await
    .map_err(|err| match err {
        PlanError::UnknownId(_) | PlanError::UnknownHost(_) => AppError::NotFound(err.message()),
        PlanError::Database(err) => AppError::Internal(err.into()),
        other => AppError::BadRequest(other.message()),
    })?;
    if planned.created {
        // The page should say "planned" on its next view, not a refresh later.
        state.refresh_now().await;
        tracing::info!(
            id = planned.window.id,
            covers = %planned.window.selector,
            starts_at = planned.window.starts_at,
            ends_at = planned.window.ends_at,
            "maintenance window planned via API"
        );
    }
    let mut window = PlannedWindow::new(&config, planned.window, now);
    window.created = Some(planned.created);
    Ok(Json(window))
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct PlannedWindows {
    /// Active and planned windows, earliest start first.
    pub(crate) windows: Vec<PlannedWindow>,
}

#[utoipa::path(
    get,
    path = "/api/maintenance",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The maintenance windows planned at runtime that have not ended: this node's and the ones its peers announced. The config's `[[maintenance]]` windows are not listed", body = PlannedWindows),
        (status = 401, description = "Missing or wrong operator token, or no auth_token configured")
    )
)]
pub(crate) async fn list(
    State(state): State<AppState>,
    Operator { config }: Operator,
) -> Result<Json<PlannedWindows>, AppError> {
    let now = Utc::now().timestamp();
    let windows = db::pending_maintenance_windows(&state.core, now)
        .await?
        .into_iter()
        .map(|row| PlannedWindow::new(&config, row, now))
        .collect();
    Ok(Json(PlannedWindows { windows }))
}

#[utoipa::path(
    delete,
    path = "/api/maintenance/{id}",
    params(("id" = i64, Path, description = "The window's id, as `POST` and `GET /api/maintenance` answer it")),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "The window, cancelled: alerts are no longer muted, and the peers drop it at this node's next heartbeat", body = PlannedWindow),
        (status = 400, description = "The window was announced by a peer: it is cancelled there"),
        (status = 401, description = "Missing or wrong Bearer token, no auth_token configured, or a `?token=` in the URL (refused on writes)"),
        (status = 404, description = "No window with this id")
    )
)]
pub(crate) async fn cancel(
    State(state): State<AppState>,
    Operator { config }: Operator,
    Path(id): Path<i64>,
) -> Result<Json<PlannedWindow>, AppError> {
    let now = Utc::now().timestamp();
    let window = planned::cancel(&state.core, id)
        .await
        .map_err(|err| match err {
            CancelError::NotFound => AppError::NotFound("no such maintenance window"),
            CancelError::FromPeer(_) => AppError::BadRequest(
                "this window was announced by a peer: cancel it on that node, or remove \
                 accept_maintenance_secs from the peer",
            ),
            CancelError::Database(err) => AppError::Internal(err.into()),
        })?;
    state.refresh_now().await;
    tracing::info!(id, "maintenance window cancelled via API");
    Ok(Json(PlannedWindow::new(&config, window, now)))
}
