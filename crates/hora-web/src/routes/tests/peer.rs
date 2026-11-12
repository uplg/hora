//! The peer-mesh endpoints, including two real nodes confirming each other.

use super::*;

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
async fn peer_probe_refuses_exec_and_push_kinds() {
    // Every exec monitor has the same empty target: answering would run
    // one of this node's local checks for another node's monitor (or
    // answer a bogus "down"). Refused outright, before any matching.
    let config = vantage_config(9);
    for kind in ["exec", "push"] {
        let body = format!(r#"{{"from":"hora-a","kind":"{kind}","target":""}}"#);
        let res = app_from(&config)
            .await
            .oneshot(probe_request(&body, Some("tok-a-to-b-16char")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST, "{kind}");
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
    let verdict: hora_core::mesh::wire::ProbeResponse =
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
    let verdict: hora_core::mesh::wire::ProbeResponse =
        serde_json::from_str(&body_text(res).await).unwrap();
    assert!(!verdict.up);
    assert!(verdict.error.is_some());
}

/// A local HTTP server answering every request `200 ok`. Returns its port.
async fn http_ok_server() -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0; 4096];
                if socket.read(&mut request).await.unwrap_or(0) == 0 {
                    return;
                }
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                    )
                    .await;
            });
        }
    });
    port
}

/// Node B with two monitors on one URL (credentials in it): `wants-200`
/// passes against [`http_ok_server`], `wants-204` fails.
fn two_checks_config(port: u16) -> String {
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
            id = "wants-200"
            name = "Wants 200"
            target = "http://127.0.0.1:{port}/health?api_key=s3cret"
            expected_status = 200
            interval_secs = 60
            timeout_secs = 2
            [[monitors]]
            id = "wants-204"
            name = "Wants 204"
            target = "http://127.0.0.1:{port}/health?api_key=s3cret"
            expected_status = 204
            interval_secs = 60
            timeout_secs = 2
            "#
    )
}

async fn probe_verdict(config: &str, body: &str) -> Result<bool, StatusCode> {
    let res = app_from(config)
        .await
        .oneshot(probe_request(body, Some("tok-a-to-b-16char")))
        .await
        .unwrap();
    if res.status() != StatusCode::OK {
        return Err(res.status());
    }
    let verdict: hora_core::mesh::wire::ProbeResponse =
        serde_json::from_str(&body_text(res).await).unwrap();
    Ok(verdict.up)
}

/// The responder used to answer with the first monitor on the target,
/// whatever it asserted: a requester whose check wants a 204 was told "up" by
/// the monitor that wants a 200.
#[tokio::test]
async fn peer_probe_answers_with_the_monitor_that_checks_the_same_way() {
    use hora_core::mesh::fingerprint::check_fingerprint;

    let port = http_ok_server().await;
    let config = two_checks_config(port);
    let parsed = hora_core::config::parse(&config).expect("config");
    let wants_200 = check_fingerprint(&parsed.monitors[0]).unwrap();
    let wants_204 = check_fingerprint(&parsed.monitors[1]).unwrap();
    assert_ne!(wants_200, wants_204);
    let masked = format!("http://127.0.0.1:{port}/health?api_key=***");
    let request = |check: &str| {
        format!(r#"{{"from":"hora-a","kind":"http","target":"{masked}","check":"{check}"}}"#)
    };

    assert_eq!(probe_verdict(&config, &request(&wants_200)).await, Ok(true));
    assert_eq!(
        probe_verdict(&config, &request(&wants_204)).await,
        Ok(false)
    );
    // A check this node does not run on the target: the same answer as a
    // target it does not have.
    assert_eq!(
        probe_verdict(&config, &request(&"0".repeat(64))).await,
        Err(StatusCode::NOT_FOUND)
    );
}

/// A 0.11 node sends no fingerprint, and its target verbatim: it is answered
/// as before, by the first monitor on the target. A 0.12 node's masked target
/// finds the same monitor.
#[tokio::test]
async fn peer_probe_without_fingerprint_matches_on_the_target_alone() {
    let port = http_ok_server().await;
    let config = two_checks_config(port);
    for target in [
        format!("http://127.0.0.1:{port}/health?api_key=s3cret"),
        format!("http://127.0.0.1:{port}/health?api_key=***"),
    ] {
        let body = format!(r#"{{"from":"hora-a","kind":"http","target":"{target}"}}"#);
        assert_eq!(probe_verdict(&config, &body).await, Ok(true), "{target}");
    }
    // Another path is another target, masked or not.
    let body = format!(
        r#"{{"from":"hora-a","kind":"http","target":"http://127.0.0.1:{port}/other?api_key=***"}}"#
    );
    assert_eq!(
        probe_verdict(&config, &body).await,
        Err(StatusCode::NOT_FOUND)
    );
}

#[tokio::test]
async fn peer_monitors_authenticates_and_discloses_probeable_targets() {
    // Same strict auth as the probe endpoint: wrong token, missing token
    // or unknown peer all answer 401.
    for (from, token) in [
        ("peer-x", Some("wrong")),
        ("peer-x", None),
        ("nobody", Some("peertok-peertok-1")),
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
                .header("x-push-token", "peertok-peertok-1")
                .extension(fake_peer())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let answer: hora_core::mesh::wire::PeerMonitors =
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
    // The peer answer reads the same snapshot as the page: it must report
    // exactly the status and p50 the authenticated summary shows.
    let (app, store) = test_app_with_pool().await;
    let now = chrono::Utc::now().timestamp();
    for (offset, latency) in [(60, 40), (120, 10), (180, 30), (240, 20), (300, 50)] {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, latency_ms) VALUES (?, 'web', 1, ?)",
        )
        .bind(now - offset)
        .bind(latency)
        .execute(store.fixture_pool())
        .await
        .unwrap();
    }
    for offset in [60, 120, 180, 240] {
        sqlx::query("INSERT INTO checks (time, monitor_id, status, latency_ms) VALUES (?, 'intra', 0, NULL)")
                .bind(now - offset)
                .execute(store.fixture_pool())
                .await
                .unwrap();
    }

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/peer/monitors?from=peer-x")
                .header("x-push-token", "peertok-peertok-1")
                .extension(fake_peer())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let peer: hora_core::mesh::wire::PeerMonitors =
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
        assert_eq!(
            answer.status.as_str(),
            view["status"].as_str().unwrap(),
            "{id}"
        );
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
    let answer = hora_core::mesh::vantage::fetch_peer_monitors(
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
        hora_core::mesh::vantage::fetch_peer_monitors(&client, &url, "hora-a", Some("wrong")).await;
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
        hora_core::mesh::confirm::confirm_with_peers(&client, &config_a, &config_a.monitors[0])
            .await
            .expect("peers were asked");
    assert!(verdict.text.contains("seen UP by Hora B"), "{verdict:?}");
    assert!(verdict.text.contains("network issue"), "{verdict:?}");
    // B, the only peer, sees it up: down from A only.
    assert!(verdict.local_only);

    // Real outage: the service is gone for B too.
    drop(service);
    let verdict =
        hora_core::mesh::confirm::confirm_with_peers(&client, &config_a, &config_a.monitors[0])
            .await
            .expect("peers were asked");
    assert_eq!(verdict.text, "confirmed down from 2/2 vantage points");
    assert!(!verdict.local_only);

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
        hora_core::mesh::confirm::confirm_with_peers(&client, &config_bad, &config_bad.monitors[0])
            .await
            .expect("peers were asked");
    assert!(
        verdict.text.contains("no peer vantage reachable"),
        "{verdict:?}"
    );
    // No answer is no contradiction: the alert goes out as usual.
    assert!(!verdict.local_only);

    // Fail open: a peer that is not even listening behaves the same.
    config_bad.peers[0].ping_url = Some(hora_core::config::Secret(
        "http://127.0.0.1:9/api/push/hora-a".to_owned(),
    ));
    let verdict =
        hora_core::mesh::confirm::confirm_with_peers(&client, &config_bad, &config_bad.monitors[0])
            .await
            .expect("peers were asked");
    assert!(
        verdict.text.contains("no peer vantage reachable"),
        "{verdict:?}"
    );
    // No answer is no contradiction: the alert goes out as usual.
    assert!(!verdict.local_only);
}

/// A maintenance window mutes this node's alerts, never what it sees: asked
/// by a peer about a monitor it has in maintenance, it still probes and
/// answers what it found. The peer's alert is the peer's to decide (the one
/// that is rebooting is not asking anyone), and nothing is recorded here.
#[tokio::test]
async fn peer_probe_answers_honestly_for_a_monitor_in_maintenance() {
    // A port nothing listens on: the service is down with its host.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    let toml = vantage_config(port);
    let (app, store) = app_with_pool(&toml).await;
    let config = hora_core::config::parse(&toml).expect("config");
    let now = chrono::Utc::now().timestamp();
    hora_core::planned::plan(
        &store,
        &config,
        hora_core::planned::Plan {
            monitors: Some("svc"),
            duration: "10m",
            reason: "kernel reboot",
            ..hora_core::planned::Plan::default()
        },
        now,
    )
    .await
    .expect("planned");
    let at = chrono::DateTime::from_timestamp(now, 0).expect("time");
    assert!(hora_core::planned::in_maintenance(&store, &config, "svc", at).await);

    let body = format!(r#"{{"from":"hora-a","kind":"tcp","target":"127.0.0.1:{port}"}}"#);
    let res = app
        .oneshot(probe_request(&body, Some("tok-a-to-b-16char")))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let verdict: hora_core::mesh::wire::ProbeResponse =
        serde_json::from_str(&body_text(res).await).unwrap();
    assert!(!verdict.up, "down is down, window or not");
    // Answering a peer is not an alert of this node's: no incident, no check.
    assert_eq!(
        hora_core::db::find_open_incident(&store, "svc")
            .await
            .unwrap(),
        None
    );
    assert!(
        hora_core::db::recent_checks(&store, "svc", 10)
            .await
            .unwrap()
            .is_empty()
    );
}
