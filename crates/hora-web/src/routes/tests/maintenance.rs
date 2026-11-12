//! Planned maintenance: the operator's API, and what a peer's heartbeat may
//! announce.

use super::*;

/// Node `yuki`: it watches its peer on `frank` (trusted to announce up to
/// 15 minutes of maintenance) and three services - two on frank, one of them
/// private, and one with no host label.
const MESH: &str = r#"
    [page]
    [server]
    auth_token = "0123456789abcdef"
    view_token = "viewviewviewview"
    [health]
    id = "hora-yuki"
    host = "yuki"
    [[peers]]
    id = "hora-frank"
    name = "Frank node"
    listen_id = "frank-in"
    expect_every_secs = 60
    listen_token = "peertok-peertok-1"
    host = "frank"
    accept_maintenance_secs = 900
    [[monitors]]
    id = "web"
    name = "Web"
    target = "https://example.com"
    interval_secs = 60
    host = "frank"
    [[monitors]]
    id = "vault"
    name = "Vault"
    target = "https://vault.example.com"
    interval_secs = 60
    host = "frank"
    public = false
    [[monitors]]
    id = "blog"
    name = "Blog"
    target = "https://blog.example.com"
    interval_secs = 60
    [[monitors]]
    id = "beat"
    name = "Beat"
    kind = "push"
    interval_secs = 60
    push_token = "s3cret-s3cret-16"
"#;

fn mesh_config() -> hora_core::config::Config {
    hora_core::config::parse(MESH).expect("config")
}

/// An operator request with another method than POST.
fn operator(method: &str, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", "Bearer 0123456789abcdef")
        .extension(fake_peer())
        .body(Body::empty())
        .expect("request")
}

async fn json(res: axum::response::Response) -> serde_json::Value {
    serde_json::from_str(&body_text(res).await).expect("json")
}

/// The peer's heartbeat, with its token and this JSON body (none if empty).
fn heartbeat(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/push/frank-in?status=up&msg=ok+mon%3D3+tick%3D2s")
        .header("x-push-token", "peertok-peertok-1")
        .header("content-type", "application/json")
        .extension(fake_peer())
        .body(Body::from(body.to_owned()))
        .expect("request")
}

async fn muted(store: &hora_core::db::Store, id: &str, at: i64) -> bool {
    let at = chrono::DateTime::from_timestamp(at, 0).expect("time");
    hora_core::planned::in_maintenance(store, &mesh_config(), id, at).await
}

#[tokio::test]
async fn maintenance_takes_the_operator_token_in_a_header() {
    let (app, store) = app_with_pool(MESH).await;
    let plan = "/api/maintenance?host=frank&duration=10m&reason=kernel+reboot";
    let refused = [
        // Anonymous.
        push(plan),
        // The right token, in the URL: refused on a write.
        push(&format!("{plan}&token=0123456789abcdef")),
        // The read-only token reads the page; it plans nothing.
        push_with(plan, "authorization", "Bearer viewviewviewview"),
        push_with(plan, "authorization", "Bearer wrong-wrong-wrong"),
        // A peer's token is not the operator's.
        push_with(plan, "x-push-token", "peertok-peertok-1"),
        // Listing and cancelling are the operator's too.
        get("/api/maintenance"),
        get("/api/maintenance?token=viewviewviewview"),
        Request::builder()
            .method("DELETE")
            .uri("/api/maintenance/1")
            .extension(fake_peer())
            .body(Body::empty())
            .expect("request"),
        operator("DELETE", "/api/maintenance/1?token=0123456789abcdef"),
    ];
    for request in refused {
        let what = format!("{} {}", request.method(), request.uri());
        let res = app.clone().oneshot(request).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{what}");
    }
    let stored = hora_core::db::pending_maintenance_windows(&store, 0)
        .await
        .unwrap();
    assert!(stored.is_empty(), "{stored:?}");
}

#[tokio::test]
async fn a_window_is_planned_once_listed_and_cancelled() {
    let (app, store) = app_with_pool(MESH).await;
    let now = chrono::Utc::now().timestamp();
    let start = now + 3600;
    let rfc = chrono::DateTime::from_timestamp(start, 0)
        .expect("time")
        .format("%Y-%m-%dT%H:%M:%SZ");
    let plan =
        format!("/api/maintenance?host=frank&starts_at={rfc}&duration=10m&reason=kernel%20reboot");

    let res = app.clone().oneshot(write(&plan)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let planned = json(res).await;
    assert_eq!(planned["starts_at"], start);
    assert_eq!(planned["ends_at"], start + 600);
    assert_eq!(planned["reason"], "kernel reboot");
    assert_eq!(planned["state"], "planned");
    assert_eq!(planned["origin"], "api");
    assert_eq!(planned["host"], "frank");
    assert_eq!(planned["created"], true);
    // What it covers here: the monitors on that host, the private one
    // included, and the watch of the peer that runs there.
    assert_eq!(
        planned["monitors"],
        serde_json::json!(["web", "vault", "frank-in"])
    );
    // Frank is not this node's own host: nothing to tell the peers.
    assert_eq!(planned["announced"], false);
    let id = planned["id"].as_i64().expect("id");

    // The host script runs every few minutes: the same call, in either
    // spelling of the start, is the same window.
    for again in [
        plan.clone(),
        format!("/api/maintenance?host=frank&starts_at={start}&duration=600s&reason=again"),
    ] {
        let repeat = json(app.clone().oneshot(write(&again)).await.unwrap()).await;
        assert_eq!(repeat["id"], id, "{again}");
        assert_eq!(repeat["created"], false);
        assert_eq!(repeat["reason"], "kernel reboot");
    }

    // Planned is not active: nothing is muted before it starts.
    assert!(!muted(&store, "web", now).await);
    assert!(!muted(&store, "web", start - 1).await);
    assert!(muted(&store, "web", start).await);
    assert!(muted(&store, "vault", start + 600).await);
    assert!(muted(&store, "frank-in", start + 1).await);
    assert!(!muted(&store, "blog", start + 1).await);
    assert!(!muted(&store, "web", start + 601).await);

    let listed = json(
        app.clone()
            .oneshot(operator("GET", "/api/maintenance"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(listed["windows"].as_array().map(Vec::len), Some(1));
    assert_eq!(listed["windows"][0]["id"], id);
    assert_eq!(listed["windows"][0]["state"], "planned");

    // Cancelled: it mutes nothing any more, and is gone from the list.
    let uri = format!("/api/maintenance/{id}");
    let res = app.clone().oneshot(operator("DELETE", &uri)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(json(res).await["id"], id);
    assert!(!muted(&store, "web", start + 1).await);
    let listed = json(
        app.clone()
            .oneshot(operator("GET", "/api/maintenance"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(listed["windows"], serde_json::json!([]));
    let res = app.oneshot(operator("DELETE", &uri)).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_malformed_plan_is_refused_and_plans_nothing() {
    let (app, store) = app_with_pool(MESH).await;
    for (bad, status) in [
        // Exactly one selector.
        (
            "/api/maintenance?duration=10m&reason=x",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/maintenance?host=frank&monitors=web&duration=10m&reason=x",
            StatusCode::BAD_REQUEST,
        ),
        // A typo'd host or monitor fails loudly.
        (
            "/api/maintenance?host=franck&duration=10m&reason=x",
            StatusCode::NOT_FOUND,
        ),
        (
            "/api/maintenance?monitors=web,nope&duration=10m&reason=x",
            StatusCode::NOT_FOUND,
        ),
        // The silence cap: 7 days.
        (
            "/api/maintenance?host=frank&duration=8d&reason=x",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/maintenance?host=frank&duration=soon&reason=x",
            StatusCode::BAD_REQUEST,
        ),
        // A duration and a reason are required.
        (
            "/api/maintenance?host=frank&reason=x",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/maintenance?host=frank&duration=10m",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/maintenance?host=frank&duration=10m&reason=++",
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/maintenance?host=frank&duration=10m&reason=x&starts_at=05:00",
            StatusCode::BAD_REQUEST,
        ),
        // Over before it was planned.
        (
            "/api/maintenance?host=frank&duration=10m&reason=x&starts_at=1700000000",
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let res = app.clone().oneshot(write(bad)).await.unwrap();
        assert_eq!(res.status(), status, "{bad}");
    }
    let stored = hora_core::db::pending_maintenance_windows(&store, 0)
        .await
        .unwrap();
    assert!(stored.is_empty(), "{stored:?}");

    // The longest accepted, and `all`.
    let res = app
        .oneshot(write(
            "/api/maintenance?monitors=all&duration=7d&reason=moving+racks",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let planned = json(res).await;
    assert_eq!(planned["all"], true);
    assert_eq!(planned["state"], "active");
    assert_eq!(
        planned["ends_at"].as_i64().unwrap() - planned["starts_at"].as_i64().unwrap(),
        7 * 86_400
    );
}

#[tokio::test]
async fn an_active_window_shows_on_the_page_for_the_right_audience() {
    let (app, _store) = app_with_pool(MESH).await;
    let res = app
        .clone()
        .oneshot(write(
            "/api/maintenance?host=frank&duration=10m&reason=kernel+reboot",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);

    // Everyone who sees a covered monitor reads the reason; the private
    // monitor is named to the operator only.
    let public = json(app.clone().oneshot(get("/api/summary")).await.unwrap()).await;
    assert_eq!(public["maintenances"][0]["reason"], "kernel reboot");
    assert_eq!(public["maintenances"][0]["monitors"], "Web");
    let card = |summary: &serde_json::Value, id: &str| {
        summary["monitors"]
            .as_array()
            .and_then(|monitors| monitors.iter().find(|monitor| monitor["id"] == id))
            .map(|monitor| monitor["maintenance"].clone())
    };
    assert_eq!(card(&public, "web"), Some("kernel reboot".into()));
    assert_eq!(card(&public, "blog"), Some(serde_json::Value::Null));
    assert_eq!(card(&public, "vault"), None, "private: not in the view");
    let full = json(
        app.clone()
            .oneshot(operator("GET", "/api/summary"))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(full["maintenances"][0]["monitors"], "Web, Vault");

    let page = body_text(app.clone().oneshot(get("/")).await.unwrap()).await;
    assert!(page.contains("kernel reboot"), "{page}");
    assert!(!page.contains("Vault") && !page.contains("Host frank"));
    let page = body_text(
        app.clone()
            .oneshot(get("/?token=0123456789abcdef"))
            .await
            .unwrap(),
    )
    .await;
    assert!(page.contains("kernel reboot") && page.contains("Vault"));

    // The timeline tells the operator who planned what; nobody else.
    let timeline = body_text(
        app.clone()
            .oneshot(get("/timeline?token=0123456789abcdef"))
            .await
            .unwrap(),
    )
    .await;
    assert!(
        timeline.contains("maintenance planned for host frank: 10 min from"),
        "{timeline}"
    );
    assert!(timeline.contains("planned on this node"), "{timeline}");
    let timeline = body_text(app.oneshot(get("/timeline")).await.unwrap()).await;
    assert!(!timeline.contains("frank"), "{timeline}");
}

#[tokio::test]
async fn a_trusted_peers_heartbeat_announces_and_withdraws_its_window() {
    let (app, store) = app_with_pool(MESH).await;
    let now = chrono::Utc::now().timestamp();
    // Two hours for the reboot of its own host: accepted, cut to the 15
    // minutes this node grants.
    let announcement = format!(
        r#"{{"maintenance":[{{"id":12,"starts_at":{},"ends_at":{},"reason":"kernel reboot","host":"frank"}}]}}"#,
        now - 10,
        now + 7200
    );
    let res = app.clone().oneshot(heartbeat(&announcement)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_text(res).await, "ok");
    assert!(muted(&store, "frank-in", now).await, "its own watch");
    assert!(muted(&store, "web", now).await && muted(&store, "vault", now).await);
    assert!(!muted(&store, "blog", now).await, "no host label");
    assert!(!muted(&store, "beat", now).await);
    assert!(muted(&store, "web", now + 890).await);
    assert!(!muted(&store, "web", now + 891).await, "cut to 900 s");
    // The heartbeat itself counts as it always did.
    let beats = hora_core::db::recent_checks(&store, "frank-in", 10)
        .await
        .unwrap();
    assert_eq!(beats.len(), 1);

    // The operator sees it, with where it came from - and cannot cancel
    // here what the peer keeps announcing.
    let listed = json(
        app.clone()
            .oneshot(operator("GET", "/api/maintenance"))
            .await
            .unwrap(),
    )
    .await;
    let window = &listed["windows"][0];
    assert_eq!(window["origin"], "peer:hora-frank");
    assert_eq!(window["state"], "active");
    assert_eq!(window["ends_at"], now - 10 + 900);
    assert_eq!(
        window["monitors"],
        serde_json::json!(["frank-in", "web", "vault"])
    );
    let uri = format!("/api/maintenance/{}", window["id"]);
    let res = app.clone().oneshot(operator("DELETE", &uri)).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);

    // The same heartbeat again: still one window.
    app.clone().oneshot(heartbeat(&announcement)).await.unwrap();
    let rows = hora_core::db::pending_maintenance_windows(&store, now)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);

    // A body that is not an announcement: the heartbeat counts, nothing
    // else changes.
    let res = app.clone().oneshot(heartbeat("not json")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(muted(&store, "web", now).await);

    // A 0.11-shaped heartbeat - no body - is accepted as ever, and says
    // "nothing planned": the window is withdrawn.
    let res = app.clone().oneshot(heartbeat("")).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(!muted(&store, "web", now).await);
    assert!(!muted(&store, "frank-in", now).await);

    // A window for another host than the peer's: its watch, nothing more.
    let elsewhere = announcement.replace(r#""host":"frank""#, r#""host":"yuki""#);
    app.oneshot(heartbeat(&elsewhere)).await.unwrap();
    assert!(muted(&store, "frank-in", now).await);
    assert!(!muted(&store, "web", now).await);
}

#[tokio::test]
async fn an_oversized_announcement_changes_nothing_and_the_heartbeat_counts() {
    let (app, store) = app_with_pool(MESH).await;
    let now = chrono::Utc::now().timestamp();
    let announcement = format!(
        r#"{{"maintenance":[{{"id":12,"starts_at":{},"ends_at":{},"reason":"kernel reboot","host":"frank"}}]}}"#,
        now - 10,
        now + 600
    );
    app.clone().oneshot(heartbeat(&announcement)).await.unwrap();
    assert!(muted(&store, "web", now).await);

    // Valid JSON, padded past what an announcement may weigh: not read as
    // "nothing planned" (which would withdraw the window), not a failure of
    // the heartbeat either.
    let padded = format!(
        r#"{{"maintenance":[],"padding":"{}"}}"#,
        "x".repeat(hora_core::planned::MAX_ANNOUNCEMENT_BYTES)
    );
    let res = app.oneshot(heartbeat(&padded)).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(muted(&store, "web", now).await, "the window was withdrawn");
}

#[tokio::test]
async fn an_announcement_is_ignored_from_a_peer_that_was_not_trusted_with_it() {
    // The shared test app's peer has no accept_maintenance_secs.
    let (app, store) = test_app_with_pool().await;
    let now = chrono::Utc::now().timestamp();
    let announcement = format!(
        r#"{{"maintenance":[{{"id":1,"starts_at":{},"ends_at":{},"reason":"reboot"}}]}}"#,
        now - 10,
        now + 600
    );
    let request = |uri: &str, token: &str| {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("x-push-token", token)
            .extension(fake_peer())
            .body(Body::from(announcement.clone()))
            .expect("request")
    };
    let res = app
        .clone()
        .oneshot(request("/api/push/peer-x", "peertok-peertok-1"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK, "the heartbeat still counts");
    // A push monitor's heartbeat is no peer's: its body is not read.
    let res = app
        .oneshot(request("/api/push/beat", "s3cret-s3cret-16"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let stored = hora_core::db::pending_maintenance_windows(&store, 0)
        .await
        .unwrap();
    assert!(stored.is_empty(), "{stored:?}");
}
