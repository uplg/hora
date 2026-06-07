//! Web layer: the server-rendered status page and the JSON API.

mod error;

use std::fmt::Write as _;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, Method, header};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, TimeDelta, Utc};
use futures_util::future::try_join_all;
use hora_core::SECONDS_PER_DAY;
use hora_core::config::{Config, Monitor};
use hora_core::db::{self, DayRow, Latest, Point};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::{Mutex, watch};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::SmartIpKeyExtractor;
use tower_http::cors::{Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use utoipa::{OpenApi, ToSchema};

use crate::error::AppError;

const SECONDS_PER_HOUR: i64 = 3_600;
const MAX_LATENCY_HOURS: i64 = 24 * 30;
const SUMMARY_CACHE_TTL: Duration = Duration::from_secs(5);

const FAVICON_SVG: &str = include_str!("../assets/favicon.svg");
const FONT_WOFF2: &[u8] = include_bytes!("../assets/CalSans-SemiBold.woff2");

// The page is fully self-contained (inline styles, same-origin font/icon, no JS).
const CSP: &str = "default-src 'self'; script-src 'none'; style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; font-src 'self'; base-uri 'none'; frame-ancestors 'none'";

static OPENAPI_JSON: LazyLock<String> =
    LazyLock::new(|| ApiDoc::openapi().to_pretty_json().unwrap_or_default());

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hora API",
        description = "Read-only JSON API of a Hora uptime monitor."
    ),
    paths(summary_json, latency_json, healthz),
    components(schemas(Summary, MonitorView, DayCell, Point))
)]
struct ApiDoc;

/// A cached summary, valid while the config is unchanged and within the TTL.
struct Cached {
    at: Instant,
    config: Arc<Config>,
    summary: Arc<Summary>,
}

/// Lock-free reads (`value`) plus a single-flight build gate (`build`).
#[derive(Default)]
struct Cache {
    value: ArcSwapOption<Cached>,
    build: Mutex<()>,
}

/// Shared application state handed to every handler.
#[derive(Clone)]
pub struct AppState {
    pool: SqlitePool,
    config: watch::Receiver<Arc<Config>>,
    cache: Arc<Cache>,
}

impl AppState {
    #[must_use]
    pub fn new(pool: SqlitePool, config: watch::Receiver<Arc<Config>>) -> Self {
        Self {
            pool,
            config,
            cache: Arc::new(Cache::default()),
        }
    }
}

/// Build the axum router: page, rate-limited JSON API, `OpenAPI`, static assets,
/// CORS, security headers and tracing.
pub fn router(state: AppState) -> Router {
    let config = state.config.borrow().clone();
    let cors = build_cors(&config.server.allowed_origins);

    let mut api = Router::new()
        .route("/api/summary", get(summary_json))
        .route("/api/monitors/{id}/latency", get(latency_json));

    // Parameters are clamped to >= 1, so `finish` always succeeds; if it ever
    // did not, the API simply runs without a rate limit rather than panicking.
    if let Some(governor) = GovernorConfigBuilder::default()
        .per_second(config.server.rate_limit_refill_secs.max(1))
        .burst_size(config.server.rate_limit_burst.max(1))
        .key_extractor(SmartIpKeyExtractor)
        .use_headers()
        .finish()
    {
        // Periodically drop idle per-IP buckets so memory stays bounded.
        let limiter = governor.limiter().clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_mins(1));
            loop {
                ticker.tick().await;
                limiter.retain_recent();
            }
        });
        api = api.layer(GovernorLayer::new(governor));
    }

    Router::new()
        .route("/", get(page))
        .route("/healthz", get(healthz))
        .route("/favicon.svg", get(favicon))
        .route("/assets/CalSans-SemiBold.woff2", get(font))
        .route("/api/openapi.json", get(openapi))
        .merge(api)
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn build_cors(origins: &[String]) -> CorsLayer {
    let cors = CorsLayer::new().allow_methods([Method::GET]);
    if origins.is_empty() {
        return cors.allow_origin(Any);
    }
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    cors.allow_origin(parsed)
}

#[utoipa::path(get, path = "/healthz", responses((status = 200, description = "Service is up")))]
async fn healthz() -> &'static str {
    "ok"
}

async fn favicon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        FAVICON_SVG,
    )
}

async fn font() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "font/woff2"),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        FONT_WOFF2,
    )
}

async fn openapi() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/json")],
        OPENAPI_JSON.as_str(),
    )
}

async fn page(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let AppState {
        pool,
        config,
        cache,
    } = state;
    let config = config.borrow().clone();
    let summary = summary_for(&pool, &config, &cache).await?;
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
async fn summary_json(State(state): State<AppState>) -> Result<Json<Arc<Summary>>, AppError> {
    let AppState {
        pool,
        config,
        cache,
    } = state;
    let config = config.borrow().clone();
    Ok(Json(summary_for(&pool, &config, &cache).await?))
}

#[derive(Debug, Deserialize)]
struct LatencyQuery {
    #[serde(default = "default_hours")]
    hours: i64,
}

fn default_hours() -> i64 {
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
async fn latency_json(
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
    Ok(Json(points))
}

// --- Summary cache (lock-free read + single-flight build) ----------------

/// Return a fresh-enough cached summary, or build exactly one (single-flight)
/// and cache it. The cache busts immediately when the config is reloaded.
async fn summary_for(
    pool: &SqlitePool,
    config: &Arc<Config>,
    cache: &Cache,
) -> anyhow::Result<Arc<Summary>> {
    if let Some(fresh) = fresh_summary(cache, config) {
        return Ok(fresh);
    }
    // Only one task builds at a time; the rest wait and reuse the result.
    let _build = cache.build.lock().await;
    if let Some(fresh) = fresh_summary(cache, config) {
        return Ok(fresh);
    }
    let summary = Arc::new(build_summary(pool, config).await?);
    cache.value.store(Some(Arc::new(Cached {
        at: Instant::now(),
        config: Arc::clone(config),
        summary: Arc::clone(&summary),
    })));
    Ok(summary)
}

fn fresh_summary(cache: &Cache, config: &Arc<Config>) -> Option<Arc<Summary>> {
    let cached = cache.value.load_full()?;
    let fresh = Arc::ptr_eq(&cached.config, config) && cached.at.elapsed() < SUMMARY_CACHE_TTL;
    fresh.then(|| Arc::clone(&cached.summary))
}

// --- View model ----------------------------------------------------------

#[derive(Serialize, ToSchema)]
struct Summary {
    title: String,
    overall: &'static str,
    overall_label: &'static str,
    generated_at: String,
    monitors: Vec<MonitorView>,
}

#[derive(Serialize, ToSchema)]
struct MonitorView {
    id: String,
    name: String,
    status: &'static str,
    last_latency_ms: Option<i64>,
    last_checked: Option<String>,
    #[serde(rename = "uptime_24h_permille")]
    uptime_permille: Option<i64>,
    #[serde(skip)]
    uptime_label: Option<String>,
    #[serde(rename = "cert_expiry_days")]
    cert_days: Option<i64>,
    #[serde(skip)]
    cert_label: Option<String>,
    #[serde(skip)]
    cert_state: &'static str,
    #[serde(rename = "history")]
    bar: Vec<DayCell>,
    #[serde(skip)]
    chart_svg: String,
}

#[derive(Serialize, ToSchema)]
struct DayCell {
    date: String,
    state: &'static str,
    #[serde(skip)]
    title: String,
}

#[derive(Template)]
#[template(path = "status.html")]
struct StatusTemplate<'a> {
    summary: &'a Summary,
}

/// Shared, read-only inputs for building each monitor's view concurrently.
struct SummaryCtx {
    now: DateTime<Utc>,
    timestamp: i64,
    since_24h: i64,
    since_history: i64,
    threshold: i64,
    cert_threshold: i64,
    history_days: u16,
}

async fn build_summary(pool: &SqlitePool, config: &Config) -> anyhow::Result<Summary> {
    let now = Utc::now();
    let timestamp = now.timestamp();
    let ctx = SummaryCtx {
        now,
        timestamp,
        since_24h: timestamp - SECONDS_PER_DAY,
        since_history: timestamp - i64::from(config.page.history_days) * SECONDS_PER_DAY,
        threshold: i64::from(config.alerts.fail_threshold.max(1)),
        cert_threshold: i64::from(config.alerts.cert_expiry_days),
        history_days: config.page.history_days,
    };

    // Build every monitor's view concurrently; the roomy pool keeps them moving.
    let monitors = try_join_all(
        config
            .monitors
            .iter()
            .map(|monitor| build_monitor_view(pool, monitor, &ctx)),
    )
    .await?;

    let overall = monitors
        .iter()
        .fold("up", |worst, m| worse(worst, m.status));

    Ok(Summary {
        title: config.page.title.clone(),
        overall,
        overall_label: overall_label(overall),
        generated_at: now.to_rfc3339(),
        monitors,
    })
}

async fn build_monitor_view(
    pool: &SqlitePool,
    monitor: &Monitor,
    ctx: &SummaryCtx,
) -> anyhow::Result<MonitorView> {
    let recent = db::recent_checks(pool, &monitor.id, ctx.threshold.max(1)).await?;
    let status = derive_status(&recent, ctx.threshold);

    let (available, total) = db::availability(pool, &monitor.id, ctx.since_24h).await?;
    let uptime_permille = (total > 0).then(|| available.saturating_mul(1000) / total);

    let daily = db::daily(pool, &monitor.id, ctx.since_history).await?;
    let bar = build_bar(&daily, ctx.now, ctx.history_days);

    let points = db::latency_series(pool, &monitor.id, ctx.since_24h).await?;
    let chart_svg = sparkline(&points, status);

    let cert_days = db::cert_not_after(pool, &monitor.id)
        .await?
        .map(|not_after| (not_after - ctx.timestamp) / SECONDS_PER_DAY);

    let latest = recent.first();
    Ok(MonitorView {
        id: monitor.id.clone(),
        name: monitor.name.clone(),
        status,
        last_latency_ms: latest.and_then(|l| l.latency_ms),
        last_checked: latest.and_then(|l| iso(l.time)),
        uptime_permille,
        uptime_label: uptime_permille.map(format_permille),
        cert_days,
        cert_label: cert_days.map(cert_label),
        cert_state: cert_state_for(cert_days, ctx.cert_threshold),
        bar,
        chart_svg,
    })
}

/// Current status from the recent checks (newest first): a single failure only
/// counts as `degraded` until `threshold` consecutive failures confirm `down`.
fn derive_status(recent: &[Latest], threshold: i64) -> &'static str {
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

fn worse(current: &'static str, candidate: &'static str) -> &'static str {
    if rank(candidate) > rank(current) {
        candidate
    } else {
        current
    }
}

fn rank(status: &str) -> u8 {
    match status {
        "up" => 0,
        "degraded" => 2,
        "down" => 3,
        _ => 1,
    }
}

fn overall_label(status: &str) -> &'static str {
    match status {
        "up" => "All systems operational",
        "degraded" => "Degraded performance",
        "down" => "Major outage",
        _ => "Awaiting data",
    }
}

fn cert_state_for(days: Option<i64>, threshold: i64) -> &'static str {
    match days {
        None => "none",
        Some(remaining) if remaining <= 0 => "expired",
        Some(remaining) if remaining <= threshold => "warn",
        Some(_) => "ok",
    }
}

fn cert_label(days: i64) -> String {
    if days <= 0 {
        "expired".to_owned()
    } else {
        format!("{days}d")
    }
}

/// Format permille (0..=1000) as a percentage with one decimal, e.g. `99.9%`.
fn format_permille(permille: i64) -> String {
    format!("{}.{}%", permille / 10, permille % 10)
}

fn iso(timestamp: i64) -> Option<String> {
    DateTime::from_timestamp(timestamp, 0).map(|dt| dt.to_rfc3339())
}

/// Build the daily uptime bar (oldest to newest), zero-filling missing days.
fn build_bar(daily: &[DayRow], now: DateTime<Utc>, days: u16) -> Vec<DayCell> {
    use std::collections::HashMap;

    let by_day: HashMap<&str, &DayRow> = daily.iter().map(|row| (row.day.as_str(), row)).collect();
    let mut cells = Vec::with_capacity(usize::from(days));

    for offset in (0..days).rev() {
        let date = (now - TimeDelta::days(i64::from(offset)))
            .format("%Y-%m-%d")
            .to_string();
        let cell = by_day.get(date.as_str()).map_or_else(
            || DayCell {
                title: format!("{date}: no data"),
                date: date.clone(),
                state: "empty",
            },
            |row| day_cell(date.clone(), row),
        );
        cells.push(cell);
    }
    cells
}

fn day_cell(date: String, row: &DayRow) -> DayCell {
    let total = row.up + row.down + row.degraded;
    let (state, title) = if total == 0 {
        ("empty", format!("{date}: no data"))
    } else if row.down == 0 && row.degraded == 0 {
        ("up", format!("{date}: 100%"))
    } else if row.down == 0 {
        ("degraded", format!("{date}: degraded"))
    } else {
        let permille = (row.up + row.degraded).saturating_mul(1000) / total;
        ("down", format!("{date}: {}", format_permille(permille)))
    };
    DayCell { date, state, title }
}

// --- Server-rendered latency chart --------------------------------------
// Colours come from CSS (the `status` class on the <svg>), not inline here.

const CHART_W: f64 = 680.0;
const CHART_H: f64 = 120.0;
const CHART_PAD: f64 = 8.0;

fn coord(value: i64) -> f64 {
    f64::from(i32::try_from(value).unwrap_or(i32::MAX))
}

fn coord_usize(value: usize) -> f64 {
    f64::from(i32::try_from(value).unwrap_or(i32::MAX))
}

/// Render the last-24h latency series as a self-contained inline SVG sparkline.
fn sparkline(points: &[Point], status: &str) -> String {
    if points.is_empty() {
        return format!(
            "<svg viewBox=\"0 0 {CHART_W} {CHART_H}\" class=\"spark {status}\" preserveAspectRatio=\"none\">\
             <text x=\"{x:.0}\" y=\"{y:.0}\" class=\"spark-empty\" text-anchor=\"middle\">no data yet</text>\
             </svg>",
            x = CHART_W / 2.0,
            y = CHART_H / 2.0,
        );
    }

    let count = points.len();
    let max = points
        .iter()
        .map(|p| p.latency_ms)
        .max()
        .unwrap_or(1)
        .max(1);
    let min = points.iter().map(|p| p.latency_ms).min().unwrap_or(0);
    let span = coord((max - min).max(1));
    let plot_h = CHART_H - 2.0 * CHART_PAD;
    let step = if count > 1 {
        (CHART_W - 2.0 * CHART_PAD) / (coord_usize(count) - 1.0)
    } else {
        0.0
    };

    let mut line = String::new();
    for (index, point) in points.iter().enumerate() {
        let x = CHART_PAD + step * coord_usize(index);
        let y = CHART_PAD + plot_h * (1.0 - coord(point.latency_ms - min) / span);
        let _ = write!(line, "{}{x:.1} {y:.1} ", if index == 0 { 'M' } else { 'L' });
    }

    let last_x = CHART_PAD + step * (coord_usize(count) - 1.0);
    let baseline = CHART_H - CHART_PAD;
    format!(
        "<svg viewBox=\"0 0 {CHART_W} {CHART_H}\" class=\"spark {status}\" preserveAspectRatio=\"none\">\
         <path class=\"spark-area\" d=\"{line}L{last_x:.1} {baseline:.1} L{CHART_PAD:.1} {baseline:.1} Z\"/>\
         <path class=\"spark-line\" d=\"{line}\"/>\
         </svg>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt as _;

    fn check(status: i64) -> Latest {
        Latest {
            time: 0,
            latency_ms: None,
            status,
        }
    }

    async fn test_app() -> Router {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        hora_core::db::migrator().run(&pool).await.expect("migrate");
        let config = hora_core::config::parse(
            "[page]\n[server]\n[[monitors]]\nid = \"web\"\nname = \"Web\"\n\
             target = \"https://example.com\"\ninterval_secs = 60\n",
        )
        .expect("config");
        let (_tx, rx) = watch::channel(Arc::new(config));
        router(AppState::new(pool, rx))
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("x-forwarded-for", "1.2.3.4")
            .body(Body::empty())
            .expect("request")
    }

    #[tokio::test]
    async fn healthz_is_ok() {
        let res = test_app().await.oneshot(get("/healthz")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn unknown_monitor_is_404() {
        let res = test_app()
            .await
            .oneshot(get("/api/monitors/nope/latency"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn known_monitor_latency_is_ok() {
        let res = test_app()
            .await
            .oneshot(get("/api/monitors/web/latency"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn summary_has_security_and_ratelimit_headers() {
        let res = test_app().await.oneshot(get("/api/summary")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(res.headers().contains_key("content-security-policy"));
        assert!(res.headers().contains_key("x-ratelimit-limit"));
    }

    #[tokio::test]
    async fn openapi_and_page_render() {
        assert_eq!(
            test_app()
                .await
                .oneshot(get("/api/openapi.json"))
                .await
                .unwrap()
                .status(),
            StatusCode::OK
        );
        assert_eq!(
            test_app().await.oneshot(get("/")).await.unwrap().status(),
            StatusCode::OK
        );
    }

    #[test]
    fn permille_formats_one_decimal() {
        assert_eq!(format_permille(1000), "100.0%");
        assert_eq!(format_permille(999), "99.9%");
        assert_eq!(format_permille(0), "0.0%");
    }

    #[test]
    fn worse_picks_higher_severity() {
        assert_eq!(worse("up", "degraded"), "degraded");
        assert_eq!(worse("down", "degraded"), "down");
        assert_eq!(worse("up", "unknown"), "unknown");
        assert_eq!(worse("degraded", "up"), "degraded");
    }

    #[test]
    fn derive_status_confirms_down_only_after_threshold() {
        assert_eq!(derive_status(&[check(0), check(0), check(0)], 3), "down");
        assert_eq!(derive_status(&[check(0)], 3), "degraded");
        assert_eq!(derive_status(&[], 3), "unknown");
        assert_eq!(derive_status(&[check(1)], 3), "up");
    }

    #[test]
    fn cert_state_thresholds() {
        assert_eq!(cert_state_for(None, 14), "none");
        assert_eq!(cert_state_for(Some(-1), 14), "expired");
        assert_eq!(cert_state_for(Some(10), 14), "warn");
        assert_eq!(cert_state_for(Some(40), 14), "ok");
    }

    #[test]
    fn build_bar_zero_fills_to_requested_days() {
        let now = DateTime::from_timestamp(1_609_459_200, 0).unwrap();
        let rows = vec![DayRow {
            day: "2021-01-01".to_owned(),
            up: 10,
            down: 0,
            degraded: 0,
        }];
        let bar = build_bar(&rows, now, 7);
        assert_eq!(bar.len(), 7);
        assert_eq!(bar.last().unwrap().state, "up");
        assert_eq!(bar[0].state, "empty");
    }

    #[test]
    fn sparkline_renders_svg_with_status_class() {
        assert!(sparkline(&[], "up").contains("no data"));
        let points = vec![
            Point {
                t: 1,
                latency_ms: 10,
            },
            Point {
                t: 2,
                latency_ms: 20,
            },
        ];
        let svg = sparkline(&points, "degraded");
        assert!(svg.contains("class=\"spark degraded\""));
        assert!(svg.contains("spark-line"));
    }
}
