use super::*;

/// Node `yuki`, watching its peer on host `frank` and four services: mail
/// and web on frank, one on yuki itself, and a wiki somewhere else (no
/// label). `peer_extra` is spliced into the peer, `extra` at the end.
fn config_with(peer_extra: &str, extra: &str) -> Config {
    crate::config::parse(&format!(
        r#"
        [page]
        [server]
        [health]
        id = "hora-yuki"
        host = "yuki"
        [[peers]]
        id = "hora-frank"
        name = "Frank"
        listen_id = "frank-in"
        expect_every_secs = 120
        listen_token = "a-long-enough-listen-token"
        ping_url = "https://frank.example.com/api/push/hora-yuki"
        {peer_extra}
        [[monitors]]
        id = "mail"
        name = "Mail"
        kind = "tcp"
        target = "mail.example.com:25"
        interval_secs = 60
        host = "frank"
        [[monitors]]
        id = "web"
        name = "Web"
        target = "https://www.example.com"
        interval_secs = 60
        host = "frank"
        [[monitors]]
        id = "wiki"
        name = "Wiki"
        target = "https://wiki.example.com"
        interval_secs = 60
        [[monitors]]
        id = "local"
        name = "Local"
        target = "https://local.example.com"
        interval_secs = 60
        host = "yuki"
        {extra}
        "#
    ))
    .expect("config")
}

/// The peer trusted for 15 minutes, on host `frank`.
fn trusting() -> Config {
    config_with("host = \"frank\"\naccept_maintenance_secs = 900", "")
}

fn at(timestamp: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(timestamp, 0).expect("time")
}

fn announced(id: i64, starts_at: i64, ends_at: i64, host: Option<&str>) -> AnnouncedWindow {
    AnnouncedWindow {
        id,
        starts_at,
        ends_at,
        reason: "kernel reboot".to_owned(),
        host: host.map(str::to_owned),
    }
}

/// 2027-01-15 08:00:00 UTC.
const NOW: i64 = 1_800_000_000;

fn host_plan<'a>(host: &'a str, starts_at: &'a str) -> Plan<'a> {
    Plan {
        host: Some(host),
        starts_at: Some(starts_at),
        duration: "10m",
        reason: "kernel reboot",
        ..Plan::default()
    }
}

#[tokio::test]
async fn one_schedule_answers_for_config_api_and_peer_windows() {
    let store = Store::in_memory().await;
    let start = at(NOW).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let end = at(NOW + 600).format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let config = config_with(
        "host = \"frank\"\naccept_maintenance_secs = 900",
        &format!(
            "[[maintenance]]\ntitle = \"Wiki move\"\nstart = \"{start}\"\nend = \"{end}\"\n\
             monitors = [\"wiki\"]\n"
        ),
    );
    // Planned here for this node's own host, an hour later.
    let later = (NOW + 3600).to_string();
    plan(&store, &config, host_plan("yuki", &later), NOW)
        .await
        .expect("planned");
    // Announced by the peer for its host, two hours later.
    let applied = apply_announcement(
        &store,
        &config.peers[0],
        true,
        &[announced(7, NOW + 7200, NOW + 7800, Some("frank"))],
        NOW,
    )
    .await
    .expect("applied");
    assert_eq!(applied.accepted.len(), 1);

    let schedule = schedule_between(&store, &config, 0, i64::MAX).await;
    let origin = |id: &str, when: i64| schedule.active(id, at(when)).map(|w| w.origin.clone());
    // The config window, now.
    assert_eq!(origin("wiki", NOW + 60), Some(Origin::Config));
    assert_eq!(origin("local", NOW + 60), None);
    // The API window: this node's own host, and nothing before it starts.
    assert_eq!(origin("local", NOW + 3599), None);
    assert_eq!(origin("local", NOW + 3600), Some(Origin::Api));
    assert_eq!(origin("local", NOW + 4200), Some(Origin::Api));
    assert_eq!(origin("local", NOW + 4201), None);
    assert_eq!(origin("mail", NOW + 3700), None);
    // The peer's: its watch and the monitors on its host - not the others.
    let from_frank = Some(Origin::Peer("hora-frank".to_owned()));
    assert_eq!(origin("frank-in", NOW + 7300), from_frank);
    assert_eq!(origin("mail", NOW + 7300), from_frank);
    assert_eq!(origin("web", NOW + 7300), from_frank);
    assert_eq!(origin("wiki", NOW + 7300), None);
    assert_eq!(origin("local", NOW + 7300), None);
    assert_eq!(origin("mail", NOW + 7199), None, "not before its start");
    assert_eq!(origin("frank-in", NOW + 7801), None, "not after its end");

    // The async door every alert path uses agrees with the schedule.
    assert!(in_maintenance(&store, &config, "mail", at(NOW + 7300)).await);
    assert!(!in_maintenance(&store, &config, "mail", at(NOW + 60)).await);
    let window = active_window(&store, &config, "mail", at(NOW + 7300))
        .await
        .expect("window");
    assert_eq!(window.title, "kernel reboot");
    assert_eq!(window.host.as_deref(), Some("frank"));
}

#[tokio::test]
async fn planning_twice_returns_the_same_window() {
    let store = Store::in_memory().await;
    let config = trusting();
    let start = (NOW + 3600).to_string();
    let first = plan(&store, &config, host_plan("frank", &start), NOW)
        .await
        .expect("planned");
    assert!(first.created);
    assert_eq!(first.window.selector, "host:frank");
    assert_eq!(first.window.ends_at - first.window.starts_at, 600);

    // The host script runs again a few minutes later: still one window.
    let again = plan(&store, &config, host_plan("frank", &start), NOW + 300)
        .await
        .expect("planned");
    assert!(!again.created);
    assert_eq!(again.window.id, first.window.id);
    // The same start as RFC 3339 is the same window.
    let rfc = at(NOW + 3600).to_rfc3339();
    let again = plan(&store, &config, host_plan("frank", &rfc), NOW)
        .await
        .expect("planned");
    assert_eq!(again.window.id, first.window.id);
    assert_eq!(
        db::pending_maintenance_windows(&store, NOW)
            .await
            .unwrap()
            .len(),
        1
    );

    // Ids in another order are the same selector; another end is not.
    let ids = |monitors: &'static str, duration: &'static str| Plan {
        monitors: Some(monitors),
        starts_at: Some("1800003600"),
        duration,
        reason: "deploy",
        ..Plan::default()
    };
    let one = plan(&store, &config, ids("web,mail", "10m"), NOW)
        .await
        .unwrap();
    let two = plan(&store, &config, ids("mail, web", "10m"), NOW)
        .await
        .unwrap();
    assert_eq!(one.window.selector, "ids:mail,web");
    assert_eq!(one.window.id, two.window.id);
    let longer = plan(&store, &config, ids("mail,web", "20m"), NOW)
        .await
        .unwrap();
    assert!(longer.created && longer.window.id != one.window.id);
    let all = plan(&store, &config, ids("all", "10m"), NOW).await.unwrap();
    assert_eq!(all.window.selector, "*");
}

#[tokio::test]
async fn a_plan_is_checked_before_anything_is_written() {
    let store = Store::in_memory().await;
    let config = trusting();
    let base = host_plan("frank", "1800003600");
    let both = Plan {
        monitors: Some("mail"),
        ..base
    };
    let ids = |monitors: &'static str| Plan {
        host: None,
        monitors: Some(monitors),
        ..base
    };
    let starting = |starts_at: &'static str| Plan {
        starts_at: Some(starts_at),
        ..base
    };
    let refused = [
        // No selector, or two.
        (Plan { host: None, ..base }, "selector"),
        (both, "selector"),
        (host_plan("typo", "1800003600"), "host"),
        (ids("mail,typo"), "id"),
        (ids(" , "), "ids"),
        // The silence cap: 7 days.
        (
            Plan {
                duration: "8d",
                ..base
            },
            "duration",
        ),
        (
            Plan {
                duration: "soon",
                ..base
            },
            "duration",
        ),
        (
            Plan {
                reason: "  ",
                ..base
            },
            "reason",
        ),
        (starting("tomorrow"), "start"),
        // Milliseconds, taken for seconds: tens of thousands of years ahead.
        (starting("1800003600000"), "start"),
        // Ended an hour ago.
        (starting("1799995800"), "over"),
    ];
    for (bad, what) in refused {
        let err = plan(&store, &config, bad, NOW).await.expect_err(what);
        let expected = match what {
            "selector" => matches!(err, PlanError::Selector),
            "host" => matches!(err, PlanError::UnknownHost(_)),
            "id" => matches!(err, PlanError::UnknownId(ref id) if id == "typo"),
            "ids" => matches!(err, PlanError::NoIds),
            "duration" => matches!(err, PlanError::InvalidDuration),
            "reason" => matches!(err, PlanError::NoReason),
            "start" => matches!(err, PlanError::InvalidStart),
            _ => matches!(err, PlanError::AlreadyOver),
        };
        assert!(expected, "{what}: {err}");
    }
    assert_eq!(
        db::pending_maintenance_windows(&store, 0).await.unwrap(),
        Vec::new()
    );

    // A window that began before now and has not ended is planned as is.
    let planned = plan(&store, &config, starting("1799999700"), NOW)
        .await
        .expect("planned");
    assert_eq!(planned.window.ends_at, NOW + 300);
    // The default start is now; a peer's listen id is an id like another.
    let peer_watch = Plan {
        monitors: Some("frank-in"),
        duration: "5m",
        reason: "frank is moving",
        ..Plan::default()
    };
    let planned = plan(&store, &config, peer_watch, NOW)
        .await
        .expect("planned");
    assert_eq!(planned.window.starts_at, NOW);
    assert!(in_maintenance(&store, &config, "frank-in", at(NOW + 10)).await);
    assert!(!in_maintenance(&store, &config, "wiki", at(NOW + 10)).await);
}

#[tokio::test]
async fn a_window_for_a_host_also_mutes_the_watch_of_the_peer_on_it() {
    // Planned on yuki, by hand, for frank: the services there and the node
    // that runs there.
    let store = Store::in_memory().await;
    let config = trusting();
    plan(&store, &config, host_plan("frank", "1800000000"), NOW)
        .await
        .unwrap();
    let schedule = schedule_between(&store, &config, 0, i64::MAX).await;
    let window = schedule.active("mail", at(NOW + 1)).expect("window");
    assert_eq!(window.monitors, ["mail", "web", "frank-in"]);
    assert_eq!(window.covered_peers(&config), ["Frank"]);
    assert!(!schedule.in_maintenance("wiki", at(NOW + 1)));
}

#[tokio::test]
async fn only_this_nodes_own_windows_are_announced() {
    let store = Store::in_memory().await;
    // This node runs on yuki.
    let config = trusting();
    let own = plan(&store, &config, host_plan("yuki", "1800003600"), NOW)
        .await
        .unwrap();
    // Another host than this node's: local only.
    plan(&store, &config, host_plan("frank", "1800003600"), NOW)
        .await
        .unwrap();
    let monitors = plan(
        &store,
        &config,
        Plan {
            monitors: Some("wiki"),
            starts_at: Some("1800001800"),
            duration: "5m",
            reason: "wiki upgrade",
            ..Plan::default()
        },
        NOW,
    )
    .await
    .unwrap();
    // More than a week ahead: not yet.
    plan(&store, &config, host_plan("yuki", "1800691201"), NOW)
        .await
        .unwrap();
    // What a peer announced here is never passed on.
    apply_announcement(
        &store,
        &config.peers[0],
        true,
        &[announced(3, NOW + 60, NOW + 120, Some("frank"))],
        NOW,
    )
    .await
    .unwrap();

    let told = announcements(&store, &config, NOW).await.unwrap();
    assert_eq!(
        told,
        [
            // Soonest first; named monitors go without a host, so a peer
            // only stops expecting this node.
            AnnouncedWindow {
                id: monitors.window.id,
                starts_at: NOW + 1800,
                ends_at: NOW + 2100,
                reason: "wiki upgrade".to_owned(),
                host: None,
            },
            AnnouncedWindow {
                id: own.window.id,
                starts_at: NOW + 3600,
                ends_at: NOW + 4200,
                reason: "kernel reboot".to_owned(),
                host: Some("yuki".to_owned()),
            },
        ]
    );
    assert!(is_announced(&config, &own.window));

    // Once it has ended it is no longer announced.
    let told = announcements(&store, &config, NOW + 4201).await.unwrap();
    assert!(told.iter().all(|window| window.id != own.window.id));

    // Cancelled: gone from the next heartbeat.
    cancel(&store, monitors.window.id).await.expect("cancelled");
    let told = announcements(&store, &config, NOW).await.unwrap();
    assert_eq!(told.len(), 1);
    assert!(matches!(
        cancel(&store, monitors.window.id).await,
        Err(CancelError::NotFound)
    ));
}

#[test]
fn a_heartbeat_reads_with_or_without_windows() {
    use crate::mesh::wire::HeartbeatBody;

    // What 0.12 sends when something is planned.
    let body = HeartbeatBody {
        maintenance: vec![announced(12, 100, 700, Some("frank"))],
    };
    let json = serde_json::to_value(&body).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"maintenance": [{
            "id": 12, "starts_at": 100, "ends_at": 700,
            "reason": "kernel reboot", "host": "frank"
        }]})
    );
    assert_eq!(serde_json::from_value::<HeartbeatBody>(json).unwrap(), body);
    // Without a host, the key is left out; without a reason, it reads empty.
    let bare: HeartbeatBody =
        serde_json::from_str(r#"{"maintenance":[{"id":1,"starts_at":5,"ends_at":9}]}"#).unwrap();
    assert_eq!(bare.maintenance[0].host, None);
    assert_eq!(bare.maintenance[0].reason, "");
    assert!(
        !serde_json::to_string(&bare).unwrap().contains("host"),
        "no host key"
    );
    // A body from a later version, with keys this one does not know.
    let newer: HeartbeatBody = serde_json::from_str(
        r#"{"version":3,"maintenance":[{"id":1,"starts_at":5,"ends_at":9,"scope":"x"}]}"#,
    )
    .unwrap();
    assert_eq!(newer.maintenance.len(), 1);
    // An empty object announces nothing, like the 0.11 heartbeat without a
    // body.
    let empty: HeartbeatBody = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, HeartbeatBody::default());
    assert_eq!(serde_json::to_string(&empty).unwrap(), "{}");
}

/// The stored windows that have not ended at `NOW`.
async fn stored(store: &Store) -> Vec<MaintenanceWindow> {
    db::pending_maintenance_windows(store, NOW).await.unwrap()
}

#[tokio::test]
async fn a_peers_window_is_accepted_only_where_the_operator_said_so() {
    let window = [announced(7, NOW + 3600, NOW + 4200, Some("frank"))];

    // No accept_maintenance_secs: ignored entirely, and said so.
    let store = Store::in_memory().await;
    let config = config_with("host = \"frank\"", "");
    let applied = apply_announcement(&store, &config.peers[0], true, &window, NOW)
        .await
        .unwrap();
    assert_eq!(applied.accepted, Vec::new());
    assert_eq!(
        applied.refused,
        ["accept_maintenance_secs is not set for this peer"]
    );
    assert_eq!(stored(&store).await, Vec::new());
    assert!(!in_maintenance(&store, &config, "frank-in", at(NOW + 3700)).await);
    // And a heartbeat that announces nothing is refused nothing.
    let applied = apply_announcement(&store, &config.peers[0], true, &[], NOW)
        .await
        .unwrap();
    assert_eq!(applied, Applied::default());

    // Trusted, but the heartbeat did not prove who sent it.
    let config = trusting();
    let applied = apply_announcement(&store, &config.peers[0], false, &window, NOW)
        .await
        .unwrap();
    assert_eq!(
        applied.refused,
        ["the heartbeat did not carry the peer's listen_token"]
    );
    assert_eq!(stored(&store).await, Vec::new());

    // Trusted and authenticated: stored under the peer's name.
    let applied = apply_announcement(&store, &config.peers[0], true, &window, NOW)
        .await
        .unwrap();
    assert_eq!(applied.refused, Vec::<&str>::new());
    let rows = stored(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].origin, "peer:hora-frank");
    assert_eq!(rows[0].remote_id, Some(7));
    assert_eq!(rows[0].selector, "host:frank");
    assert_eq!(rows[0].reason, "kernel reboot");

    // The same heartbeat again is the same window, not a second one.
    let applied = apply_announcement(&store, &config.peers[0], true, &window, NOW + 60)
        .await
        .unwrap();
    assert_eq!(applied, Applied::default());
    assert_eq!(stored(&store).await.len(), 1);

    // The operator stops trusting the peer: the stored window mutes nothing.
    let distrusting = config_with("host = \"frank\"", "");
    assert!(in_maintenance(&store, &config, "mail", at(NOW + 3700)).await);
    assert!(!in_maintenance(&store, &distrusting, "mail", at(NOW + 3700)).await);
    assert!(!in_maintenance(&store, &distrusting, "frank-in", at(NOW + 3700)).await);
    assert!(resolve(&distrusting, &rows[0]).is_none());
}

#[tokio::test]
async fn a_peers_window_is_cut_to_the_accepted_length() {
    let store = Store::in_memory().await;
    let config = trusting();
    // Two hours asked, fifteen minutes accepted: cut from its start.
    let applied = apply_announcement(
        &store,
        &config.peers[0],
        true,
        &[announced(1, NOW + 600, NOW + 7800, Some("frank"))],
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(applied.refused, Vec::<&str>::new(), "cut, not refused");
    assert_eq!(applied.accepted[0].starts_at, NOW + 600);
    assert_eq!(applied.accepted[0].ends_at, NOW + 600 + 900);
    assert!(in_maintenance(&store, &config, "mail", at(NOW + 1500)).await);
    assert!(!in_maintenance(&store, &config, "mail", at(NOW + 1501)).await);
    assert!(!in_maintenance(&store, &config, "frank-in", at(NOW + 1501)).await);

    // A tighter bound set later applies to what is already stored.
    let tighter = config_with("host = \"frank\"\naccept_maintenance_secs = 60", "");
    assert!(in_maintenance(&store, &tighter, "mail", at(NOW + 660)).await);
    assert!(!in_maintenance(&store, &tighter, "mail", at(NOW + 661)).await);

    // More than a week ahead, or ending before it starts: refused.
    let applied = apply_announcement(
        &store,
        &config.peers[0],
        true,
        &[
            announced(1, NOW + 600, NOW + 7800, Some("frank")),
            announced(2, NOW + MAX_ANNOUNCE_AHEAD_SECS + 1, NOW + 700_000, None),
            announced(3, NOW + 900, NOW + 900, None),
        ],
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        applied.refused,
        [
            "a window that starts more than 7 days ahead",
            "a window that ends before it starts"
        ]
    );
    assert!(applied.accepted.is_empty() && applied.withdrawn.is_empty());
    assert_eq!(stored(&store).await.len(), 1);
}

#[tokio::test]
async fn a_peer_mutes_its_own_host_and_never_another() {
    let store = Store::in_memory().await;
    let config = trusting();
    let peer = &config.peers[0];
    // It names this node's host, not its own: only its watch is muted.
    let applied = apply_announcement(
        &store,
        peer,
        true,
        &[announced(1, NOW, NOW + 600, Some("yuki"))],
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        applied.refused,
        ["a host that is not this peer's (only its watch is muted)"]
    );
    assert_eq!(applied.accepted[0].selector, "peer");
    let muted = |id: &'static str| {
        let (store, config) = (store.clone(), config.clone());
        async move { in_maintenance(&store, &config, id, at(NOW + 60)).await }
    };
    assert!(muted("frank-in").await);
    for id in ["local", "mail", "web", "wiki"] {
        assert!(!muted(id).await, "{id}");
    }

    // Without a host (it planned named monitors): its watch, nothing else.
    apply_announcement(
        &store,
        peer,
        true,
        &[announced(2, NOW, NOW + 600, None)],
        NOW,
    )
    .await
    .unwrap();
    assert!(muted("frank-in").await);
    assert!(!muted("mail").await);

    // Its own host: the monitors labelled with it, and no other - the wiki
    // carries no host label and is never muted by a peer.
    apply_announcement(
        &store,
        peer,
        true,
        &[announced(3, NOW, NOW + 600, Some("frank"))],
        NOW,
    )
    .await
    .unwrap();
    assert!(muted("frank-in").await && muted("mail").await && muted("web").await);
    assert!(!muted("wiki").await && !muted("local").await);

    // A peer entry without a host can only ever mute its own watch, and a
    // row that claims more (a hand-edited database) is read the same way.
    let hostless = config_with("accept_maintenance_secs = 900", "");
    assert!(in_maintenance(&store, &hostless, "frank-in", at(NOW + 60)).await);
    assert!(!in_maintenance(&store, &hostless, "mail", at(NOW + 60)).await);
    for forged in ["*", "ids:wiki,local", "host:yuki"] {
        let row = MaintenanceWindow {
            id: 99,
            starts_at: NOW,
            ends_at: NOW + 600,
            reason: "forged".to_owned(),
            selector: forged.to_owned(),
            origin: "peer:hora-frank".to_owned(),
            remote_id: Some(99),
            created_at: NOW,
        };
        let window = resolve(&config, &row).expect("resolves to the watch");
        assert!(!window.all, "{forged}");
        assert_eq!(window.monitors, ["frank-in"], "{forged}");
    }
}

#[tokio::test]
async fn a_window_no_longer_announced_is_withdrawn() {
    let store = Store::in_memory().await;
    let config = trusting();
    let peer = &config.peers[0];
    let reboot = announced(1, NOW + 3600, NOW + 4200, Some("frank"));
    let deploy = announced(2, NOW + 60, NOW + 360, None);
    apply_announcement(&store, peer, true, &[reboot.clone(), deploy.clone()], NOW)
        .await
        .unwrap();
    assert_eq!(stored(&store).await.len(), 2);
    // A window planned on this node is not the peer's to withdraw.
    let own = plan(&store, &config, host_plan("yuki", "1800003600"), NOW)
        .await
        .unwrap();

    // The reboot was cancelled over there: the next heartbeat lists one.
    let applied = apply_announcement(&store, peer, true, std::slice::from_ref(&deploy), NOW + 60)
        .await
        .unwrap();
    assert_eq!(applied.withdrawn.len(), 1);
    assert_eq!(applied.withdrawn[0].remote_id, Some(1));
    assert!(!in_maintenance(&store, &config, "mail", at(NOW + 3700)).await);
    assert!(in_maintenance(&store, &config, "frank-in", at(NOW + 100)).await);

    // A heartbeat without a list (nothing planned any more, or a node back
    // on 0.11) withdraws the rest - even a window under way.
    let applied = apply_announcement(&store, peer, true, &[], NOW + 120)
        .await
        .unwrap();
    assert_eq!(applied.withdrawn.len(), 1);
    assert!(!in_maintenance(&store, &config, "frank-in", at(NOW + 130)).await);
    // What it muted before being withdrawn stays on record (it counts
    // against the peer's budget); the rest of it is given back.
    let left = stored(&store).await;
    assert_eq!(left.len(), 2);
    assert!(left.iter().any(|row| row.id == own.window.id));
    let ended = left.iter().find(|row| row.remote_id == Some(2)).unwrap();
    assert_eq!((ended.starts_at, ended.ends_at), (NOW + 60, NOW + 119));

    // The same id announced with other hours replaces the window.
    apply_announcement(&store, peer, true, std::slice::from_ref(&reboot), NOW)
        .await
        .unwrap();
    let moved = announced(1, NOW + 7200, NOW + 7800, Some("frank"));
    let applied = apply_announcement(&store, peer, true, &[moved], NOW)
        .await
        .unwrap();
    assert_eq!((applied.withdrawn.len(), applied.accepted.len()), (1, 1));
    assert!(!in_maintenance(&store, &config, "mail", at(NOW + 3700)).await);
    assert!(in_maintenance(&store, &config, "mail", at(NOW + 7300)).await);

    // A window that ended is history: no heartbeat withdraws it.
    let applied = apply_announcement(&store, peer, true, &[], NOW + 7801)
        .await
        .unwrap();
    assert_eq!(applied.withdrawn, Vec::new());
    let mut kept = db::maintenance_windows_between(&store, NOW + 7200, NOW + 7800)
        .await
        .unwrap();
    assert_eq!(kept.len(), 1);
    // And the peer's own announcement cannot be cancelled from here.
    let row = kept.remove(0);
    assert!(matches!(
        cancel(&store, row.id).await,
        Err(CancelError::FromPeer(peer)) if peer == "hora-frank"
    ));
}

/// A script on the peer that plans "now, for ten minutes" every minute:
/// each window is within the accepted length, and together they would mute
/// the host for good. The budget (four accepted lengths a day) ends it.
#[tokio::test]
async fn a_peer_cannot_chain_windows_past_its_daily_budget() {
    let store = Store::in_memory().await;
    let config = trusting();
    let peer = &config.peers[0];
    let budget = 900 * PEER_BUDGET_WINDOWS;
    let mut refused = false;
    for minute in 0..180 {
        let now = NOW + minute * 60;
        // The windows still running, as the peer's heartbeat lists them.
        let listed: Vec<AnnouncedWindow> = ((minute - 9).max(0)..=minute)
            .map(|planned| {
                let start = NOW + planned * 60;
                announced(100 + planned, start, start + 600, Some("frank"))
            })
            .collect();
        let applied = apply_announcement(&store, peer, true, &listed, now)
            .await
            .unwrap();
        refused |= applied
            .refused
            .contains(&"this peer has used its maintenance budget for 24 hours");
    }
    assert!(refused, "the budget was never reached");
    assert!(in_maintenance(&store, &config, "mail", at(NOW + budget - 1)).await);
    for later in [budget + 1, 2 * 3600, 3 * 3600 - 1] {
        assert!(
            !in_maintenance(&store, &config, "mail", at(NOW + later)).await,
            "still muted {later} s in"
        );
        assert!(!in_maintenance(&store, &config, "frank-in", at(NOW + later)).await);
    }

    // A day later the peer has its budget again.
    let tomorrow = NOW + SECONDS_PER_DAY + budget + 60;
    let applied = apply_announcement(
        &store,
        peer,
        true,
        &[announced(900, tomorrow, tomorrow + 600, Some("frank"))],
        tomorrow,
    )
    .await
    .unwrap();
    assert_eq!(applied.refused, Vec::<&str>::new());
    assert!(in_maintenance(&store, &config, "mail", at(tomorrow + 599)).await);
}

/// The ordinary case is untouched: a window listed again on every heartbeat
/// while it runs keeps its hours, and several in a day fit.
#[tokio::test]
async fn a_window_announced_again_keeps_its_hours() {
    let store = Store::in_memory().await;
    let config = trusting();
    let peer = &config.peers[0];
    let reboot = announced(1, NOW, NOW + 900, Some("frank"));
    for elapsed in [0, 60, 300, 840] {
        let applied = apply_announcement(
            &store,
            peer,
            true,
            std::slice::from_ref(&reboot),
            NOW + elapsed,
        )
        .await
        .unwrap();
        assert_eq!(applied.refused, Vec::<&str>::new());
        assert!(applied.withdrawn.is_empty(), "replaced {elapsed} s in");
    }
    assert!(in_maintenance(&store, &config, "mail", at(NOW + 899)).await);
    // Three more reboots the same day: four accepted lengths in all.
    for (id, hours) in [(2, 3), (3, 6), (4, 9)] {
        let start = NOW + hours * 3600;
        let applied = apply_announcement(
            &store,
            peer,
            true,
            &[announced(id, start, start + 900, Some("frank"))],
            start,
        )
        .await
        .unwrap();
        assert_eq!(applied.refused, Vec::<&str>::new(), "reboot {id}");
        assert!(in_maintenance(&store, &config, "mail", at(start + 899)).await);
    }
    // The fifth is one too many.
    let start = NOW + 12 * 3600;
    let applied = apply_announcement(
        &store,
        peer,
        true,
        &[announced(5, start, start + 900, Some("frank"))],
        start,
    )
    .await
    .unwrap();
    assert_eq!(
        applied.refused,
        ["this peer has used its maintenance budget for 24 hours"]
    );
    assert!(!in_maintenance(&store, &config, "mail", at(start + 10)).await);
}

#[test]
fn what_is_covered_is_said_one_way() {
    assert_eq!(covered(true, &["web"]), "all monitors");
    assert_eq!(covered(false, &[] as &[&str]), "no monitor of this node");
    assert_eq!(covered(false, &["Web", "API"]), "Web, API");
}

/// The byte limit of an announcement holds the fullest one a peer may
/// send: every window allowed, each with the longest reason in four-byte
/// characters.
#[test]
fn the_fullest_announcement_fits_its_size_limit() {
    let reason = "\u{1F6E0}".repeat(MAX_SILENCE_REASON_CHARS);
    let windows: Vec<AnnouncedWindow> = (0..MAX_ANNOUNCED_WINDOWS)
        .map(|index| AnnouncedWindow {
            id: i64::MAX - i64::try_from(index).unwrap(),
            starts_at: i64::MAX,
            ends_at: i64::MAX,
            reason: reason.clone(),
            host: Some("h".repeat(64)),
        })
        .collect();
    let body = serde_json::to_vec(&crate::mesh::wire::HeartbeatBody {
        maintenance: windows,
    })
    .unwrap();
    assert!(body.len() <= MAX_ANNOUNCEMENT_BYTES, "{} bytes", body.len());
    // And not by a mile: the limit is derived, not a round number to grow into.
    assert!(MAX_ANNOUNCEMENT_BYTES < 2 * body.len());
    assert_eq!(MAX_ANNOUNCE_AHEAD_SECS, 7 * 86_400);
}

fn wanted(remote_id: i64, starts_at: i64, ends_at: i64) -> Wanted {
    Wanted {
        remote_id,
        starts_at,
        ends_at,
        selector: "peer".to_owned(),
        reason: "r".to_owned(),
    }
}

fn granted(remote_id: i64, starts_at: i64, ends_at: i64) -> MaintenanceWindow {
    MaintenanceWindow {
        id: remote_id,
        starts_at,
        ends_at,
        reason: "r".to_owned(),
        selector: "peer".to_owned(),
        origin: "peer:hora-frank".to_owned(),
        remote_id: Some(remote_id),
        created_at: starts_at,
    }
}

/// What weighs on a window announced now: what the peer was granted in the
/// 24 hours before it starts - not earlier, not the window's own stored row.
#[test]
fn the_budget_counts_the_day_before_a_window_and_nothing_else() {
    const HOUR: i64 = 3600;
    let budget = HOUR;
    let ends = |history: &[MaintenanceWindow], new: Wanted| {
        let mut refused = Vec::new();
        within_budget(vec![new], history, budget, NOW, &mut refused)
            .first()
            .map(|kept| kept.ends_at)
    };
    // Nothing granted before: the whole window.
    assert_eq!(ends(&[], wanted(1, NOW, NOW + 1800)), Some(NOW + 1800));
    // An hour used twelve hours ago: nothing left.
    let half_day = [granted(9, NOW - 13 * HOUR, NOW - 12 * HOUR)];
    assert_eq!(ends(&half_day, wanted(1, NOW, NOW + 1800)), None);
    // The same hour, thirty hours ago: outside the day, it weighs nothing.
    let long_ago = [granted(9, NOW - 31 * HOUR, NOW - 30 * HOUR)];
    assert_eq!(
        ends(&long_ago, wanted(1, NOW, NOW + 1800)),
        Some(NOW + 1800)
    );
    // Half of it inside the day: half an hour is left.
    let straddling = [granted(9, NOW - 24 * HOUR - 1800, NOW - 24 * HOUR + 1800)];
    assert_eq!(
        ends(&straddling, wanted(1, NOW, NOW + HOUR)),
        Some(NOW + 1800)
    );

    // The window's own stored row is not history: it is weighed afresh...
    let own = [granted(1, NOW - 1800, NOW + 1800)];
    assert_eq!(
        ends(&own, wanted(1, NOW - 1800, NOW + 1800)),
        Some(NOW + 1800)
    );
    // ...but the same id with another start is another window that ran (a
    // peer that moves its window forward every heartbeat), and so is
    // another id that started at the same second.
    let moved = [granted(1, NOW - HOUR, NOW)];
    assert_eq!(ends(&moved, wanted(1, NOW, NOW + 1800)), None);
    // Laid over an hour already granted, a window gets nothing for what
    // sticks out: the budget is spent, whoever's id spent it.
    let twin = [granted(2, NOW - HOUR, NOW)];
    assert_eq!(ends(&twin, wanted(1, NOW - HOUR, NOW + 1800)), None);
    let half = [granted(2, NOW - 1800, NOW)];
    assert_eq!(
        ends(&half, wanted(1, NOW - 1800, NOW + HOUR)),
        Some(NOW + 1800)
    );
    // A stored row counts as far as it ran: its future is given back.
    let withdrawn = [granted(9, NOW - 1800, NOW + 10 * HOUR)];
    assert_eq!(
        ends(&withdrawn, wanted(1, NOW, NOW + HOUR)),
        Some(NOW + 1800)
    );
}

/// Trust takes a watched peer that proves who it is, whatever validation
/// let through: take away any one of the three and it is gone.
#[test]
fn only_a_watched_authenticated_peer_is_trusted_with_windows() {
    let peer = trusting().peers[0].clone();
    assert_eq!(trusted_secs(&peer), Some(900));
    let mut unwatched = peer.clone();
    unwatched.expect_every_secs = None;
    assert_eq!(trusted_secs(&unwatched), None);
    let mut anonymous = peer.clone();
    anonymous.listen_token = None;
    assert_eq!(trusted_secs(&anonymous), None);
    let mut open = peer;
    open.allow_unauthenticated = true;
    assert_eq!(trusted_secs(&open), None);
}

#[test]
fn a_window_without_overlap_ends_where_its_allowance_does() {
    assert_eq!(end_within(&[], 100, 500, 50), 150);
    assert_eq!(end_within(&[], 100, 500, 400), 500);
    assert_eq!(end_within(&[], 100, 500, 401), 500);
    assert_eq!(end_within(&[(200, 300)], 100, 500, 150), 350);
    assert_eq!(measure(&[(0, 10), (10, 20), (30, 40)], 5, 35), 20);
    assert_eq!(merged(&[(10, 20), (0, 10), (15, 18)], 0, 100), [(0, 20)]);
}

#[tokio::test]
async fn the_limits_of_an_announcement_are_inclusive() {
    let store = Store::in_memory().await;
    let config = trusting();
    let peer = &config.peers[0];
    // Exactly a week ahead is accepted; a second more is not.
    let edge = NOW + MAX_ANNOUNCE_AHEAD_SECS;
    let applied = apply_announcement(
        &store,
        peer,
        true,
        &[announced(1, edge, edge + 60, None)],
        NOW,
    )
    .await
    .unwrap();
    assert_eq!((applied.accepted.len(), applied.refused.len()), (1, 0));
    // As many windows as a heartbeat may carry are all read; one more and
    // the rest is refused, said so.
    let many = |count: usize| -> Vec<AnnouncedWindow> {
        (0..count)
            .map(|index| {
                let start = NOW + 10 + i64::try_from(index).unwrap();
                announced(100 + i64::try_from(index).unwrap(), start, start + 1, None)
            })
            .collect()
    };
    let applied = apply_announcement(&store, peer, true, &many(MAX_ANNOUNCED_WINDOWS), NOW)
        .await
        .unwrap();
    assert!(
        !applied
            .refused
            .contains(&"more windows than a heartbeat may announce")
    );
    let applied = apply_announcement(&store, peer, true, &many(MAX_ANNOUNCED_WINDOWS + 1), NOW)
        .await
        .unwrap();
    assert!(
        applied
            .refused
            .contains(&"more windows than a heartbeat may announce")
    );

    // The same window with another reason is another window: replaced.
    let store = Store::in_memory().await;
    let first = announced(1, NOW + 60, NOW + 120, None);
    apply_announcement(&store, peer, true, std::slice::from_ref(&first), NOW)
        .await
        .unwrap();
    let reworded = AnnouncedWindow {
        reason: "disk swap".to_owned(),
        ..first
    };
    let applied = apply_announcement(&store, peer, true, &[reworded], NOW)
        .await
        .unwrap();
    assert_eq!((applied.withdrawn.len(), applied.accepted.len()), (1, 1));
    assert_eq!(applied.accepted[0].reason, "disk swap");
}

#[tokio::test]
async fn a_host_window_covers_that_host_and_is_announced_only_from_it() {
    let store = Store::in_memory().await;
    let config = trusting();
    // This node's own host: its one monitor there, and not the peer's watch.
    let own = plan(&store, &config, host_plan("yuki", "1800003600"), NOW)
        .await
        .unwrap();
    let resolved = resolve(&config, &own.window).expect("resolved");
    assert_eq!(resolved.monitors, ["local"]);
    assert_eq!(resolved.covered_peers(&config), [] as [&str; 0]);
    assert!(is_announced(&config, &own.window));
    // The peer's host: its monitors here and the watch of the peer on it,
    // which is not this node's to announce.
    let theirs = plan(&store, &config, host_plan("frank", "1800007200"), NOW)
        .await
        .unwrap();
    let resolved = resolve(&config, &theirs.window).expect("resolved");
    assert_eq!(resolved.monitors, ["mail", "web", "frank-in"]);
    assert_eq!(resolved.covered_peers(&config), ["Frank"]);
    assert!(!is_announced(&config, &theirs.window));
    // Without a peer to tell, nothing is announced.
    let alone = config_with("", "");
    let mut silent = alone.clone();
    silent.peers.clear();
    assert!(!is_announced(&silent, &own.window));

    let schedule = Schedule::merged(&config, &[own.window.clone(), theirs.window.clone()]);
    assert_eq!(schedule.windows().len(), 2);
}

#[test]
fn errors_say_what_went_wrong() {
    assert_eq!(
        PlanError::InvalidDuration.message(),
        crate::silence::INVALID_DURATION
    );
    assert_eq!(
        PlanError::InvalidDuration.to_string(),
        crate::silence::INVALID_DURATION
    );
    assert!(std::error::Error::source(&PlanError::InvalidDuration).is_none());
    let database = PlanError::Database(sqlx::Error::PoolClosed);
    assert!(std::error::Error::source(&database).is_some());
    assert_ne!(database.message(), "");
    assert_eq!(
        CancelError::NotFound.to_string(),
        "no such maintenance window"
    );
    assert!(
        CancelError::FromPeer("hora-frank".to_owned())
            .to_string()
            .contains("hora-frank")
    );
}

#[test]
fn overlapping_time_is_counted_once() {
    let muted = [(100, 200), (150, 300), (500, 600)];
    assert_eq!(measure(&muted, 0, 1000), 300);
    assert_eq!(measure(&muted, 250, 550), 100);
    // 100 s to spend from 0: the gap before the first span takes them
    // all, and the span itself, muted already, comes free.
    assert_eq!(end_within(&muted, 0, 1000, 100), 300);
    assert_eq!(end_within(&muted, 0, 1000, 99), 99);
    // 150: the gap (100), free through the spans to 300, then 50 more.
    assert_eq!(end_within(&muted, 0, 1000, 150), 350);
    // Enough for everything.
    assert_eq!(end_within(&muted, 0, 1000, 10_000), 1000);
    // Nothing to spend: only time already muted is left.
    assert_eq!(end_within(&muted, 100, 400, 0), 300);
    assert_eq!(end_within(&muted, 0, 400, 0), 0);
    assert_eq!(end_within(&muted, 0, 400, -5), 0);
}

#[test]
fn a_repeated_refusal_is_logged_once_an_hour() {
    let why = "a test refusal";
    assert!(refusal_due("peer-log-test", why, 1_000));
    // The same announcement comes back with every heartbeat.
    assert!(!refusal_due("peer-log-test", why, 1_060));
    assert!(!refusal_due("peer-log-test", why, 4_599));
    assert!(refusal_due("peer-log-test", why, 4_600));
    // Another peer, or another reason, has its own line.
    assert!(refusal_due("peer-log-other", why, 1_060));
    assert!(refusal_due("peer-log-test", "another test refusal", 1_060));
}

#[test]
fn selectors_and_starts_parse_one_way() {
    for selector in [
        Selector::All,
        Selector::Peer,
        Selector::Ids(vec!["api".to_owned(), "web".to_owned()]),
        Selector::Host("frank".to_owned()),
        Selector::Host("rack 4: left".to_owned()),
    ] {
        assert_eq!(Selector::parse(&selector.to_string()), Some(selector));
    }
    assert_eq!(Selector::parse("everything"), None);
    assert_eq!(parse_start("1800000000"), Some(NOW));
    assert_eq!(parse_start("2027-01-15T08:00:00Z"), Some(NOW));
    assert_eq!(parse_start("2027-01-15T09:00:00+01:00"), Some(NOW));
    for bad in ["", "05:00", "-5", "tomorrow", "2027-01-15"] {
        assert_eq!(parse_start(bad), None, "{bad}");
    }
}
