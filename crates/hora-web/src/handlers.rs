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

use hora_core::config::{Config, Kind};
use hora_core::db::{self, Point};
use hora_core::peer::{HealthReport, PeerSeen};

use crate::error::AppError;
use crate::history;
use crate::metrics;
use crate::render::{badge, status_color, svg_response, uptime_color};
use crate::summary::{
    DayCell, IncidentView, MaintenanceView, MonitorView, StatusTemplate, Summary, format_permille,
};
use crate::text;
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
    paths(
        summary_json,
        latency_json,
        push,
        silence,
        status_badge,
        uptime_badge,
        healthz
    ),
    components(schemas(
        Summary,
        MonitorView,
        IncidentView,
        MaintenanceView,
        DayCell,
        Point,
        HealthReport,
        PeerSeen,
        SilenceResponse
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
/// `full` selects the authenticated view that includes private monitors; both
/// views are cached (one slot each).
pub(crate) async fn state_summary(state: AppState, full: bool) -> Arc<Summary> {
    let AppState {
        pool,
        config,
        cache,
        ..
    } = state;
    let config = config.borrow().clone();
    summary_for(&pool, &config, &cache, full).await
}

/// Whether the request carries the configured viewer token, as
/// `Authorization: Bearer <token>` or `?token=`. With no token configured
/// nothing is private (config validation enforces that), so every caller gets
/// the public view and the answer is simply `false`.
pub(crate) fn is_authenticated(
    headers: &HeaderMap,
    query_token: Option<&str>,
    config: &Config,
) -> bool {
    let Some(expected) = &config.server.auth_token else {
        return false;
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or(query_token);
    provided.is_some_and(|token| ct_eq(token, expected.as_ref()))
}

pub(crate) async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
) -> Result<Response, AppError> {
    let config = state.config.borrow().clone();
    let authenticated = is_authenticated(&headers, auth_query.token.as_deref(), &config);
    let summary = state_summary(state, authenticated).await;

    // Text clients (curl, wget, or an explicit text/plain Accept) get the
    // aligned plain-text rendering; everyone else the HTML page.
    let wants_text = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|ua| ua.starts_with("curl/") || ua.starts_with("Wget/"))
        || headers
            .get(header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|accept| accept.contains("text/plain") && !accept.contains("text/html"));

    if wants_text {
        let body = text::render(&summary);
        Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response())
    } else {
        let html = StatusTemplate {
            summary: summary.as_ref(),
        }
        .render()?;
        Ok(Html(html).into_response())
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct AuthQuery {
    #[serde(default)]
    pub(crate) token: Option<String>,
}

#[utoipa::path(
    get,
    path = "/api/summary",
    responses((status = 200, description = "Status of every monitor", body = Summary))
)]
pub(crate) async fn summary_json(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
) -> Json<Arc<Summary>> {
    let config = state.config.borrow().clone();
    let authenticated = is_authenticated(&headers, auth_query.token.as_deref(), &config);
    Json(state_summary(state, authenticated).await)
}

pub(crate) async fn metrics_prometheus(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
) -> impl IntoResponse {
    let config = state.config.borrow().clone();
    let authenticated = is_authenticated(&headers, auth_query.token.as_deref(), &config);
    let summary = state_summary(state, authenticated).await;
    let body = metrics::render(&summary);
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
}

/// Recent incidents restricted to what the caller may see: incidents of
/// private monitors - and of monitors no longer in the config - only reach
/// authenticated viewers. For anonymous viewers the surviving incidents are
/// also sanitized: failure reasons collapse to their safe category (the stored
/// reason carries body snippets and DNS answers) unless the monitor opts in
/// with `public_error_detail`, and topology annotations drop any name that is
/// not a public monitor's.
async fn visible_incidents(
    pool: &sqlx::SqlitePool,
    config: &Config,
    authenticated: bool,
    limit: i64,
) -> Result<Vec<db::Incident>, AppError> {
    let mut incidents = db::recent_incidents(pool, limit).await?;
    if !authenticated {
        let public: Vec<&hora_core::config::Monitor> = config
            .monitors
            .iter()
            .filter(|monitor| monitor.public)
            .collect();
        let visible: std::collections::HashSet<&str> =
            public.iter().map(|monitor| monitor.id.as_str()).collect();
        // cause/impacted store display names; allow ids too in case older rows
        // recorded those.
        let nameable: std::collections::HashSet<&str> = public
            .iter()
            .flat_map(|monitor| [monitor.id.as_str(), monitor.name.as_str()])
            .collect();
        // Monitors that opted into publishing their full failure detail.
        let detailed: std::collections::HashSet<&str> = public
            .iter()
            .filter(|monitor| monitor.public_error_detail)
            .map(|monitor| monitor.id.as_str())
            .collect();
        incidents.retain(|incident| visible.contains(incident.monitor_id.as_str()));
        // Operator notes (`hora annotate`) deliberately survive sanitization:
        // they are written *for* visitors, unlike the captured failure detail.
        for incident in &mut incidents {
            if !detailed.contains(incident.monitor_id.as_str()) {
                incident.error = incident
                    .error
                    .as_deref()
                    .map(|reason| hora_core::probe::public_reason(reason).to_owned());
            }
            incident.cause = incident
                .cause
                .take()
                .filter(|cause| nameable.contains(cause.as_str()));
            // `impacted` is a JSON list of names; keep the public ones only.
            incident.impacted = incident.impacted.as_deref().and_then(|json| {
                let names: Vec<String> = serde_json::from_str::<Vec<String>>(json)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|name| nameable.contains(name.as_str()))
                    .collect();
                (!names.is_empty()).then(|| serde_json::to_string(&names).unwrap_or_default())
            });
        }
    }
    Ok(incidents)
}

/// Map of monitor id to display name, for rendering incidents.
fn monitor_names(config: &Config) -> std::collections::HashMap<String, String> {
    config
        .monitors
        .iter()
        .map(|monitor| (monitor.id.clone(), monitor.name.clone()))
        .collect()
}

pub(crate) async fn history_page(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
) -> Result<Html<String>, AppError> {
    let config = state.config.borrow().clone();
    let authenticated = is_authenticated(&headers, auth_query.token.as_deref(), &config);
    let incidents = visible_incidents(&state.pool, &config, authenticated, 100).await?;
    let html = history::HistoryTemplate {
        title: config.page.title.clone(),
        incidents: history::incident_rows(&incidents, &monitor_names(&config)),
    }
    .render()?;
    Ok(Html(html))
}

pub(crate) async fn history_atom(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
) -> Result<impl IntoResponse, AppError> {
    let config = state.config.borrow().clone();
    let authenticated = is_authenticated(&headers, auth_query.token.as_deref(), &config);
    let incidents = visible_incidents(&state.pool, &config, authenticated, 50).await?;
    // Absolute feed links: scheme from the proxy's x-forwarded-proto (plain
    // http when absent), host from the Host header.
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|proto| *proto == "https" || *proto == "http")
        .unwrap_or("http");
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let base_url = format!("{proto}://{host}");
    let body = history::render_atom(&incidents, &monitor_names(&config), &base_url);
    Ok((
        [(header::CONTENT_TYPE, "application/atom+xml; charset=utf-8")],
        body,
    ))
}

#[derive(Debug, Deserialize)]
pub(crate) struct LatencyQuery {
    #[serde(default = "default_hours")]
    hours: i64,
    #[serde(default)]
    token: Option<String>,
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
    headers: HeaderMap,
    Query(query): Query<LatencyQuery>,
) -> Result<Json<Vec<Point>>, AppError> {
    let AppState { pool, config, .. } = state;
    let config = config.borrow().clone();
    // A private monitor answers exactly like a missing one (404) unless the
    // caller is authenticated - its existence is not revealed either way.
    let visible = config.monitors.iter().any(|monitor| {
        monitor.id == id
            && (monitor.public || is_authenticated(&headers, query.token.as_deref(), &config))
    });
    if !visible {
        return Err(AppError::NotFound("unknown monitor"));
    }
    let LatencyQuery { hours, .. } = query;
    let window = hours.clamp(1, MAX_LATENCY_HOURS) * SECONDS_PER_HOUR;
    let since = Utc::now().timestamp() - window;
    // Average into at most MAX_LATENCY_POINTS buckets in SQL, so a 10s-interval
    // monitor over 720h (~260k raw rows) never materializes more than the cap.
    // Ceiling division keeps the bucket count under the cap even for short
    // windows, where flooring would produce up to ~2x the buckets.
    let max_points = i64::try_from(MAX_LATENCY_POINTS).expect("MAX_LATENCY_POINTS fits in i64");
    // (Manual ceil: `i64::div_ceil` is still unstable.)
    let bucket_secs = ((window + max_points - 1) / max_points).max(1);
    let points = db::latency_series(&pool, &id, since, bucket_secs).await?;
    // The SQL already respects the cap; downsample stays as a pure backstop.
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

#[derive(Debug, Deserialize)]
pub(crate) struct SilenceQuery {
    /// Comma-separated monitor ids, or `all` (stored as the `*` wildcard).
    monitors: String,
    /// How long to mute, e.g. `10m`, `1h30m`. Capped at 7 days.
    duration: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    token: Option<String>,
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
        ("reason" = Option<String>, Query, description = "Optional note recorded with the silence"),
        ("token" = Option<String>, Query, description = "Viewer token (or Authorization: Bearer)")
    ),
    responses(
        (status = 200, description = "Alerts muted until the returned time", body = SilenceResponse),
        (status = 400, description = "Unparseable duration or empty monitor list"),
        (status = 401, description = "Missing or wrong token, or no auth_token configured"),
        (status = 404, description = "Unknown monitor id")
    )
)]
pub(crate) async fn silence(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SilenceQuery>,
) -> Result<Json<SilenceResponse>, AppError> {
    let config = state.config.borrow().clone();
    // Muting alerts is an operator action: it strictly requires the configured
    // viewer token. Without one the endpoint is closed (unlike the read-only
    // views, where "no token" just means "everything is public").
    if config.server.auth_token.is_none()
        || !is_authenticated(&headers, query.token.as_deref(), &config)
    {
        return Err(AppError::Unauthorized(
            "silencing requires server.auth_token and a matching token",
        ));
    }

    let duration_secs = hora_core::parse_duration(&query.duration)
        .filter(|secs| *secs <= hora_core::MAX_SILENCE_SECS)
        .ok_or(AppError::BadRequest(
            "invalid duration (use e.g. 10m, 1h30m; max 7d)",
        ))?;

    let monitors: Vec<String> = if query.monitors.trim() == "all" || query.monitors.trim() == "*" {
        vec!["*".to_owned()]
    } else {
        let ids: Vec<String> = query
            .monitors
            .split(',')
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        if ids.is_empty() {
            return Err(AppError::BadRequest("no monitor ids given"));
        }
        // Validate every id so a typo'd deploy hook fails loudly instead of
        // silencing nothing.
        if ids
            .iter()
            .any(|id| !config.monitors.iter().any(|monitor| monitor.id == *id))
        {
            return Err(AppError::NotFound("unknown monitor id"));
        }
        ids
    };

    let until = Utc::now().timestamp() + i64::try_from(duration_secs).unwrap_or(i64::MAX);
    // Bound the stored reason like push messages, so a buggy hook can't bloat the DB.
    let reason = query
        .reason
        .as_deref()
        .map(|reason| reason.chars().take(MAX_PUSH_MSG_CHARS).collect::<String>());
    for id in &monitors {
        db::insert_silence(&state.pool, id, until, reason.as_deref()).await?;
    }
    tracing::info!(monitors = ?monitors, until, "alerts silenced via API");
    Ok(Json(SilenceResponse { monitors, until }))
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
    // Badges are embeddable and unauthenticated: a private monitor's badge is
    // a 404, not a leak.
    let summary = state_summary(state, false).await;
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
    let summary = state_summary(state, false).await;
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
