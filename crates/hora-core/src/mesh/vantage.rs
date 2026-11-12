//! Per-vantage latency aggregation: a background task polls each peer's
//! `/api/peer/monitors` and keeps an in-memory map of "how does this target
//! look from over there" - status and 24h median per peer - which the status
//! page reads to render "80 ms from EU, 220 ms from US" on each card.
//!
//! Strictly read-only and fail-open: an unreachable, slow or misconfigured
//! peer keeps its last answer for a few rounds (one lost poll must not make
//! its column vanish), then drops out of the map until it answers again; page
//! builds never wait on the network (they read the last snapshot).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use reqwest::Client;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::{Config, Kind, Monitor};
use crate::mesh::wire::PeerMonitors;
use crate::status::MonitorState;

/// How often the peers are polled. Latency medians move slowly; a minute
/// keeps the display fresh without turning the mesh into a chat room.
const POLL_INTERVAL: Duration = Duration::from_mins(1);

/// Per-request deadline; a peer slower than this has nothing fresh to say.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// How many polls in a row a peer may miss before its last answer is
/// dropped: a blip keeps the view, a peer gone for minutes loses it.
const MAX_MISSED_POLLS: u32 = 3;

/// Cap on a peer's response body: a mesh member is trusted, but a compromised
/// one must not be able to balloon this node's memory.
const MAX_RESPONSE_BYTES: usize = 512 * 1024;

/// One peer's view of a target this node also monitors.
#[derive(Debug, Clone)]
pub struct PeerVantage {
    /// The peer's display name (`[[peers]].name`).
    pub peer: String,
    /// `up` | `degraded` | `down` | `unknown`, from that vantage.
    pub status: MonitorState,
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
/// as a tcp connect and as a dns lookup are different measurements. The
/// target is keyed as peers disclose it, secrets masked (see
/// [`disclosed_target`](super::wire::disclosed_target)), so a local monitor
/// whose URL carries a credential still finds its peers' views - and so does
/// one a 0.11 peer still lists verbatim.
#[must_use]
pub fn key(kind: Kind, target: &str) -> String {
    format!(
        "{}|{}",
        kind.as_str(),
        super::wire::disclosed_target(target)
    )
}

/// The vantages relevant to one monitor, cloned out of the snapshot.
#[must_use]
pub fn for_monitor<S: std::hash::BuildHasher>(
    map: &HashMap<String, Vec<PeerVantage>, S>,
    monitor: &Monitor,
) -> Vec<PeerVantage> {
    map.get(&key(monitor.kind(), monitor.target()))
        .cloned()
        .unwrap_or_default()
}

/// Spawn the poller: every minute, ask each peer for its monitors
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
        let mut kept = Kept::new();
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = shutdown.changed() => break,
            }
            let snapshot = config.borrow().clone();
            let answers = poll_once(&client, &snapshot).await;
            map.store(Arc::new(fold_round(&mut kept, answers)));
        }
    })
}

/// Each peer's last answer and how many polls it has missed since.
type Kept = HashMap<String, (PeerMonitors, u32)>;

/// Fold one round's answers (peer name, answer; in configuration order) into
/// the kept answers and build the map from them: a peer that missed this poll
/// keeps its last answer for [`MAX_MISSED_POLLS`] rounds; a peer no longer
/// asked (removed from the config) is forgotten.
fn fold_round(
    kept: &mut Kept,
    answers: Vec<(String, Option<PeerMonitors>)>,
) -> HashMap<String, Vec<PeerVantage>> {
    kept.retain(|peer, _| answers.iter().any(|(name, _)| name == peer));
    let mut map = HashMap::new();
    for (peer_name, answer) in answers {
        if let Some(answer) = answer {
            kept.insert(peer_name.clone(), (answer, 0));
        } else {
            tracing::debug!(peer = %peer_name, "vantage poll failed or peer unreachable");
            if let Some((_, missed)) = kept.get_mut(&peer_name) {
                *missed += 1;
                if *missed > MAX_MISSED_POLLS {
                    kept.remove(&peer_name);
                }
            }
        }
        if let Some((answer, _)) = kept.get(&peer_name) {
            merge_peer(&mut map, &peer_name, answer);
        }
    }
    map
}

/// One polling round: every peer with an API origin is asked concurrently.
/// Returns each one's answer (`None` on any failure), in configuration order.
async fn poll_once(client: &Client, config: &Config) -> Vec<(String, Option<PeerMonitors>)> {
    let Some(from) = config.health.as_ref().map(|health| health.id.clone()) else {
        // Without an identity the peers cannot authenticate us: nothing to ask.
        return Vec::new();
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
    futures_util::future::join_all(asks).await
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
                status: monitor.status,
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
    let mut builder = client.get(url).query(&[("from", from)]);
    if let Some(token) = token {
        builder = builder.header(crate::mesh::wire::PUSH_TOKEN_HEADER, token);
    }
    crate::http::fetch_json_capped(builder, MAX_RESPONSE_BYTES, POLL_TIMEOUT).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mesh::wire::PeerMonitor;

    fn answer(entries: &[(&str, &str, Option<i64>)]) -> PeerMonitors {
        PeerMonitors {
            monitors: entries
                .iter()
                .map(|&(target, status, p50_ms)| PeerMonitor {
                    kind: Kind::Tcp,
                    target: target.to_owned(),
                    status: serde_json::from_value(serde_json::json!(status)).unwrap(),
                    p50_ms,
                    check: None,
                })
                .collect(),
        }
    }

    #[test]
    fn a_missed_poll_keeps_the_peer_for_a_few_rounds() {
        let mut kept = Kept::new();
        let round = |b: Option<PeerMonitors>| {
            vec![
                ("Hora B".to_owned(), b),
                (
                    "Hora C".to_owned(),
                    Some(answer(&[("db:5432", "up", Some(85))])),
                ),
            ]
        };
        let db = |map: &HashMap<String, Vec<PeerVantage>>| {
            map[&key(Kind::Tcp, "db:5432")]
                .iter()
                .map(|vantage| vantage.peer.clone())
                .collect::<Vec<_>>()
        };
        let map = fold_round(
            &mut kept,
            round(Some(answer(&[("db:5432", "up", Some(220))]))),
        );
        assert_eq!(db(&map), ["Hora B", "Hora C"]);
        // B misses polls: its last view stays for MAX_MISSED_POLLS rounds...
        for _ in 0..MAX_MISSED_POLLS {
            assert_eq!(
                db(&fold_round(&mut kept, round(None))),
                ["Hora B", "Hora C"]
            );
        }
        // ...then goes.
        assert_eq!(db(&fold_round(&mut kept, round(None))), ["Hora C"]);
        // A peer removed from the config is forgotten at once.
        fold_round(
            &mut kept,
            round(Some(answer(&[("db:5432", "up", Some(220))]))),
        );
        let only_c = vec![(
            "Hora C".to_owned(),
            Some(answer(&[("db:5432", "up", Some(85))])),
        )];
        assert_eq!(db(&fold_round(&mut kept, only_c)), ["Hora C"]);
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
        assert_eq!(
            map[&key(Kind::Tcp, "other:80")][0].status,
            MonitorState::Down
        );

        // The same target under another kind is a different measurement.
        assert!(!map.contains_key(&key(Kind::Dns, "db:5432")));
    }

    /// A peer lists a credential-bearing URL masked (0.12) or verbatim (0.11):
    /// either way the local monitor, which holds the secret, finds it.
    #[test]
    fn a_target_with_secrets_matches_its_masked_disclosure() {
        let local = "https://user:hunter2@api.example.com/health?api_key=s3cret";
        let masked = crate::mesh::wire::disclosed_target(local);
        assert_eq!(masked, "https://***:***@api.example.com/health?api_key=***");
        assert_eq!(crate::mesh::wire::disclosed_target(&masked), masked);

        let monitor = crate::config::Monitor::ad_hoc(Kind::Http, local.to_owned()).unwrap();
        for listed in [masked.as_str(), local] {
            let mut map = HashMap::new();
            let mut answer = answer(&[("unused", "up", Some(10))]);
            answer.monitors[0].kind = Kind::Http;
            listed.clone_into(&mut answer.monitors[0].target);
            merge_peer(&mut map, "B", &answer);
            assert_eq!(for_monitor(&map, &monitor).len(), 1, "{listed}");
            assert!(map.keys().all(|key| !key.contains("hunter2")), "{map:?}");
        }
    }

    #[test]
    fn for_monitor_matches_its_own_key_only() {
        let mut map = HashMap::new();
        merge_peer(&mut map, "B", &answer(&[("db:5432", "up", Some(10))]));

        let monitor = crate::config::Monitor::ad_hoc(Kind::Tcp, "db:5432".to_owned()).unwrap();
        assert_eq!(for_monitor(&map, &monitor).len(), 1);
        let elsewhere =
            crate::config::Monitor::ad_hoc(Kind::Tcp, "elsewhere:1".to_owned()).unwrap();
        assert!(for_monitor(&map, &elsewhere).is_empty());
    }
}
