use hora_core::db::DayRow;
use hora_core::status::{CheckStatus, MonitorState};

use super::monitor::{format_slo_pct, topology_context};
use super::status::{
    CertState, SloState, build_bar, cert_days_left, cert_label, cert_state_for, day_cell, slo_state,
};
use super::*;

use crate::visibility::Audience;
fn check(status: CheckStatus) -> Latest {
    Latest {
        time: 0,
        latency_ms: None,
        status,
        error: None,
        reason: None,
    }
}
#[test]
fn derive_status_confirms_down_only_after_threshold() {
    let down = CheckStatus::Down;
    assert_eq!(
        db::derive_status(&[check(down), check(down), check(down)], 3),
        MonitorState::Down
    );
    assert_eq!(db::derive_status(&[check(down)], 3), MonitorState::Degraded);
    assert_eq!(db::derive_status(&[], 3), MonitorState::Unknown);
    assert_eq!(
        db::derive_status(&[check(CheckStatus::Up)], 3),
        MonitorState::Up
    );
}

#[test]
fn cert_state_thresholds() {
    const DAY: i64 = 86_400;
    assert_eq!(cert_state_for(None, 14), CertState::None);
    assert_eq!(cert_state_for(Some(-1), 14), CertState::Expired);
    assert_eq!(cert_state_for(Some(10 * DAY), 14), CertState::Warn);
    assert_eq!(cert_state_for(Some(40 * DAY), 14), CertState::Ok);
}

/// 23 hours left and 23 hours gone both truncated to 0 days, and 0 read as
/// expired: the page said "Expired: browsers refuse it" of a certificate
/// browsers still accepted, and the metric could not tell the two apart.
#[test]
fn a_certificate_in_its_last_day_is_not_expired() {
    const HOUR: i64 = 3600;
    assert_eq!(cert_state_for(Some(23 * HOUR), 14), CertState::Warn);
    assert_eq!(cert_state_for(Some(0), 14), CertState::Warn);
    assert_eq!(cert_state_for(Some(-23 * HOUR), 14), CertState::Expired);

    // The day figure keeps the side: positive while valid, negative after.
    assert_eq!(cert_days_left(23 * HOUR), 1);
    assert_eq!(cert_days_left(1), 1);
    assert_eq!(cert_days_left(-1), -1);
    assert_eq!(cert_days_left(-23 * HOUR), -1);
    assert_eq!(cert_days_left(-25 * HOUR), -2);
    // Whole days are unchanged above one.
    assert_eq!(cert_days_left(40 * 24 * HOUR + 5), 40);
    assert_eq!(cert_days_left(47 * HOUR), 1);

    assert_eq!(cert_label(40 * 24 * HOUR), "40 d");
    assert_eq!(cert_label(23 * HOUR + 59), "23 h");
    assert_eq!(cert_label(12 * 60), "12 min");
    assert_eq!(cert_label(-1), "expired");
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
fn day_cell_reds_only_real_outages() {
    let row = |up, down| DayRow {
        day: "2021-01-01".to_owned(),
        up,
        down,
        degraded: 0,
    };
    assert_eq!(day_cell("d".to_owned(), &row(100, 0)).state, "up"); // 100%
    assert_eq!(day_cell("d".to_owned(), &row(1439, 1)).state, "degraded"); // ~99.9% blip
    assert_eq!(day_cell("d".to_owned(), &row(1400, 40)).state, "degraded"); // ~97% - mostly up
    assert_eq!(day_cell("d".to_owned(), &row(800, 640)).state, "down"); // ~56% - real outage
}
#[test]
fn slo_state_compares_p95() {
    assert_eq!(slo_state(Some(200), Some(150)), SloState::Met);
    assert_eq!(slo_state(Some(200), Some(250)), SloState::Breached);
    assert_eq!(slo_state(None, Some(150)), SloState::None);
    assert_eq!(slo_state(Some(200), None), SloState::None);
}

#[test]
fn topology_context_hides_private_names_from_public_view() {
    let config = hora_core::config::parse(
        r#"
            [page]
            [server]
            auth_token = "seekrit-long-token"
            [[monitors]]
            id = "db"
            name = "Internal DB"
            target = "https://db.internal"
            interval_secs = 60
            public = false
            [[monitors]]
            id = "edge"
            name = "Edge"
            target = "https://example.com"
            interval_secs = 60
            depends_on = ["db"]
            [[monitors]]
            id = "worker"
            name = "Worker"
            target = "https://worker.internal"
            interval_secs = 60
            public = false
            depends_on = ["db"]
        "#,
    )
    .expect("config");
    // Everything is down.
    let statuses: HashMap<String, MonitorState> = ["db", "edge", "worker"]
        .into_iter()
        .map(|id| (id.to_owned(), MonitorState::Down))
        .collect();

    let operator = Audience::Operator;
    let operator = Visibility::new(&config, &operator);
    let public = Audience::Public;
    let public = Visibility::new(&config, &public);
    // A group token holder is no operator: the private DB and worker
    // belong to no group of theirs.
    let group = Audience::Group("App".to_owned());
    let group = Visibility::new(&config, &group);

    // Operator: the private upstream is named as the cause.
    let edge = &config.monitors[1];
    let (cause, _) = topology_context(edge, &statuses, &config.monitors, &operator);
    assert_eq!(cause.as_deref(), Some("Internal DB"));
    // Public or another group: a private monitor's name never leaves
    // through a cause.
    for vis in [&public, &group] {
        let (cause, _) = topology_context(edge, &statuses, &config.monitors, vis);
        assert_eq!(cause, None);
    }

    // Impacted lists drop private dependents outside the operator view.
    let db = &config.monitors[0];
    let (_, impacted) = topology_context(db, &statuses, &config.monitors, &operator);
    assert_eq!(impacted.len(), 2, "{impacted:?}");
    for vis in [&public, &group] {
        let (_, impacted) = topology_context(db, &statuses, &config.monitors, vis);
        assert_eq!(impacted, vec!["Edge".to_owned()]);
    }
}

#[test]
fn budget_durations_and_pct_format() {
    assert_eq!(format_slo_pct(9990), "99.9");
    assert_eq!(format_slo_pct(9995), "99.95");
    assert_eq!(format_slo_pct(9900), "99");
}

/// Three monitors - `ok` (up), `bad` (degraded, a detailed reason) and
/// `hidden` (private) - with one check each, and their config.
async fn three_monitors() -> (hora_core::db::Store, hora_core::config::Config, i64) {
    let store = hora_core::db::Store::in_memory().await;
    let config = hora_core::config::parse(
        r#"
            [page]
            [server]
            auth_token = "seekrit-long-token"
            [[monitors]]
            id = "ok"
            name = "Fine"
            target = "https://ok.example.com"
            interval_secs = 60
            [[monitors]]
            id = "bad"
            name = "Failing"
            target = "https://bad.example.com"
            interval_secs = 60
            [[monitors]]
            id = "hidden"
            name = "Hidden"
            target = "https://hidden.example.com"
            interval_secs = 60
            public = false
        "#,
    )
    .unwrap();
    let now = Utc::now().timestamp();
    for (id, status, error) in [
        ("ok", 1, None),
        ("bad", 2, Some("HTTP 500: stack trace")),
        ("hidden", 1, None),
    ] {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, latency_ms, error) VALUES (?, ?, ?, 40, ?)",
        )
        .bind(now - 60)
        .bind(id)
        .bind(status)
        .bind(error)
        .execute(store.fixture_pool())
        .await
        .unwrap();
    }
    (store, config, now)
}

#[tokio::test]
async fn derived_views_share_unchanged_cards_and_redact_the_rest() {
    let (store, config, _) = three_monitors().await;
    let built = build_summary(
        &store,
        &config,
        &mut BuildState::default(),
        &[],
        &HashMap::new(),
    )
    .await;
    assert_eq!(built.summary.monitors.len(), 3);

    let audience = Audience::Public;
    let public = derive(&built, &config, &Visibility::new(&config, &audience));
    let ids: Vec<&str> = public.monitors.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["ok", "bad"]);
    // The healthy card is the operator's own allocation...
    assert!(Arc::ptr_eq(&public.monitors[0], &built.summary.monitors[0]));
    // ...the failing one a copy with the safe reason, sharing the heavy parts.
    let (theirs, ours) = (&public.monitors[1], &built.summary.monitors[1]);
    assert_eq!(theirs.last_error.as_deref(), Some("HTTP 500"));
    assert_eq!(ours.last_error.as_deref(), Some("HTTP 500: stack trace"));
    assert!(Arc::ptr_eq(&theirs.spark, &ours.spark));
    assert!(std::ptr::eq(theirs.bar.as_ptr(), ours.bar.as_ptr()));
    assert_eq!(public.groups.len(), 1);
    assert_eq!(public.groups[0].ids, ["ok", "bad"]);
    assert_eq!(public.overall, Overall::Degraded);
}

#[tokio::test]
async fn each_audience_lists_the_incidents_it_may_see() {
    let (store, config, now) = three_monitors().await;
    // Two finished incidents: the hidden monitor's, a day ago, and the
    // public one's, three days ago (with a detailed reason).
    for (id, ended, error) in [
        ("ok", now - 3 * 86_400, "HTTP 503: upstream db-7 refused"),
        ("hidden", now - 86_400, "timeout"),
    ] {
        sqlx::query(
            "INSERT INTO incidents (monitor_id, started_at, ended_at, duration_s, error, created_at) \
             VALUES (?, ?, ?, 600, ?, ?)",
        )
        .bind(id)
        .bind(ended - 600)
        .bind(ended)
        .bind(error)
        .bind(ended - 600)
        .execute(store.fixture_pool())
        .await
        .unwrap();
    }
    let built = build_summary(
        &store,
        &config,
        &mut BuildState::default(),
        &[],
        &HashMap::new(),
    )
    .await;
    // The operator sees both incidents, newest first, and the last day.
    let titles: Vec<&str> = built
        .summary
        .recent
        .iter()
        .map(|i| i.title.as_str())
        .collect();
    assert_eq!(
        titles,
        ["Hidden was down for 10 min", "Fine was down for 10 min"]
    );
    assert_eq!(built.summary.quiet_days, Some(1));

    let audience = Audience::Public;
    let public = derive(&built, &config, &Visibility::new(&config, &audience));
    // The public list leaves the private monitor's incident out, the reason
    // reduced to its safe category; its quiet days count from its own.
    assert_eq!(public.recent.len(), 1);
    assert_eq!(public.recent[0].title, "Fine was down for 10 min");
    assert_eq!(
        public.recent[0].note.as_deref(),
        Some("Last answer: HTTP 503.")
    );
    assert_eq!(public.quiet_days, Some(3));
    // No event marker reaches a derived view.
    assert!(public.events.is_empty());
}

#[test]
fn maintenance_banners_name_only_what_the_audience_may_see() {
    let config = hora_core::config::parse(
        r#"
        [page]
        [server]
        auth_token = "0123456789abcdef"
        [[monitors]]
        id = "site"
        name = "Site"
        target = "https://example.com"
        interval_secs = 60
        [[monitors]]
        id = "vault"
        name = "Vault"
        target = "https://vault.internal"
        interval_secs = 60
        public = false
        [[maintenance]]
        title = "Shared work"
        start = "2026-01-01T00:00:00Z"
        end = "2026-01-02T00:00:00Z"
        monitors = ["site", "vault"]
        [[maintenance]]
        title = "Vault rekey"
        start = "2026-01-01T00:00:00Z"
        end = "2026-01-02T00:00:00Z"
        monitors = ["vault"]
        "#,
    )
    .expect("config");
    let now = "2026-01-01T12:00:00Z".parse().expect("time");

    let schedule = hora_core::planned::Schedule::merged(&config, &[]);

    // The operator's view: both windows, every monitor named.
    let operator = Audience::Operator;
    let visibility = Visibility::new(&config, &operator);
    let (all, _) = build_maintenances(&schedule, &config, now, &visibility);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].monitors, "Site, Vault");

    // The public's: the private monitor is not named in the shared window,
    // and the window that covers nothing else is not announced at all.
    let public = Audience::Public;
    let visibility = Visibility::new(&config, &public);
    let (seen, _) = build_maintenances(&schedule, &config, now, &visibility);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].reason, "Shared work");
    assert_eq!(seen[0].monitors, "Site");
}

/// Windows planned at runtime reach each audience like the config's: the
/// reason and the hours to whoever sees a covered monitor, the names of the
/// monitors it may see, and the host and the announcing peer to the
/// operator alone.
/// Node `yuki` with a trusted peer on host `frank`, a public and a private
/// monitor there (group `Web`) and a blog elsewhere (group `Other`); the
/// peer's announced reboot under way, and two windows planned here for
/// later: one for the private monitor alone, then one for the public one.
async fn planned_fixture() -> (Store, Config, i64) {
    let config = hora_core::config::parse(
        r#"
        [page]
        [server]
        auth_token = "0123456789abcdef"
        [health]
        id = "hora-yuki"
        [[peers]]
        id = "hora-frank"
        name = "Frank node"
        listen_id = "frank-in"
        expect_every_secs = 120
        listen_token = "a-long-enough-listen-token"
        host = "frank"
        accept_maintenance_secs = 900
        [[monitors]]
        id = "site"
        name = "Site"
        group = "Web"
        target = "https://example.com"
        interval_secs = 60
        host = "frank"
        [[monitors]]
        id = "vault"
        name = "Vault"
        group = "Web"
        target = "https://vault.internal"
        interval_secs = 60
        public = false
        host = "frank"
        [[monitors]]
        id = "blog"
        name = "Blog"
        group = "Other"
        target = "https://blog.example.com"
        interval_secs = 60
        "#,
    )
    .expect("config");
    let store = Store::in_memory().await;
    let now = Utc::now().timestamp();
    // Under way: the peer announced the reboot of its host.
    hora_core::planned::apply_announcement(
        &store,
        &config.peers[0],
        true,
        &[hora_core::mesh::wire::AnnouncedWindow {
            id: 1,
            starts_at: now - 60,
            ends_at: now + 540,
            reason: "kernel reboot".to_owned(),
            host: Some("frank".to_owned()),
        }],
        now,
    )
    .await
    .unwrap();
    // To come, planned here: one for the private monitor alone, a later one
    // for the public one.
    for (monitors, offset, reason) in [("vault", 3600, "vault rekey"), ("site", 7200, "new disks")]
    {
        hora_core::planned::plan(
            &store,
            &config,
            hora_core::planned::Plan {
                monitors: Some(monitors),
                starts_at: Some(&(now + offset).to_string()),
                duration: "10m",
                reason,
                ..hora_core::planned::Plan::default()
            },
            now,
        )
        .await
        .unwrap();
    }
    (store, config, now)
}

#[tokio::test]
async fn planned_windows_reach_each_audience_without_naming_what_it_may_not_see() {
    let (store, config, now) = planned_fixture().await;
    let built = build_summary(
        &store,
        &config,
        &mut BuildState::default(),
        &[],
        &HashMap::new(),
    )
    .await;

    // The operator: everything, with where it came from.
    let operator = &built.summary;
    assert_eq!(operator.maintenances.len(), 1);
    assert_eq!(operator.maintenances[0].reason, "kernel reboot");
    assert_eq!(operator.maintenances[0].monitors, "Site, Vault");
    assert_eq!(
        operator.maintenances[0].note.as_deref(),
        Some("Host frank, mutes the watch of Frank node, announced by Frank node.")
    );
    let card = |summary: &Summary, id: &str| {
        summary
            .monitors
            .iter()
            .find(|view| view.id == id)
            .and_then(|view| view.maintenance.clone())
    };
    assert_eq!(card(operator, "site").as_deref(), Some("kernel reboot"));
    assert_eq!(card(operator, "vault").as_deref(), Some("kernel reboot"));
    assert_eq!(card(operator, "blog"), None, "no host label, never covered");
    let reasons = |views: &[MaintenanceView]| {
        views
            .iter()
            .map(|view| view.reason.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(reasons(&operator.upcoming), ["vault rekey", "new disks"]);

    // The public: the window, its reason and its hours - the private
    // monitor is not named, nor the host, nor the peer.
    let audience = Audience::Public;
    let public = derive(&built, &config, &Visibility::new(&config, &audience));
    assert_eq!(public.maintenances.len(), 1);
    assert_eq!(public.maintenances[0].reason, "kernel reboot");
    assert_eq!(public.maintenances[0].monitors, "Site");
    assert_eq!(public.maintenances[0].ids, ["site"]);
    assert_eq!(public.maintenances[0].note, None);
    assert_eq!(public.maintenances[0].end.timestamp(), now + 540);
    let json = serde_json::to_string(&public.maintenances).unwrap();
    assert!(!json.contains("Vault") && !json.contains("frank"), "{json}");
    assert_eq!(card(&public, "site").as_deref(), Some("kernel reboot"));
    // The window that covers a private monitor only is not announced ahead.
    assert_eq!(reasons(&public.upcoming), ["new disks"]);
    assert!(public.upcoming.iter().all(|view| view.note.is_none()));

    // A group's page: the windows touching the group, and no other.
    let web = for_group(&public, &config, "Web").expect("group");
    assert_eq!(reasons(&web.maintenances), ["kernel reboot"]);
    assert_eq!(reasons(&web.upcoming), ["new disks"]);
    let other = for_group(&public, &config, "Other").expect("group");
    assert!(other.maintenances.is_empty() && other.upcoming.is_empty());

    // On the page: the hero is the window, and the next one is announced.
    let ask = crate::status_page::Ask {
        operator: false,
        update: None,
        token_given: false,
        group_page: false,
    };
    let page = crate::status_page::build(&public, &config, &ask);
    let next = page.next.expect("the next window");
    assert!(next.body.ends_with(": new disks."), "{}", next.body);
    assert_eq!(next.when, None);
    let ask = crate::status_page::Ask {
        operator: true,
        ..ask
    };
    let page = crate::status_page::build(operator, &config, &ask);
    let next = page.next.expect("the next window");
    assert!(next.body.ends_with(": vault rekey."), "{}", next.body);
}
