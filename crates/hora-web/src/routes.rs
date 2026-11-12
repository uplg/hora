//! The axum router and request-scoped middleware.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::http::{HeaderName, HeaderValue, Method, Request, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Router, body::Body};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_http::classify::{
    ClassifiedResponse, ClassifyResponse, NeverClassifyEos, ServerErrorsAsFailures,
    ServerErrorsFailureClass, SharedClassifier,
};
use tower_http::compression::CompressionLayer;
use tower_http::compression::predicate::{DefaultPredicate, Predicate};
use tower_http::cors::{Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::auth::{log_failed_auth, presents_credential, refuse_query_token};
use crate::handlers::annotations::{announce, announce_clear, post_event, silence};
use crate::handlers::api::{latency_json, metrics_prometheus, summary_json};
use crate::handlers::assets::{apple_touch_icon, asset, favicon, logo, logo_dark, manifest};
use crate::handlers::badges::{heatmap_svg, status_badge, uptime_badge};
use crate::handlers::maintenance;
use crate::handlers::pages::{
    group_page, history_atom, history_page, incident_page, monitor_page, page, report_page,
    timeline_page, watchers_page,
};
use crate::handlers::peer::{peer_monitors, peer_probe};
use crate::handlers::push::{post_alert, push};
use crate::handlers::{healthz, openapi};
use crate::{AppState, CSP, ConfiguredIp, PERMISSIONS_POLICY};

/// How much more generous the page limiter is than the API one, in both burst
/// and refill rate: a history page pulls one heatmap image per monitor, and a
/// status page refreshes itself, so a real visitor needs more headroom than an
/// API client - while a loop hammering `/report/...` still hits a wall.
const PAGE_LIMIT_FACTOR: u32 = 4;

/// How long a request may take before it is answered 503 and its work
/// dropped. Without a deadline a request stuck behind a slow database (or a
/// first build that does not come) held its connection for ever, and one such
/// request kept a graceful stop waiting until the supervisor killed the
/// process.
///
/// Above everything a handler legitimately waits for: an operator write
/// waits up to [`crate::snapshot::WRITE_VISIBLE_TIMEOUT`] (20 s) for the
/// build that shows it, a peer's probe runs for at most
/// `mesh::confirm::PROBE_DEADLINE` (10 s), and the pages answer from memory.
/// Below the 60 s most reverse proxies give an upstream, so the client reads
/// Hora's own answer rather than the proxy's 504.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

// The deadline must not cut a wait a handler bounds itself.
const _: () = {
    assert!(REQUEST_TIMEOUT.as_secs() > crate::snapshot::WRITE_VISIBLE_TIMEOUT.as_secs());
    assert!(REQUEST_TIMEOUT.as_secs() > hora_core::mesh::confirm::PROBE_DEADLINE.as_secs());
};

/// The largest request body an extractor buffers. The bodies Hora takes are
/// a few fields of JSON; axum's default (2 MiB) let a few hundred slow
/// uploads hold several hundred MiB.
const MAX_BODY_BYTES: usize = 64 * 1024;

/// Build the axum router: page, rate-limited JSON API, `OpenAPI`, static assets,
/// CORS, security headers, tracing and the request deadline
/// ([`REQUEST_TIMEOUT`]).
///
/// The `[server]` settings read here (`allowed_origins`, `rate_limit_*`,
/// `client_ip_header`) are fixed for the router's lifetime: changing them
/// needs a restart, unlike the rest of the config, which handlers read live.
pub fn router(state: AppState) -> Router {
    router_with_deadline(state, REQUEST_TIMEOUT)
}

/// [`router`] with a deadline of the caller's choosing (the tests' way to
/// reach it without waiting for it).
pub(crate) fn router_with_deadline(state: AppState, deadline: Duration) -> Router {
    let config = state.config.borrow().clone();
    let cors = build_cors(&config.server.allowed_origins);
    let key = ConfiguredIp::from_config(config.server.client_ip_header.as_deref())
        .trusting(config.server.trusted_proxies);
    let refill = Duration::from_secs(config.server.rate_limit_refill_secs.max(1));
    let burst = config.server.rate_limit_burst.max(1);

    // Every write refuses `?token=` (see `refuse_query_token`, which lets
    // the GETs through); the read-only views keep it first-class. A 401 to a
    // request that showed a credential is logged, behind the rate limiter.
    let failed_auth = || middleware::from_fn_with_state(key.clone(), log_failed_auth);
    let api = Router::new()
        .route("/api/summary", get(summary_json))
        .route("/api/monitors/{id}/latency", get(latency_json))
        .route("/api/peer/probe", post(peer_probe))
        .route("/api/peer/monitors", get(peer_monitors))
        .route("/api/monitors/{id}/alert", post(post_alert))
        .route("/api/push/{id}", post(push))
        .route("/api/silence", post(silence))
        .route("/api/announce", post(announce).delete(announce_clear))
        .route("/api/event", post(post_event))
        .route(
            "/api/maintenance",
            post(maintenance::plan).get(maintenance::list),
        )
        .route("/api/maintenance/{id}", delete(maintenance::cancel))
        .route_layer(middleware::from_fn(refuse_query_token))
        .route_layer(failed_auth());
    let api = rate_limited(api, refill, burst, key.clone());

    // Every other dynamic route reads the database (the report and heatmaps
    // scan weeks of history), so it gets its own, more generous, per-IP limit.
    let pages = Router::new()
        .route("/", get(page))
        .route("/healthz", get(healthz))
        .route("/api/badge/{id}/status", get(status_badge))
        .route("/api/badge/{id}/uptime", get(uptime_badge))
        .route("/api/monitors/{id}/heatmap.svg", get(heatmap_svg))
        .route("/metrics", get(metrics_prometheus))
        .route("/history", get(history_page))
        .route("/history.atom", get(history_atom))
        .route("/incident/{id}", get(incident_page))
        .route("/monitor/{id}", get(monitor_page))
        .route("/watchers", get(watchers_page))
        .route("/timeline", get(timeline_page))
        .route("/status/{group}", get(group_page))
        .route("/report/{month}", get(report_page))
        .route_layer(failed_auth());
    let pages = rate_limited(
        pages,
        refill / PAGE_LIMIT_FACTOR,
        burst.saturating_mul(PAGE_LIMIT_FACTOR),
        key,
    );

    Router::new()
        // Static: compiled-in assets and the generated document, never limited.
        .route("/favicon.svg", get(favicon))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
        .route("/manifest.webmanifest", get(manifest))
        .route("/logo", get(logo))
        .route("/logo-dark", get(logo_dark))
        .route("/assets/{*path}", get(asset))
        .route("/api/openapi.json", get(openapi))
        .merge(pages)
        .merge(api)
        // What a JSON body may weigh (an alert, a peer's probe request): the
        // extractors buffer it before the handler has checked any token.
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        // Innermost of the global layers, so the 503 of a request past its
        // deadline still gets the security headers, the request id and its
        // line in the trace like any other answer.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            deadline,
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(CSP),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static(PERMISSIONS_POLICY),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("cross-origin-opener-policy"),
            HeaderValue::from_static("same-origin"),
        ))
        .layer(cors)
        .layer(middleware::from_fn(no_store_with_credentials))
        // Outside the layer above, so it sees its `no-store` (see `compressible`).
        .layer(CompressionLayer::new().compress_when(DefaultPredicate::new().and(compressible)))
        // The trace span carries the request id so every log line emitted while
        // handling a request can be correlated back to it.
        .layer(
            TraceLayer::new(SharedClassifier::new(StartingIsNoFailure))
                .make_span_with(make_request_span),
        )
        // Outermost: stamp each request with an id (honouring a well-formed
        // inbound `x-request-id`) before any other layer runs, and echo it on
        // the response.
        .layer(middleware::from_fn(request_id))
        .with_state(state)
}

/// Whether a response may be gzipped. HTML, JSON, the text exposition and the
/// stylesheet shrink about fivefold; the default predicate already leaves
/// images and tiny bodies alone. Two more are left as they are:
///
/// - fonts: woff2 is compressed already;
/// - a credentialed viewer's response (marked `no-store` by
///   [`no_store_with_credentials`]): its page carries the token in its links,
///   and compressing a secret next to request-controlled text is the setting
///   of the BREACH attack. Those are the operator's own views, not the traffic.
fn compressible(
    _status: StatusCode,
    _version: axum::http::Version,
    headers: &axum::http::HeaderMap,
    _extensions: &axum::http::Extensions,
) -> bool {
    let has = |name: header::HeaderName, needle: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.contains(needle))
    };
    !has(header::CONTENT_TYPE, "font/") && !has(header::CACHE_CONTROL, "no-store")
}

/// A response to a request that carried a credential (`Authorization`,
/// `X-Push-Token` or `?token=`) is that viewer's own: an operator or group
/// view, or a write's answer. Mark it `no-store` so neither a shared cache in
/// front of Hora nor the browser's disk cache keeps it (nothing else sets a
/// `Cache-Control` on the pages, so heuristic caching would apply).
pub(crate) async fn no_store_with_credentials(request: Request<Body>, next: Next) -> Response {
    let credentialed = presents_credential(&request);
    let mut response = next.run(request).await;
    if credentialed {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// Put `routes` behind a per-IP token bucket (`burst` requests, one more every
/// `period`), answering 429 with `x-ratelimit-*` headers past it.
fn rate_limited(
    routes: Router<AppState>,
    period: Duration,
    burst: u32,
    key: ConfiguredIp,
) -> Router<AppState> {
    // Parameters are clamped above zero, so `finish` always succeeds; if it
    // ever did not, the routes simply run without a rate limit rather than
    // panicking.
    let Some(governor) = GovernorConfigBuilder::default()
        .period(period.max(Duration::from_millis(1)))
        .burst_size(burst.max(1))
        .key_extractor(key)
        .use_headers()
        .finish()
    else {
        return routes;
    };
    // Periodically drop idle per-IP buckets so memory stays bounded. The task
    // holds the limiter weakly and ends once the router (the layer owning
    // it) is dropped - at shutdown, or when a test discards its app.
    let limiter = Arc::downgrade(governor.limiter());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_mins(1));
        loop {
            ticker.tick().await;
            let Some(limiter) = limiter.upgrade() else {
                break;
            };
            limiter.retain_recent();
        }
    });
    routes.layer(GovernorLayer::new(governor))
}

/// The header carrying the per-request correlation id.
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";

/// Stamp the request with a correlation id and echo it on the response. An
/// inbound `x-request-id` (e.g. from a front proxy) is preserved when it looks
/// like an id; anything else is replaced by a fresh opaque id, so a client
/// cannot forge arbitrary text into every log line. Runs outermost, so every
/// inner layer - including the trace span - sees the id.
pub(crate) async fn request_id(mut request: Request<Body>, next: Next) -> Response {
    let id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .filter(|value| is_valid_request_id(value.as_bytes()))
        .cloned()
        .unwrap_or_else(|| {
            HeaderValue::from_str(&new_request_id())
                .unwrap_or_else(|_| HeaderValue::from_static("unknown"))
        });
    request.headers_mut().insert(REQUEST_ID_HEADER, id.clone());
    let mut response = next.run(request).await;
    response.headers_mut().insert(REQUEST_ID_HEADER, id);
    response
}

/// Whether an inbound request id is acceptable: 1 to 64 of `[A-Za-z0-9._-]`
/// (covers UUIDs, hex trace ids and the usual proxy formats).
fn is_valid_request_id(raw: &[u8]) -> bool {
    (1..=64).contains(&raw.len())
        && raw
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// Mint an opaque request id: a per-process random prefix (so ids never collide
/// across restarts) followed by a monotonic counter. Uses only `std`, so no
/// extra dependency just to generate an id.
pub(crate) fn new_request_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    static PREFIX: LazyLock<u64> = LazyLock::new(|| {
        // `RandomState` is seeded from the OS RNG; hashing nothing yields a value
        // derived from that seed - a cheap source of per-process randomness.
        std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish()
    });

    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:016x}{counter:016x}", *PREFIX)
}

/// Build the tracing span for a request, tagged with its `x-request-id` so log
/// lines can be correlated. The id is always present: the request first passes
/// through the [`request_id`] middleware, which is the outermost layer.
/// Marks a response as the expected answer of a node still starting (see
/// [`crate::handlers::healthz`]): a 503 that is not a failure to log.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Starting;

/// The trace layer's classifier: a server error is a failure, logged at ERROR
/// (tower-http's default), except a response marked [`Starting`]. Docker's
/// HEALTHCHECK polls `/healthz` from the first second, and every 503 before
/// the scheduler's first tick was an ERROR line on a perfectly normal boot.
#[derive(Clone, Copy, Debug)]
struct StartingIsNoFailure;

impl ClassifyResponse for StartingIsNoFailure {
    type FailureClass = ServerErrorsFailureClass;
    type ClassifyEos = NeverClassifyEos<ServerErrorsFailureClass>;

    fn classify_response<B>(
        self,
        res: &Response<B>,
    ) -> ClassifiedResponse<Self::FailureClass, Self::ClassifyEos> {
        if res.extensions().get::<Starting>().is_some() {
            return ClassifiedResponse::Ready(Ok(()));
        }
        ServerErrorsAsFailures::new().classify_response(res)
    }

    fn classify_error<E: std::fmt::Display + 'static>(self, error: &E) -> Self::FailureClass {
        ServerErrorsAsFailures::new().classify_error(error)
    }
}

pub(crate) fn make_request_span(request: &Request<Body>) -> tracing::Span {
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown");
    // Log only the path, never the query: push and viewer tokens travel as
    // `?token=...`, and the full URI would leak them into the access log.
    tracing::info_span!(
        "request",
        method = %request.method(),
        path = %request.uri().path(),
        request_id,
    )
}

pub(crate) fn build_cors(origins: &[String]) -> CorsLayer {
    let cors = CorsLayer::new().allow_methods([Method::GET]);
    if origins.is_empty() {
        return cors.allow_origin(Any);
    }
    let parsed: Vec<HeaderValue> = origins
        .iter()
        .filter_map(|origin| {
            origin
                .parse()
                .map_err(|_| tracing::warn!("ignoring invalid allowed_origin {origin:?}"))
                .ok()
        })
        .collect();
    cors.allow_origin(parsed)
}

#[cfg(test)]
mod tests;
