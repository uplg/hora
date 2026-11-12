//! `hora peers`: compare this node's monitors with each peer's.

use anyhow::Context as _;
use hora_core::config;

use super::{CliError, usage};

/// `hora peers diff`: compare this node's probeable monitors (kind + target,
/// and what each asserts) with each peer's, over the authenticated
/// `/api/peer/monitors` exchange.
/// A peer lists its targets with the secrets of their URLs masked, so both
/// sides are compared masked (`disclosed_target`); a 0.11 peer that still
/// lists them verbatim compares equal all the same. A target both nodes watch
/// with different assertions is drift too: the peer refuses to confirm a
/// check it does not run (a 0.11 peer lists no fingerprint, and is compared
/// on targets alone).
/// `confirm_with_peers` only works on monitors both nodes know, and nothing
/// else verifies that alignment - this does, and exits non-zero on any drift
/// or unreachable peer so it can gate a config deploy.
pub(crate) async fn peers(args: &[String]) -> Result<(), CliError> {
    if args.first().map(String::as_str) != Some("diff") {
        return Err(usage("Usage: hora peers diff"));
    }
    let config = config::load_from(&config::path()).context("loading configuration")?;
    let Some(from) = config.health.as_ref().map(|health| health.id.clone()) else {
        return Err(CliError::Failed(
            "hora peers diff needs [health].id (the identity the peers authenticate).".to_owned(),
        ));
    };
    // Each askable peer paired with the URL that made it askable.
    let askable: Vec<(&hora_core::config::Peer, String)> = config
        .peers
        .iter()
        .filter_map(|peer| peer.monitors_url().map(|url| (peer, url)))
        .collect();
    if askable.is_empty() {
        return Err(CliError::Failed(
            "No askable peers: [[peers]] entries need a ping_url to derive the API origin."
                .to_owned(),
        ));
    }

    let mut local = Checks::new();
    for monitor in &config.monitors {
        if let Some(check) = hora_core::mesh::fingerprint::check_fingerprint(monitor) {
            local
                .entry((
                    monitor.kind().as_str().to_owned(),
                    hora_core::mesh::wire::disclosed_target(monitor.target()),
                ))
                .or_default()
                .insert(check);
        }
    }

    let client = hora_core::http::client(None).context("building HTTP client")?;
    let mut drift = false;
    println!("hora peers diff - {} local probeable monitors", local.len());
    for (peer, url) in askable {
        let answer = hora_core::mesh::vantage::fetch_peer_monitors(
            &client,
            &url,
            &from,
            peer.ping_token.as_ref().map(AsRef::as_ref),
        )
        .await;
        println!();
        drift |= print_peer_diff(&peer.name, &local, answer.as_ref());
    }
    if drift {
        println!();
        println!("Configs have drifted - multi-vantage confirmation only covers shared monitors.");
        return Err(CliError::Silent);
    }
    Ok(())
}

/// The probeable monitors of one node: per kind and (masked) target, the
/// fingerprints of the checks it runs there - usually one.
type Checks = std::collections::BTreeMap<(String, String), std::collections::BTreeSet<String>>;

/// Print one peer's diff against the local monitors; `true` means drift
/// (or an unreachable peer - a mesh you cannot ask is a mesh out of sync).
fn print_peer_diff(
    peer_name: &str,
    local: &Checks,
    answer: Option<&hora_core::mesh::wire::PeerMonitors>,
) -> bool {
    let Some(answer) = answer else {
        println!("{peer_name}: UNREACHABLE (or refused the exchange)");
        return true;
    };
    // `None` for a target where the peer lists a monitor without fingerprint
    // (a 0.11 node): there is nothing to compare the checks with.
    let mut remote: std::collections::BTreeMap<(String, String), Option<_>> =
        std::collections::BTreeMap::new();
    for monitor in &answer.monitors {
        let key = (
            monitor.kind.as_str().to_owned(),
            hora_core::mesh::wire::disclosed_target(&monitor.target),
        );
        let checks = remote
            .entry(key)
            .or_insert_with(|| Some(std::collections::BTreeSet::new()));
        match (&monitor.check, checks.as_mut()) {
            (Some(check), Some(checks)) => {
                checks.insert(check.clone());
            }
            _ => *checks = None,
        }
    }
    let missing: Vec<_> = local
        .keys()
        .filter(|key| !remote.contains_key(*key))
        .collect();
    let extra: Vec<_> = remote
        .keys()
        .filter(|key| !local.contains_key(*key))
        .collect();
    let differing: Vec<_> = local
        .iter()
        .filter(|(key, ours)| {
            remote
                .get(*key)
                .and_then(Option::as_ref)
                .is_some_and(|theirs| theirs != *ours)
        })
        .map(|(key, _)| key)
        .collect();
    if missing.is_empty() && extra.is_empty() && differing.is_empty() {
        println!("{peer_name}: in sync ({} monitors)", answer.monitors.len());
        return false;
    }
    println!("{peer_name}: DRIFT");
    for (kind, target) in missing {
        println!("  missing there: {kind} {target}");
    }
    for (kind, target) in extra {
        println!("  only there:    {kind} {target}");
    }
    for (kind, target) in differing {
        println!("  same target, checked differently: {kind} {target}");
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use hora_core::config::Kind;
    use hora_core::mesh::wire::{PeerMonitor, PeerMonitors, disclosed_target};
    use hora_core::status::MonitorState;

    fn local(target: &str, checks: &[&str]) -> Checks {
        Checks::from([(
            (Kind::Http.as_str().to_owned(), disclosed_target(target)),
            checks.iter().map(|&check| check.to_owned()).collect(),
        )])
    }

    fn listing(monitors: &[(&str, Option<&str>)]) -> PeerMonitors {
        PeerMonitors {
            monitors: monitors
                .iter()
                .map(|&(target, check)| PeerMonitor {
                    kind: Kind::Http,
                    target: target.to_owned(),
                    status: MonitorState::Up,
                    p50_ms: None,
                    check: check.map(str::to_owned),
                })
                .collect(),
        }
    }

    /// A URL carrying credentials is in sync with a peer that lists it masked
    /// (0.12) and with one that still lists it verbatim (0.11); a different
    /// path is still a drift.
    #[test]
    fn targets_are_compared_with_their_secrets_masked() {
        let url = "https://user:hunter2@api.example.com/health?api_key=s3cret";
        let local = local(url, &["aa"]);
        let masked = disclosed_target(url);
        assert!(!print_peer_diff(
            "B",
            &local,
            Some(&listing(&[(&masked, Some("aa"))]))
        ));
        assert!(!print_peer_diff(
            "B",
            &local,
            Some(&listing(&[(url, None)]))
        ));
        assert!(print_peer_diff(
            "B",
            &local,
            Some(&listing(&[("https://api.example.com/other", Some("aa"))]))
        ));
    }

    /// The same target on both nodes used to be "in sync" whatever each
    /// asserted on it.
    #[test]
    fn a_target_checked_differently_is_drift() {
        let url = "https://api.example.com/health";
        let ours = local(url, &["aa"]);
        assert!(!print_peer_diff(
            "B",
            &ours,
            Some(&listing(&[(url, Some("aa"))]))
        ));
        assert!(print_peer_diff(
            "B",
            &ours,
            Some(&listing(&[(url, Some("bb"))]))
        ));
        // Two checks there, one here: the second has no counterpart.
        assert!(print_peer_diff(
            "B",
            &ours,
            Some(&listing(&[(url, Some("aa")), (url, Some("bb"))]))
        ));
        let both = local(url, &["aa", "bb"]);
        assert!(!print_peer_diff(
            "B",
            &both,
            Some(&listing(&[(url, Some("bb")), (url, Some("aa"))]))
        ));
        // A 0.11 peer says nothing of its checks: targets alone are compared.
        assert!(!print_peer_diff("B", &ours, Some(&listing(&[(url, None)]))));
    }
}
