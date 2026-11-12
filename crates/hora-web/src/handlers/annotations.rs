//! Operator writes: announcements, event markers and silences.

use axum::Json;
use axum::extract::{Query, State};
use chrono::Utc;
use serde::Deserialize;

use hora_core::db::{self};

use crate::AppState;
use crate::auth::Operator;
use crate::error::AppError;
use hora_core::MAX_EVENT_TITLE_CHARS;
use hora_core::announce::{self, Announcement};
use hora_core::silence::{self, SilenceError};

#[derive(Debug, Deserialize)]
pub(crate) struct AnnounceQuery {
    /// Banner title (required, bounded).
    title: String,
    #[serde(default)]
    body: Option<String>,
    /// `info` (default) | `warning` | `critical` | `resolved`.
    #[serde(default)]
    severity: Option<String>,
    /// Auto-expiry: a duration (`4h`, `90m`) or the next occurrence of a UTC
    /// time (`18:00`), like `hora announce --until`; absent = until cleared.
    #[serde(default)]
    until: Option<String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AnnounceResponse {
    pub(crate) id: i64,
    /// When the banner auto-expires (unix epoch seconds), if bounded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) until: Option<i64>,
}

#[utoipa::path(
    post,
    path = "/api/announce",
    params(
        ("title" = String, Query, description = "Banner title"),
        ("body" = Option<String>, Query, description = "Banner body"),
        ("severity" = Option<String>, Query, description = "info (default), warning, critical or resolved"),
        ("until" = Option<String>, Query, description = "Auto-expiry: a duration (e.g. 4h) or the next occurrence of a UTC time (HH:MM, e.g. 18:00)")
    ),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Announcement pinned to the status page", body = AnnounceResponse),
        (status = 400, description = "Empty title, unknown severity or unparseable duration"),
        (status = 401, description = "Missing or wrong Bearer token, no auth_token configured, or a `?token=` in the URL (refused on writes)")
    )
)]
pub(crate) async fn announce(
    State(state): State<AppState>,
    // Publishing to every visitor is an operator action, like /api/silence.
    _operator: Operator,
    Query(query): Query<AnnounceQuery>,
) -> Result<Json<AnnounceResponse>, AppError> {
    let severity = match query.severity.as_deref() {
        None => announce::Severity::Info,
        Some(value) => {
            announce::parse_severity(value).map_err(|err| AppError::BadRequest(err.message()))?
        }
    };
    let until = match query.until.as_deref() {
        None => None,
        Some(raw) => Some(announce::parse_until(raw, Utc::now().timestamp()).ok_or(
            AppError::BadRequest(announce::AnnounceError::InvalidUntil.message()),
        )?),
    };
    let announcement = Announcement::new(
        &query.title,
        query.body.as_deref().unwrap_or(""),
        severity,
        until,
    )
    .map_err(|err| AppError::BadRequest(err.message()))?;
    let id = announcement.pin(&state.core).await?;
    // Visitors should see the banner on their next view, not a refresh later.
    state.refresh_now().await;
    tracing::info!(
        title = %announcement.title,
        severity = %announcement.severity,
        "announcement pinned via API"
    );
    Ok(Json(AnnounceResponse { id, until }))
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AnnounceClearResponse {
    /// How many still-pinned announcements were removed.
    pub(crate) cleared: u64,
}

#[utoipa::path(
    delete,
    path = "/api/announce",
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Every ad-hoc announcement removed", body = AnnounceClearResponse),
        (status = 401, description = "Missing or wrong Bearer token, no auth_token configured, or a `?token=` in the URL (refused on writes)")
    )
)]
pub(crate) async fn announce_clear(
    State(state): State<AppState>,
    _operator: Operator,
) -> Result<Json<AnnounceClearResponse>, AppError> {
    let cleared = db::clear_announcements(&state.core, Utc::now().timestamp()).await?;
    state.refresh_now().await;
    tracing::info!(cleared, "announcements cleared via API");
    Ok(Json(AnnounceClearResponse { cleared }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct EventQuery {
    /// The event's title (required, bounded), e.g. `deploy api v2.3`.
    title: String,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct EventResponse {
    pub(crate) id: i64,
}

#[utoipa::path(
    post,
    path = "/api/event",
    params(
        ("title" = String, Query, description = "Event title, e.g. `deploy api v2.3`")
    ),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Event marker recorded", body = EventResponse),
        (status = 400, description = "Empty title"),
        (status = 401, description = "Missing or wrong Bearer token, no auth_token configured, or a `?token=` in the URL (refused on writes)")
    )
)]
pub(crate) async fn post_event(
    State(state): State<AppState>,
    // Recording a change marker is an operator action, same gesture as a
    // silence.
    _operator: Operator,
    Query(query): Query<EventQuery>,
) -> Result<Json<EventResponse>, AppError> {
    let title = hora_core::bounded(&query.title, MAX_EVENT_TITLE_CHARS);
    if title.is_empty() {
        return Err(AppError::BadRequest("title must not be empty"));
    }
    let id = db::insert_event(&state.core, &title).await?;
    // The marker should appear on the operator's sparklines on the next view,
    // not a refresh later.
    state.refresh_now().await;
    tracing::info!(%title, "event marker recorded via API");
    Ok(Json(EventResponse { id }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct SilenceQuery {
    /// Comma-separated monitor ids, or `all` (stored as the `*` wildcard).
    monitors: String,
    /// How long to mute, e.g. `10m`, `1h30m`. Capped at 7 days.
    duration: String,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct SilenceResponse {
    /// The silenced monitor ids (`["*"]` for all).
    monitors: Vec<String>,
    /// When the silence expires (unix epoch seconds, UTC).
    until: i64,
}

#[utoipa::path(
    post,
    path = "/api/silence",
    params(
        ("monitors" = String, Query, description = "Comma-separated monitor ids, or `all`"),
        ("duration" = String, Query, description = "How long to mute (e.g. 10m, 1h30m; max 7d)"),
        ("reason" = Option<String>, Query, description = "Optional note recorded with the silence")
    ),
    security(("bearer" = [])),
    responses(
        (status = 200, description = "Alerts muted until the returned time", body = SilenceResponse),
        (status = 400, description = "Unparseable duration or empty monitor list"),
        (status = 401, description = "Missing or wrong Bearer token, no auth_token configured, or a `?token=` in the URL (refused on writes)"),
        (status = 404, description = "Unknown monitor id")
    )
)]
pub(crate) async fn silence(
    State(state): State<AppState>,
    // Muting alerts is an operator action: it strictly requires the
    // configured operator token.
    Operator { config }: Operator,
    Query(query): Query<SilenceQuery>,
) -> Result<Json<SilenceResponse>, AppError> {
    let silenced = silence::apply(
        &state.core,
        &config,
        &query.monitors,
        &query.duration,
        query.reason.as_deref(),
    )
    .await
    .map_err(|err| match err {
        SilenceError::InvalidDuration => {
            AppError::BadRequest("invalid duration (use e.g. 10m, 1h30m; max 7d)")
        }
        SilenceError::NoIds => AppError::BadRequest("no monitor ids given"),
        SilenceError::UnknownId(_) => AppError::NotFound("unknown monitor id"),
        SilenceError::Database(err) => AppError::Internal(err.into()),
    })?;
    let (monitors, until) = (silenced.ids, silenced.until);
    tracing::info!(monitors = ?monitors, until, "alerts silenced via API");
    Ok(Json(SilenceResponse { monitors, until }))
}
