//! The pages: incidents, timeline, monthly report, group pages and tenants.

use super::*;

#[tokio::test]
async fn incident_page_sanitizes_for_anonymous_and_serves_markdown() {
    let (app, store) = test_app_with_pool().await;
    let id = hora_core::db::insert_incident_start(
        &store,
        "web",
        Some("HTTP 503: secret stack trace"),
        None,
        None,
        &[],
        Some("HTTP/2 503\n\nsecret body"),
        Some("deploy api v2.3, 3 min before"),
    )
    .await
    .unwrap();
    hora_core::db::update_incident_vantage(&store, id, "seen UP by Hora B")
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
    assert!(full.contains("deploy api v2.3, 3 min before"));
    assert!(full.contains("seen UP by Hora B"));
    assert!(full.contains("# Post-mortem"), "{full}");

    let res = app.oneshot(get("/incident/99999")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn private_monitor_incident_page_is_hidden_from_anonymous() {
    let (app, store) = test_app_with_pool().await;
    let id =
        hora_core::db::insert_incident_start(&store, "intra", None, None, None, &[], None, None)
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
    let (app, store) = test_app_with_pool().await;
    hora_core::db::insert_event(&store, "deploy api v2.3")
        .await
        .unwrap();
    hora_core::db::insert_silence(
        &store,
        "web",
        chrono::Utc::now().timestamp() + 600,
        Some("deploying"),
    )
    .await
    .unwrap();
    let incident_id = hora_core::db::insert_incident_start(
        &store,
        "web",
        Some("HTTP 503: secret detail"),
        None,
        None,
        &[],
        None,
        None,
    )
    .await
    .unwrap();
    hora_core::db::insert_announcement(
        &store,
        "Fiber cut",
        "ETA 6pm",
        hora_core::config::Severity::Warning,
        None,
    )
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
        "silenced Web for 10 min",
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

/// This month, as the report route spells it.
fn this_month() -> String {
    chrono::Utc::now().format("%Y-%m").to_string()
}

#[tokio::test]
async fn report_renders_and_rejects_bad_months() {
    let month = this_month();
    let res = test_app()
        .await
        .oneshot(get(&format!("/report/{month}")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = body_text(res).await;
    let label = chrono::Utc::now().format("%B %Y").to_string();
    assert!(
        body.contains("Service report") && body.contains(&label),
        "{body}"
    );
    // Anonymous: the private monitor stays out of the report.
    assert!(!body.contains("Intra"), "{body}");

    // Malformed, future, or older than the retained history (a full
    // history scan for an always-empty report).
    for bad in [
        "/report/never",
        "/report/2999-01",
        "/report/0001-01",
        "/report/2021-01",
    ] {
        let res = test_app().await.oneshot(get(bad)).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
}

#[tokio::test]
async fn group_report_scopes_and_honours_the_group_token() {
    // The group token reveals the group's private monitor on ITS report.
    let res = test_app()
        .await
        .oneshot(get(&format!(
            "/report/{}?group=App&token=appappappappapp1",
            this_month()
        )))
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
        .oneshot(get(&format!("/report/{}?group=Nope", this_month())))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
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
async fn group_page_negotiates_plain_text() {
    let res = test_app()
        .await
        .oneshot(
            Request::builder()
                .uri("/status/App")
                .header("user-agent", "curl/8.0")
                .extension(fake_peer())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "text/plain; charset=utf-8"
    );
    assert!(body_text(res).await.contains("Web"));

    // Names are padded per group, so the figure columns line up.
    let res = test_app()
        .await
        .oneshot(
            Request::builder()
                .uri("/status/App?token=appappappappapp1")
                .header("user-agent", "curl/8.0")
                .extension(fake_peer())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(res).await;
    let widths: Vec<usize> = body
        .lines()
        .filter(|line| line.contains("Web") || line.contains("Intra"))
        .map(|line| line.chars().count())
        .collect();
    assert_eq!(widths.len(), 2, "{body}");
    assert_eq!(widths[0], widths[1], "{body}");
}

/// Two tenants sharing a node, with a cross-tenant dependency: "Web"
/// (group App, public) depends on "Internal DB" (group Other, private).
const TENANTS: &str = r#"
        [page]
        title = "Hosting"
        [server]
        auth_token = "0123456789abcdef"
        [server.group_tokens]
        App = "appappappappapp1"
        [[monitors]]
        id = "web"
        name = "Web"
        target = "https://example.com"
        interval_secs = 60
        group = "App"
        depends_on = ["db"]
        [[monitors]]
        id = "intra"
        name = "Intra"
        target = "https://intra.example.com"
        interval_secs = 60
        group = "App"
        public = false
        [[monitors]]
        id = "db"
        name = "Internal DB"
        target = "https://db.internal"
        interval_secs = 60
        group = "Other"
        public = false
    "#;

#[tokio::test]
async fn group_token_gets_its_detail_but_no_operator_data() {
    let (app, store) = app_with_pool(TENANTS).await;
    let now = chrono::Utc::now().timestamp();
    // Web and the DB are down (3 failures meet the threshold); Intra has
    // a latency series (so its sparkline can carry markers) and a
    // detailed failure.
    for id in ["web", "db"] {
        for offset in [60, 120, 180] {
            sqlx::query("INSERT INTO checks (time, monitor_id, status, error) VALUES (?, ?, 0, 'HTTP 503: boom')")
                    .bind(now - offset)
                    .bind(id)
                    .execute(store.fixture_pool())
                    .await
                    .unwrap();
        }
    }
    for (offset, status) in [(7200, 1), (3600, 1), (60, 0)] {
        sqlx::query("INSERT INTO checks (time, monitor_id, status, latency_ms, error) VALUES (?, 'intra', ?, 40, 'HTTP 500: tenant secret')")
                .bind(now - offset)
                .bind(status)
                .execute(store.fixture_pool())
                .await
                .unwrap();
    }
    sqlx::query("INSERT INTO events (title, created_at) VALUES ('deploy billing v9', ?)")
        .bind(now - 1800)
        .execute(store.fixture_pool())
        .await
        .unwrap();

    // The operator sees everything: the cross-tenant cause on the status
    // page, the deploy marker on the monitor's response-time chart.
    let page = |uri: &str| {
        let app = app.clone();
        let uri = uri.to_owned();
        async move { body_text(app.oneshot(get(&uri)).await.unwrap()).await }
    };
    let operator = page("/status/App?token=0123456789abcdef").await;
    assert!(operator.contains("Internal DB"), "{operator}");
    let operator = page("/monitor/intra?token=0123456789abcdef").await;
    assert!(operator.contains("deploy billing v9"), "{operator}");

    // The group token: its private monitor with full detail...
    let group = page("/status/App?token=appappappappapp1").await;
    assert!(group.contains("Intra"), "{group}");
    assert!(group.contains("tenant secret"), "{group}");
    // ...but neither another tenant's private monitor nor deploy titles.
    assert!(!group.contains("Internal DB"), "{group}");
    let group_monitor = page("/monitor/intra?token=appappappappapp1").await;
    assert!(group_monitor.contains("tenant secret"), "{group_monitor}");
    assert!(
        !group_monitor.contains("deploy billing v9"),
        "{group_monitor}"
    );

    // Anonymous: public monitors only, sanitized; the private monitor's own
    // page is a 404, like a missing one.
    let public = page("/status/App").await;
    assert!(!public.contains("Intra") && !public.contains("tenant secret"));
    assert!(!public.contains("Internal DB") && !public.contains("deploy billing v9"));
    let res = app.oneshot(get("/monitor/intra")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn anonymous_history_fills_its_page_past_private_incidents() {
    let (app, store) = test_app_with_pool().await;
    // One old public incident, buried under more private ones than the
    // page shows.
    hora_core::db::insert_incident_start(
        &store,
        "web",
        Some("HTTP 503"),
        None,
        None,
        &[],
        None,
        None,
    )
    .await
    .unwrap();
    for _ in 0..120 {
        hora_core::db::insert_incident_start(&store, "intra", None, None, None, &[], None, None)
            .await
            .unwrap();
    }
    let page = body_text(app.oneshot(get("/history")).await.unwrap()).await;
    assert!(
        page.contains("Web"),
        "the public incident must still be listed"
    );
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
async fn pages_share_one_stylesheet_and_no_inline_style() {
    let app = test_app().await;
    let version = crate::handlers::assets::ASSET_VERSION.as_str();
    for uri in ["/", "/status/App", "/history", "/timeline", "/monitor/web"] {
        let res = app.clone().oneshot(get(uri)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        // The CSP drops 'unsafe-inline': nothing may rely on it.
        let csp = res.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            !csp.contains("unsafe-inline") && csp.contains("script-src 'none'"),
            "{csp}"
        );
        let body = body_text(res).await;
        assert!(
            !body.contains("style=") && !body.contains("<style"),
            "{uri}:\n{body}"
        );
        assert!(!body.contains("<script"), "{uri}");
        assert!(
            body.contains(&format!("/assets/hora.css?v={version}")),
            "{uri}"
        );
        // The navigation is on every page.
        assert!(body.contains("href=\"/timeline\""), "{uri}");
    }

    // The stylesheet and fonts are immutable, versioned assets.
    let res = app.clone().oneshot(get("/assets/hora.css")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("immutable")
    );
    let res = app
        .clone()
        .oneshot(get("/assets/fonts/MonaSansMono.woff2"))
        .await
        .unwrap();
    assert_eq!(res.headers()["content-type"], "font/woff2");
    let res = app.oneshot(get("/assets/nope.css")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn theme_and_token_travel_with_the_navigation() {
    let app = test_app().await;
    let body = body_text(
        app.clone()
            .oneshot(get("/?theme=dark&token=0123456789abcdef"))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        body.contains("<html lang=\"en\" data-theme=\"dark\">"),
        "{body}"
    );
    assert!(
        // `&` escaped as `&amp;` in the attribute (the same `&` once parsed).
        body.contains("href=\"/history?token=0123456789abcdef&amp;theme=dark\""),
        "{body}"
    );
    // The operator sees the operator's links; an unknown theme is ignored.
    assert!(body.contains("/watchers?token="), "{body}");
    let body = body_text(app.oneshot(get("/?theme=sepia")).await.unwrap()).await;
    assert!(body.contains("<html lang=\"en\">") && !body.contains("/watchers"));
}

#[tokio::test]
async fn status_page_states_every_state_in_words() {
    let (app, store) = test_app_with_pool().await;
    let now = chrono::Utc::now().timestamp();
    for offset in [30, 60, 90] {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, error) VALUES (?, 'web', 0, 'HTTP 503')",
        )
        .bind(now - offset)
        .execute(store.fixture_pool())
        .await
        .unwrap();
    }
    let body = body_text(app.oneshot(get("/")).await.unwrap()).await;
    // The sentence, the owl's pose, the shape and the word.
    assert!(body.contains("Web is down."), "{body}");
    assert!(body.contains("data-state=\"down\""), "{body}");
    assert!(body.contains("Needs attention"), "{body}");
    assert!(
        body.contains("#g-down") && body.contains(">Down</span>"),
        "{body}"
    );
    // The daily bar is one svg per monitor, never a node per day.
    assert!(!body.contains("class=\"day "), "{body}");
}

#[tokio::test]
async fn watchers_page_is_the_operators_only() {
    let app = test_app().await;
    let res = app.clone().oneshot(get("/watchers")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let body = body_text(
        app.oneshot(get("/watchers?token=0123456789abcdef"))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        body.contains("Peer X") && body.contains("test-node"),
        "{body}"
    );
}

#[tokio::test]
async fn curl_reads_the_same_sentence_with_words() {
    let res = test_app()
        .await
        .oneshot(
            Request::builder()
                .uri("/")
                .header("user-agent", "curl/8.0")
                .extension(fake_peer())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = body_text(res).await;
    // Nothing has data yet: the sentence says so, each row has its word,
    // and a peer's name stays off the public text.
    assert!(body.contains("Waiting for the first checks."), "{body}");
    assert!(body.contains("No data"), "{body}");
    assert!(!body.contains("Peer X"), "{body}");
}

/// The peers' view of the test app's `web` monitor: `status` from Paris.
fn web_seen_from_paris(status: &str) -> hora_core::mesh::vantage::VantageMap {
    let map = hora_core::mesh::vantage::new_map();
    map.store(Arc::new(std::collections::HashMap::from([(
        "http|https://example.com".to_owned(),
        vec![hora_core::mesh::vantage::PeerVantage {
            peer: "Paris".to_owned(),
            status: serde_json::from_value(serde_json::json!(status)).unwrap(),
            p50_ms: Some(80),
        }],
    )])));
    map
}

/// Web is down from this node: three failed checks and the incident open.
async fn web_down_here(store: &hora_core::db::Store) {
    let now = chrono::Utc::now().timestamp();
    for offset in [30, 60, 90] {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, error) VALUES (?, 'web', 0, 'timeout')",
        )
        .bind(now - offset)
        .execute(store.fixture_pool())
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO incidents (monitor_id, started_at, created_at) VALUES ('web', ?, ?)")
        .bind(now - 9 * 60)
        .bind(now - 9 * 60)
        .execute(store.fixture_pool())
        .await
        .unwrap();
}

/// Down on this node only, while the peer watching the same target sees it
/// up: the page says "Up" with a calm "Not an outage", never "Web is down".
#[tokio::test]
async fn a_down_seen_from_here_only_reads_as_up_with_the_notice() {
    let (app, store) = test_app_with_vantage(web_seen_from_paris("up")).await;
    web_down_here(&store).await;

    let body = body_text(app.clone().oneshot(get("/")).await.unwrap()).await;
    assert!(body.contains("Not an outage"), "{body}");
    // The peer's view is what says so: the vantage data reached the page.
    assert!(body.contains("Paris reaches it normally"), "{body}");
    assert!(body.contains("Everything is running."), "{body}");
    assert!(!body.contains("Web is down."), "{body}");
    assert!(
        body.contains("</a><span class=\"state\"><span class=\"word up\">Up</span>"),
        "{body}"
    );
    // One tab stop a row: the name's link carries the bar's words, the bar's
    // own link (the same page) is out of the tab order and the reading order.
    assert!(body.contains(">Web<span class=\"vh\">. "), "{body}");
    assert!(
        body.contains(
            "<a class=\"bar\" href=\"/monitor/web\" tabindex=\"-1\" aria-hidden=\"true\">"
        ),
        "{body}"
    );
    assert!(!body.contains("Needs attention"), "{body}");
    assert!(!body.contains("Down ·"), "{body}");
    // Who saw what is the operator's business.
    assert!(!body.contains("Why this is not an outage"), "{body}");
    let operator = body_text(
        app.clone()
            .oneshot(get("/?token=0123456789abcdef"))
            .await
            .unwrap(),
    )
    .await;
    assert!(operator.contains("Why this is not an outage"), "{operator}");
    // The footnote says what this config does with such a down.
    assert!(
        operator.contains("channels, marked as seen from here only"),
        "{operator}"
    );
    // The monitor's own page agrees.
    let page = body_text(app.oneshot(get("/monitor/web")).await.unwrap()).await;
    assert!(page.contains("Not an outage"), "{page}");
    assert!(page.contains("word up\">Up</span>"), "{page}");
}

/// The control: when the peer sees it down too, it is an outage, with how
/// long it has lasted.
#[tokio::test]
async fn a_down_the_peers_confirm_is_an_outage_with_its_length() {
    let (app, store) = test_app_with_vantage(web_seen_from_paris("down")).await;
    web_down_here(&store).await;

    let body = body_text(app.clone().oneshot(get("/")).await.unwrap()).await;
    assert!(body.contains("Web is down."), "{body}");
    assert!(!body.contains("Not an outage"), "{body}");
    assert!(body.contains("Needs attention"), "{body}");
    assert!(body.contains("9 min"), "{body}");
    assert!(body.contains("Since "), "{body}");
    assert!(!body.contains("No incident in"), "{body}");
    let page = body_text(app.oneshot(get("/monitor/web")).await.unwrap()).await;
    assert!(page.contains("Down · 9 min"), "{page}");
}

/// The status pages are rendered once per audience and shared: a token
/// given in one request is only ever spliced into that request's response,
/// never into the shared render another viewer gets.
#[tokio::test]
async fn a_token_never_reaches_another_viewers_page() {
    let app = test_app().await;
    let fetch = |uri: &str, bearer: Option<&str>| {
        let mut request = Request::builder().uri(uri).extension(fake_peer());
        if let Some(token) = bearer {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let app = app.clone();
        let request = request.body(Body::empty()).unwrap();
        async move { body_text(app.oneshot(request).await.unwrap()).await }
    };

    // The operator by query: its links carry the token...
    let operator = fetch("/?token=0123456789abcdef&theme=dark", None).await;
    assert!(
        operator.contains("/history?token=0123456789abcdef"),
        "{operator}"
    );
    // ...the operator by header, same audience and same cached render, does
    // not get it; neither does anyone else.
    let by_header = fetch("/", Some("0123456789abcdef")).await;
    assert!(by_header.contains("Operator"), "{by_header}");
    assert!(!by_header.contains("0123456789abcdef"), "{by_header}");
    assert!(!by_header.contains("data-theme"), "{by_header}");

    // A wrong token falls back to the public view: echoed in its own links
    // only, never into the next anonymous visitor's page.
    let wrong = fetch("/?token=guess-guess-guess", None).await;
    assert!(wrong.contains("?token=guess-guess-guess"), "{wrong}");
    let public = fetch("/", None).await;
    assert!(!public.contains("guess-guess-guess"), "{public}");
    assert!(!public.contains("0123456789abcdef"), "{public}");

    // A group's token on its page, then the same page anonymously.
    let group = fetch("/status/App?token=appappappappapp1", None).await;
    assert!(
        group.contains("Intra") && group.contains("appappappappapp1"),
        "{group}"
    );
    let anonymous = fetch("/status/App", None).await;
    assert!(!anonymous.contains("appappappappapp1"), "{anonymous}");
    assert!(!anonymous.contains("Intra"), "{anonymous}");

    // The report is cached the same way: a group's token stays in its own
    // response, after the report's own `?group=` in the month links.
    let month = this_month();
    let scoped = fetch(
        &format!("/report/{month}?group=App&token=appappappappapp1"),
        None,
    )
    .await;
    assert!(
        scoped.contains("?group=App&amp;token=appappappappapp1\""),
        "{scoped}"
    );
    let again = fetch(&format!("/report/{month}?group=App"), None).await;
    assert!(!again.contains("appappappappapp1"), "{again}");
    assert!(again.contains("?group=App\""), "{again}");
}

/// The page no longer reloads itself unless asked: `?refresh=` within
/// 10..=3600 seconds, for kiosks; anything else is ignored.
#[tokio::test]
async fn refresh_is_off_by_default_and_bounded_when_asked() {
    let app = test_app().await;
    let page = |uri: &'static str| {
        let app = app.clone();
        async move { body_text(app.oneshot(get(uri)).await.unwrap()).await }
    };
    let plain = page("/").await;
    assert!(!plain.contains("http-equiv=\"refresh\""), "{plain}");
    assert!(!plain.contains("Updates every"), "{plain}");

    let kiosk = page("/?refresh=60").await;
    assert!(
        kiosk.contains("<meta http-equiv=\"refresh\" content=\"60\">"),
        "{kiosk}"
    );
    assert!(kiosk.contains("Updates every 1 min"), "{kiosk}");
    // The kiosk's theme switch keeps it refreshing; links elsewhere do not.
    assert!(
        kiosk.contains("href=\"?refresh=60&amp;theme=light\""),
        "{kiosk}"
    );
    assert!(kiosk.contains("href=\"/history\""), "{kiosk}");

    for ignored in ["/?refresh=5", "/?refresh=86400", "/?refresh=soon"] {
        let body = page(ignored).await;
        assert!(!body.contains("http-equiv=\"refresh\""), "{ignored}");
    }
    // The shared render was not touched by the kiosk's request.
    assert!(!page("/").await.contains("http-equiv=\"refresh\""));
}

/// A phone can put the status page on its home screen: the manifest names
/// the operator's page and points at icons that are served.
#[tokio::test]
async fn the_page_can_go_on_a_home_screen() {
    let app = test_app().await;
    let page = body_text(app.clone().oneshot(get("/")).await.unwrap()).await;
    assert!(
        page.contains("<link rel=\"manifest\" href=\"/manifest.webmanifest\">"),
        "{page}"
    );
    assert!(page.contains("rel=\"apple-touch-icon\""), "{page}");

    let res = app
        .clone()
        .oneshot(get("/manifest.webmanifest"))
        .await
        .unwrap();
    assert_eq!(res.headers()["content-type"], "application/manifest+json");
    let manifest: serde_json::Value = serde_json::from_str(&body_text(res).await).unwrap();
    assert_eq!(manifest["display"], "standalone");
    assert_eq!(manifest["start_url"], "/");
    for icon in manifest["icons"].as_array().unwrap() {
        let src = icon["src"].as_str().unwrap();
        let res = app.clone().oneshot(get(src)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{src}");
        assert_eq!(res.headers()["content-type"], "image/png");
    }
    let res = app.oneshot(get("/apple-touch-icon.png")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

/// `[page] logo` takes the initial tile's place in the header and the
/// report, served at `/logo` under its digest; without one, `/logo` is 404.
/// `logo_dark` adds the dark theme's, at `/logo-dark`.
#[tokio::test]
async fn the_operator_logo_replaces_the_initial() {
    let (app, _store, tx) = app_with_reload(
        r#"
        [page]
        title = "Synthese"
        [server]
        "#,
    )
    .await;
    let page = body_text(app.clone().oneshot(get("/")).await.unwrap()).await;
    assert!(
        page.contains(r#"<span class="op-logo" aria-hidden="true">S</span>"#),
        "{page}"
    );
    let res = app.clone().oneshot(get("/logo")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let mut config = (**tx.borrow()).clone();
    config.page.logo_image = Some(hora_core::config::Logo {
        bytes: b"<svg xmlns='http://www.w3.org/2000/svg'/>"
            .as_slice()
            .into(),
        content_type: "image/svg+xml",
        version: "abc123".to_owned(),
    });
    tx.send_replace(Arc::new(config));
    for path in ["/", "/report/2026-09"] {
        let page = body_text(app.clone().oneshot(get(path)).await.unwrap()).await;
        assert!(
            page.contains(r#"<img class="op-logo" src="/logo?v=abc123" alt="">"#),
            "{path}: {page}"
        );
        assert!(!page.contains(r#"<span class="op-logo""#), "{path}");
    }
    let res = app.clone().oneshot(get("/logo-dark")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    // With a dark logo, both are in the page; the stylesheet shows the one
    // of the theme in force, `?theme=` before the system's.
    let mut config = (**tx.borrow()).clone();
    config.page.logo_dark_image = Some(hora_core::config::Logo {
        bytes: b"<svg/>".as_slice().into(),
        content_type: "image/svg+xml",
        version: "def456".to_owned(),
    });
    tx.send_replace(Arc::new(config));
    let page = body_text(app.clone().oneshot(get("/?theme=dark")).await.unwrap()).await;
    assert!(
        page.contains(concat!(
            r#"<img class="op-logo for-light" src="/logo?v=abc123" alt="">"#,
            r#"<img class="op-logo for-dark" src="/logo-dark?v=def456" alt="">"#
        )),
        "{page}"
    );
    assert!(
        page.contains(r#"<html lang="en" data-theme="dark""#),
        "{page}"
    );
    let res = app
        .clone()
        .oneshot(get("/logo-dark?v=def456"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_text(res).await, "<svg/>");

    let res = app.oneshot(get("/logo?v=abc123")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(res.headers()["content-type"], "image/svg+xml");
    assert!(
        res.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'none'")
    );
    assert!(body_text(res).await.starts_with("<svg"));
}

/// A newer Hora shows on the operator's status page, release notes and all;
/// visitors never see it.
#[tokio::test]
async fn a_new_hora_release_shows_to_the_operator_only() {
    let toml = r#"
        [page]
        [server]
        auth_token = "0123456789abcdef"
        [[monitors]]
        id = "web"
        name = "Web"
        target = "https://example.com"
        interval_secs = 60
    "#;
    let config = Arc::new(hora_core::config::parse(toml).expect("config"));
    let client = hora_core::http::client(None).expect("client");
    let notifier = hora_core::notifications::shared(&config, &client);
    let (_tx, rx) = watch::channel(config);
    let updates = hora_core::updates::new_slot();
    updates.store(Some(Arc::new(hora_core::updates::Update {
        version: "v9.0.0".to_owned(),
        url: "https://github.com/uplg/hora/releases/tag/v9.0.0".to_owned(),
        summary: "Your logo in the header.".to_owned(),
        checked_at: 0,
    })));
    let app = router(
        AppState::new(
            hora_core::db::Store::in_memory().await,
            rx,
            Arc::new(AtomicU64::new(fresh_tick())),
            notifier,
        )
        .with_updates(updates),
    );

    let operator = Request::builder()
        .uri("/")
        .header("authorization", "Bearer 0123456789abcdef")
        .extension(fake_peer())
        .body(Body::empty())
        .unwrap();
    let page = body_text(app.clone().oneshot(operator).await.unwrap()).await;
    assert!(page.contains("Hora v9.0.0 is out"), "{page}");
    assert!(page.contains("<p>Your logo in the header.</p>"), "{page}");
    assert!(
        page.contains(r#"<a href="https://github.com/uplg/hora/releases/tag/v9.0.0" rel="noopener">Release notes</a>"#),
        "{page}"
    );
    assert!(
        page.contains(&format!("You run {}", hora_core::updates::RUNNING)),
        "{page}"
    );

    let page = body_text(app.oneshot(get("/")).await.unwrap()).await;
    assert!(!page.contains("v9.0.0"), "{page}");
}

#[tokio::test]
async fn monitor_page_never_shows_target_credentials() {
    let (app, _store) = app_with_pool(
        r#"
        [page]
        [server]
        auth_token = "0123456789abcdef"
        [server.group_tokens]
        App = "appappappappapp1"
        [[monitors]]
        id = "api"
        name = "API"
        target = "https://ops:hunter2@api.example.com/health?api_key=k3y"
        interval_secs = 60
        group = "App"
    "#,
    )
    .await;
    for token in ["appappappappapp1", "0123456789abcdef"] {
        let uri = format!("/monitor/api?token={token}");
        let page = body_text(app.clone().oneshot(get(&uri)).await.unwrap()).await;
        assert!(page.contains("api.example.com/health"), "{page}");
        assert!(!page.contains("hunter2") && !page.contains("k3y"), "{page}");
    }
}

/// A group token opens what its group's pages link to (the incident, the
/// latency series, the heatmap of a private monitor of the group), and the
/// group page lists the group's own recent incidents even when the newest
/// ones overall all belong to someone else.
#[tokio::test]
async fn group_token_follows_its_pages_links_and_sees_its_own_incidents() {
    let (app, store) = app_with_pool(TENANTS).await;
    let start = |monitor: &'static str| {
        let store = store.clone();
        async move {
            hora_core::db::insert_incident_start(&store, monitor, None, None, None, &[], None, None)
                .await
                .unwrap()
        }
    };
    let ours = start("intra").await;
    let mut theirs = 0;
    for _ in 0..3 {
        theirs = start("db").await;
    }
    let token = "token=appappappappapp1";

    let page = body_text(
        app.clone()
            .oneshot(get(&format!("/status/App?{token}")))
            .await
            .unwrap(),
    )
    .await;
    assert!(page.contains(&format!("/incident/{ours}")), "{page}");
    assert!(!page.contains(&format!("/incident/{theirs}")), "{page}");

    for uri in [
        format!("/incident/{ours}?{token}"),
        format!("/api/monitors/intra/latency?{token}"),
        format!("/api/monitors/intra/heatmap.svg?{token}"),
    ] {
        let res = app.clone().oneshot(get(&uri)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
    }
    // Another group's incident and monitor stay unknown to it, and the
    // group's private monitor stays unknown without the token.
    for uri in [
        format!("/incident/{theirs}?{token}"),
        format!("/api/monitors/db/latency?{token}"),
        format!("/incident/{ours}"),
        "/api/monitors/intra/heatmap.svg".to_owned(),
    ] {
        let res = app.clone().oneshot(get(&uri)).await.unwrap();
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

/// The page and the badge go by the threshold the alert goes by: a monitor
/// with its own is down, or not yet, when its alert says so - not when the
/// global threshold would.
#[tokio::test]
async fn a_monitor_is_down_on_the_page_by_its_own_fail_threshold() {
    let monitor = |id: &str, more: &str| {
        format!(
            "[[monitors]]\nid = \"{id}\"\nname = \"{id}\"\n\
             target = \"https://{id}.example.com\"\ninterval_secs = 60\n{more}\n"
        )
    };
    let (app, store) = app_with_pool(&format!(
        "[page]\n[server]\n{}{}{}",
        monitor("strict", "fail_threshold = 1"),
        monitor("lenient", ""),
        monitor("patient", "fail_threshold = 5"),
    ))
    .await;
    let now = chrono::Utc::now().timestamp();
    // One failed check for the first two, three in a row for the third:
    // what the global threshold (3) calls degraded, degraded and down.
    for (id, failures) in [("strict", 1), ("lenient", 1), ("patient", 3)] {
        for ago in 0..failures {
            sqlx::query(
                "INSERT INTO checks (time, monitor_id, status, error) VALUES (?, ?, 0, 'HTTP 503')",
            )
            .bind(now - 60 * ago)
            .bind(id)
            .execute(store.fixture_pool())
            .await
            .unwrap();
        }
    }

    let summary: serde_json::Value = serde_json::from_str(
        &body_text(app.clone().oneshot(get("/api/summary")).await.unwrap()).await,
    )
    .unwrap();
    let status = |id: &str| {
        summary["monitors"]
            .as_array()
            .unwrap()
            .iter()
            .find(|view| view["id"] == id)
            .unwrap_or_else(|| panic!("{id} is on the page"))["status"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(status("strict"), "down", "confirmed on its first failure");
    assert_eq!(status("lenient"), "degraded", "the global three");
    assert_eq!(status("patient"), "degraded", "three of its five");
    assert_eq!(summary["overall"], "down");

    for (id, word) in [
        ("strict", "down"),
        ("lenient", "degraded"),
        ("patient", "degraded"),
    ] {
        let badge = body_text(
            app.clone()
                .oneshot(get(&format!("/api/badge/{id}/status")))
                .await
                .unwrap(),
        )
        .await;
        assert!(badge.contains(&format!(">{word}<")), "{id}: {badge}");
    }
}
