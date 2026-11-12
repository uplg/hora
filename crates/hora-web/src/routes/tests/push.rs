//! Producer writes: push heartbeats, pushed alerts and the refused `?token=`.

use super::*;

#[tokio::test]
async fn push_records_heartbeat_with_token() {
    let res = test_app()
        .await
        .oneshot(beat("/api/push/beat?status=up"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn push_rejects_unknown_status_and_negative_ping() {
    let (app, store) = test_app_with_pool().await;
    for bad in [
        "/api/push/beat?status=dwon",
        "/api/push/beat?status=0",
        "/api/push/beat?ping=-5",
        // Over a day, up to what an i64 holds: a job's bug, not a latency.
        "/api/push/beat?ping=86400001",
        "/api/push/beat?ping=9223372036854775807",
    ] {
        let res = app.clone().oneshot(beat(bad)).await.unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{bad}");
    }
    // Nothing was recorded by the rejected pushes.
    let recorded = hora_core::db::recent_checks(&store, "beat", 10)
        .await
        .unwrap();
    assert!(recorded.is_empty());

    for good in [
        "/api/push/beat",
        "/api/push/beat?status=down&ping=0",
        "/api/push/beat?status=degraded",
        "/api/push/beat?ping=86400000",
    ] {
        let res = app.clone().oneshot(beat(good)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{good}");
    }
}

#[tokio::test]
async fn push_rejects_wrong_token() {
    let res = test_app()
        .await
        .oneshot(push_with("/api/push/beat", "x-push-token", "wrong"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn push_to_an_unknown_id_answers_like_a_wrong_token() {
    // "web" exists but is an HTTP monitor, not a push target; "nope" does
    // not exist at all. Both read like a wrong token - and like a known id
    // with no token at all: no id oracle.
    let app = test_app().await;
    for (uri, token) in [
        ("/api/push/web", Some("x")),
        ("/api/push/nope", None),
        ("/api/push/beat", Some("wrong")),
        ("/api/push/beat", None),
        ("/api/push/peer-x", None),
    ] {
        let request = match token {
            Some(token) => push_with(uri, "x-push-token", token),
            None => push(uri),
        };
        let res = app.clone().oneshot(request).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(body_text(res).await, "invalid push token", "{uri}");
    }
}

#[tokio::test]
async fn push_records_peer_heartbeat() {
    // A watched peer's listen id accepts heartbeats, like a push monitor.
    let res = test_app()
        .await
        .oneshot(push_with(
            "/api/push/peer-x",
            "x-push-token",
            "peertok-peertok-1",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test]
async fn push_rejects_wrong_peer_token() {
    let res = test_app()
        .await
        .oneshot(push_with("/api/push/peer-x", "x-push-token", "nope"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

/// The id alone opens a push id only where the config says so in so many
/// words - never because a token happens to be missing.
#[tokio::test]
async fn only_the_explicit_opt_out_accepts_a_bare_push_id() {
    let toml = r#"
        [page]
        [server]
        [health]
        id = "test-node"
        [[peers]]
        id = "open-peer"
        name = "Open peer"
        expect_every_secs = 60
        allow_unauthenticated = true
        [[peers]]
        id = "peer-x"
        name = "Peer X"
        expect_every_secs = 60
        listen_token = "peertok-peertok-1"
        [[monitors]]
        id = "open"
        name = "Open"
        kind = "push"
        interval_secs = 60
        allow_unauthenticated = true
        [[monitors]]
        id = "beat"
        name = "Beat"
        kind = "push"
        interval_secs = 60
        push_token = "s3cret-s3cret-16"
        "#;
    let (app, _store, reload) = app_with_reload(toml).await;
    // A config that slipped past validation with neither a token nor the
    // opt-out stays closed.
    let mut config = hora_core::config::parse(toml).expect("config");
    config.monitors[1].push_token = None;
    config.peers[1].listen_token = None;
    reload.send(Arc::new(config)).expect("reload");
    for (uri, expected) in [
        ("/api/push/open", StatusCode::OK),
        ("/api/push/open-peer", StatusCode::OK),
        ("/api/push/beat", StatusCode::UNAUTHORIZED),
        ("/api/push/peer-x", StatusCode::UNAUTHORIZED),
    ] {
        let res = app.clone().oneshot(push(uri)).await.unwrap();
        assert_eq!(res.status(), expected, "{uri}");
    }
}

/// A JSON POST to the alert endpoint, with an optional `X-Push-Token` (the
/// item token) and/or `Authorization: Bearer` (the global token).
fn alert(uri: &str, body: &str, push_token: Option<&str>, bearer: Option<&str>) -> Request<Body> {
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

const QUERY_TOKEN_REFUSED: &str = "?token= is not accepted on write endpoints: send the token \
    in the Authorization: Bearer header (or X-Push-Token for a push or peer token)";

#[tokio::test]
async fn query_tokens_are_refused_on_every_write() {
    // A write never takes its credential from the URL: the right token there
    // is refused like a wrong one, and nothing is done.
    let (app, store) = test_app_with_pool().await;
    let delete = |uri: &str| {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .extension(fake_peer())
            .body(Body::empty())
            .expect("request")
    };
    for request in [
        push("/api/push/beat?token=s3cret-s3cret-16"),
        push("/api/push/peer-x?token=peertok-peertok-1"),
        push("/api/event?title=deploy&token=0123456789abcdef"),
        push("/api/silence?monitors=web&duration=10m&token=0123456789abcdef"),
        push("/api/announce?title=x&token=0123456789abcdef"),
        delete("/api/announce?token=0123456789abcdef"),
        alert(
            "/api/monitors/web/alert?token=0123456789abcdef",
            r#"{"title":"deploy started"}"#,
            None,
            None,
        ),
        // A valid header next to it does not rescue the request: the token
        // is in the URL, which is the problem.
        write("/api/event?title=deploy&token=0123456789abcdef"),
        beat("/api/push/beat?token="),
        // Percent-encoded, it is the same parameter.
        push("/api/event?title=deploy&%74oken=0123456789abcdef"),
        push("/api/peer/probe?token=peertok-peertok-1"),
    ] {
        let label = format!("{} {}", request.method(), request.uri());
        let res = app.clone().oneshot(request).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{label}");
        assert!(res.headers().get("deprecation").is_none(), "{label}");
        assert_eq!(body_text(res).await, QUERY_TOKEN_REFUSED, "{label}");
    }
    assert!(
        hora_core::db::recent_events(&store, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        hora_core::db::recent_checks(&store, "beat", 10)
            .await
            .unwrap()
            .is_empty()
    );

    // The same writes through headers go through.
    for (request, expected) in [
        (beat("/api/push/beat"), StatusCode::OK),
        (write("/api/event?title=deploy"), StatusCode::OK),
        (
            alert(
                "/api/monitors/beat/alert",
                r#"{"title":"t"}"#,
                Some("s3cret-s3cret-16"),
                None,
            ),
            StatusCode::ACCEPTED,
        ),
    ] {
        let uri = request.uri().to_string();
        let res = app.clone().oneshot(request).await.unwrap();
        assert_eq!(res.status(), expected, "{uri}");
    }

    // The read-only views keep ?token=, for both tokens.
    for token in ["0123456789abcdef", "viewviewviewview"] {
        let res = app
            .clone()
            .oneshot(get(&format!("/api/summary?token={token}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{token}");
        assert!(body_text(res).await.contains("\"intra\""), "{token}");
    }
}

/// The read-only token reads what the operator reads of the monitors, and
/// writes nothing: every write answers it like a wrong token.
#[tokio::test]
async fn the_view_token_never_authorizes_a_write() {
    let (app, store) = test_app_with_pool().await;
    let viewer = |uri: &str| push_with(uri, "authorization", "Bearer viewviewviewview");
    let mut delete = viewer("/api/announce");
    *delete.method_mut() = axum::http::Method::DELETE;
    for request in [
        viewer("/api/event?title=deploy"),
        viewer("/api/silence?monitors=web&duration=10m"),
        viewer("/api/announce?title=x"),
        delete,
        alert(
            "/api/monitors/web/alert",
            r#"{"title":"x"}"#,
            None,
            Some("viewviewviewview"),
        ),
        // Nor is it anyone's push token.
        push_with("/api/push/beat", "x-push-token", "viewviewviewview"),
    ] {
        let label = format!("{} {}", request.method(), request.uri());
        let res = app.clone().oneshot(request).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{label}");
    }
    assert!(
        hora_core::db::recent_events(&store, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        hora_core::db::recent_pushed_alerts(&store, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn alert_dispatches_with_item_token_and_records_it() {
    let (app, store) = test_app_with_pool().await;
    let res = app
            .oneshot(alert(
                "/api/monitors/beat/alert",
                r#"{"severity":"error","title":"Patch failed","message":"3/12 ops","tags":{"task_id":"t1"}}"#,
                Some("s3cret-s3cret-16"),
                None,
            ))
            .await
            .unwrap();
    assert_eq!(res.status(), StatusCode::ACCEPTED);

    let alerts = hora_core::db::recent_pushed_alerts(&store, 10)
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
async fn a_request_body_past_the_limit_is_refused() {
    // Whoever sends it: the extractor buffers the body before the handler
    // has looked at a token.
    let huge = format!(r#"{{"title":"x","message":"{}"}}"#, "y".repeat(70 * 1024));
    let res = test_app()
        .await
        .oneshot(alert(
            "/api/monitors/beat/alert",
            &huge,
            Some("s3cret-s3cret-16"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn alert_accepts_global_auth_token_for_a_non_push_monitor() {
    // "web" is an HTTP monitor with no push_token: the operator token
    // authorizes, as a Bearer header.
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
async fn alert_does_not_reveal_which_ids_exist() {
    // Without the operator token, an unknown id, a private monitor and a
    // public one all answer the same 401 - no existence oracle.
    for uri in [
        "/api/monitors/nope/alert",
        "/api/monitors/intra/alert",
        "/api/monitors/web/alert",
    ] {
        let res = test_app()
            .await
            .oneshot(alert(uri, r#"{"title":"x"}"#, Some("guess"), None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(
            body_text(res).await,
            "alerting requires the monitor's push_token (X-Push-Token) or server.auth_token",
            "{uri}"
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
                Some("s3cret-s3cret-16"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{body}");
    }
}

#[tokio::test]
async fn alert_coalesces_a_repeated_dedup_key() {
    let (app, store) = test_app_with_pool().await;
    let body = r#"{"severity":"error","title":"flap","dedup_key":"ekb:mat"}"#;

    let first = app
        .clone()
        .oneshot(alert(
            "/api/monitors/beat/alert",
            body,
            Some("s3cret-s3cret-16"),
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
            Some("s3cret-s3cret-16"),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::ACCEPTED);
    assert!(body_text(second).await.contains("coalesced"));

    let alerts = hora_core::db::recent_pushed_alerts(&store, 10)
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
