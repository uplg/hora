//! Operator writes: events, announcements and silences.

use super::*;

#[tokio::test]
async fn event_requires_token_records_and_validates() {
    let (app, store) = test_app_with_pool().await;
    // Recording a change marker is an operator action: closed anonymously.
    let res = app
        .clone()
        .oneshot(push("/api/event?title=deploy+api+v2.3"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let res = app
        .clone()
        .oneshot(write("/api/event?title=deploy+api+v2.3"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let events = hora_core::db::recent_events(&store, 10).await.unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].title, "deploy api v2.3");

    let res = app.oneshot(write("/api/event?title=++")).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn events_reach_authenticated_viewers_only() {
    let (app, store) = test_app_with_pool().await;
    hora_core::db::insert_event(&store, "deploy api v2.3")
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
        .oneshot(write(
            "/api/announce?title=Fiber+cut&body=ETA+6pm&severity=warning&until=4h",
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
        .uri("/api/announce")
        .header("authorization", "Bearer 0123456789abcdef")
        .extension(fake_peer())
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app.oneshot(get("/api/summary")).await.unwrap();
    assert!(!body_text(res).await.contains("Fiber cut"));
}

#[tokio::test]
async fn announce_until_takes_a_utc_clock_time() {
    let res = test_app()
        .await
        .oneshot(write("/api/announce?title=Fiber+cut&until=18:00"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body: serde_json::Value = serde_json::from_str(&body_text(res).await).unwrap();
    let until = body["until"].as_i64().expect("until");
    let now = chrono::Utc::now().timestamp();
    assert!(until > now && until <= now + 86_400, "{until}");
    assert_eq!(until % 86_400, 18 * 3600, "18:00 UTC");
}

#[tokio::test]
async fn announce_rejects_bad_severity_and_empty_title() {
    for bad in [
        "/api/announce?title=x&severity=panic",
        "/api/announce?title=+",
        "/api/announce?title=x&until=nope",
    ] {
        let res = test_app().await.oneshot(write(bad)).await.unwrap();
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
    let (app, store) = test_app_with_pool().await;
    let res = app
        .oneshot(write(
            "/api/silence?monitors=web&duration=10m&reason=deploy",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    let now = chrono::Utc::now().timestamp();
    assert!(
        hora_core::db::is_silenced(&store, "web", now)
            .await
            .unwrap()
    );
    assert!(
        !hora_core::db::is_silenced(&store, "beat", now)
            .await
            .unwrap()
    );
    // Within the requested window, never past it.
    assert!(
        !hora_core::db::is_silenced(&store, "web", now + 601)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn silence_accepts_a_watched_peer() {
    let (app, store) = test_app_with_pool().await;
    let res = app
        .oneshot(write("/api/silence?monitors=web,peer-x&duration=5m"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let now = chrono::Utc::now().timestamp();
    assert!(
        hora_core::db::is_silenced(&store, "peer-x", now)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn silence_all_uses_the_wildcard() {
    let (app, store) = test_app_with_pool().await;
    let res = app
        .oneshot(write("/api/silence?monitors=all&duration=5m"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let now = chrono::Utc::now().timestamp();
    assert!(
        hora_core::db::is_silenced(&store, "beat", now)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn silence_rejects_unknown_monitor_and_bad_duration() {
    let res = test_app()
        .await
        .oneshot(write("/api/silence?monitors=nope&duration=10m"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let res = test_app()
        .await
        .oneshot(write("/api/silence?monitors=web&duration=tomorrow"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
}
