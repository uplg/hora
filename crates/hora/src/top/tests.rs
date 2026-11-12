use crossterm::event::KeyCode;
use ratatui::style::Color;

use super::app::{Input, announce_action, follow_up, input_key, silence_action};
use super::fetch::{FetchError, Fetched, Summary};
use super::ui::overall_style;
use super::*;

#[test]
fn args_parse_flags_and_fall_back() {
    let (url, token, interval) = parse_args(&[
        "--url".to_owned(),
        "https://s.example".to_owned(),
        "--token".to_owned(),
        "tok".to_owned(),
        "--interval".to_owned(),
        "2".to_owned(),
    ])
    .expect("parse");
    assert_eq!(url, "https://s.example");
    assert_eq!(token.as_deref(), Some("tok"));
    assert_eq!(interval, Duration::from_secs(2));

    // Unknown flags fail loudly; a zero interval is clamped.
    assert!(parse_args(&["--nope".to_owned()]).is_err());
    let (_, _, interval) = parse_args(&[
        "--url".to_owned(),
        "https://s".to_owned(),
        "--interval".to_owned(),
        "0".to_owned(),
    ])
    .expect("parse");
    assert_eq!(interval, Duration::from_secs(1));
}

#[test]
fn summary_json_from_the_real_api_shape_deserializes() {
    let summary: Summary = serde_json::from_str(
        r#"{
                "title": "uplg.status",
                "overall": "degraded",
                "overall_label": "Degraded performance",
                "incidents": [],
                "maintenances": [],
                "groups": [{"name": "Frank", "ids": ["web"]}],
                "peers": [],
                "monitors": [{
                    "id": "web",
                    "name": "Web",
                    "status": "degraded",
                    "last_latency_ms": 812,
                    "last_error": "slow",
                    "last_checked": "12s ago",
                    "uptime_24h_permille": 998,
                    "latency_p50_ms": 120,
                    "latency_p95_ms": 800,
                    "latency_p99_ms": 950,
                    "history": [],
                    "group": "Frank",
                    "maintenance": null
                }]
            }"#,
    )
    .expect("summary shape");
    assert_eq!(summary.monitors[0].uptime_permille, Some(998));
    assert_eq!(summary.monitors[0].p95, Some(800));
    assert_eq!(summary.overall_label, "Degraded performance");
    // The header colour comes from the status, not from the wording.
    assert_eq!(summary.overall, "degraded");
    assert_eq!(overall_style(&summary.overall).bg, Some(Color::Yellow));
    assert_eq!(overall_style("down").bg, Some(Color::Red));
    assert_eq!(overall_style("").bg, Some(Color::DarkGray));
}

fn summary_of(ids: &[&str]) -> Summary {
    let monitors: Vec<String> = ids
        .iter()
        .map(|id| format!(r#"{{"id": "{id}", "name": "{id}", "status": "up"}}"#))
        .collect();
    serde_json::from_str(&format!(
        r#"{{"title": "t", "overall": "up", "overall_label": "All systems operational",
                "monitors": [{}]}}"#,
        monitors.join(",")
    ))
    .expect("summary")
}

fn api_at(url: String) -> Api {
    Api {
        client: hora_core::http::client(None).expect("client"),
        url,
        token: Some("tok".to_owned()),
    }
}

#[tokio::test]
async fn summary_keeps_the_selection_and_asks_for_its_sparkline() {
    let mut app = App::new("http://x".to_owned());
    assert_eq!(
        apply(&mut app, Fetched::Summary(Ok(summary_of(&["a", "b"])))),
        FollowUp::Spark("a".to_owned())
    );
    app.select_delta(1);
    // "b" moved to the front: the selection follows the monitor.
    assert_eq!(
        apply(&mut app, Fetched::Summary(Ok(summary_of(&["b", "a"])))),
        FollowUp::Spark("b".to_owned())
    );
    assert_eq!(app.table.selected(), Some(0));
}

#[tokio::test]
async fn a_late_sparkline_for_another_monitor_is_dropped() {
    let mut app = App::new("http://x".to_owned());
    apply(&mut app, Fetched::Summary(Ok(summary_of(&["a", "b"]))));
    apply(
        &mut app,
        Fetched::Spark {
            monitor_id: "b".to_owned(),
            result: Ok(vec![1, 2, 3]),
        },
    );
    assert_eq!(app.spark, Vec::<u64>::new());
    apply(
        &mut app,
        Fetched::Spark {
            monitor_id: "a".to_owned(),
            result: Ok(vec![4, 5]),
        },
    );
    assert_eq!(app.spark, vec![4, 5]);
}

/// A sparkline whose fetch failed used to be an empty chart and nothing
/// else; the header now says why, until the selection moves or it loads.
#[tokio::test]
async fn a_failed_sparkline_says_so() {
    let mut app = App::new("http://x".to_owned());
    apply(&mut app, Fetched::Summary(Ok(summary_of(&["a", "b"]))));
    assert_eq!(app.status_error(), None);
    apply(
        &mut app,
        Fetched::Spark {
            monitor_id: "a".to_owned(),
            result: Err(FetchError::Status {
                status: reqwest::StatusCode::BAD_GATEWAY,
                detail: String::new(),
            }),
        },
    );
    assert_eq!(
        app.status_error(),
        Some("latency history: server answered HTTP 502 Bad Gateway")
    );
    assert!(!app.cooling_down());
    // The summary's own error comes first; a fresh summary does not hide the
    // sparkline's, a loaded sparkline clears it.
    apply(&mut app, Fetched::Summary(Ok(summary_of(&["a", "b"]))));
    assert!(app.status_error().is_some());
    apply(
        &mut app,
        Fetched::Spark {
            monitor_id: "a".to_owned(),
            result: Ok(vec![1]),
        },
    );
    assert_eq!(app.status_error(), None);

    // A rate-limited sparkline backs off like a rate-limited summary.
    apply(
        &mut app,
        Fetched::Spark {
            monitor_id: "a".to_owned(),
            result: Err(FetchError::Status {
                status: reqwest::StatusCode::TOO_MANY_REQUESTS,
                detail: String::new(),
            }),
        },
    );
    assert!(app.cooling_down());
}

/// While the server's 429 back-off runs, nothing goes out: not the refresh
/// an action asks for, not the sparkline a summary asks for.
#[tokio::test]
async fn follow_ups_wait_out_the_rate_limit() {
    // A server that accepts and never answers: a started fetch stays in flight.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let (tx, _results) = mpsc::channel(8);
    let mut fetcher = Fetcher::new(api_at(url), tx);

    let mut app = App::new("http://x".to_owned());
    let refused = FetchError::Status {
        status: reqwest::StatusCode::TOO_MANY_REQUESTS,
        detail: String::new(),
    };
    apply(&mut app, Fetched::Summary(Err(refused)));
    assert!(app.cooling_down());

    let next = apply(&mut app, Fetched::Action(Ok("silenced web".to_owned())));
    assert_eq!(next, FollowUp::Refresh);
    follow_up(&mut app, &mut fetcher, next);
    assert!(
        fetcher.summary(),
        "no refresh went out during the back-off (none was in flight)"
    );

    // Without a back-off the same follow-up starts the refresh.
    let (tx, _results) = mpsc::channel(8);
    let mut fetcher = Fetcher::new(
        api_at(format!("http://{}", listener.local_addr().expect("addr"))),
        tx,
    );
    let mut app = App::new("http://x".to_owned());
    follow_up(&mut app, &mut fetcher, FollowUp::Refresh);
    assert!(!fetcher.summary(), "the refresh is in flight");
    drop(listener);
}

#[tokio::test]
async fn rate_limiting_is_detected_from_the_status_not_the_text() {
    let mut app = App::new("http://x".to_owned());
    let refused = FetchError::Status {
        status: reqwest::StatusCode::TOO_MANY_REQUESTS,
        detail: String::new(),
    };
    assert_eq!(
        apply(&mut app, Fetched::Summary(Err(refused))),
        FollowUp::Nothing
    );
    assert!(app.cooling_down());
    assert!(app.error.as_deref().is_some_and(|e| e.contains("429")));

    // A body that merely mentions 429 is an ordinary error.
    let mut app = App::new("http://x".to_owned());
    let other = FetchError::Status {
        status: reqwest::StatusCode::BAD_GATEWAY,
        detail: "upstream 429".to_owned(),
    };
    apply(&mut app, Fetched::Summary(Err(other)));
    assert!(!app.cooling_down());
    assert_eq!(
        app.error.as_deref(),
        Some("server answered HTTP 502 Bad Gateway")
    );
}

#[tokio::test]
async fn action_outcomes_become_typed_notices_and_refresh() {
    let mut app = App::new("http://x".to_owned());
    assert_eq!(
        apply(&mut app, Fetched::Action(Err("failed somehow".to_owned()))),
        FollowUp::Refresh
    );
    assert_eq!(app.notice, Some(Err("failed somehow".to_owned())));
    // A success whose wording contains "failed" still reads as success.
    apply(
        &mut app,
        Fetched::Action(Ok("announced [info] \"failed disk swapped\"".to_owned())),
    );
    assert!(matches!(app.notice, Some(Ok(_))));
}

#[test]
fn input_bar_builds_actions_without_touching_the_network() {
    let mut app = App::new("http://x".to_owned());
    app.input = Some(Input::Silence {
        monitor_id: "web".to_owned(),
        buffer: "10m".to_owned(),
    });
    assert!(input_key(&mut app, KeyCode::Backspace).is_none());
    assert!(input_key(&mut app, KeyCode::Char('h')).is_none());
    let action = input_key(&mut app, KeyCode::Enter)
        .expect("submitted")
        .expect("valid");
    assert!(app.input.is_none(), "Enter closes the bar");
    assert_eq!(action.path, "/api/silence");
    assert!(action.params.contains(&("duration", "10h".to_owned())));
    // Enter with the bar already closed is a no-op.
    assert!(input_key(&mut app, KeyCode::Enter).is_none());

    let action =
        announce_action("Fiber cut :: ETA 6pm --severity critical --until 4h").expect("announce");
    assert_eq!(action.done, "announced [critical] \"Fiber cut\"");
    assert!(action.params.contains(&("body", "ETA 6pm".to_owned())));
    assert!(action.params.contains(&("until", "4h".to_owned())));
    assert!(announce_action(":: body only").is_err());
    assert!(announce_action("t --severity panic").is_err());
    // The clock-time form the hint advertises is accepted (the API takes
    // it too); garbage is refused before it reaches the server.
    let action = announce_action("Fiber cut --until 18:00").expect("announce");
    assert!(action.params.contains(&("until", "18:00".to_owned())));
    assert!(announce_action("t --until soon").is_err());
    assert!(silence_action("web", "  ").is_err());
}

/// The UI must never wait on the network: against a server that accepts
/// connections but never answers, starting a fetch returns at once, and a
/// second summary fetch is skipped while the first is in flight.
#[tokio::test]
async fn fetches_run_in_the_background_one_per_kind() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let (tx, mut results) = mpsc::channel(8);
    let mut fetcher = Fetcher::new(api_at(url), tx);

    assert!(fetcher.summary(), "first fetch starts");
    assert!(!fetcher.summary(), "second is skipped while in flight");
    fetcher
        .action(silence_action("web", "10m").expect("action"))
        .expect("action starts");
    assert!(
        fetcher
            .action(silence_action("web", "10m").expect("action"))
            .is_err(),
        "one action at a time"
    );
    // Nothing has answered, and nothing blocked getting here.
    assert!(
        tokio::time::timeout(Duration::from_millis(100), results.recv())
            .await
            .is_err()
    );
    drop(listener);
}

#[tokio::test]
async fn fetch_errors_carry_the_http_status() {
    let app = axum::Router::new()
        .route(
            "/api/summary",
            axum::routing::get(|| async {
                (axum::http::StatusCode::TOO_MANY_REQUESTS, "slow down")
            }),
        )
        .route(
            "/api/silence",
            axum::routing::post(|| async {
                (axum::http::StatusCode::BAD_REQUEST, "invalid duration")
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });

    let (tx, mut results) = mpsc::channel(8);
    let mut fetcher = Fetcher::new(api_at(url.clone()), tx);
    fetcher.summary();
    match results.recv().await {
        Some(Fetched::Summary(Err(err))) => assert!(err.is_rate_limited(), "{err}"),
        _ => panic!("expected a failed summary"),
    }
    fetcher
        .action(silence_action("web", "nope").expect("action"))
        .expect("starts");
    match results.recv().await {
        Some(Fetched::Action(Err(reason))) => {
            assert_eq!(reason, "server refused (400 Bad Request): invalid duration");
        }
        _ => panic!("expected a refused action"),
    }

    // Without a token, an action is refused before any request.
    let (tx, _results) = mpsc::channel(8);
    let mut anonymous = Fetcher::new(
        Api {
            token: None,
            ..api_at(url)
        },
        tx,
    );
    let refused = anonymous
        .action(silence_action("web", "10m").expect("action"))
        .expect_err("needs a token");
    assert!(refused.contains("needs a token"));
}

#[test]
fn the_compact_hint_shows_past_a_fifth_of_free_pages() {
    use super::fetch::Storage;
    let storage = |db_bytes, reclaimable_bytes| Storage {
        db_bytes,
        reclaimable_bytes,
    };
    assert_eq!(super::ui::compact_hint(&storage(100 << 20, 20 << 20)), None);
    let hint = super::ui::compact_hint(&storage(100 << 20, 21 << 20)).expect("a hint");
    assert!(hint.contains("21.0 MiB of 100.0 MiB"), "{hint}");
    // The summary of a server without the field still reads.
    let summary: Summary =
        serde_json::from_str(r#"{"title":"t","overall_label":"ok","monitors":[]}"#).unwrap();
    assert!(summary.storage.is_none());
}

/// `hora top` declares the shapes it reads from the API by hand, every
/// optional field defaulted: a field renamed on the server would not fail
/// anything, it would show "-" for ever. So read a real `/api/summary` (and
/// a real latency series) from the real router, through top's own client,
/// and check that everything top displays arrived.
#[tokio::test]
async fn top_reads_what_the_real_api_serves() {
    let url = real_api().await;
    let (tx, mut results) = mpsc::channel(8);
    let mut fetcher = Fetcher::new(
        Api {
            token: Some(REAL_API_TOKEN.to_owned()),
            ..api_at(url)
        },
        tx,
    );
    fetcher.summary();
    let summary = match results.recv().await {
        Some(Fetched::Summary(Ok(summary))) => summary,
        Some(Fetched::Summary(Err(err))) => panic!("summary: {err}"),
        _ => panic!("expected a summary"),
    };

    // The header.
    assert_eq!(summary.title, "Acme status");
    assert_eq!(summary.overall, "degraded");
    assert_eq!(summary.overall_label, "Degraded performance");
    let storage = summary.storage.as_ref().expect("the operator's storage");
    assert!(storage.db_bytes > 0);
    assert!(storage.reclaimable_bytes <= storage.db_bytes);

    // The table: every column of a monitor that has data.
    let web = summary
        .monitors
        .iter()
        .find(|monitor| monitor.id == "web")
        .expect("web");
    assert_eq!(web.name, "Web");
    assert_eq!(web.status, "up");
    assert_eq!(web.group.as_deref(), Some("Front"));
    assert_eq!(web.uptime_permille, Some(1000));
    let (p50, p95, p99) = (
        web.p50.expect("p50"),
        web.p95.expect("p95"),
        web.p99.expect("p99"),
    );
    assert!((40..=80).contains(&p50), "{p50}");
    assert!(p50 <= p95 && p95 <= p99 && p99 <= 80, "{p50} {p95} {p99}");
    // The trouble panel: why a monitor is not up.
    let api = summary
        .monitors
        .iter()
        .find(|monitor| monitor.id == "api")
        .expect("api");
    assert_eq!(api.status, "degraded");
    assert_eq!(api.last_error.as_deref(), Some("timed out after 10s"));
    assert_eq!(api.uptime_permille, Some(0));

    // The banners and the channels' health.
    assert_eq!(summary.incidents.len(), 1);
    assert_eq!(summary.incidents[0].title, "Fiber cut");
    assert_eq!(summary.incidents[0].severity, "critical");
    assert_eq!(summary.channels.len(), 1);
    assert_eq!(summary.channels[0].name, "pager");
    assert!(!summary.channels[0].failing);
    assert_eq!(summary.channels[0].consecutive_failures, 0);
    assert_eq!(summary.channels[0].failing_for_secs, None);

    // The sparkline: the selected monitor's latency series.
    fetcher.spark("web".to_owned());
    match results.recv().await {
        Some(Fetched::Spark { monitor_id, result }) => {
            assert_eq!(monitor_id, "web");
            let points = result.expect("latency series");
            assert_ne!(points, Vec::<u64>::new());
            assert!(points.iter().all(|&ms| (40..=80).contains(&ms)));
        }
        _ => panic!("expected a sparkline"),
    }
}

/// The operator token of [`real_api`].
const REAL_API_TOKEN: &str = "0123456789abcdef";

/// The real router on a local port, over a database with something of
/// everything `hora top` shows: two monitors with checks (one failing, with
/// its reason), a group, a pinned announcement, a channel. Returns its URL.
async fn real_api() -> String {
    use hora_core::probe::{FailureKind, Outcome};
    use hora_core::status::CheckStatus;

    const TOKEN: &str = REAL_API_TOKEN;
    let config = hora_core::config::parse(&format!(
        r#"
        [page]
        title = "Acme status"
        [server]
        auth_token = "{TOKEN}"
        [[channels]]
        name = "pager"
        type = "webhook"
        url = "https://example.com/hook"
        [[monitors]]
        id = "web"
        name = "Web"
        group = "Front"
        target = "https://example.com"
        interval_secs = 60
        [[monitors]]
        id = "api"
        name = "Api"
        target = "https://api.example.com"
        interval_secs = 60
        "#
    ))
    .expect("config");
    let store = hora_core::db::Store::in_memory().await;
    let check = |status, latency_ms, error: Option<&str>| Outcome {
        status,
        latency_ms: Some(latency_ms),
        status_code: None,
        error: error.map(str::to_owned),
        reason: error.map(|_| FailureKind::Timeout),
        snapshot: None,
    };
    for latency in [40, 60, 80] {
        hora_core::db::insert_check(&store, "web", &check(CheckStatus::Up, latency, None))
            .await
            .expect("check");
    }
    hora_core::db::insert_check(
        &store,
        "api",
        &check(CheckStatus::Down, 900, Some("timed out after 10s")),
    )
    .await
    .expect("check");
    hora_core::db::insert_announcement(
        &store,
        "Fiber cut",
        "",
        hora_core::announce::Severity::Critical,
        None,
    )
    .await
    .expect("announcement");

    let config = std::sync::Arc::new(config);
    let client = hora_core::http::client(None).expect("client");
    let notifier = hora_core::notifications::shared(&config, &client);
    let (_reload, config_rx) = tokio::sync::watch::channel(config);
    let app = hora_web::router(hora_web::AppState::new(
        store,
        config_rx,
        std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        notifier,
    ))
    .into_make_service_with_connect_info::<std::net::SocketAddr>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    tokio::spawn(async move { axum::serve(listener, app).await });
    url
}
