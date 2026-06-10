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

use futures_util::future::join_all;
use hora_notify::Event;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::config::{Config, Health, Peer};
use crate::db;
use crate::notifications::Notifiers;
use crate::scheduler::heartbeat_outcome_for;

/// Floor on the scheduler-liveness tolerance, so a node whose fastest monitor
/// ticks very frequently is not flagged unhealthy by a momentary scheduling jitter.
const MIN_LIVENESS_TOLERANCE: i64 = 30;

/// How long a witness `/healthz` poll may take before it is treated as unreachable.
const WITNESS_TIMEOUT: Duration = Duration::from_secs(5);

/// How long an outbound heartbeat `POST` may take before it is abandoned.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(10);

// --- /healthz report ------------------------------------------------------

/// The body of `/healthz`: this node's own health plus its view of every peer it
/// watches. `status` is `"ok"` only when the node is fully healthy, so an external
/// keyword monitor (e.g. `UptimeRobot` matching `"ok"`) detects trouble; the rest of
/// the document is ignored by such pollers but read by other Hora for quorum.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HealthReport {
    /// `"ok"` when the scheduler and database are both healthy, else `"degraded"`.
    pub status: String,
    pub scheduler_ok: bool,
    pub db_ok: bool,
    /// Seconds since the most recent scheduler tick; `-1` if it has not ticked yet.
    pub last_tick_age: i64,
    /// This node's identity (`[health].id`), absent if no `[health]` section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// This node's view of each watched peer, keyed by the peer's global id.
    pub peers: HashMap<String, PeerSeen>,
}

/// One node's view of a peer, as reported on `/healthz`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PeerSeen {
    /// `"up"`, `"down"`, or `"unknown"` (never seen).
    pub state: String,
    /// Seconds since the peer's last heartbeat; `-1` if never seen.
    pub age: i64,
}

/// Build the `/healthz` report for the current configuration and scheduler state.
pub async fn report(pool: &SqlitePool, config: &Config, last_tick: &AtomicU64) -> HealthReport {
    let now = chrono::Utc::now().timestamp();
    let (scheduler_ok, last_tick_age) = scheduler_liveness(config, last_tick, now);
    let db_ok = db_ok(pool).await;

    let mut peers = HashMap::new();
    for peer in config.peers.iter().filter(|peer| peer.is_watched()) {
        peers.insert(peer.id.clone(), peer_seen(pool, peer, now).await);
    }

    HealthReport {
        status: if scheduler_ok && db_ok {
            "ok"
        } else {
            "degraded"
        }
        .to_owned(),
        scheduler_ok,
        db_ok,
        last_tick_age,
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
pub(crate) async fn db_ok(pool: &SqlitePool) -> bool {
    let query = sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(pool);
    matches!(
        tokio::time::timeout(Duration::from_secs(2), query).await,
        Ok(Ok(_))
    )
}

/// This node's view of one peer, measured from its last *positive* heartbeat
/// (recorded misses are ignored, see [`db::last_heartbeat_time`]): `up` within the
/// expected interval, `down` once that lapses, `unknown` if never seen. `age` is
/// the seconds since that last real heartbeat, so it stays meaningful through an
/// outage instead of resetting on each recorded miss.
async fn peer_seen(pool: &SqlitePool, peer: &Peer, now: i64) -> PeerSeen {
    let expect = i64::try_from(peer.expect_every_secs.unwrap_or(0)).unwrap_or(i64::MAX);
    match db::last_heartbeat_time(pool, peer.listen_id()).await {
        Ok(Some(last)) => {
            let age = (now - last).max(0);
            PeerSeen {
                state: if now - last > expect { "down" } else { "up" }.to_owned(),
                age,
            }
        }
        _ => PeerSeen {
            state: "unknown".to_owned(),
            age: -1,
        },
    }
}

// --- Outbound heartbeat (the dead-man) ------------------------------------

/// Spawn the outbound heartbeat: every `health.interval_secs`, when this node is
/// locally healthy, `POST` a heartbeat to each peer's `ping_url` and to the
/// optional `heartbeat_url`. When unhealthy it stays silent - that silence is the
/// down signal the receiver detects. A shutdown signal stops it between ticks.
#[must_use]
pub fn spawn_heartbeat(
    config: watch::Receiver<Arc<Config>>,
    pool: SqlitePool,
    client: Client,
    last_tick: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
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
            let db_ok = db_ok(&pool).await;
            if !(scheduler_ok && db_ok) {
                warn!(scheduler_ok, db_ok, "heartbeat skipped: node unhealthy");
                continue;
            }

            let digest = format!("ok mon={} tick={}s", snapshot.monitors.len(), age.max(0));
            for peer in &snapshot.peers {
                if let Some(url) = &peer.ping_url {
                    send_heartbeat(
                        &client,
                        url.as_ref(),
                        peer.ping_token.as_ref().map(AsRef::as_ref),
                        &digest,
                    )
                    .await;
                }
            }
            if let Some(url) = &health.heartbeat_url {
                send_heartbeat(&client, url.as_ref(), None, &digest).await;
            }
        }
    })
}

/// Fire one heartbeat: `POST {url}?status=up&msg={digest}` with the token (if any)
/// in the `X-Push-Token` header, so it stays out of the receiver's access logs.
/// Fire-and-forget: a failure is logged (without the URL, which may carry
/// credentials) and the next tick tries again.
async fn send_heartbeat(client: &Client, url: &str, token: Option<&str>, digest: &str) {
    let mut request = client
        .post(url)
        .query(&[("status", "up"), ("msg", digest)])
        .timeout(HEARTBEAT_TIMEOUT);
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

/// The alert state of one watched peer. Kept so a transition is reported once and
/// a long partition does not re-fire every tick.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PeerAlert {
    Healthy,
    /// Down locally and no witness reachable: probably *this* node is isolated, so
    /// we stayed silent.
    Isolated,
    /// Down locally but a witness still sees the peer: a partition, reported once
    /// as a (low-severity) link-degraded event, not an outage.
    Partition,
    Down,
}

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
/// effect without a restart (the captured `peer` is replaced by a fresh task).
#[must_use]
pub(crate) fn spawn_watch(
    peer: Peer,
    config: watch::Receiver<Arc<Config>>,
    pool: SqlitePool,
    client: Client,
    notifier: Notifiers,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        // `is_watched()` guarantees `expect_every_secs` is set.
        let expect = peer.expect_every_secs.unwrap_or(60);
        // When this watch started: heartbeats within `grace_secs` of it are not
        // alerted, so a node whose persisted history looks instantly stale after a
        // restart (e.g. both peers rebooting together) doesn't fire a false down.
        let started = chrono::Utc::now().timestamp();
        let mut ticker = tokio::time::interval(Duration::from_secs(expect));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut consecutive_down: u32 = 0;
        let mut alerted = PeerAlert::Healthy;

        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            // `None` = never pinged yet: stays unknown (the startup grace), nothing
            // to react to.
            let Some(outcome) = heartbeat_outcome_for(&pool, peer.listen_id(), expect).await else {
                continue;
            };

            let snapshot = config.borrow().clone();
            let now = chrono::Utc::now();
            if snapshot.in_maintenance(peer.listen_id(), now) {
                continue; // Muted: the miss is still recorded, only alerts are skipped.
            }
            let threshold = snapshot.alerts.fail_threshold.max(1);

            if outcome.up {
                consecutive_down = 0;
                if alerted != PeerAlert::Healthy {
                    // Only announce recovery if we actually alerted (Isolated was silent).
                    if matches!(alerted, PeerAlert::Down | PeerAlert::Partition) {
                        info!(peer = %peer.id, "peer recovered");
                        dispatch(
                            &notifier,
                            &peer,
                            Event::Recovered {
                                monitor: &peer.name,
                            },
                        )
                        .await;
                    }
                    alerted = PeerAlert::Healthy;
                }
                continue;
            }

            consecutive_down = consecutive_down.saturating_add(1);
            // Hold off during the post-startup grace window (see `started`).
            let grace = i64::try_from(
                snapshot
                    .health
                    .as_ref()
                    .map_or(0, |health| health.grace_secs),
            )
            .unwrap_or(0);
            let in_grace = now.timestamp() - started < grace;
            if consecutive_down < threshold || alerted == PeerAlert::Down || in_grace {
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
            match verdict {
                Verdict::Confirmed => {
                    error!(peer = %peer.id, "peer down");
                    dispatch(
                        &notifier,
                        &peer,
                        Event::Down {
                            monitor: &peer.name,
                            error: outcome.error.as_deref(),
                            cause: None,
                            impacted: &[],
                        },
                    )
                    .await;
                    alerted = PeerAlert::Down;
                }
                Verdict::Partition(witness) if alerted != PeerAlert::Partition => {
                    warn!(peer = %peer.id, %witness, "peer unreachable but a witness sees it up: treating as a partition, not an outage");
                    dispatch(
                        &notifier,
                        &peer,
                        Event::PeerLinkDegraded {
                            peer: &peer.name,
                            witness: &witness,
                        },
                    )
                    .await;
                    alerted = PeerAlert::Partition;
                }
                Verdict::Isolated if alerted != PeerAlert::Isolated => {
                    warn!(peer = %peer.id, "peer down but no witness reachable: possible local isolation, not alerting");
                    alerted = PeerAlert::Isolated;
                }
                // Already in this suppressed state: nothing to re-announce.
                Verdict::Partition(_) | Verdict::Isolated => {}
            }
        }
    })
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
                    .is_some_and(|view| view.state == "up")
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
/// with something other than a healthy report.
async fn fetch_witness(client: &Client, url: &str) -> Option<HealthReport> {
    /// A healthy report is a small JSON object; cap the body so a compromised
    /// peer can't stream hundreds of MB into memory within the timeout.
    const MAX_REPORT_BYTES: usize = 64 * 1024;

    let mut response = client.get(url).timeout(WITNESS_TIMEOUT).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_REPORT_BYTES {
                    return None; // Oversized: treat as an unreachable/unhealthy witness.
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            // A transport error mid-body is not a clean end of stream; a report
            // we could not fully read must not vouch for a healthy witness.
            Err(_) => return None,
        }
    }
    serde_json::from_slice::<HealthReport>(&body).ok()
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

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use crate::config;

    async fn memory_pool() -> SqlitePool {
        let options = SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("connect in-memory");
        db::migrator().run(&pool).await.expect("run migrations");
        pool
    }

    async fn insert(pool: &SqlitePool, id: &str, time: i64, status: i64) {
        sqlx::query(
            "INSERT INTO checks (time, monitor_id, status, latency_ms, status_code, error) \
             VALUES (?, ?, ?, NULL, NULL, NULL)",
        )
        .bind(time)
        .bind(id)
        .bind(status)
        .execute(pool)
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
        let pool = memory_pool().await;
        let base = cfg("[page]\n[server]\n[health]\nid=\"a\"\n\
             [[peers]]\nid=\"x\"\nname=\"X\"\nexpect_every_secs=100\n");
        let mut peer = base.peers[0].clone();
        let now = 1_000_000;

        // Never seen.
        peer.id = "never".to_owned();
        assert_eq!(peer_seen(&pool, &peer, now).await.state, "unknown");

        // Recent up ping.
        peer.id = "up".to_owned();
        insert(&pool, "up", now - 10, 1).await;
        let seen = peer_seen(&pool, &peer, now).await;
        assert_eq!(seen.state, "up");
        assert_eq!(seen.age, 10);

        // A lone recorded miss (no positive heartbeat ever) is still unknown:
        // staleness is measured from real heartbeats, not from recorded misses.
        peer.id = "miss".to_owned();
        insert(&pool, "miss", now - 1, 0).await;
        assert_eq!(peer_seen(&pool, &peer, now).await.state, "unknown");

        // Stale up ping, with a fresh miss on top: age is measured from the last
        // real heartbeat (200s), so it is down despite the recent miss row.
        peer.id = "stale".to_owned();
        insert(&pool, "stale", now - 200, 1).await;
        insert(&pool, "stale", now - 1, 0).await;
        assert_eq!(peer_seen(&pool, &peer, now).await.state, "down");
        assert_eq!(peer_seen(&pool, &peer, now).await.age, 200);
    }

    #[tokio::test]
    async fn report_includes_watched_peers_and_status() {
        let pool = memory_pool().await;
        let config = cfg("[page]\n[server]\n[health]\nid=\"hora-a\"\n\
             [[peers]]\nid=\"hora-b\"\nname=\"B\"\nexpect_every_secs=100\n\
             [[peers]]\nid=\"hc\"\nname=\"HC\"\nping_url=\"https://hc-ping.com/x\"\n");
        let report = report(&pool, &config, &AtomicU64::new(0)).await;
        assert_eq!(report.id.as_deref(), Some("hora-a"));
        assert!(report.db_ok);
        // No monitors -> scheduler trivially alive, so status is ok.
        assert_eq!(report.status, "ok");
        // Only the watched peer (hora-b) appears; the OUT-only peer (hc) does not.
        assert!(report.peers.contains_key("hora-b"));
        assert!(!report.peers.contains_key("hc"));
        assert_eq!(report.peers["hora-b"].state, "unknown");
    }
}
