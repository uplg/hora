//! Mutual surveillance: the outbound dead-man heartbeat, the peer watches, and
//! the quorum check that distinguishes a real peer outage from a network
//! partition.
//!
//! The wire is deliberately plain HTTP: a heartbeat is a `POST` to a URL (a
//! peer's `/api/push/{id}`, or any external receiver), and a witness query is a
//! `GET /healthz` whose JSON body carries this node's view of its peers. Nothing
//! here is a bespoke protocol, so any half-channel can terminate at an external
//! service (healthchecks.io, `UptimeRobot`) instead of another Hora.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::db::Store;
use futures_util::future::join_all;
use hora_notify::Event;
use reqwest::Client;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use super::wire::{HealthReport, HealthStatus, HeartbeatBody, PeerSeen, PeerState};
use crate::config::{Config, Health, Peer};
use crate::db;
use crate::heartbeat::{Cadence, heartbeat_expected_since, heartbeat_outcome_for};
use crate::notifications::Notifiers;
use crate::scheduler::{AlertCell, AlertLevel, AlertState, muted};

/// Floor on the scheduler-liveness tolerance, so a node whose fastest monitor
/// ticks very frequently is not flagged unhealthy by a momentary scheduling jitter.
const MIN_LIVENESS_TOLERANCE: i64 = 30;

/// How long a witness `/healthz` poll may take before it is treated as unreachable.
const WITNESS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an outbound heartbeat `POST` may take before it is abandoned.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

/// Hard deadline for one whole round of outbound heartbeats (they are sent
/// concurrently, so this is one slow receiver's worth plus slack): a dead
/// receiver can never delay the dead-man cadence to the others.
const HEARTBEAT_ROUND_DEADLINE: Duration = Duration::from_secs(12);

/// A witness `/healthz` report is a small JSON object; cap the body so a
/// compromised peer can't stream hundreds of MB into memory within the timeout.
const MAX_REPORT_BYTES: usize = 64 * 1024;

// --- /healthz report ------------------------------------------------------

/// Build the `/healthz` report for the current configuration and scheduler state.
pub async fn report(store: &Store, config: &Config, last_tick: &AtomicU64) -> HealthReport {
    let now = chrono::Utc::now().timestamp();
    let (scheduler_ok, last_tick_age) = scheduler_liveness(config, last_tick, now);
    let db_ok = db_ok(store).await;

    let mut peers = HashMap::new();
    for peer in config.peers.iter().filter(|peer| peer.is_watched()) {
        peers.insert(peer.id.clone(), peer_seen(store, peer, now).await);
    }

    let probes_ok = !crate::probe::local::recent(now);

    HealthReport {
        status: if scheduler_ok && db_ok && probes_ok {
            HealthStatus::Ok
        } else {
            HealthStatus::Degraded
        },
        scheduler_ok,
        db_ok,
        last_tick_age,
        probes_ok,
        id: config.health.as_ref().map(|health| health.id.clone()),
        peers,
    }
}

/// Whether the scheduler loop is demonstrably alive, plus the age of the last
/// tick. The liveness beacon (`last_tick`) advances at the cadence of the
/// *fastest* monitor (it is a `fetch_max` across all of them), so the tolerance is
/// derived from the smallest interval - not the largest, which would let a slow
/// daily monitor mask a wedged scheduler for hours. A node with no monitors has
/// nothing to go stale, so it is trivially alive.
pub(crate) fn scheduler_liveness(config: &Config, last_tick: &AtomicU64, now: i64) -> (bool, i64) {
    let last = last_tick.load(Ordering::Relaxed);
    if last == 0 {
        // Never ticked: at startup, "alive" only once there is nothing to warm up.
        return (config.monitors.is_empty(), -1);
    }
    let age = (now - i64::try_from(last).unwrap_or(i64::MAX)).max(0);
    let Some(min_interval) = config
        .monitors
        .iter()
        .map(|monitor| monitor.interval_secs)
        .min()
    else {
        return (true, age);
    };
    let tolerance = i64::try_from(min_interval.saturating_mul(2))
        .unwrap_or(i64::MAX)
        .max(MIN_LIVENESS_TOLERANCE);
    (age <= tolerance, age)
}

/// Whether the database answers a trivial query within a short timeout. A wedged
/// or locked database makes this false, which (with the scheduler check) stops the
/// outbound heartbeat - so the dead-man fires rather than a zombie pinging "ok".
pub(crate) async fn db_ok(store: &Store) -> bool {
    matches!(
        tokio::time::timeout(Duration::from_secs(2), store.ping()).await,
        Ok(Ok(()))
    )
}

/// This node's view of one peer, measured from its last *positive* heartbeat
/// (recorded misses are ignored, see [`db::last_heartbeat_time`]): `up` within the
/// expected interval, `down` once that lapses, `unknown` if never seen. `age` is
/// the seconds since that last real heartbeat, so it stays meaningful through an
/// outage instead of resetting on each recorded miss.
async fn peer_seen(store: &Store, peer: &Peer, now: i64) -> PeerSeen {
    let expect = i64::try_from(peer.expect_every_secs.unwrap_or(0)).unwrap_or(i64::MAX);
    match db::last_heartbeat_time(store, peer.listen_id()).await {
        Ok(Some(last)) => {
            let age = (now - last).max(0);
            PeerSeen {
                state: if now - last > expect {
                    PeerState::Down
                } else {
                    PeerState::Up
                },
                age,
            }
        }
        _ => PeerSeen {
            state: PeerState::Unknown,
            age: -1,
        },
    }
}

// --- Outbound heartbeat (the dead-man) ------------------------------------

/// Whether at least one notification channel still delivers: false only when
/// there are channels and every one of them has reached the watchdog's
/// failure threshold. A node without any channel has nothing to deliver
/// through, by choice: that is not a failure to report.
fn alerts_can_leave(channels: &[hora_notify::ChannelHealthEntry], threshold: u32) -> bool {
    channels.is_empty()
        || channels
            .iter()
            .any(|channel| channel.health.consecutive_failures < threshold.max(1))
}

/// Spawn the outbound heartbeat: every `health.interval_secs`, when this node is
/// locally healthy, `POST` a heartbeat to each peer's `ping_url` and to the
/// optional `heartbeat_url`. When unhealthy it stays silent - that silence is the
/// down signal the receiver detects. A shutdown signal stops it between ticks.
///
/// Unhealthy includes "no alert can get out": a node whose every notification
/// channel is failing still probes and still writes its history, but it can no
/// longer tell anyone about an outage - exactly the state a dead-man exists to
/// reveal. Its silence makes the peer (or the external watchdog) say so.
#[must_use]
pub fn spawn_heartbeat(
    config: watch::Receiver<Arc<Config>>,
    store: Store,
    client: Client,
    notifier: Notifiers,
    last_tick: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // What the last heartbeat announced, sent again if the windows
        // cannot be read: to the peers a heartbeat without a list means
        // "nothing planned", and one failed read must not withdraw a window.
        let mut announced = HeartbeatBody::default();
        loop {
            // Re-read the interval each cycle so a changed `interval_secs` (or a
            // newly added/removed `[health]`) applies live without a restart.
            let interval = config
                .borrow()
                .health
                .as_ref()
                .map_or_else(|| Duration::from_mins(1), Health::interval);
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                _ = shutdown.changed() => break,
            }
            let snapshot = config.borrow().clone();
            let Some(health) = snapshot.health.as_ref() else {
                continue;
            };

            let now = chrono::Utc::now().timestamp();
            let (scheduler_ok, age) = scheduler_liveness(&snapshot, &last_tick, now);
            let db_ok = db_ok(&store).await;
            if !(scheduler_ok && db_ok) {
                warn!(scheduler_ok, db_ok, "heartbeat skipped: node unhealthy");
                continue;
            }
            let channels = notifier.load().health_snapshot();
            if !alerts_can_leave(&channels, snapshot.alerts.channel_fail_threshold) {
                warn!(
                    channels = channels.len(),
                    "heartbeat skipped: every notification channel is failing, no alert can get out"
                );
                continue;
            }

            let digest = format!("ok mon={} tick={}s", snapshot.monitors.len(), age.max(0));
            match crate::planned::announcements(&store, &snapshot, now).await {
                Ok(maintenance) => announced = HeartbeatBody { maintenance },
                Err(err) => warn!("cannot read the planned maintenance windows: {err:#}"),
            }
            // The peers are told what is planned; the extra dead-man target
            // is not a Hora and gets the bare ping it always got.
            let body = (!announced.maintenance.is_empty()).then_some(&announced);
            let targets = snapshot
                .peers
                .iter()
                .filter_map(|peer| {
                    peer.ping_url.as_ref().map(|url| {
                        (
                            url.as_ref(),
                            peer.ping_token.as_ref().map(AsRef::as_ref),
                            body,
                        )
                    })
                })
                .chain(
                    health
                        .heartbeat_url
                        .as_ref()
                        .map(|url| (url.as_ref(), None, None)),
                );
            // Concurrently, under one deadline: sequential 10 s timeouts would
            // let a single dead receiver push every other heartbeat late.
            let round = join_all(
                targets
                    .map(|(url, token, body)| send_heartbeat(&client, url, token, &digest, body)),
            );
            if tokio::time::timeout(HEARTBEAT_ROUND_DEADLINE, round)
                .await
                .is_err()
            {
                warn!("heartbeat round abandoned past its deadline");
            }
        }
    })
}

/// Fire one heartbeat: `POST {url}?status=up&msg={digest}` with the token (if any)
/// in the `X-Push-Token` header, so it stays out of the receiver's access logs.
/// Fire-and-forget: a failure is logged (without the URL, which may carry
/// credentials) and the next tick tries again.
///
/// `body` carries the planned maintenance windows, as JSON, when there are
/// any: a receiver that does not know about them (Hora 0.11, an external
/// dead-man service) reads the query string and never the body, so the
/// heartbeat counts for it exactly as before.
async fn send_heartbeat(
    client: &Client,
    url: &str,
    token: Option<&str>,
    digest: &str,
    body: Option<&HeartbeatBody>,
) {
    let mut request = client
        .post(url)
        .query(&[("status", "up"), ("msg", digest)])
        .timeout(HEARTBEAT_TIMEOUT);
    if let Some(body) = body {
        request = request.json(body);
    }
    if let Some(token) = token {
        request = request.header("x-push-token", token);
    }
    match request.send().await {
        Ok(response) if response.status().is_success() => {}
        Ok(response) => warn!(status = %response.status(), "heartbeat ping rejected"),
        // The error Display embeds the URL (possibly with credentials), so omit it.
        Err(_err) => warn!("heartbeat ping failed (network error)"),
    }
}

// --- Peer watches (the inbound side, with quorum) -------------------------

/// The quorum verdict for a peer this node can no longer reach.
enum Verdict {
    /// No witness vouches for it (or none to consult): a real outage.
    Confirmed,
    /// A witness still sees it up: a partition on the local-to-peer link.
    Partition(String),
    /// No witness reachable at all: this node is likely the isolated one.
    Isolated,
}

/// Spawn the watch task for one peer (the IN side). The supervisor owns these and
/// reconciles them on config reload, so adding, removing or changing a peer takes
/// effect without a restart (the captured `peer` is replaced by a fresh task,
/// which picks up the previous one's alert state from `alert_state`).
#[must_use]
pub(crate) fn spawn_watch(
    peer: Peer,
    config: watch::Receiver<Arc<Config>>,
    store: Store,
    client: Client,
    notifier: Notifiers,
    alert_state: AlertCell,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `is_watched()` guarantees `expect_every_secs` is set.
        let expect = peer.expect_every_secs.unwrap_or(60);
        let cadence = Cadence::Every(expect);
        // When this watch started: heartbeats within `grace_secs` of it are not
        // alerted, so a node whose persisted history looks instantly stale after a
        // restart (e.g. both peers rebooting together) doesn't fire a false down.
        let started = chrono::Utc::now().timestamp();
        // A peer that never pinged at all is judged from when its first
        // heartbeat was expected (persisted across restarts), not left unknown.
        let expected_since = heartbeat_expected_since(&store, peer.listen_id(), started).await;
        let mut ticker = tokio::time::interval(Duration::from_secs(expect));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut state = alert_state.get().unwrap_or_default();

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            let now = chrono::Utc::now();
            // `None` = never pinged and not yet late (or a read error): nothing
            // to react to.
            let Some(outcome) = heartbeat_outcome_for(
                &store,
                peer.listen_id(),
                &cadence,
                expected_since,
                // A peer's watch ticks at its own heartbeat cadence already.
                0,
                now.timestamp(),
            )
            .await
            else {
                continue;
            };

            // Muted by a maintenance window (the config's, one planned here,
            // or one this very peer announced before going down) or an ad-hoc
            // silence, exactly like a monitor: the miss is still recorded,
            // only alerts are skipped. Nothing is counted meanwhile, so a peer
            // still silent when the window ends is confirmed and announced
            // from there, as a down that just began.
            if muted(&store, &config, peer.listen_id()).await {
                continue;
            }
            let snapshot = config.borrow().clone();
            let threshold = snapshot.alerts.fail_threshold.max(1);

            if outcome.is_up() {
                let previous = state.level;
                if state.observe_up() {
                    // Only announce recovery if we actually alerted (Isolated was silent).
                    if matches!(previous, AlertLevel::Down | AlertLevel::Partition) {
                        info!(peer = %peer.id, "peer recovered");
                        dispatch(
                            &notifier,
                            &peer,
                            Event::Recovered {
                                monitor: &peer.name,
                                local_only: false,
                            },
                        )
                        .await;
                    }
                    state.level = AlertLevel::Healthy;
                }
                alert_state.set(state);
                continue;
            }

            let confirmed = state.observe_down(threshold);
            alert_state.set(state);
            // Hold off during the post-startup grace window (see `started`).
            let grace = i64::try_from(
                snapshot
                    .health
                    .as_ref()
                    .map_or(0, |health| health.grace_secs),
            )
            .unwrap_or(0);
            let in_grace = now.timestamp() - started < grace;
            if !confirmed || in_grace {
                continue;
            }

            // Confirmed down by the local view; consult witnesses before alerting if
            // quorum is on. Re-evaluated every tick until it resolves (so a partition
            // that becomes a real outage escalates).
            let verdict = if snapshot.health.as_ref().is_some_and(|health| health.quorum) {
                confirm_down(&client, &snapshot, &peer).await
            } else {
                Verdict::Confirmed
            };
            announce_verdict(&notifier, &peer, &outcome, verdict, &mut state).await;
            alert_state.set(state);
        }
    })
}

/// Act on the quorum verdict for a peer confirmed down locally, moving `state`
/// to the level announced. A suppressed state (partition, isolation) is
/// announced once, not on every tick it persists.
async fn announce_verdict(
    notifier: &Notifiers,
    peer: &Peer,
    outcome: &crate::probe::Outcome,
    verdict: Verdict,
    state: &mut AlertState,
) {
    match verdict {
        Verdict::Confirmed => {
            error!(peer = %peer.id, "peer down");
            dispatch(
                notifier,
                peer,
                Event::Down {
                    monitor: &peer.name,
                    error: outcome.error.as_deref(),
                    cause: None,
                    impacted: &[],
                    vantage: None,
                    event: None,
                    local_only: false,
                },
            )
            .await;
            state.level = AlertLevel::Down;
        }
        Verdict::Partition(witness) if state.level != AlertLevel::Partition => {
            warn!(peer = %peer.id, %witness, "peer unreachable but a witness sees it up: treating as a partition, not an outage");
            dispatch(
                notifier,
                peer,
                Event::PeerLinkDegraded {
                    peer: &peer.name,
                    witness: &witness,
                },
            )
            .await;
            state.level = AlertLevel::Partition;
        }
        Verdict::Isolated if state.level != AlertLevel::Isolated => {
            warn!(peer = %peer.id, "peer down but no witness reachable: possible local isolation, not alerting");
            state.level = AlertLevel::Isolated;
        }
        // Already in this suppressed state: nothing to re-announce.
        Verdict::Partition(_) | Verdict::Isolated => {}
    }
}

/// Ask the other peers whether they still see `target`. Witnesses are the peers
/// (other than the target) with a resolvable `/healthz`: if any reports the target
/// up, this is a partition; if none is reachable, this node is likely isolated;
/// otherwise the outage is confirmed. With no witnesses to consult it is confirmed
/// (the honest two-node behaviour).
async fn confirm_down(client: &Client, config: &Config, target: &Peer) -> Verdict {
    let witnesses: Vec<(String, String)> = config
        .peers
        .iter()
        .filter(|peer| peer.id != target.id)
        .filter_map(|peer| {
            peer.effective_witness_url()
                .map(|url| (peer.name.clone(), url))
        })
        .collect();
    if witnesses.is_empty() {
        return Verdict::Confirmed;
    }

    // Poll every witness concurrently, so one slow node can't delay the verdict.
    let polls = witnesses
        .iter()
        .map(|(name, url)| async move { (name.clone(), fetch_witness(client, url).await) });
    let results = join_all(polls).await;

    let mut reachable = 0u32;
    let mut partition = None;
    for (name, report) in results {
        if let Some(seen) = report {
            reachable += 1;
            if partition.is_none()
                && seen
                    .peers
                    .get(&target.id)
                    .is_some_and(|view| view.state == PeerState::Up)
            {
                partition = Some(name);
            }
        }
    }
    if let Some(name) = partition {
        Verdict::Partition(name)
    } else if reachable == 0 {
        Verdict::Isolated
    } else {
        Verdict::Confirmed
    }
}

/// Fetch a witness's `/healthz` report, or `None` if it is unreachable or replies
/// with something other than a complete, healthy report.
async fn fetch_witness(client: &Client, url: &str) -> Option<HealthReport> {
    crate::http::fetch_json_capped(client.get(url), MAX_REPORT_BYTES, WITNESS_TIMEOUT).await
}

/// Deliver an event to the peer's routed channels (or all, if unrouted).
async fn dispatch(notifier: &Notifiers, peer: &Peer, event: Event<'_>) {
    notifier
        .load_full()
        .dispatch(event, peer.notify.as_deref())
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_dead_man_goes_silent_when_no_channel_delivers() {
        let entry = |name: &str, failures| hora_notify::ChannelHealthEntry {
            name: name.to_owned(),
            health: hora_notify::ChannelHealth {
                consecutive_failures: failures,
                ..hora_notify::ChannelHealth::default()
            },
        };
        // No channel configured: nothing to judge.
        assert!(alerts_can_leave(&[], 3));
        // One channel still delivering is enough.
        assert!(alerts_can_leave(&[entry("ntfy", 5), entry("mail", 2)], 3));
        assert!(alerts_can_leave(&[entry("ntfy", 0)], 3));
        // Every channel at the watchdog threshold: nothing gets out.
        assert!(!alerts_can_leave(&[entry("ntfy", 3), entry("mail", 9)], 3));
        assert!(!alerts_can_leave(&[entry("ntfy", 1)], 0));
    }

    use crate::config;

    async fn insert(store: &Store, id: &str, time: i64, status: i64) {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
             VALUES (?, ?, ?, NULL, NULL, NULL)",
        )
        .bind(time)
        .bind(id)
        .bind(status)
        .execute(store.sqlx())
        .await
        .expect("insert check");
    }

    fn cfg(toml: &str) -> Config {
        config::parse(toml).expect("valid config")
    }

    #[test]
    fn liveness_alive_without_monitors() {
        let config = cfg("[page]\n[server]\n");
        let (ok, age) = scheduler_liveness(&config, &AtomicU64::new(0), 1000);
        assert!(ok);
        assert_eq!(age, -1);
    }

    #[test]
    fn liveness_uses_fastest_interval_and_warms_up() {
        let config = cfg("[page]\n[server]\n\
             [[monitors]]\nid=\"slow\"\nname=\"S\"\ntarget=\"https://e.com\"\ninterval_secs=86400\n\
             [[monitors]]\nid=\"fast\"\nname=\"F\"\ntarget=\"https://e.com\"\ninterval_secs=30\n");
        // Fresh tick: age 10 <= 2*min(30)=60 -> alive (the slow monitor must not relax it).
        let tick = AtomicU64::new(1000);
        assert_eq!(scheduler_liveness(&config, &tick, 1010), (true, 10));
        // Stale: age 200 > 60 -> not alive, even though it is well within the slow interval.
        assert!(!scheduler_liveness(&config, &tick, 1200).0);
        // Never ticked while monitors exist -> warming up, not alive.
        assert_eq!(
            scheduler_liveness(&config, &AtomicU64::new(0), 1000),
            (false, -1)
        );
    }

    #[tokio::test]
    async fn peer_seen_reflects_freshness_and_status() {
        let store = Store::in_memory().await;
        let base = cfg("[page]\n[server]\n[health]\nid=\"a\"\n\
             [[peers]]\nid=\"x\"\nname=\"X\"\nexpect_every_secs=100\n\
             listen_token=\"a-long-enough-listen-token\"\n");
        let mut peer = base.peers[0].clone();
        let now = 1_000_000;

        // Never seen.
        peer.id = "never".to_owned();
        assert_eq!(
            peer_seen(&store, &peer, now).await.state,
            PeerState::Unknown
        );

        // Recent up ping.
        peer.id = "up".to_owned();
        insert(&store, "up", now - 10, 1).await;
        let seen = peer_seen(&store, &peer, now).await;
        assert_eq!(seen.state, PeerState::Up);
        assert_eq!(seen.age, 10);

        // A lone recorded miss (no positive heartbeat ever) is still unknown:
        // staleness is measured from real heartbeats, not from recorded misses.
        peer.id = "miss".to_owned();
        insert(&store, "miss", now - 1, 0).await;
        assert_eq!(
            peer_seen(&store, &peer, now).await.state,
            PeerState::Unknown
        );

        // Stale up ping, with a fresh miss on top: age is measured from the last
        // real heartbeat (200s), so it is down despite the recent miss row.
        peer.id = "stale".to_owned();
        insert(&store, "stale", now - 200, 1).await;
        insert(&store, "stale", now - 1, 0).await;
        assert_eq!(peer_seen(&store, &peer, now).await.state, PeerState::Down);
        assert_eq!(peer_seen(&store, &peer, now).await.age, 200);
    }

    #[tokio::test]
    async fn report_includes_watched_peers_and_status() {
        let store = Store::in_memory().await;
        let config = cfg("[page]\n[server]\n[health]\nid=\"hora-a\"\n\
             [[peers]]\nid=\"hora-b\"\nname=\"B\"\nexpect_every_secs=100\n\
             listen_token=\"a-long-enough-listen-token\"\n\
             [[peers]]\nid=\"hc\"\nname=\"HC\"\nping_url=\"https://hc-ping.com/x\"\n");
        let report = report(&store, &config, &AtomicU64::new(0)).await;
        assert_eq!(report.id.as_deref(), Some("hora-a"));
        assert!(report.db_ok);
        // No monitors -> scheduler trivially alive, so status is ok.
        assert_eq!(report.status, HealthStatus::Ok);
        // Only the watched peer (hora-b) appears; the OUT-only peer (hc) does not.
        assert!(report.peers.contains_key("hora-b"));
        assert!(!report.peers.contains_key("hc"));
        assert_eq!(report.peers["hora-b"].state, PeerState::Unknown);
    }

    /// Node `yuki` watching its peer on `frank` every second, alerting on
    /// the first missed heartbeat, through a webhook at `hook`. The peer may
    /// announce up to 15 minutes of maintenance.
    fn watching_config(hook: &str) -> Config {
        cfg(&format!(
            r#"
            [page]
            [server]
            [alerts]
            fail_threshold = 1
            [health]
            id = "hora-yuki"
            grace_secs = 0
            [[channels]]
            name = "hook"
            type = "webhook"
            url = "{hook}"
            [[peers]]
            id = "hora-frank"
            name = "Frank"
            listen_id = "frank-in"
            expect_every_secs = 1
            listen_token = "a-long-enough-listen-token"
            host = "frank"
            accept_maintenance_secs = 900
            "#
        ))
    }

    /// The peer heartbeats, announcing its reboot: a window under way that
    /// lasts `secs` more seconds. Then its watch starts.
    async fn watch_through_a_window(
        store: &Store,
        config: &Config,
        secs: i64,
    ) -> (JoinHandle<()>, watch::Sender<bool>) {
        let now = chrono::Utc::now().timestamp();
        db::insert_push(
            store,
            "frank-in",
            crate::status::CheckStatus::Up,
            None,
            None,
        )
        .await
        .unwrap();
        let applied = crate::planned::apply_announcement(
            store,
            &config.peers[0],
            true,
            &[crate::mesh::wire::AnnouncedWindow {
                id: 1,
                starts_at: now - 1,
                ends_at: now + secs,
                reason: "kernel reboot".to_owned(),
                host: Some("frank".to_owned()),
            }],
            now,
        )
        .await
        .unwrap();
        assert_eq!(applied.accepted.len(), 1);
        let client = crate::http::client(None).unwrap();
        let (_config_tx, config_rx) = watch::channel(Arc::new(config.clone()));
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = spawn_watch(
            config.peers[0].clone(),
            config_rx,
            store.clone(),
            client.clone(),
            crate::notifications::shared(config, &client),
            AlertCell::default(),
            shutdown_rx,
        );
        (task, shutdown_tx)
    }

    /// The peer said it would be down for a few seconds and is still silent
    /// when they are over: nothing during the window, "peer down" after it.
    #[tokio::test]
    async fn a_peer_still_silent_after_its_window_is_announced() {
        let (hook, posts) = crate::testing::webhook_sink().await;
        let config = watching_config(&hook);
        let store = Store::in_memory().await;
        let (task, _shutdown) = watch_through_a_window(&store, &config, 4).await;

        // It has gone silent, and the watch sees it - and says nothing.
        tokio::time::sleep(Duration::from_millis(3200)).await;
        assert_eq!(crate::testing::events(&posts), Vec::<String>::new());
        let checks = db::recent_checks(&store, "frank-in", 10).await.unwrap();
        assert!(
            checks
                .iter()
                .any(|check| check.status == crate::status::CheckStatus::Down),
            "the missed heartbeats are recorded: {checks:?}"
        );

        // The window is over and the peer is not back: the alert goes out.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while crate::testing::events(&posts).is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(crate::testing::events(&posts), ["down"]);
        assert_eq!(posts.lock().unwrap()[0]["monitor"], "Frank");

        // And when it comes back, so does its recovery.
        db::insert_push(
            &store,
            "frank-in",
            crate::status::CheckStatus::Up,
            None,
            None,
        )
        .await
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while crate::testing::events(&posts).len() < 2 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(crate::testing::events(&posts), ["down", "recovered"]);
        task.abort();
    }

    /// The peer is back, heartbeating, before its window ends: no alert
    /// then, none when the window ends, and no recovery of a down nobody
    /// was told about.
    #[tokio::test]
    async fn a_peer_back_before_its_window_ends_is_never_announced() {
        let (hook, posts) = crate::testing::webhook_sink().await;
        let config = watching_config(&hook);
        let store = Store::in_memory().await;
        let (task, _shutdown) = watch_through_a_window(&store, &config, 4).await;

        // Silent for a while inside the window...
        tokio::time::sleep(Duration::from_millis(2500)).await;
        // ...then heartbeating again, through its end and past it.
        let beating = {
            let store = store.clone();
            tokio::spawn(async move {
                loop {
                    db::insert_push(
                        &store,
                        "frank-in",
                        crate::status::CheckStatus::Up,
                        None,
                        None,
                    )
                    .await
                    .unwrap();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
            })
        };
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(crate::testing::events(&posts), Vec::<String>::new());
        beating.abort();
        task.abort();
    }

    /// The heartbeat a node sends while something is planned carries it, to
    /// the peers and to them only; with nothing planned it is the bare ping
    /// 0.11 sent.
    #[tokio::test]
    async fn the_heartbeat_carries_the_planned_windows() {
        let (peer_hook, peer_posts) = crate::testing::webhook_sink().await;
        let (other_hook, other_posts) = crate::testing::webhook_sink().await;
        let config = cfg(&format!(
            r#"
            [page]
            [server]
            [health]
            id = "hora-frank"
            host = "frank"
            interval_secs = 1
            heartbeat_url = "{other_hook}"
            [[peers]]
            id = "hora-yuki"
            name = "Yuki"
            ping_url = "{peer_hook}"
            ping_token = "a-long-enough-ping-token"
            "#
        ));
        let store = Store::in_memory().await;
        let client = crate::http::client(None).unwrap();

        // Nothing planned: no body at all (the sinks only record JSON).
        send_heartbeat(&client, &peer_hook, Some("token"), "ok", None).await;
        assert!(peer_posts.lock().unwrap().is_empty());

        let now = chrono::Utc::now().timestamp();
        let planned = crate::planned::plan(
            &store,
            &config,
            crate::planned::Plan {
                host: Some("frank"),
                starts_at: Some(&(now + 3600).to_string()),
                duration: "10m",
                reason: "kernel reboot",
                ..crate::planned::Plan::default()
            },
            now,
        )
        .await
        .unwrap();

        let (_config_tx, config_rx) = watch::channel(Arc::new(config.clone()));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = spawn_heartbeat(
            config_rx,
            store.clone(),
            client.clone(),
            crate::notifications::shared(&config, &client),
            Arc::new(AtomicU64::new(0)),
            shutdown_rx,
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        while peer_posts.lock().unwrap().is_empty() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        task.abort();
        let posts = peer_posts.lock().unwrap();
        assert_eq!(
            posts.first().expect("a heartbeat with a body"),
            &serde_json::json!({"maintenance": [{
                "id": planned.window.id,
                "starts_at": now + 3600,
                "ends_at": now + 4200,
                "reason": "kernel reboot",
                "host": "frank"
            }]})
        );
        // The receiver reads it back as the wire type.
        let body: HeartbeatBody = serde_json::from_value(posts[0].clone()).unwrap();
        assert_eq!(body.maintenance[0].host.as_deref(), Some("frank"));
        // The external dead-man target is not told.
        assert!(other_posts.lock().unwrap().is_empty());
    }
}
