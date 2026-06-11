//! Multi-vantage confirmation: when a monitor confirms down locally, ask the
//! peers to probe the same target from their side before alerting, and
//! annotate the alert with the verdict - *"confirmed down from 3/3 vantage
//! points"* (a real outage) vs *"seen UP by hora-b"* (likely a network
//! problem near this node). Two Raspberry Pi at two homes become a
//! distributed Pingdom.
//!
//! Robustness contract, in order of importance:
//! 1. **Fail open.** Peers being slow, broken, unreachable or misconfigured
//!    never blocks, delays past a hard deadline, or suppresses the alert.
//!    The worst possible outcome of this module is an alert *without* a
//!    vantage annotation - exactly what Hora sent before the feature.
//! 2. **Never a proxy.** The responder ([`hora-web`]'s `/api/peer/probe`)
//!    only probes targets present in *its own* configuration, so a leaked
//!    token cannot turn a peer into an SSRF relay. Both nodes must know the
//!    monitor - which pairs naturally with sharing the config in git.
//! 3. **A disputed down still alerts.** A peer seeing the target up softens
//!    the message, never silences it: geo-partial outages are real outages.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::{Config, Kind, Monitor};

/// Hard per-peer deadline for one confirmation probe. The responder bounds
/// its own probe by the monitor's timeout; this is the requester's backstop,
/// and the worst case the confirmation can add to an alert (probes run
/// concurrently).
pub const PROBE_DEADLINE: Duration = Duration::from_secs(10);

/// A confirmation probe response is one small JSON object; cap the body so a
/// compromised peer can't stream hundreds of MB into memory within the timeout.
const MAX_RESPONSE_BYTES: usize = 16 * 1024;

/// How many peer names are spelled out in the annotation before "…".
const MAX_NAMED_PEERS: usize = 3;

/// What one node asks another to probe. The responder matches `kind` +
/// `target` against its own monitors and refuses anything else.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProbeRequest {
    /// The requesting node's `[health].id`; the responder authenticates it
    /// against that peer's `listen_token`.
    pub from: String,
    pub kind: Kind,
    pub target: String,
}

/// The vantage's verdict on one probe.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProbeResponse {
    pub up: bool,
    /// The failure reason when not up, bounded by the responder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One peer's view of the target, from the requester's perspective.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// The peer probed and also sees it down.
    Down,
    /// The peer probed and sees it **up**: the disagreement that matters.
    Up,
    /// No usable answer: unreachable, an error, a timeout, or the peer does
    /// not know this target (404). Counts for nothing on either side.
    Unknown,
}

/// Whether multi-vantage confirmation applies to this monitor under this
/// config: the per-monitor override, else the `[health]` default - and never
/// for push monitors (nothing to probe).
#[must_use]
pub fn enabled(config: &Config, monitor: &Monitor) -> bool {
    if monitor.kind == Kind::Push {
        return false;
    }
    monitor.confirm_with_peers.unwrap_or_else(|| {
        config
            .health
            .as_ref()
            .is_some_and(|health| health.confirm_with_peers)
    })
}

/// Ask every confirmable peer to probe `monitor`'s target, concurrently and
/// with a hard deadline, and summarize the verdicts into the alert
/// annotation. `None` when confirmation is disabled for this monitor or no
/// peer can be asked - the alert then reads exactly as it always did.
pub async fn confirm_with_peers(
    client: &reqwest::Client,
    config: &Config,
    monitor: &Monitor,
) -> Option<String> {
    if !enabled(config, monitor) {
        return None;
    }
    let from = config.health.as_ref()?.id.clone();
    let peers: Vec<(String, String, Option<String>)> = config
        .peers
        .iter()
        .filter_map(|peer| {
            peer.probe_url().map(|url| {
                (
                    peer.name.clone(),
                    url,
                    peer.ping_token
                        .as_ref()
                        .map(|token| token.as_ref().to_owned()),
                )
            })
        })
        .collect();
    if peers.is_empty() {
        return None;
    }

    let request = ProbeRequest {
        from,
        kind: monitor.kind,
        target: monitor.target.clone(),
    };
    let probes = peers.iter().map(|(name, url, token)| {
        let request = &request;
        async move {
            let verdict = probe_peer(client, url, token.as_deref(), request).await;
            (name.as_str(), verdict)
        }
    });
    let views = futures_util::future::join_all(probes).await;
    Some(summarize(&views))
}

/// One confirmation probe against one peer. Every failure mode - transport,
/// HTTP status, malformed or oversized body, timeout - collapses to
/// [`Verdict::Unknown`]: an answer we could not fully trust never tips the
/// verdict either way.
async fn probe_peer(
    client: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    request: &ProbeRequest,
) -> Verdict {
    let mut builder = client.post(url).json(request).timeout(PROBE_DEADLINE);
    if let Some(token) = token {
        builder = builder.header("x-push-token", token);
    }
    // Belt and braces: reqwest's timeout covers the request, the outer one
    // guards the bounded body read below as well.
    let outcome = tokio::time::timeout(PROBE_DEADLINE + Duration::from_secs(2), async {
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
        serde_json::from_slice::<ProbeResponse>(&body).ok()
    })
    .await;

    match outcome {
        Ok(Some(response)) if response.up => Verdict::Up,
        Ok(Some(_)) => Verdict::Down,
        Ok(None) | Err(_) => Verdict::Unknown,
    }
}

/// Word the verdicts into the alert annotation. This node's own view counts
/// as one "down" vantage, so the totals read naturally ("2/2" with one
/// agreeing peer).
pub(crate) fn summarize(views: &[(&str, Verdict)]) -> String {
    use std::fmt::Write as _;

    let down = 1 + views
        .iter()
        .filter(|(_, verdict)| *verdict == Verdict::Down)
        .count();
    let seen_up: Vec<&str> = views
        .iter()
        .filter(|(_, verdict)| *verdict == Verdict::Up)
        .map(|(name, _)| *name)
        .collect();
    let unknown = views
        .iter()
        .filter(|(_, verdict)| *verdict == Verdict::Unknown)
        .count();
    let answered = 1 + views.len() - unknown;

    let mut out = if seen_up.is_empty() {
        if answered == 1 {
            // Every peer was unreachable or unaware: nothing was confirmed.
            "no peer vantage reachable, unconfirmed".to_owned()
        } else {
            format!("confirmed down from {down}/{answered} vantage points")
        }
    } else {
        let mut names = seen_up
            .iter()
            .take(MAX_NAMED_PEERS)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        if seen_up.len() > MAX_NAMED_PEERS {
            let _ = write!(names, " (+{})", seen_up.len() - MAX_NAMED_PEERS);
        }
        format!(
            "seen UP by {names} - down from {down}/{answered} vantage points \
             (network issue near this node?)"
        )
    };
    if unknown > 0 {
        let _ = write!(out, ", {unknown} vantage(s) unreachable");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_peers_agree_down() {
        let views = vec![("hora-b", Verdict::Down), ("hora-c", Verdict::Down)];
        assert_eq!(summarize(&views), "confirmed down from 3/3 vantage points");
    }

    #[test]
    fn a_peer_seeing_up_softens_but_never_silences() {
        let views = vec![("hora-b", Verdict::Up), ("hora-c", Verdict::Down)];
        let text = summarize(&views);
        assert!(text.contains("seen UP by hora-b"), "{text}");
        assert!(text.contains("down from 2/3"), "{text}");
        assert!(text.contains("network issue"), "{text}");
    }

    #[test]
    fn unreachable_peers_are_counted_not_trusted() {
        let views = vec![("hora-b", Verdict::Down), ("hora-c", Verdict::Unknown)];
        let text = summarize(&views);
        assert!(text.contains("confirmed down from 2/2"), "{text}");
        assert!(text.contains("1 vantage(s) unreachable"), "{text}");

        let none = vec![("hora-b", Verdict::Unknown)];
        let text = summarize(&none);
        assert!(text.contains("no peer vantage reachable"), "{text}");
    }

    #[test]
    fn many_up_peers_are_capped_in_the_message() {
        let views = vec![
            ("a", Verdict::Up),
            ("b", Verdict::Up),
            ("c", Verdict::Up),
            ("d", Verdict::Up),
        ];
        let text = summarize(&views);
        assert!(text.contains("a, b, c (+1)"), "{text}");
    }

    #[test]
    fn enabled_resolves_override_then_global_and_skips_push() {
        let config = crate::config::parse(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            confirm_with_peers = true
            [[peers]]
            id = "hora-b"
            name = "B"
            ping_url = "https://b.example/api/push/hora-a"
            [[monitors]]
            id = "on"
            name = "On"
            target = "https://example.com"
            interval_secs = 60
            [[monitors]]
            id = "off"
            name = "Off"
            target = "https://example.com"
            interval_secs = 60
            confirm_with_peers = false
            [[monitors]]
            id = "beat"
            name = "Beat"
            kind = "push"
            interval_secs = 60
            "#,
        )
        .expect("config");
        assert!(enabled(&config, &config.monitors[0])); // global default
        assert!(!enabled(&config, &config.monitors[1])); // explicit opt-out
        assert!(!enabled(&config, &config.monitors[2])); // push: never
    }

    #[test]
    fn dto_roundtrip() {
        let request = ProbeRequest {
            from: "hora-a".to_owned(),
            kind: Kind::Tcp,
            target: "db.example.com:5432".to_owned(),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert!(json.contains("\"kind\":\"tcp\""), "{json}");
        let back: ProbeRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.target, request.target);

        let response: ProbeResponse = serde_json::from_str(r#"{"up":false}"#).unwrap();
        assert!(!response.up && response.error.is_none());
    }
}
