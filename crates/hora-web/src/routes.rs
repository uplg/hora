//! The axum router and request-scoped middleware.

use std::sync::LazyLock;
use std::time::Duration;

use axum::http::{HeaderValue, Method, Request, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Router, body::Body};
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_http::cors::{Any, CorsLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::handlers::{
    announce, announce_clear, favicon, font, group_page, healthz, heatmap_svg, history_atom,
    history_page, incident_page, latency_json, metrics_prometheus, openapi, page, peer_monitors,
    peer_probe, post_alert, post_event, push, report_page, silence, status_badge, summary_json,
    timeline_page, uptime_badge,
};
use crate::{AppState, CSP, ConfiguredIp};

/// Build the axum router: page, rate-limited JSON API, `OpenAPI`, static assets,
/// CORS, security headers and tracing.
pub fn router(state: AppState) -> Router {
    let config = state.config.borrow().clone();
    let cors = build_cors(&config.server.allowed_origins);

    let mut api = Router::new()
        .route("/api/summary", get(summary_json))
        .route("/api/monitors/{id}/latency", get(latency_json))
        .route("/api/monitors/{id}/alert", post(post_alert))
        .route("/api/push/{id}", post(push))
        .route("/api/silence", post(silence))
        .route("/api/announce", post(announce).delete(announce_clear))
        .route("/api/event", post(post_event))
        .route("/api/peer/probe", post(peer_probe))
        .route("/api/peer/monitors", get(peer_monitors));

    // Parameters are clamped to >= 1, so `finish` always succeeds; if it ever
    // did not, the API simply runs without a rate limit rather than panicking.
    if let Some(governor) = GovernorConfigBuilder::default()
        .per_second(config.server.rate_limit_refill_secs.max(1))
        .burst_size(config.server.rate_limit_burst.max(1))
        .key_extractor(ConfiguredIp::from_config(
            config.server.client_ip_header.as_deref(),
        ))
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
        .route("/api/badge/{id}/status", get(status_badge))
        .route("/api/badge/{id}/uptime", get(uptime_badge))
        // With the badges (outside the rate limiter): the history page embeds
        // one <img> per monitor, which would eat a per-IP burst on its own.
        .route("/api/monitors/{id}/heatmap.svg", get(heatmap_svg))
        .route("/metrics", get(metrics_prometheus))
        .route("/history", get(history_page))
        .route("/history.atom", get(history_atom))
        .route("/incident/{id}", get(incident_page))
        .route("/timeline", get(timeline_page))
        .route("/status/{group}", get(group_page))
        .route("/report/{month}", get(report_page))
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
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        ))
        .layer(cors)
        // The trace span carries the request id so every log line emitted while
        // handling a request can be correlated back to it.
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span))
        // Outermost: stamp each request with an id (honouring an inbound
        // `x-request-id`) before any other layer runs, and echo it on the response.
        .layer(middleware::from_fn(request_id))
        .with_state(state)
}

/// The header carrying the per-request correlation id.
pub(crate) const REQUEST_ID_HEADER: &str = "x-request-id";

/// Stamp the request with a correlation id and echo it on the response. An
/// inbound `x-request-id` (e.g. from a front proxy) is preserved; otherwise a
/// fresh opaque id is minted. Runs outermost, so every inner layer - including
/// the trace span - sees the id.
pub(crate) async fn request_id(mut request: Request<Body>, next: Next) -> Response {
    let id = request
        .headers()
        .get(REQUEST_ID_HEADER)
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
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use axum::http::StatusCode;
    use tokio::sync::watch;
    use tower::ServiceExt as _;
    async fn test_app() -> Router {
        test_app_with_pool().await.0
    }

    async fn test_app_with_pool() -> (Router, sqlx::SqlitePool) {
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
            r#"
            [page]
            [server]
            auth_token = "0123456789abcdef"
            [server.group_tokens]
            App = "appappappappapp1"
            [health]
            id = "test-node"
            [[peers]]
            id = "peer-x"
            name = "Peer X"
            expect_every_secs = 60
            listen_token = "peertok"
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            group = "App"
            [[monitors]]
            id = "intra"
            name = "Intra"
            target = "https://intra.example.com"
            interval_secs = 60
            group = "App"
            public = false
            [[monitors]]
            id = "beat"
            name = "Beat"
            kind = "push"
            interval_secs = 60
            push_token = "s3cret"
            "#,
        )
        .expect("config");
        let config = Arc::new(config);
        let client = hora_core::http::client(None).expect("client");
        let notifier = hora_core::notifications::shared(&config, &client);
        let (_tx, rx) = watch::channel(config);
        let app = router(AppState::new(
            pool.clone(),
            rx,
            Arc::new(AtomicU64::new(0)),
            notifier,
        ));
        (app, pool)
    }

    /// The rate limiter keys on the peer address; oneshot has no real
    /// connection, so supply one.
    fn fake_peer() -> axum::extract::ConnectInfo<std::net::SocketAddr> {
        axum::extract::ConnectInfo(std::net::SocketAddr::from(([127, 0, 0, 1], 12345)))
    }

    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .extension(fake_peer())
            .body(Body::empty())
            .expect("request")
    }
    #[tokio::test]
    async fn healthz_is_ok() {
        let res = test_app().await.oneshot(get("/healthz")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    fn push(uri: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .extension(fake_peer())
            .body(Body::empty())
            .expect("request")
    }

    #[tokio::test]
    async fn push_records_heartbeat_with_token() {
        let res = test_app()
            .await
            .oneshot(push("/api/push/beat?token=s3cret&status=up"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn push_rejects_wrong_token() {
        let res = test_app()
            .await
            .oneshot(push("/api/push/beat?token=wrong"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn push_to_non_push_monitor_is_404() {
        // "web" exists but is an HTTP monitor, not a push target.
        let res = test_app()
            .await
            .oneshot(push("/api/push/web?token=x"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn push_records_peer_heartbeat() {
        // A watched peer's listen id accepts heartbeats, like a push monitor.
        let res = test_app()
            .await
            .oneshot(push("/api/push/peer-x?token=peertok"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn push_rejects_wrong_peer_token() {
        let res = test_app()
            .await
            .oneshot(push("/api/push/peer-x?token=nope"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    /// A JSON POST to the alert endpoint, with an optional `X-Push-Token` (the
    /// item token) and/or `Authorization: Bearer` (the global token).
    fn alert(
        uri: &str,
        body: &str,
        push_token: Option<&str>,
        bearer: Option<&str>,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header("content-type", "application/json")
            .extension(fake_peer());
        if let Some(token) = push_token {
            builder = builder.header("x-push-token", token);
        }
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_owned())).expect("request")
    }

    #[tokio::test]
    async fn alert_dispatches_with_item_token_and_records_it() {
        let (app, pool) = test_app_with_pool().await;
        let res = app
            .oneshot(alert(
                "/api/monitors/beat/alert",
                r#"{"severity":"error","title":"Patch failed","message":"3/12 ops","tags":{"task_id":"t1"}}"#,
                Some("s3cret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let alerts = hora_core::db::recent_pushed_alerts(&pool, 10)
            .await
            .unwrap();
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].monitor_id, "beat");
        assert_eq!(alerts[0].severity, "error");
        assert_eq!(alerts[0].title, "Patch failed");
        // The tag is folded into the stored message alongside the producer text.
        assert!(
            alerts[0].message.contains("3/12 ops") && alerts[0].message.contains("task_id=t1"),
            "{}",
            alerts[0].message
        );
    }

    #[tokio::test]
    async fn alert_accepts_global_auth_token_for_a_non_push_monitor() {
        // "web" is an HTTP monitor with no push_token: the global viewer token
        // authorizes, via either the Bearer header or the ?token= query.
        let res = test_app()
            .await
            .oneshot(alert(
                "/api/monitors/web/alert",
                r#"{"title":"deploy started"}"#,
                None,
                Some("0123456789abcdef"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        let res = test_app()
            .await
            .oneshot(alert(
                "/api/monitors/web/alert?token=0123456789abcdef",
                r#"{"title":"deploy started"}"#,
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn alert_rejects_wrong_or_missing_token() {
        for (push_token, bearer) in [(Some("wrong"), None), (None, None), (None, Some("nope"))] {
            let res = test_app()
                .await
                .oneshot(alert(
                    "/api/monitors/beat/alert",
                    r#"{"title":"x"}"#,
                    push_token,
                    bearer,
                ))
                .await
                .unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "{push_token:?} {bearer:?}"
            );
        }
    }

    #[tokio::test]
    async fn alert_unknown_monitor_is_404() {
        let res = test_app()
            .await
            .oneshot(alert(
                "/api/monitors/nope/alert",
                r#"{"title":"x"}"#,
                None,
                Some("0123456789abcdef"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn alert_rejects_empty_title_and_bad_severity() {
        for body in [r#"{"title":"   "}"#, r#"{"severity":"boom","title":"x"}"#] {
            let res = test_app()
                .await
                .oneshot(alert(
                    "/api/monitors/beat/alert",
                    body,
                    Some("s3cret"),
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{body}");
        }
    }

    #[tokio::test]
    async fn alert_coalesces_a_repeated_dedup_key() {
        let (app, pool) = test_app_with_pool().await;
        let body = r#"{"severity":"error","title":"flap","dedup_key":"ekb:mat"}"#;

        let first = app
            .clone()
            .oneshot(alert(
                "/api/monitors/beat/alert",
                body,
                Some("s3cret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        assert!(body_text(first).await.contains("dispatched"));

        // Same key inside the window: coalesced, not dispatched or recorded again.
        let second = app
            .clone()
            .oneshot(alert(
                "/api/monitors/beat/alert",
                body,
                Some("s3cret"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        assert!(body_text(second).await.contains("coalesced"));

        let alerts = hora_core::db::recent_pushed_alerts(&pool, 10)
            .await
            .unwrap();
        assert_eq!(alerts.len(), 1, "the repeat must not add a second row");
    }

    #[tokio::test]
    async fn alert_history_respects_public_and_private_visibility() {
        let (app, _pool) = test_app_with_pool().await;
        // A public monitor's alert ("web" has no push_token: global token).
        let res = app
            .clone()
            .oneshot(alert(
                "/api/monitors/web/alert",
                r#"{"severity":"warning","title":"web alert","message":"web detail"}"#,
                None,
                Some("0123456789abcdef"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);
        // A private monitor's alert ("intra" is public = false).
        let res = app
            .clone()
            .oneshot(alert(
                "/api/monitors/intra/alert",
                r#"{"severity":"error","title":"intra alert","message":"intra detail"}"#,
                None,
                Some("0123456789abcdef"),
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::ACCEPTED);

        // Anonymous: the public monitor's title shows, its message is collapsed,
        // and the private monitor's alert is hidden outright.
        let anon = body_text(app.clone().oneshot(get("/history")).await.unwrap()).await;
        assert!(anon.contains("web alert"), "{anon}");
        assert!(!anon.contains("web detail"), "{anon}");
        assert!(!anon.contains("intra alert"), "{anon}");

        // Authenticated: full detail, private monitor included.
        let full = body_text(
            app.oneshot(get("/history?token=0123456789abcdef"))
                .await
                .unwrap(),
        )
        .await;
        assert!(
            full.contains("web detail")
                && full.contains("intra alert")
                && full.contains("intra detail"),
            "{full}"
        );
    }

    #[tokio::test]
    async fn event_requires_token_records_and_validates() {
        let (app, pool) = test_app_with_pool().await;
        // Recording a change marker is an operator action: closed anonymously.
        let res = app
            .clone()
            .oneshot(push("/api/event?title=deploy+api+v2.3"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        let res = app
            .clone()
            .oneshot(push(
                "/api/event?title=deploy+api+v2.3&token=0123456789abcdef",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let events = hora_core::db::recent_events(&pool, 10).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].title, "deploy api v2.3");

        let res = app
            .oneshot(push("/api/event?title=++&token=0123456789abcdef"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn events_reach_authenticated_viewers_only() {
        let (app, pool) = test_app_with_pool().await;
        hora_core::db::insert_event(&pool, "deploy api v2.3")
            .await
            .unwrap();

        // Deploy titles are operator info: hidden from the anonymous history
        // page and from the public sparklines.
        let anon = body_text(app.clone().oneshot(get("/history")).await.unwrap()).await;
        assert!(!anon.contains("deploy api v2.3"), "{anon}");
        let page = body_text(app.clone().oneshot(get("/")).await.unwrap()).await;
        assert!(!page.contains("deploy api v2.3"), "{page}");

        let full = body_text(
            app.oneshot(get("/history?token=0123456789abcdef"))
                .await
                .unwrap(),
        )
        .await;
        assert!(full.contains("deploy api v2.3"), "{full}");
    }

    #[tokio::test]
    async fn incident_page_sanitizes_for_anonymous_and_serves_markdown() {
        let (app, pool) = test_app_with_pool().await;
        let id = hora_core::db::insert_incident_start(
            &pool,
            "web",
            Some("HTTP 503: secret stack trace"),
            None,
            &[],
            Some("HTTP/2 503\n\nsecret body"),
            Some("deploy api v2.3, 3m before"),
        )
        .await
        .unwrap();
        hora_core::db::update_incident_vantage(&pool, id, "seen UP by Hora B")
            .await
            .unwrap();

        // Anonymous ("web" is public, no public_error_detail): the reason
        // collapses to its category; snapshot, change and vantage stay private.
        let anon = body_text(
            app.clone()
                .oneshot(get(&format!("/incident/{id}")))
                .await
                .unwrap(),
        )
        .await;
        assert!(anon.contains("HTTP 503"), "{anon}");
        for hidden in ["secret stack trace", "secret body", "deploy api", "Hora B"] {
            assert!(!anon.contains(hidden), "{hidden} leaked:\n{anon}");
        }

        // Authenticated: full detail plus the copyable markdown post-mortem.
        let full = body_text(
            app.clone()
                .oneshot(get(&format!("/incident/{id}?token=0123456789abcdef")))
                .await
                .unwrap(),
        )
        .await;
        assert!(full.contains("secret stack trace") && full.contains("secret body"));
        assert!(full.contains("deploy api v2.3, 3m before"));
        assert!(full.contains("seen UP by Hora B"));
        assert!(full.contains("# Post-mortem"), "{full}");

        let res = app.oneshot(get("/incident/99999")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn private_monitor_incident_page_is_hidden_from_anonymous() {
        let (app, pool) = test_app_with_pool().await;
        let id = hora_core::db::insert_incident_start(&pool, "intra", None, None, &[], None, None)
            .await
            .unwrap();

        // A private monitor's post-mortem answers like a missing page.
        let res = app
            .clone()
            .oneshot(get(&format!("/incident/{id}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = app
            .oneshot(get(&format!("/incident/{id}?token=0123456789abcdef")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn timeline_merges_and_keeps_operator_streams_private() {
        let (app, pool) = test_app_with_pool().await;
        hora_core::db::insert_event(&pool, "deploy api v2.3")
            .await
            .unwrap();
        hora_core::db::insert_silence(
            &pool,
            "web",
            chrono::Utc::now().timestamp() + 600,
            Some("deploying"),
        )
        .await
        .unwrap();
        let incident_id = hora_core::db::insert_incident_start(
            &pool,
            "web",
            Some("HTTP 503: secret detail"),
            None,
            &[],
            None,
            None,
        )
        .await
        .unwrap();
        hora_core::db::insert_announcement(&pool, "Fiber cut", "ETA 6pm", "warning", None)
            .await
            .unwrap();

        // Anonymous: the public incident (sanitized) and the announcement -
        // never the operator streams (events, silences) or the raw reason.
        let anon = body_text(app.clone().oneshot(get("/timeline")).await.unwrap()).await;
        assert!(anon.contains("Web down"), "{anon}");
        assert!(anon.contains("HTTP 503") && !anon.contains("secret detail"));
        assert!(anon.contains("Fiber cut"));
        for hidden in ["deploy api v2.3", "silenced", "deploying"] {
            assert!(!anon.contains(hidden), "{hidden} leaked:\n{anon}");
        }

        // Authenticated: everything, with the post-mortem link.
        let full = body_text(
            app.oneshot(get("/timeline?token=0123456789abcdef"))
                .await
                .unwrap(),
        )
        .await;
        for shown in [
            "deploy api v2.3",
            "silenced Web for 10m",
            "deploying",
            "secret detail",
            "Fiber cut",
        ] {
            assert!(full.contains(shown), "{shown} missing:\n{full}");
        }
        assert!(
            full.contains(&format!("/incident/{incident_id}?token=")),
            "{full}"
        );
    }

    #[tokio::test]
    async fn report_renders_and_rejects_bad_months() {
        let res = test_app()
            .await
            .oneshot(get("/report/2021-01"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_text(res).await;
        assert!(
            body.contains("SLA report") && body.contains("January 2021"),
            "{body}"
        );
        // Anonymous: the private monitor stays out of the report.
        assert!(!body.contains("Intra"), "{body}");

        for bad in ["/report/never", "/report/2999-01"] {
            let res = test_app().await.oneshot(get(bad)).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[tokio::test]
    async fn group_report_scopes_and_honours_the_group_token() {
        // The group token reveals the group's private monitor on ITS report.
        let res = test_app()
            .await
            .oneshot(get("/report/2021-01?group=App&token=appappappappapp1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_text(res).await;
        assert!(body.contains("Intra") && body.contains("Web"), "{body}");
        // Scoped: the push monitor (ungrouped) is not in a group report.
        assert!(!body.contains("Beat"), "{body}");

        // An unknown group answers like a missing page.
        let res = test_app()
            .await
            .oneshot(get("/report/2021-01?group=Nope"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    /// Build an app from an arbitrary config TOML (the shared `test_app` has a
    /// fixed one; the peer-probe tests need targets bound to live local ports).
    async fn app_from(toml: &str) -> Router {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        hora_core::db::migrator().run(&pool).await.expect("migrate");
        let config = Arc::new(hora_core::config::parse(toml).expect("config"));
        let client = hora_core::http::client(None).expect("client");
        let notifier = hora_core::notifications::shared(&config, &client);
        let (tx, rx) = watch::channel(config);
        // Keep the sender alive for the app's lifetime.
        std::mem::forget(tx);
        router(AppState::new(
            pool,
            rx,
            Arc::new(AtomicU64::new(0)),
            notifier,
        ))
    }

    /// Node B's config for the peer-probe tests: it knows the tcp target and
    /// expects requests from peer `hora-a` with this token.
    fn vantage_config(target_port: u16) -> String {
        format!(
            r#"
            [page]
            [server]
            [health]
            id = "hora-b"
            [[peers]]
            id = "hora-a"
            name = "A"
            expect_every_secs = 60
            listen_token = "tok-a-to-b-16char"
            [[monitors]]
            id = "svc"
            name = "Svc"
            kind = "tcp"
            target = "127.0.0.1:{target_port}"
            interval_secs = 60
            timeout_secs = 2
            "#
        )
    }

    fn probe_request(body: &str, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/peer/probe")
            .header("content-type", "application/json")
            .extension(fake_peer());
        if let Some(token) = token {
            builder = builder.header("x-push-token", token);
        }
        builder.body(Body::from(body.to_owned())).expect("request")
    }

    #[tokio::test]
    async fn peer_probe_authenticates_strictly() {
        let service = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();
        let body = format!(r#"{{"from":"hora-a","kind":"tcp","target":"127.0.0.1:{port}"}}"#);

        // Wrong token, missing token, unknown peer: all 401, indistinguishable.
        for (from, token) in [
            ("hora-a", Some("wrong")),
            ("hora-a", None),
            ("nobody", Some("tok-a-to-b-16char")),
        ] {
            let body = body.replace("hora-a", from);
            let res = app_from(&vantage_config(port))
                .await
                .oneshot(probe_request(&body, token))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{from} {token:?}");
        }
    }

    #[tokio::test]
    async fn peer_probe_refuses_targets_outside_its_config() {
        let service = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();

        // The SSRF guard: an authenticated peer asking for an arbitrary target
        // (or the right target under another kind) gets a 404, never a probe.
        for body in [
            r#"{"from":"hora-a","kind":"tcp","target":"169.254.169.254:80"}"#.to_owned(),
            format!(r#"{{"from":"hora-a","kind":"http","target":"127.0.0.1:{port}"}}"#),
        ] {
            let res = app_from(&vantage_config(port))
                .await
                .oneshot(probe_request(&body, Some("tok-a-to-b-16char")))
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::NOT_FOUND, "{body}");
        }
    }

    #[tokio::test]
    async fn peer_probe_reports_its_own_vantage() {
        let service = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();
        let body = format!(r#"{{"from":"hora-a","kind":"tcp","target":"127.0.0.1:{port}"}}"#);
        let config = vantage_config(port);

        // Service listening: up from this vantage.
        let res = app_from(&config)
            .await
            .oneshot(probe_request(&body, Some("tok-a-to-b-16char")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let verdict: hora_core::confirm::ProbeResponse =
            serde_json::from_str(&body_text(res).await).unwrap();
        assert!(verdict.up);

        // Service gone: down from this vantage, with a reason.
        drop(service);
        let res = app_from(&config)
            .await
            .oneshot(probe_request(&body, Some("tok-a-to-b-16char")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let verdict: hora_core::confirm::ProbeResponse =
            serde_json::from_str(&body_text(res).await).unwrap();
        assert!(!verdict.up);
        assert!(verdict.error.is_some());
    }

    #[tokio::test]
    async fn peer_monitors_authenticates_and_discloses_probeable_targets() {
        // Same strict auth as the probe endpoint: wrong token, missing token
        // or unknown peer all answer 401.
        for (from, token) in [
            ("peer-x", Some("wrong")),
            ("peer-x", None),
            ("nobody", Some("peertok")),
        ] {
            let mut builder = Request::builder()
                .uri(format!("/api/peer/monitors?from={from}"))
                .extension(fake_peer());
            if let Some(token) = token {
                builder = builder.header("x-push-token", token);
            }
            let res = test_app()
                .await
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{from} {token:?}");
        }

        // Authenticated: every probeable monitor (private ones included - a
        // mesh member is operator infrastructure), never the push targets.
        let res = test_app()
            .await
            .oneshot(
                Request::builder()
                    .uri("/api/peer/monitors?from=peer-x")
                    .header("x-push-token", "peertok")
                    .extension(fake_peer())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let answer: hora_core::confirm::PeerMonitors =
            serde_json::from_str(&body_text(res).await).unwrap();
        let targets: Vec<&str> = answer
            .monitors
            .iter()
            .map(|monitor| monitor.target.as_str())
            .collect();
        assert!(targets.contains(&"https://example.com"));
        assert!(targets.contains(&"https://intra.example.com"));
        assert_eq!(answer.monitors.len(), 2, "push monitors are not targets");
    }

    #[tokio::test]
    async fn peer_monitors_match_the_full_summary() {
        // The peer answer skips the page rebuild; it must still report exactly
        // the status and p50 the authenticated summary shows.
        let (app, pool) = test_app_with_pool().await;
        let now = chrono::Utc::now().timestamp();
        for (offset, latency) in [(60, 40), (120, 10), (180, 30), (240, 20), (300, 50)] {
            sqlx::query(
                "INSERT INTO checks (time, monitor_id, status, latency_ms) VALUES (?, 'web', 1, ?)",
            )
            .bind(now - offset)
            .bind(latency)
            .execute(&pool)
            .await
            .unwrap();
        }
        for offset in [60, 120, 180, 240] {
            sqlx::query("INSERT INTO checks (time, monitor_id, status, latency_ms) VALUES (?, 'intra', 0, NULL)")
                .bind(now - offset)
                .execute(&pool)
                .await
                .unwrap();
        }

        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/peer/monitors?from=peer-x")
                    .header("x-push-token", "peertok")
                    .extension(fake_peer())
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let peer: hora_core::confirm::PeerMonitors =
            serde_json::from_str(&body_text(res).await).unwrap();
        let res = app
            .oneshot(get("/api/summary?token=0123456789abcdef"))
            .await
            .unwrap();
        let summary: serde_json::Value = serde_json::from_str(&body_text(res).await).unwrap();

        for (id, target) in [
            ("web", "https://example.com"),
            ("intra", "https://intra.example.com"),
        ] {
            let view = summary["monitors"]
                .as_array()
                .unwrap()
                .iter()
                .find(|view| view["id"] == id)
                .unwrap();
            let answer = peer.monitors.iter().find(|m| m.target == target).unwrap();
            assert_eq!(answer.status, view["status"].as_str().unwrap(), "{id}");
            assert_eq!(answer.p50_ms, view["latency_p50_ms"].as_i64(), "{id}");
        }
        let web = peer
            .monitors
            .iter()
            .find(|m| m.target == "https://example.com")
            .unwrap();
        assert_eq!((web.status.as_str(), web.p50_ms), ("up", Some(30)));
        let intra = peer
            .monitors
            .iter()
            .find(|m| m.target == "https://intra.example.com")
            .unwrap();
        assert_eq!((intra.status.as_str(), intra.p50_ms), ("down", None));
    }

    /// The vantage exchange end to end: the poller-side fetch against a peer
    /// served over real HTTP, exactly as `hora peers diff` and the display
    /// poller consume it.
    #[tokio::test]
    async fn vantage_fetch_reads_a_real_peer() {
        let service = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();
        let app_b = app_from(&vantage_config(port)).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_b = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app_b.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });

        let client = hora_core::http::client(None).expect("client");
        let url = format!("http://{addr_b}/api/peer/monitors");
        let answer = hora_core::vantage::fetch_peer_monitors(
            &client,
            &url,
            "hora-a",
            Some("tok-a-to-b-16char"),
        )
        .await
        .expect("peer answered");
        assert_eq!(answer.monitors.len(), 1);
        assert_eq!(answer.monitors[0].target, format!("127.0.0.1:{port}"));

        // A wrong token fails closed into None, never a panic or a partial read.
        let refused =
            hora_core::vantage::fetch_peer_monitors(&client, &url, "hora-a", Some("wrong")).await;
        assert!(refused.is_none());
    }

    /// The full two-node round trip: node A confirms a down with node B over
    /// real HTTP (B served on a localhost socket), in every disagreement mode.
    #[tokio::test]
    async fn multi_vantage_confirms_across_two_real_nodes() {
        // The monitored "service": a local TCP listener both nodes target.
        let service = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = service.local_addr().unwrap().port();

        // Node B, served for real so node A's HTTP client talks to it.
        let app_b = app_from(&vantage_config(port)).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_b = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app_b.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });

        // Node A: same target, peer B at its real address, confirmation on.
        let config_a = hora_core::config::parse(&format!(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            confirm_with_peers = true
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "http://{addr_b}/api/push/hora-a"
            ping_token = "tok-a-to-b-16char"
            [[monitors]]
            id = "svc"
            name = "Svc"
            kind = "tcp"
            target = "127.0.0.1:{port}"
            interval_secs = 60
            timeout_secs = 2
            "#
        ))
        .expect("config a");
        let client = hora_core::http::client(None).expect("client");

        // Disagreement: A thinks it is down, B still reaches it.
        let verdict =
            hora_core::confirm::confirm_with_peers(&client, &config_a, &config_a.monitors[0])
                .await
                .expect("peers were asked");
        assert!(verdict.contains("seen UP by Hora B"), "{verdict}");
        assert!(verdict.contains("network issue"), "{verdict}");

        // Real outage: the service is gone for B too.
        drop(service);
        let verdict =
            hora_core::confirm::confirm_with_peers(&client, &config_a, &config_a.monitors[0])
                .await
                .expect("peers were asked");
        assert_eq!(verdict, "confirmed down from 2/2 vantage points");

        // Fail open: a wrong token makes B answer 401 - the alert is
        // annotated as unconfirmed, never blocked.
        let mut config_bad = hora_core::config::parse(&format!(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            confirm_with_peers = true
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "http://{addr_b}/api/push/hora-a"
            ping_token = "wrong-token-16chars"
            [[monitors]]
            id = "svc"
            name = "Svc"
            kind = "tcp"
            target = "127.0.0.1:{port}"
            interval_secs = 60
            timeout_secs = 2
            "#
        ))
        .expect("config bad");
        let verdict =
            hora_core::confirm::confirm_with_peers(&client, &config_bad, &config_bad.monitors[0])
                .await
                .expect("peers were asked");
        assert!(verdict.contains("no peer vantage reachable"), "{verdict}");

        // Fail open: a peer that is not even listening behaves the same.
        config_bad.peers[0].ping_url = Some(hora_core::config::Secret(
            "http://127.0.0.1:9/api/push/hora-a".to_owned(),
        ));
        let verdict =
            hora_core::confirm::confirm_with_peers(&client, &config_bad, &config_bad.monitors[0])
                .await
                .expect("peers were asked");
        assert!(verdict.contains("no peer vantage reachable"), "{verdict}");
    }

    /// Read a response body as text (the pages are small).
    async fn body_text(res: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .expect("body");
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn group_page_filters_and_404s_unknown() {
        let res = test_app().await.oneshot(get("/status/App")).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = body_text(res).await;
        // Anonymous: the group's public monitor only - and no peers section.
        assert!(body.contains("Web"), "{body}");
        assert!(!body.contains("Intra"), "{body}");
        assert!(!body.contains("Peer X"), "{body}");

        let res = test_app().await.oneshot(get("/status/Nope")).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn group_token_reveals_its_group_and_nothing_else() {
        // The group token unlocks the group's private monitors...
        let res = test_app()
            .await
            .oneshot(get("/status/App?token=appappappappapp1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_text(res).await.contains("Intra"));

        // ...but is NOT a global viewer token: the main summary stays public.
        let res = test_app()
            .await
            .oneshot(get("/api/summary?token=appappappappapp1"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!body_text(res).await.contains("intra"));
    }

    #[tokio::test]
    async fn announce_requires_token_pins_and_clears() {
        // Closed without the viewer token.
        let res = test_app()
            .await
            .oneshot(push("/api/announce?title=Fiber+cut"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // Pinned: the banner shows up in the public summary for everyone.
        let (app, _pool) = test_app_with_pool().await;
        let res = app
            .clone()
            .oneshot(push(
                "/api/announce?title=Fiber+cut&body=ETA+6pm&severity=warning&until=4h&token=0123456789abcdef",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app.clone().oneshot(get("/api/summary")).await.unwrap();
        let body = body_text(res).await;
        assert!(
            body.contains("Fiber cut") && body.contains("warning"),
            "{body}"
        );

        // Cleared via DELETE: gone from the summary.
        let req = Request::builder()
            .method("DELETE")
            .uri("/api/announce?token=0123456789abcdef")
            .extension(fake_peer())
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let res = app.oneshot(get("/api/summary")).await.unwrap();
        assert!(!body_text(res).await.contains("Fiber cut"));
    }

    #[tokio::test]
    async fn announce_rejects_bad_severity_and_empty_title() {
        for bad in [
            "/api/announce?title=x&severity=panic&token=0123456789abcdef",
            "/api/announce?title=+&token=0123456789abcdef",
            "/api/announce?title=x&until=nope&token=0123456789abcdef",
        ] {
            let res = test_app().await.oneshot(push(bad)).await.unwrap();
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
        }
    }

    #[tokio::test]
    async fn silence_requires_the_viewer_token() {
        let res = test_app()
            .await
            .oneshot(push("/api/silence?monitors=web&duration=10m"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn silence_mutes_the_monitor_in_the_database() {
        let (app, pool) = test_app_with_pool().await;
        let res = app
            .oneshot(push(
                "/api/silence?monitors=web&duration=10m&reason=deploy&token=0123456789abcdef",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        let now = chrono::Utc::now().timestamp();
        assert!(hora_core::db::is_silenced(&pool, "web", now).await.unwrap());
        assert!(
            !hora_core::db::is_silenced(&pool, "beat", now)
                .await
                .unwrap()
        );
        // Within the requested window, never past it.
        assert!(
            !hora_core::db::is_silenced(&pool, "web", now + 601)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn silence_all_uses_the_wildcard() {
        let (app, pool) = test_app_with_pool().await;
        let res = app
            .oneshot(push(
                "/api/silence?monitors=all&duration=5m&token=0123456789abcdef",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let now = chrono::Utc::now().timestamp();
        assert!(
            hora_core::db::is_silenced(&pool, "beat", now)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn silence_rejects_unknown_monitor_and_bad_duration() {
        let res = test_app()
            .await
            .oneshot(push(
                "/api/silence?monitors=nope&duration=10m&token=0123456789abcdef",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let res = test_app()
            .await
            .oneshot(push(
                "/api/silence?monitors=web&duration=tomorrow&token=0123456789abcdef",
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn heatmap_is_svg_and_unknown_is_404() {
        let res = test_app()
            .await
            .oneshot(get("/api/monitors/web/heatmap.svg"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get("content-type").unwrap(), "image/svg+xml");

        let res = test_app()
            .await
            .oneshot(get("/api/monitors/nope/heatmap.svg"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
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
    async fn response_carries_a_minted_request_id() {
        let res = test_app().await.oneshot(get("/healthz")).await.unwrap();
        let id = res
            .headers()
            .get(REQUEST_ID_HEADER)
            .expect("x-request-id present")
            .to_str()
            .expect("ascii");
        // Minted ids are 32 hex chars (16-char random prefix + 16-char counter).
        assert_eq!(id.len(), 32);
        assert!(id.bytes().all(|b| b.is_ascii_hexdigit()));
    }

    #[tokio::test]
    async fn inbound_request_id_is_preserved() {
        let request = Request::builder()
            .uri("/healthz")
            .header(REQUEST_ID_HEADER, "trace-from-proxy")
            .body(Body::empty())
            .expect("request");
        let res = test_app().await.oneshot(request).await.unwrap();
        assert_eq!(
            res.headers().get(REQUEST_ID_HEADER).unwrap(),
            "trace-from-proxy"
        );
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
    #[tokio::test]
    async fn status_badge_is_svg() {
        let app = test_app().await;
        let res = app
            .clone()
            .oneshot(get("/api/badge/web/status"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers().get("content-type").unwrap(), "image/svg+xml");
        assert!(body_text(res).await.contains(r#"id="s""#));

        let res = app
            .clone()
            .oneshot(get("/api/badge/web/status?style=flat-square"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(!body_text(res).await.contains(r#"id="s""#));

        let res = app
            .oneshot(get("/api/badge/web/status?style=for-the-badge"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        assert!(body_text(res).await.contains(r#"height="28""#));
    }

    #[tokio::test]
    async fn unknown_badge_is_404() {
        let res = test_app()
            .await
            .oneshot(get("/api/badge/nope/uptime"))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND);
    }
}
