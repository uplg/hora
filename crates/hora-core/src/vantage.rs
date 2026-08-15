//! Per-vantage latency aggregation: a background task polls each peer's
//! `/api/peer/monitors` and keeps an in-memory map of "how does this target
//! look from over there" - status and 24h median per peer - which the status
//! page reads to render "80 ms from EU, 220 ms from US" on each card.
//!
//! Strictly read-only and fail-open: an unreachable, slow or misconfigured
//! peer just drops out of the map until the next round; page builds never
//! wait on the network (they read the last snapshot).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use reqwest::Client;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::{Config, Kind, Monitor};
use crate::confirm::PeerMonitors;

/// How often the peers are polled. Latency medians move slowly; a minute
/// keeps the display fresh without turning the mesh into a chat room.
const POLL_INTERVAL: Duration = Duration::from_mins(1);

/// Per-request deadline; a peer slower than this has nothing fresh to say.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Cap on a peer's response body: a mesh member is trusted, but a compromised
/// one must not be able to balloon this node's memory.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

/// One peer's view of a target this node also monitors.
#[derive(Debug, Clone)]
pub struct PeerVantage {
    /// The peer's display name (`[[peers]].name`).
    pub peer: String,
    /// `up` | `degraded` | `down` | `unknown`, from that vantage.
    pub status: String,
    /// That vantage's 24h median latency, when it has one.
    pub p50_ms: Option<i64>,
}

/// The shared snapshot the web layer reads: `kind|target` → the peers' views,
/// in configuration order. Swapped atomically by the poller.
pub type VantageMap = Arc<ArcSwap<HashMap<String, Vec<PeerVantage>>>>;

/// An empty map, for wiring before the first poll (and for tests).
#[must_use]
pub fn new_map() -> VantageMap {
    Arc::new(ArcSwap::from_pointee(HashMap::new()))
}

/// The map key for a monitor's target. Kind-qualified: the same `host:port`
/// as a tcp connect and as a dns lookup are different measurements.
#[must_use]
pub fn key(kind: Kind, target: &str) -> String {
    format!("{}|{target}", kind.as_str())
}

/// The vantages relevant to one monitor, cloned out of the snapshot.
#[must_use]
pub fn for_monitor<S: std::hash::BuildHasher>(
    map: &HashMap<String, Vec<PeerVantage>, S>,
    monitor: &Monitor,
) -> Vec<PeerVantage> {
    map.get(&key(monitor.kind, &monitor.target))
        .cloned()
        .unwrap_or_default()
}

/// Spawn the poller: every [`POLL_INTERVAL`], ask each peer for its monitors
/// and swap in a fresh map. Self-gating like the heartbeat - it reads the
/// live config each round, so peers added or removed on reload apply without
/// a restart, and a config with no askable peers costs one no-op per round.
#[must_use]
pub fn spawn_poller(
    config: watch::Receiver<Arc<Config>>,
    client: Client,
    map: VantageMap,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            let snapshot = config.borrow().clone();
            let fresh = poll_once(&client, &snapshot).await;
            map.store(Arc::new(fresh));
        }
    })
}

/// One polling round: every peer with an API origin is asked concurrently;
/// failures just leave that peer out of the fresh map.
async fn poll_once(client: &Client, config: &Config) -> HashMap<String, Vec<PeerVantage>> {
    let mut map: HashMap<String, Vec<PeerVantage>> = HashMap::new();
    let Some(from) = config.health.as_ref().map(|health| health.id.clone()) else {
        // Without an identity the peers cannot authenticate us: nothing to ask.
        return map;
    };

    let from = &from;
    let asks = config.peers.iter().filter_map(|peer| {
        let url = peer.monitors_url()?;
        Some(async move {
            let answer = fetch_peer_monitors(
                client,
                &url,
                from,
                peer.ping_token.as_ref().map(std::convert::AsRef::as_ref),
            )
            .await;
            (peer.name.clone(), answer)
        })
    });
    for (peer_name, answer) in futures_util::future::join_all(asks).await {
        let Some(answer) = answer else {
            tracing::debug!(peer = %peer_name, "vantage poll failed or peer unreachable");
            continue;
        };
        merge_peer(&mut map, &peer_name, &answer);
    }
    map
}

/// Fold one peer's disclosure into the map (kept separate so the merge is
/// testable without a network).
pub(crate) fn merge_peer<S: std::hash::BuildHasher>(
    map: &mut HashMap<String, Vec<PeerVantage>, S>,
    peer_name: &str,
    answer: &PeerMonitors,
) {
    for monitor in &answer.monitors {
        map.entry(key(monitor.kind, &monitor.target))
            .or_default()
            .push(PeerVantage {
                peer: peer_name.to_owned(),
                status: monitor.status.clone(),
                p50_ms: monitor.p50_ms,
            });
    }
}

/// `GET /api/peer/monitors` on one peer, bounded in time and size. Any
/// failure - transport, status, oversized or malformed body - is `None`.
/// Shared with `hora peers diff`, so the CLI and the poller read identically.
pub async fn fetch_peer_monitors(
    client: &Client,
    url: &str,
    from: &str,
    token: Option<&str>,
) -> Option<PeerMonitors> {
    let mut builder = client
        .get(url)
        .query(&[("from", from)])
        .timeout(POLL_TIMEOUT);
    if let Some(token) = token {
        builder = builder.header("x-push-token", token);
    }
    let outcome = tokio::time::timeout(POLL_TIMEOUT + Duration::from_secs(2), async {
        let mut response = builder.send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) => {
                    if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                        return None;
                    }
                    body.extend_from_slice(&chunk);
                }
                Ok(None) => break,
                Err(_) => return None,
            }
        }
        serde_json::from_slice::<PeerMonitors>(&body).ok()
    })
    .await;
    outcome.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::confirm::PeerMonitor;

    fn answer(entries: &[(&str, &str, Option<i64>)]) -> PeerMonitors {
        PeerMonitors {
            monitors: entries
                .iter()
                .map(|&(target, status, p50_ms)| PeerMonitor {
                    kind: Kind::Tcp,
                    target: target.to_owned(),
                    status: status.to_owned(),
                    p50_ms,
                })
                .collect(),
        }
    }

    #[test]
    fn merge_groups_by_kind_and_target() {
        let mut map = HashMap::new();
        merge_peer(&mut map, "Hora B", &answer(&[("db:5432", "up", Some(220))]));
        merge_peer(
            &mut map,
            "Hora C",
            &answer(&[("db:5432", "up", Some(85)), ("other:80", "down", None)]),
        );

        let db = &map[&key(Kind::Tcp, "db:5432")];
        assert_eq!(db.len(), 2);
        assert_eq!((db[0].peer.as_str(), db[0].p50_ms), ("Hora B", Some(220)));
        assert_eq!((db[1].peer.as_str(), db[1].p50_ms), ("Hora C", Some(85)));
        assert_eq!(map[&key(Kind::Tcp, "other:80")][0].status, "down");

        // The same target under another kind is a different measurement.
        assert!(!map.contains_key(&key(Kind::Dns, "db:5432")));
    }

    #[test]
    fn for_monitor_matches_its_own_key_only() {
        let mut map = HashMap::new();
        merge_peer(&mut map, "B", &answer(&[("db:5432", "up", Some(10))]));

        let mut monitor = crate::config::Monitor::ad_hoc(Kind::Tcp, "db:5432".to_owned());
        assert_eq!(for_monitor(&map, &monitor).len(), 1);
        monitor.target = "elsewhere:1".to_owned();
        assert!(for_monitor(&map, &monitor).is_empty());
    }
}
