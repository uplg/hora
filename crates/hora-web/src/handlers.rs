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

use hora_core::config::{Config, Kind, Monitor};
use hora_core::db::{self, Point};
use hora_core::notifications::{AlertSeverity, Event};
use hora_core::peer::{HealthReport, PeerSeen};

use crate::error::AppError;
use crate::flood;
use crate::history;
use crate::metrics;
use crate::render::{badge, status_color, svg_response, uptime_color};
use crate::summary::{
    DayCell, IncidentView, MaintenanceView, MonitorView, StatusTemplate, Summary, format_permille,
};
use crate::text;
use crate::{
    AppState, FAVICON_SVG, FONT_WOFF2, MAX_ALERT_DEDUP_CHARS, MAX_ALERT_TAG_CHARS, MAX_ALERT_TAGS,
    MAX_ALERT_TITLE_CHARS, MAX_LATENCY_HOURS, MAX_LATENCY_POINTS, MAX_PUSH_MSG_CHARS,
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
        post_alert,
        silence,
        announce,
        announce_clear,
        peer_probe,
        status_badge,
        uptime_badge,
        heatmap_svg,
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
        SilenceResponse,
        AnnounceResponse,
        AnnounceClearResponse,
        AlertRequest,
        AlertResponse,
        hora_core::confirm::ProbeRequest,
        hora_core::confirm::ProbeResponse
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

/// The per-group status page (`/status/{group}`): the monitors of one display
/// group, nothing else - lightweight multi-tenancy for an operator hosting
/// several clients' services on one Hora. Anonymous viewers get the group's
/// public monitors; the global viewer token, or this group's own
/// `server.group_tokens` entry, reveals the group's full view (and only this
/// group's). An unknown group - or a fully private one viewed without a
/// token - answers 404, exactly like a missing page.
pub(crate) async fn group_page(
    State(state): State<AppState>,
    Path(group): Path<String>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
) -> Result<Response, AppError> {
    let config = state.config.borrow().clone();
    let token = auth_query.token.as_deref();
    let full = is_authenticated(&headers, token, &config)
        || group_token_matches(&headers, token, &config, &group);
    let summary = state_summary(state, full).await;
    let view = crate::summary::for_group(&summary, &config, &group)
        .ok_or(AppError::NotFound("unknown group"))?;
    let html = StatusTemplate { summary: &view }.render()?;
    Ok(Html(html).into_response())
}

#[derive(Debug, Deserialize)]
pub(crate) struct AnnounceQuery {
    /// Banner title (required, bounded).
    title: String,
    #[serde(default)]
    body: Option<String>,
    /// `info` (default) | `warning` | `critical` | `resolved`.
    #[serde(default)]
    severity: Option<String>,
    /// Auto-expiry as a duration (`4h`, `90m`); absent = until cleared.
    #[serde(default)]
    until: Option<String>,
    #[serde(default)]
    token: Option<String>,
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
        ("until" = Option<String>, Query, description = "Auto-expiry as a duration (e.g. 4h)"),
        ("token" = Option<String>, Query, description = "Viewer token (or Authorization: Bearer)")
    ),
    responses(
        (status = 200, description = "Announcement pinned to the status page", body = AnnounceResponse),
        (status = 400, description = "Empty title, unknown severity or unparseable duration"),
        (status = 401, description = "Missing or wrong token, or no auth_token configured")
    )
)]
pub(crate) async fn announce(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<AnnounceQuery>,
) -> Result<Json<AnnounceResponse>, AppError> {
    let config = state.config.borrow().clone();
    // Publishing to every visitor is an operator action: like /api/silence,
    // the endpoint is closed without a configured viewer token.
    if config.server.auth_token.is_none()
        || !is_authenticated(&headers, query.token.as_deref(), &config)
    {
        return Err(AppError::Unauthorized(
            "announcing requires server.auth_token and a matching token",
        ));
    }

    let title: String = query.title.trim().chars().take(200).collect();
    if title.is_empty() {
        return Err(AppError::BadRequest("title must not be empty"));
    }
    let severity = match query.severity.as_deref() {
        None => "info",
        Some(value) => severity_or_400(value)?,
    };
    let until = match query.until.as_deref() {
        None => None,
        Some(raw) => Some(
            hora_core::parse_duration(raw)
                .map(|secs| Utc::now().timestamp() + i64::try_from(secs).unwrap_or(i64::MAX))
                .ok_or(AppError::BadRequest("invalid until (use e.g. 4h, 90m)"))?,
        ),
    };
    let body: String = query
        .body
        .as_deref()
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_PUSH_MSG_CHARS)
        .collect();
    let id = db::insert_announcement(&state.pool, &title, &body, severity, until).await?;
    // Visitors should see the banner now, not when the summary cache rolls.
    state.cache.invalidate();
    tracing::info!(%title, severity, "announcement pinned via API");
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
    params(("token" = Option<String>, Query, description = "Viewer token (or Authorization: Bearer)")),
    responses(
        (status = 200, description = "Every ad-hoc announcement removed", body = AnnounceClearResponse),
        (status = 401, description = "Missing or wrong token, or no auth_token configured")
    )
)]
pub(crate) async fn announce_clear(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
) -> Result<Json<AnnounceClearResponse>, AppError> {
    let config = state.config.borrow().clone();
    if config.server.auth_token.is_none()
        || !is_authenticated(&headers, auth_query.token.as_deref(), &config)
    {
        return Err(AppError::Unauthorized(
            "announcing requires server.auth_token and a matching token",
        ));
    }
    let cleared = db::clear_announcements(&state.pool, Utc::now().timestamp()).await?;
    state.cache.invalidate();
    tracing::info!(cleared, "announcements cleared via API");
    Ok(Json(AnnounceClearResponse { cleared }))
}

/// Validate a severity label, or answer 400.
fn severity_or_400(value: &str) -> Result<&'static str, AppError> {
    match value {
        "info" => Ok("info"),
        "warning" => Ok("warning"),
        "critical" => Ok("critical"),
        "resolved" => Ok("resolved"),
        _ => Err(AppError::BadRequest(
            "severity must be info, warning, critical or resolved",
        )),
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReportQuery {
    /// Restrict the report to one display group - the report twin of the
    /// `/status/{group}` page, and what an operator hands a client.
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    token: Option<String>,
}

/// The printable monthly SLA report (`/report/2026-05`, optionally
/// `?group=X`). Anonymous viewers get the public monitors; the viewer token
/// includes the private ones, and on a `?group=` report the group's own
/// `server.group_tokens` entry does too - so a client can be handed *their*
/// report and nothing else. Server-rendered and print-first: "Save as PDF"
/// is the export.
pub(crate) async fn report_page(
    State(state): State<AppState>,
    Path(month): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ReportQuery>,
) -> Result<Html<String>, AppError> {
    let config = state.config.borrow().clone();
    let token = query.token.as_deref();
    let full = is_authenticated(&headers, token, &config)
        || query
            .group
            .as_deref()
            .is_some_and(|group| group_token_matches(&headers, token, &config, group));
    // Validate the month *before* building, so a malformed path is a clean
    // 400 and never reaches the database.
    if hora_core::report::month_bounds(&month).is_none() {
        return Err(AppError::BadRequest(
            "month must be YYYY-MM and not in the future",
        ));
    }
    let report = hora_core::report::build(&state.pool, &config, &month).await?;
    let groups = crate::report::group_rows(&report, |row| {
        (full || row.public)
            && query
                .group
                .as_deref()
                .is_none_or(|group| row.group.as_deref() == Some(group))
    });
    // A scoped report with nothing visible answers like the group page: 404,
    // revealing neither the group's existence nor its members.
    if groups.is_empty() && query.group.is_some() {
        return Err(AppError::NotFound("unknown group"));
    }
    let title = match &query.group {
        Some(group) => format!("{} · {group}", config.page.title),
        None => config.page.title.clone(),
    };
    let html = crate::report::ReportTemplate {
        title,
        label: report.label.clone(),
        generated: Utc::now().format("%Y-%m-%d %H:%M UTC").to_string(),
        groups,
    }
    .render()?;
    Ok(Html(html))
}

/// Whether the request carries the group's own viewer token (Bearer or
/// `?token=`). A group token authenticates *that group's page only* - it is
/// never accepted by [`is_authenticated`], so it reveals nothing else.
fn group_token_matches(
    headers: &HeaderMap,
    query_token: Option<&str>,
    config: &Config,
    group: &str,
) -> bool {
    let Some(expected) = config.server.group_tokens.get(group) else {
        return false;
    };
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .or(query_token);
    provided.is_some_and(|token| ct_eq(token, expected.as_ref()))
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
                // The captured response (headers, body start) is operator
                // detail like the full reason: same opt-in to publish it.
                incident.snapshot = None;
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

/// Recent pushed alerts restricted to what the caller may see, mirroring
/// [`visible_incidents`]: anonymous viewers get only public monitors' alerts,
/// and the free-form message (which can carry producer detail like file paths)
/// is collapsed unless the monitor opted in with `public_error_detail`. The
/// title and severity always show (a short headline, like an incident's
/// existence). Authenticated viewers see everything.
async fn visible_pushed_alerts(
    pool: &sqlx::SqlitePool,
    config: &Config,
    authenticated: bool,
    limit: i64,
) -> Result<Vec<db::PushedAlert>, AppError> {
    let mut alerts = db::recent_pushed_alerts(pool, limit).await?;
    if !authenticated {
        let public: std::collections::HashSet<&str> = config
            .monitors
            .iter()
            .filter(|monitor| monitor.public)
            .map(|monitor| monitor.id.as_str())
            .collect();
        let detailed: std::collections::HashSet<&str> = config
            .monitors
            .iter()
            .filter(|monitor| monitor.public && monitor.public_error_detail)
            .map(|monitor| monitor.id.as_str())
            .collect();
        alerts.retain(|alert| public.contains(alert.monitor_id.as_str()));
        for alert in &mut alerts {
            if !detailed.contains(alert.monitor_id.as_str()) {
                alert.message.clear();
            }
        }
    }
    Ok(alerts)
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
    let pushed_alerts = visible_pushed_alerts(&state.pool, &config, authenticated, 100).await?;
    // The heatmap section lists what this viewer may see; the images load
    // lazily from the API. Push monitors have no latency series to show.
    let heatmaps = config
        .monitors
        .iter()
        .filter(|monitor| (monitor.public || authenticated) && monitor.kind != Kind::Push)
        .map(|monitor| history::HeatmapRef {
            id: monitor.id.clone(),
            name: monitor.name.clone(),
        })
        .collect();
    let token_query = auth_query
        .token
        .as_deref()
        .filter(|_| authenticated)
        .map(|token| format!("?token={}", history::url_encode(token)))
        .unwrap_or_default();
    let names = monitor_names(&config);
    let html = history::HistoryTemplate {
        title: config.page.title.clone(),
        incidents: history::incident_rows(&incidents, &names),
        pushed_alerts: history::alert_rows(&pushed_alerts, &names),
        heatmaps,
        token_query,
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

#[utoipa::path(
    post,
    path = "/api/peer/probe",
    request_body = hora_core::confirm::ProbeRequest,
    responses(
        (status = 200, description = "This vantage's verdict on the target", body = hora_core::confirm::ProbeResponse),
        (status = 401, description = "Unknown requesting peer, or missing/wrong X-Push-Token"),
        (status = 404, description = "The target is not in this node's configuration")
    )
)]
pub(crate) async fn peer_probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<hora_core::confirm::ProbeRequest>,
) -> Result<Json<hora_core::confirm::ProbeResponse>, AppError> {
    let config = state.config.borrow().clone();

    // Authenticate the requesting peer: it must be configured here, it must
    // have a listen_token (probing is strictly more sensitive than a push
    // heartbeat, so the id alone never authorizes), and the X-Push-Token
    // header must match. Unknown peers answer exactly like a wrong token.
    let authorized = config
        .peers
        .iter()
        .find(|peer| peer.id == request.from)
        .and_then(|peer| peer.listen_token.as_ref())
        .is_some_and(|expected| {
            headers
                .get("x-push-token")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|token| ct_eq(token, expected.as_ref()))
        });
    if !authorized {
        return Err(AppError::Unauthorized("unknown peer or invalid token"));
    }

    // The SSRF guard: only targets present in THIS node's configuration are
    // probed - a peer is a vantage point, never a proxy. The matched
    // monitor's own settings (timeout, assertions, proxy) drive the probe.
    let monitor = config
        .monitors
        .iter()
        .find(|monitor| {
            monitor.kind == request.kind
                && monitor.target == request.target
                && monitor.kind != Kind::Push
        })
        .ok_or(AppError::NotFound(
            "target not in this node's configuration",
        ))?;

    // Single attempt (no retries): the requester wants a fast vantage check,
    // not this node's anti-flap pipeline. The outer timeout is a backstop on
    // top of the probe's own; if even that elapses, the target is
    // unresponsive *from here*, which is a down verdict in its own right.
    let mut probe_monitor = monitor.clone();
    probe_monitor.probe_retries = Some(0);
    let client = hora_core::http::probe_client(monitor.proxy.as_deref())
        .map_err(|err| AppError::Internal(err.into()))?;
    let outcome = match tokio::time::timeout(
        hora_core::confirm::PROBE_DEADLINE,
        hora_core::probe::run(&client, &probe_monitor),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            return Ok(Json(hora_core::confirm::ProbeResponse {
                up: false,
                error: Some("probe timed out at this vantage".to_owned()),
            }));
        }
    };
    Ok(Json(hora_core::confirm::ProbeResponse {
        up: outcome.up,
        // Bounded: the reason crosses the wire into another node's logs.
        error: outcome.error.map(|error| error.chars().take(200).collect()),
    }))
}

#[utoipa::path(
    get,
    path = "/api/monitors/{id}/heatmap.svg",
    params(("id" = String, Path, description = "Monitor id")),
    responses(
        (status = 200, description = "28-day hours-by-days latency heatmap (SVG)"),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn heatmap_svg(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
) -> Result<impl IntoResponse, AppError> {
    let AppState { pool, config, .. } = state;
    let config = config.borrow().clone();
    // Same visibility rule as the latency endpoint: a private monitor answers
    // exactly like a missing one unless the caller is authenticated.
    let monitor = config
        .monitors
        .iter()
        .find(|monitor| {
            monitor.id == id
                && (monitor.public
                    || is_authenticated(&headers, auth_query.token.as_deref(), &config))
        })
        .ok_or(AppError::NotFound("unknown monitor"))?;
    let now = Utc::now().timestamp();
    let since = (now / 86_400 - (crate::heatmap::HEATMAP_DAYS - 1)) * 86_400;
    let cells = db::latency_hourly(&pool, &id, since).await?;
    Ok(svg_response(crate::heatmap::render(
        &cells,
        now,
        &monitor.name,
    )))
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

/// The JSON body of `POST /api/monitors/{id}/alert`. Every field is optional at
/// the deserialization layer so a missing one yields a clean 400 from the
/// handler rather than a 422 from the extractor.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub(crate) struct AlertRequest {
    /// `info` (default) | `warning` | `error` | `critical`.
    #[serde(default)]
    severity: Option<String>,
    /// Short headline (required, bounded).
    #[serde(default)]
    title: Option<String>,
    /// Free-form detail (optional, bounded).
    #[serde(default)]
    message: Option<String>,
    /// Coalescing key for the server-side anti-flood window (optional).
    #[serde(default)]
    dedup_key: Option<String>,
    /// Arbitrary key/value pairs folded into the message (optional).
    #[serde(default)]
    tags: std::collections::HashMap<String, String>,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
pub(crate) struct AlertResponse {
    /// `dispatched` (sent to the channels and recorded) or `coalesced` (a
    /// `dedup_key` repeat dropped inside the anti-flood window).
    status: &'static str,
    /// The accepted severity.
    severity: &'static str,
    /// The recorded alert's id - only when `dispatched`.
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<i64>,
    /// The dedup key - echoed only when `coalesced`.
    #[serde(skip_serializing_if = "Option::is_none")]
    dedup_key: Option<String>,
    /// Repeats coalesced in this window so far - only when `coalesced`.
    #[serde(skip_serializing_if = "Option::is_none")]
    suppressed: Option<u64>,
    /// Seconds until the window reopens - only when `coalesced`.
    #[serde(skip_serializing_if = "Option::is_none")]
    retry_after_secs: Option<i64>,
}

#[utoipa::path(
    post,
    path = "/api/monitors/{id}/alert",
    params(
        ("id" = String, Path, description = "Monitor id the alert is attached to"),
        ("token" = Option<String>, Query, description = "server.auth_token (or Authorization: Bearer)")
    ),
    request_body = AlertRequest,
    responses(
        (status = 202, description = "Alert dispatched to the monitor's channels (or coalesced)", body = AlertResponse),
        (status = 400, description = "Empty title or unknown severity"),
        (status = 401, description = "Missing/wrong X-Push-Token and no matching server.auth_token"),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn post_alert(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(auth_query): Query<AuthQuery>,
    Json(request): Json<AlertRequest>,
) -> Result<(StatusCode, Json<AlertResponse>), AppError> {
    let config = state.config.borrow().clone();

    // The monitor must exist; its id is already public (page + API), so a 404
    // here reveals nothing a viewer could not already see.
    let monitor = config
        .monitors
        .iter()
        .find(|monitor| monitor.id == id)
        .ok_or(AppError::NotFound("unknown monitor"))?;

    // Authenticate with the monitor's own push_token (preferred, via the
    // X-Push-Token header kept out of access logs) or the global viewer token.
    // Dispatching to channels can flood, so - unlike a read-only view - the
    // endpoint stays closed unless a credential is configured and matches.
    let item_token_ok = monitor.push_token.as_ref().is_some_and(|expected| {
        headers
            .get("x-push-token")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|token| ct_eq(token, expected.as_ref()))
    });
    if !item_token_ok && !is_authenticated(&headers, auth_query.token.as_deref(), &config) {
        return Err(AppError::Unauthorized(
            "alerting requires the monitor's push_token (X-Push-Token) or server.auth_token",
        ));
    }

    // Validate the body.
    let severity = match request.severity.as_deref() {
        None | Some("") => AlertSeverity::Info,
        Some(value) => AlertSeverity::parse(value).ok_or(AppError::BadRequest(
            "severity must be info, warning, error or critical",
        ))?,
    };
    let title: String = request
        .title
        .as_deref()
        .unwrap_or("")
        .trim()
        .chars()
        .take(MAX_ALERT_TITLE_CHARS)
        .collect();
    if title.is_empty() {
        return Err(AppError::BadRequest("title must not be empty"));
    }
    let dedup_key: Option<String> = request
        .dedup_key
        .as_deref()
        .map(str::trim)
        .filter(|key| !key.is_empty())
        .map(|key| key.chars().take(MAX_ALERT_DEDUP_CHARS).collect());
    // The producer's text, with any tags folded in ("just enrich the message"),
    // bounded so a buggy producer can't bloat the row.
    let message = render_alert_message(request.message.as_deref().unwrap_or(""), &request.tags);

    // Anti-flood: a dedup_key that recurs inside the window is coalesced here,
    // so the rate-limiting lives in Hora and every producer benefits.
    let window = i64::try_from(config.alerts.push_alert_window_secs).unwrap_or(i64::MAX);
    let now = Utc::now().timestamp();
    if let flood::Admission::Coalesce {
        suppressed,
        retry_after,
    } = state.flood.admit(&id, dedup_key.as_deref(), now, window)
    {
        tracing::info!(monitor = %id, suppressed, "pushed alert coalesced");
        return Ok((
            StatusCode::ACCEPTED,
            Json(AlertResponse {
                status: "coalesced",
                severity: severity.as_str(),
                id: None,
                dedup_key,
                suppressed: Some(suppressed),
                retry_after_secs: Some(retry_after),
            }),
        ));
    }

    // Record the timeline line first (so the 202 reflects a durable row), then
    // fan the alert out to the monitor's channels. The dispatch is spawned, so a
    // slow channel never holds up the producer.
    let alert_id = db::insert_pushed_alert(
        &state.pool,
        &id,
        severity.as_str(),
        &title,
        &message,
        dedup_key.as_deref(),
    )
    .await?;
    spawn_alert_dispatch(&state, monitor, severity, title.clone(), message);

    tracing::info!(monitor = %id, severity = severity.as_str(), %title, "pushed alert dispatched");
    Ok((
        StatusCode::ACCEPTED,
        Json(AlertResponse {
            status: "dispatched",
            severity: severity.as_str(),
            id: Some(alert_id),
            dedup_key,
            suppressed: None,
            retry_after_secs: None,
        }),
    ))
}

/// Fan a pushed alert out to a monitor's channels in the background, so a slow
/// channel never blocks the 202. The owned strings outlive the request.
fn spawn_alert_dispatch(
    state: &AppState,
    monitor: &Monitor,
    severity: AlertSeverity,
    title: String,
    message: String,
) {
    let notifier = state.notifier.clone();
    let name = monitor.name.clone();
    let notify = monitor.notify.clone();
    tokio::spawn(async move {
        notifier
            .load_full()
            .dispatch(
                Event::Alert {
                    monitor: &name,
                    severity,
                    title: &title,
                    message: &message,
                },
                notify.as_deref(),
            )
            .await;
    });
}

/// Build the stored/sent alert message: the producer's text, then each tag as a
/// `key=value` line (sorted, so the same alert always reads identically). Every
/// part is trimmed and bounded.
fn render_alert_message(message: &str, tags: &std::collections::HashMap<String, String>) -> String {
    let mut out: String = message.trim().chars().take(MAX_PUSH_MSG_CHARS).collect();
    let mut pairs: Vec<(&String, &String)> = tags.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    for (key, value) in pairs.into_iter().take(MAX_ALERT_TAGS) {
        let key: String = key.trim().chars().take(MAX_ALERT_TAG_CHARS).collect();
        if key.is_empty() {
            continue;
        }
        let value: String = value.trim().chars().take(MAX_ALERT_TAG_CHARS).collect();
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&key);
        out.push('=');
        out.push_str(&value);
    }
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn alert_message_folds_tags_in_sorted_order() {
        let tags = HashMap::from([
            ("task_id".to_owned(), "t1".to_owned()),
            ("patch_id".to_owned(), "9f3c".to_owned()),
        ]);
        // Producer text first, then each tag as `key=value`, keys sorted so the
        // same alert always reads identically.
        assert_eq!(
            render_alert_message("3/12 ops failed", &tags),
            "3/12 ops failed\npatch_id=9f3c\ntask_id=t1"
        );
    }

    #[test]
    fn alert_message_handles_empty_text_and_tags() {
        assert_eq!(render_alert_message("  boom  ", &HashMap::new()), "boom");
        // No producer text: the message is just the tag lines.
        let tags = HashMap::from([("k".to_owned(), "v".to_owned())]);
        assert_eq!(render_alert_message("", &tags), "k=v");
        // Nothing at all yields an empty message.
        assert_eq!(render_alert_message("", &HashMap::new()), "");
    }
}
