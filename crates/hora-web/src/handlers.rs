//! HTTP endpoint handlers and their request/response types.

use std::sync::{Arc, LazyLock};

use askama::Template;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;
use utoipa::OpenApi;

use hora_core::config::Kind;
use hora_core::db::{self, Point};
use hora_core::peer::{HealthReport, PeerSeen};

use crate::error::AppError;
use crate::render::{badge, status_color, svg_response, uptime_color};
use crate::summary::{
    DayCell, IncidentView, MaintenanceView, MonitorView, StatusTemplate, Summary, format_permille,
};
use crate::{
    AppState, FAVICON_SVG, FONT_WOFF2, MAX_LATENCY_HOURS, MAX_LATENCY_POINTS, MAX_PUSH_MSG_CHARS,
    SECONDS_PER_HOUR, summary_for,
};

/// The `OpenAPI` document, generated once at startup (empty if generation fails).
pub(crate) static OPENAPI_JSON: LazyLock<String> = LazyLock::new(|| {
    ApiDoc::openapi().to_pretty_json().unwrap_or_else(|err| {
        tracing::error!("failed to generate OpenAPI document: {err}");
        String::new()
    })
});

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hora API",
        description = "Read-only JSON API of a Hora uptime monitor."
    ),
    paths(summary_json, latency_json, push, status_badge, uptime_badge, healthz),
    components(schemas(
        Summary,
        MonitorView,
        IncidentView,
        MaintenanceView,
        DayCell,
        Point,
        HealthReport,
        PeerSeen
    ))
)]
struct ApiDoc;

#[utoipa::path(
    get,
    path = "/healthz",
    responses((status = 200, description = "Node health and its view of watched peers", body = HealthReport))
)]
pub(crate) async fn healthz(State(state): State<AppState>) -> Json<HealthReport> {
    let config = state.config.borrow().clone();
    Json(hora_core::peer::report(&state.pool, &config, &state.last_tick).await)
}

pub(crate) async fn favicon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        FAVICON_SVG,
    )
}

pub(crate) async fn font() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        FONT_WOFF2,
    )
}

pub(crate) async fn openapi() -> Response {
    if OPENAPI_JSON.is_empty() {
        // The document is static; an empty one means generation failed at startup.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OpenAPI generation failed",
        )
            .into_response();
    }
    (
        [(header::CONTENT_TYPE, "application/json")],
        OPENAPI_JSON.as_str(),
    )
        .into_response()
}

/// Fetch (or build) the cached summary from the request state. Infallible: a
/// failing monitor degrades to an `unknown` card rather than failing the page.
pub(crate) async fn state_summary(state: AppState) -> Arc<Summary> {
    let AppState {
        pool, config, cache, ..
    } = state;
    let config = config.borrow().clone();
    summary_for(&pool, &config, &cache).await
}

pub(crate) async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let summary = state_summary(state).await;
    let html = StatusTemplate {
        summary: summary.as_ref(),
    }
    .render()?;
    Ok(Html(html))
}

#[utoipa::path(
    get,
    path = "/api/summary",
    responses((status = 200, description = "Status of every monitor", body = Summary))
)]
pub(crate) async fn summary_json(State(state): State<AppState>) -> Json<Arc<Summary>> {
    Json(state_summary(state).await)
}

#[derive(Debug, Deserialize)]
pub(crate) struct LatencyQuery {
    #[serde(default = "default_hours")]
    hours: i64,
}

pub(crate) fn default_hours() -> i64 {
    24
}

#[utoipa::path(
    get,
    path = "/api/monitors/{id}/latency",
    params(
        ("id" = String, Path, description = "Monitor id"),
        ("hours" = Option<i64>, Query, description = "Look-back window in hours (1..=720)")
    ),
    responses(
        (status = 200, description = "Latency samples, oldest first", body = [Point]),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn latency_json(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LatencyQuery>,
) -> Result<Json<Vec<Point>>, AppError> {
    let AppState { pool, config, .. } = state;
    if !config.borrow().monitors.iter().any(|m| m.id == id) {
        return Err(AppError::NotFound("unknown monitor"));
    }
    let LatencyQuery { hours } = query;
    let since = Utc::now().timestamp() - hours.clamp(1, MAX_LATENCY_HOURS) * SECONDS_PER_HOUR;
    let points = db::latency_series(&pool, &id, since).await?;
    // Bound the response (a 10s-interval monitor over 720h is ~260k points); the
    // shape is preserved by sampling evenly.
    Ok(Json(downsample(points, MAX_LATENCY_POINTS)))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PushQuery {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    ping: Option<i64>,
}

#[utoipa::path(
    post,
    path = "/api/push/{id}",
    params(
        ("id" = String, Path, description = "Push monitor id"),
        ("token" = Option<String>, Query, description = "Push token, if the monitor sets one"),
        ("status" = Option<String>, Query, description = "up (default), down or degraded"),
        ("msg" = Option<String>, Query, description = "Optional detail recorded with the heartbeat"),
        ("ping" = Option<i64>, Query, description = "Optional round-trip latency in ms")
    ),
    responses(
        (status = 200, description = "Heartbeat recorded"),
        (status = 401, description = "Missing or wrong token"),
        (status = 404, description = "Unknown push monitor")
    )
)]
pub(crate) async fn push(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<PushQuery>,
    headers: HeaderMap,
) -> Result<&'static str, AppError> {
    let config = state.config.borrow().clone();
    // A push id is either a push monitor or a watched peer's listen id (peers
    // heartbeat the same endpoint); the expected token comes from whichever matches.
    let expected_token = if let Some(monitor) = config
        .monitors
        .iter()
        .find(|monitor| monitor.id == id && monitor.kind == Kind::Push)
    {
        monitor.push_token.as_ref()
    } else if let Some(peer) = config
        .peers
        .iter()
        .find(|peer| peer.is_watched() && peer.listen_id() == id)
    {
        peer.listen_token.as_ref()
    } else {
        return Err(AppError::NotFound("unknown push target"));
    };

    // A configured token is required; without one, the id alone authorizes. Prefer
    // the `X-Push-Token` header (kept out of access logs) over the `?token=` query.
    if let Some(expected) = expected_token {
        let provided = headers
            .get("x-push-token")
            .and_then(|value| value.to_str().ok())
            .or(query.token.as_deref());
        if !provided.is_some_and(|token| ct_eq(token, expected.as_ref())) {
            return Err(AppError::Unauthorized("invalid push token"));
        }
    }

    let status = match query.status.as_deref() {
        Some("down") => 0,
        Some("degraded") => 2,
        _ => 1,
    };
    // Bound the stored message so a buggy or hostile pusher can't bloat the DB.
    let msg = query
        .msg
        .as_deref()
        .map(|msg| msg.chars().take(MAX_PUSH_MSG_CHARS).collect::<String>());
    db::insert_push(&state.pool, &id, status, query.ping, msg.as_deref()).await?;
    Ok("ok")
}

#[utoipa::path(
    get,
    path = "/api/badge/{id}/status",
    params(("id" = String, Path, description = "Monitor id")),
    responses(
        (status = 200, description = "Status badge (SVG)"),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn status_badge(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let summary = state_summary(state).await;
    let monitor = summary
        .monitors
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| AppError::NotFound("unknown monitor"))?;
    Ok(svg_response(badge(
        "status",
        monitor.status,
        status_color(monitor.status),
    )))
}

#[utoipa::path(
    get,
    path = "/api/badge/{id}/uptime",
    params(("id" = String, Path, description = "Monitor id")),
    responses(
        (status = 200, description = "24h uptime badge (SVG)"),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn uptime_badge(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, AppError> {
    let summary = state_summary(state).await;
    let monitor = summary
        .monitors
        .iter()
        .find(|m| m.id == id)
        .ok_or_else(|| AppError::NotFound("unknown monitor"))?;
    let (message, color) = match monitor.uptime_permille {
        Some(permille) => (format_permille(permille), uptime_color(permille)),
        None => ("n/a".to_owned(), "#9f9f9f"),
    };
    Ok(svg_response(badge("uptime", &message, color)))
}

/// Sample a series down to at most `max` points, keeping its overall shape.
pub(crate) fn downsample(points: Vec<Point>, max: usize) -> Vec<Point> {
    if points.len() <= max || max == 0 {
        return points;
    }
    let step = points.len().div_ceil(max);
    points.into_iter().step_by(step).collect()
}

/// Constant-time string comparison so a wrong push token can't be brute-forced
/// by timing. The length may leak (it is not the secret).
pub(crate) fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}
