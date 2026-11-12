//! The validation pass run once at load: everything serde's types cannot
//! express (cross-references, bounds, mutually exclusive fields).

mod monitor;

use std::collections::HashSet;

use super::{Channel, Config, Kind, Secret, parse_cron};
pub(crate) use monitor::monitor_from_raw;

/// The shortest access token Hora accepts. Tokens are compared online, behind
/// a rate limit, so the bound is about guessability, not offline cracking:
/// 16 characters rule out the words and short numbers people type by hand.
pub const MIN_TOKEN_CHARS: usize = 16;

/// A configured access token must be non-empty: an empty one (often the result
/// of an unset `${VAR}` expanding to "") would authorize a blank `?token=`, since
/// the constant-time compare treats `"" == ""` as a match. It must also be at
/// least [`MIN_TOKEN_CHARS`] long: a short token guards an endpoint in name
/// only, and a warning in the log is not where that gets noticed.
fn validate_token(label: &str, token: Option<&Secret>) -> anyhow::Result<()> {
    if let Some(token) = token {
        anyhow::ensure!(!token.is_empty(), "{label} must not be empty");
        anyhow::ensure!(
            token.as_ref().chars().count() >= MIN_TOKEN_CHARS,
            "{label} is too short: use at least {MIN_TOKEN_CHARS} characters \
             (e.g. `openssl rand -hex 16`)"
        );
    }
    Ok(())
}

/// A token this node *sends* (a peer's `ping_token`): non-empty, but its
/// length is the receiver's rule - an external dead-man service hands out
/// whatever it hands out - so a short one only warns.
fn validate_outbound_token(label: &str, token: Option<&Secret>) -> anyhow::Result<()> {
    if let Some(token) = token {
        anyhow::ensure!(!token.is_empty(), "{label} must not be empty");
        if token.as_ref().chars().count() < MIN_TOKEN_CHARS {
            tracing::warn!(
                "{label} is short (under {MIN_TOKEN_CHARS} chars); prefer a long random token"
            );
        }
    }
    Ok(())
}

/// Whether a URL targets this very host (`localhost`, `127.0.0.0/8`, `::1`):
/// the one place cleartext http carries no network risk.
fn is_loopback_url(raw: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// Upper bound on every configured period (`interval_secs`, `timeout_secs`,
/// `expect_every_secs`): 30 days. Nothing sensible is slower, and a typo'd
/// huge value would overflow `Instant` arithmetic in the tick timers - a panic
/// (which aborts the daemon in a release build) instead of a load error.
const MAX_PERIOD_SECS: u64 = 30 * 24 * 3600;

/// A period in seconds must be positive and at most [`MAX_PERIOD_SECS`].
fn validate_period(label: &str, secs: u64) -> anyhow::Result<()> {
    anyhow::ensure!(secs > 0, "{label} must be > 0");
    anyhow::ensure!(
        secs <= MAX_PERIOD_SECS,
        "{label} must be at most {MAX_PERIOD_SECS} (30 days)"
    );
    Ok(())
}

/// The longest `host` label: a machine's short name, not a description.
const MAX_HOST_CHARS: usize = 64;

/// A `host` label (on a monitor, a peer or `[health]`) is free text, kept
/// short and printable: it travels in a query string, in a heartbeat and
/// into the peers' logs, and two labels must compare equal when they read
/// the same - so no blank, no surrounding space and no control character.
pub(super) fn validate_host_label(label: &str, host: Option<&str>) -> anyhow::Result<()> {
    let Some(host) = host else { return Ok(()) };
    anyhow::ensure!(!host.is_empty(), "{label} must not be empty");
    anyhow::ensure!(
        host.trim() == host && !host.chars().any(char::is_control),
        "{label} {host:?} must not start or end with a space, nor hold control characters"
    );
    anyhow::ensure!(
        host.chars().count() <= MAX_HOST_CHARS,
        "{label} must be at most {MAX_HOST_CHARS} characters"
    );
    Ok(())
}

/// Ids that appear in URLs (`/api/badge/{id}`, `/api/push/{id}`) must stay
/// URL-safe.
fn is_url_safe_id(id: &str) -> bool {
    id.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Every `notify` route must name a configured channel.
fn validate_routes(
    owner: &str,
    routes: Option<&[String]>,
    channel_names: &HashSet<&str>,
) -> anyhow::Result<()> {
    for route in routes.unwrap_or_default() {
        anyhow::ensure!(
            channel_names.contains(route.as_str()),
            "{owner}: notify references unknown channel {route:?}"
        );
    }
    Ok(())
}

pub(super) fn validate(config: &Config) -> anyhow::Result<()> {
    validate_token("server.auth_token", config.server.auth_token.as_ref())?;
    validate_token("server.view_token", config.server.view_token.as_ref())?;
    // The same secret in both would make the "read-only" token the operator's:
    // the operator token is matched first.
    if let (Some(operator), Some(view)) = (&config.server.auth_token, &config.server.view_token) {
        anyhow::ensure!(
            operator != view,
            "server.view_token must differ from server.auth_token (it is the read-only token)"
        );
    }
    for (group, token) in &config.server.group_tokens {
        validate_token(&format!("server.group_tokens[{group:?}]"), Some(token))?;
        // A token for a group no monitor belongs to guards nothing - that is
        // a typo'd group name, not a future plan.
        anyhow::ensure!(
            config
                .monitors
                .iter()
                .any(|monitor| monitor.group.as_deref() == Some(group)),
            "server.group_tokens: no monitor belongs to group {group:?}"
        );
    }
    let channel_names = validate_channels(config)?;

    for window in &config.maintenance {
        anyhow::ensure!(
            window.start < window.end,
            "maintenance window {:?}: start must be before end",
            window.title
        );
    }

    let mut seen = HashSet::new();
    for monitor in &config.monitors {
        anyhow::ensure!(!monitor.id.is_empty(), "monitor id must not be empty");
        // The id appears in URLs (`/api/badge/{id}`, `/api/push/{id}`), so keep it
        // URL-safe.
        anyhow::ensure!(
            is_url_safe_id(&monitor.id),
            "monitor id {:?} must be alphanumeric, '-' or '_'",
            monitor.id
        );
        anyhow::ensure!(
            seen.insert(monitor.id.as_str()),
            "duplicate monitor id: {}",
            monitor.id
        );
        validate_period(
            &format!("monitor {} interval_secs", monitor.id),
            monitor.interval_secs,
        )?;
        validate_period(
            &format!("monitor {} timeout_secs", monitor.id),
            monitor.timeout_secs,
        )?;
        // A probe outliving its interval delays the next tick (the scheduler
        // never overlaps probes), so the effective cadence is the timeout,
        // not the interval. Push monitors probe nothing; their timeout is inert.
        if monitor.kind() != Kind::Push && monitor.timeout_secs > monitor.interval_secs {
            tracing::warn!(
                "monitor {}: timeout_secs ({}) exceeds interval_secs ({}) - a hung probe \
                 delays the next check by up to the timeout",
                monitor.id,
                monitor.timeout_secs,
                monitor.interval_secs
            );
        }
        // A private monitor without a configured token would be visible to no
        // one at all - fail fast rather than silently hide it. (Emptiness is
        // already rejected by validate_token above, so presence is enough.)
        anyhow::ensure!(
            monitor.public
                || config.server.auth_token.is_some()
                || config.server.view_token.is_some(),
            "monitor {}: public = false requires server.auth_token or server.view_token \
             to be set",
            monitor.id
        );
        // Anonymous viewers never see a private monitor, so the opt-in is inert;
        // warn rather than fail, since `public` may be toggled temporarily.
        if monitor.public_error_detail && !monitor.public {
            tracing::warn!(
                "monitor {}: public_error_detail has no effect on a private monitor",
                monitor.id
            );
        }
        validate_token(
            &format!("monitor {}: push_token", monitor.id),
            monitor.push_token.as_ref(),
        )?;
        validate_push_auth(monitor)?;
        validate_routes(
            &format!("monitor {}", monitor.id),
            monitor.notify.as_deref(),
            &channel_names,
        )?;
    }

    crate::topology::validate_dag(&config.monitors)?;

    validate_peers(config, &seen, &channel_names)?;
    validate_hosts(config)?;

    validate_confirm(config)?;
    validate_exec(config)?;
    validate_digest(config, &channel_names)?;
    validate_routes("updates", config.updates.notify.as_deref(), &channel_names)?;
    validate_routes(
        "alerts.notify_unconfirmed",
        config.alerts.notify_unconfirmed.as_deref(),
        &channel_names,
    )?;
    Ok(())
}

/// Without a token the id alone authorizes /api/push/{id}, and ids are not
/// secrets (a public monitor's id is served on the page and API): anyone
/// could forge heartbeats to mask an outage. So a push monitor names its
/// token, or says in so many words that it does without one - a bare id may
/// be acceptable on a private network, and then it still warns.
fn validate_push_auth(monitor: &super::Monitor) -> anyhow::Result<()> {
    let push = monitor.kind() == Kind::Push;
    anyhow::ensure!(
        push || !monitor.allow_unauthenticated,
        "monitor {}: allow_unauthenticated only applies to push monitors",
        monitor.id
    );
    anyhow::ensure!(
        !(monitor.allow_unauthenticated && monitor.push_token.is_some()),
        "monitor {}: allow_unauthenticated contradicts push_token - keep one",
        monitor.id
    );
    if push && monitor.push_token.is_none() {
        anyhow::ensure!(
            monitor.allow_unauthenticated,
            "monitor {}: a push monitor requires a push_token (its id appears on the \
             status page/API, so the id alone must not authorize heartbeats); set \
             push_token, or allow_unauthenticated = true to accept anyone who can reach \
             /api/push",
            monitor.id
        );
        tracing::warn!(
            "monitor {}: push monitor accepts unauthenticated heartbeats \
             (allow_unauthenticated) - its id appears on the status page/API, so anyone \
             who can reach /api/push can forge them",
            monitor.id
        );
    }
    Ok(())
}

/// Multi-vantage confirmation needs peers that can actually be asked: a
/// `[health]` section and at least one peer with a `ping_url` (its API origin).
/// A flag that can never confirm anything is a config mistake, not a plan.
fn validate_confirm(config: &Config) -> anyhow::Result<()> {
    let confirmable_peers = config.peers.iter().any(|peer| peer.probe_url().is_some());
    let confirm_requested = config
        .health
        .as_ref()
        .is_some_and(|health| health.confirm_with_peers)
        || config
            .monitors
            .iter()
            .any(|monitor| monitor.confirm_with_peers == Some(true));
    if confirm_requested {
        anyhow::ensure!(
            config.health.is_some(),
            "confirm_with_peers requires a [health] section"
        );
        anyhow::ensure!(
            confirmable_peers,
            "confirm_with_peers requires at least one peer with a ping_url \
             (its API origin is where probe requests go)"
        );
        // A peer's "seen up" turns a down into a local-only one. Over
        // cleartext http anyone on the path can write that answer (and read
        // the token that asks for it), so a peer that confirms must be
        // reached over https - or on this very host, where there is no path.
        for peer in &config.peers {
            let Some(url) = &peer.ping_url else { continue };
            anyhow::ensure!(
                !url.as_ref().starts_with("http://") || is_loopback_url(url.as_ref()),
                "peer {}: confirm_with_peers requires an https ping_url (over http anyone \
                 on the network path can forge the peer's answer and hide an outage); \
                 use https, or turn confirm_with_peers off",
                peer.id
            );
        }
    }
    for monitor in &config.monitors {
        anyhow::ensure!(
            !(monitor.confirm_with_peers == Some(true)
                && matches!(monitor.kind(), Kind::Push | Kind::Exec)),
            "monitor {}: confirm_with_peers needs a network probe (push monitors \
             have no target, and exec checks are local to this host)",
            monitor.id
        );
    }
    Ok(())
}

/// Exec probes are gated on `HORA_EXEC_DIR` (deployment-level consent): a
/// config declaring one without the variable - or with a directory that does
/// not exist - fails at load, not at the first probe.
fn validate_exec(config: &Config) -> anyhow::Result<()> {
    if !config.monitors.iter().any(|m| m.kind() == Kind::Exec) {
        return Ok(());
    }
    let Some(dir) = &config.exec_dir else {
        anyhow::bail!(
            "exec monitors require the HORA_EXEC_DIR environment variable \
             (the directory whose executables they may run)"
        );
    };
    anyhow::ensure!(
        dir.is_dir(),
        "HORA_EXEC_DIR {} is not a directory",
        dir.display()
    );
    Ok(())
}

/// Digest: the cron must parse at load (not at the first missed send), and
/// routes must name real channels, like a monitor's `notify`.
fn validate_digest(
    config: &Config,
    channel_names: &std::collections::HashSet<&str>,
) -> anyhow::Result<()> {
    if let Some(digest) = &config.digest {
        parse_cron(&digest.schedule).map_err(|err| {
            anyhow::anyhow!("digest: invalid schedule {:?}: {err}", digest.schedule)
        })?;
        validate_routes("digest", digest.notify.as_deref(), channel_names)?;
    }
    Ok(())
}

/// Validate the notification channels (unique non-empty names; a warning for
/// one disabled by an empty required field, and for a cleartext URL) and
/// return the set of valid channel names for route checks.
fn validate_channels(config: &Config) -> anyhow::Result<HashSet<&str>> {
    let mut channel_names = HashSet::new();
    for channel in &config.channels {
        anyhow::ensure!(!channel.name().is_empty(), "channel name must not be empty");
        anyhow::ensure!(
            channel_names.insert(channel.name()),
            "duplicate channel name: {}",
            channel.name()
        );
        // Not an error: the other channels still deliver, and the value can
        // be set and the config reloaded while the daemon runs. But never
        // silent - a channel that goes away unnoticed is an outage unheard.
        if let Some(field) = channel.empty_required_field() {
            tracing::warn!(
                "channel {}: {field} is empty, the channel is disabled until it is set",
                channel.name()
            );
        }
        // These channels send credentials over the configured URL (in the URL
        // itself, or - for Matrix - in a header); warn on cleartext http.
        let url = match channel {
            Channel::Discord { webhook_url, .. } | Channel::Slack { webhook_url, .. } => {
                Some(webhook_url.as_ref())
            }
            Channel::Webhook { url, .. }
            | Channel::Ntfy { url, .. }
            | Channel::Gotify { url, .. } => Some(url.as_ref()),
            Channel::Matrix { homeserver, .. } => Some(homeserver.as_str()),
            Channel::Telegram { .. }
            | Channel::FreeMobile { .. }
            | Channel::Email { .. }
            | Channel::Pushover { .. } => None,
        };
        if url.is_some_and(|url| url.starts_with("http://")) {
            tracing::warn!(
                "channel {}: URL uses http:// - credentials are sent in cleartext, use https",
                channel.name()
            );
        }
    }
    Ok(channel_names)
}

/// The `host` labels of the node and of its peers (a monitor's is checked
/// with the monitor), and what each peer may announce for its own.
fn validate_hosts(config: &Config) -> anyhow::Result<()> {
    if let Some(health) = &config.health {
        validate_host_label("health.host", health.host.as_deref())?;
    }
    for peer in &config.peers {
        validate_host_label(&format!("peer {}: host", peer.id), peer.host.as_deref())?;
        validate_accept_maintenance(peer)?;
    }
    Ok(())
}

/// `accept_maintenance_secs` lets a peer mute alerts on this node, so it is
/// only taken from a peer that proves who it is. The announcements ride the
/// heartbeat: the peer must be watched, and its heartbeat authenticated by a
/// `listen_token` - with `allow_unauthenticated` anyone who can reach
/// `/api/push` could declare the maintenance that hides an outage. The
/// bound is the longest window the API plans (7 days): more could never be
/// used, and a typo'd huge value should not read as a plan.
fn validate_accept_maintenance(peer: &super::Peer) -> anyhow::Result<()> {
    let Some(secs) = peer.accept_maintenance_secs else {
        return Ok(());
    };
    anyhow::ensure!(
        secs > 0,
        "peer {}: accept_maintenance_secs must be > 0 (remove the key to ignore the \
         peer's maintenance announcements)",
        peer.id
    );
    anyhow::ensure!(
        secs <= crate::MAX_SILENCE_SECS,
        "peer {}: accept_maintenance_secs must be at most {} (7 days)",
        peer.id,
        crate::MAX_SILENCE_SECS
    );
    anyhow::ensure!(
        !peer.allow_unauthenticated,
        "peer {}: accept_maintenance_secs cannot be combined with allow_unauthenticated \
         (anyone who can reach /api/push could then declare a maintenance that mutes \
         alerts here); set a listen_token",
        peer.id
    );
    anyhow::ensure!(
        peer.is_watched(),
        "peer {}: accept_maintenance_secs needs expect_every_secs (the announcements \
         arrive on the peer's heartbeat)",
        peer.id
    );
    Ok(())
}

/// Validate the `[health]` section and the `[[peers]]` mesh: peers require a
/// health section; a watched peer's `listen_id` must be URL-safe and clash with
/// neither a monitor nor another peer; and every peer must do something (ping out,
/// be watched, or both).
fn validate_peers(
    config: &Config,
    monitor_ids: &HashSet<&str>,
    channel_names: &HashSet<&str>,
) -> anyhow::Result<()> {
    if let Some(health) = &config.health {
        anyhow::ensure!(!health.id.is_empty(), "health.id must not be empty");
        validate_period("health.interval_secs", health.interval_secs)?;
        if let Some(url) = &health.heartbeat_url {
            // Checked at load, not at the first ping an interval later; and
            // without quoting the value, which usually carries a secret path.
            let parsed = reqwest::Url::parse(url.as_ref()).ok();
            anyhow::ensure!(
                parsed.as_ref().is_some_and(|url| {
                    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
                }),
                "health.heartbeat_url must be an http(s) URL with a host"
            );
        }
    }
    anyhow::ensure!(
        config.peers.is_empty() || config.health.is_some(),
        "[[peers]] require a [health] section (it sets this node's id and heartbeat interval)"
    );

    let mut peer_ids: HashSet<&str> = HashSet::new();
    let mut listen_ids: HashSet<&str> = HashSet::new();
    for peer in &config.peers {
        anyhow::ensure!(!peer.id.is_empty(), "peer id must not be empty");
        anyhow::ensure!(
            peer_ids.insert(peer.id.as_str()),
            "duplicate peer id: {}",
            peer.id
        );
        anyhow::ensure!(
            peer.ping_url.is_some() || peer.is_watched(),
            "peer {}: set ping_url (to heartbeat it) and/or expect_every_secs (to watch it)",
            peer.id
        );
        validate_token(
            &format!("peer {}: listen_token", peer.id),
            peer.listen_token.as_ref(),
        )?;
        validate_outbound_token(
            &format!("peer {}: ping_token", peer.id),
            peer.ping_token.as_ref(),
        )?;
        anyhow::ensure!(
            peer.is_watched() || !peer.allow_unauthenticated,
            "peer {}: allow_unauthenticated only applies to a watched peer (expect_every_secs)",
            peer.id
        );
        anyhow::ensure!(
            !(peer.allow_unauthenticated && peer.listen_token.is_some()),
            "peer {}: allow_unauthenticated contradicts listen_token - keep one",
            peer.id
        );
        if let Some(every) = peer.expect_every_secs {
            validate_period(&format!("peer {}: expect_every_secs", peer.id), every)?;
            // Same reasoning as a push monitor's token: peer ids are exposed
            // on the unauthenticated /healthz (witnesses need them), so an
            // unprotected listen id lets anyone forge the peer's heartbeats
            // and hide that it is down.
            if peer.listen_token.is_none() {
                anyhow::ensure!(
                    peer.allow_unauthenticated,
                    "peer {}: a watched peer requires a listen_token (its id appears in \
                     /healthz, so the id alone must not authorize heartbeats); set \
                     listen_token, or allow_unauthenticated = true to accept anyone who \
                     can reach /api/push",
                    peer.id
                );
                tracing::warn!(
                    "peer {}: watched without a listen_token (allow_unauthenticated) - its \
                     id appears in /healthz, so anyone who can reach /api/push can forge \
                     its heartbeats",
                    peer.id
                );
            }
            // The id appears in `/api/push/{id}`, so keep it URL-safe.
            let listen_id = peer.listen_id();
            anyhow::ensure!(
                is_url_safe_id(listen_id),
                "peer {}: listen_id {listen_id:?} must be alphanumeric, '-' or '_'",
                peer.id
            );
            anyhow::ensure!(
                !monitor_ids.contains(listen_id),
                "peer {}: listen_id {listen_id:?} clashes with a monitor id",
                peer.id
            );
            anyhow::ensure!(
                listen_ids.insert(listen_id),
                "peer {}: duplicate listen_id {listen_id:?}",
                peer.id
            );
        }
        validate_routes(
            &format!("peer {}", peer.id),
            peer.notify.as_deref(),
            channel_names,
        )?;
        // A push token sent over cleartext http to the peer would leak in transit.
        if let (Some(url), Some(_token)) = (&peer.ping_url, &peer.ping_token)
            && url.as_ref().starts_with("http://")
        {
            tracing::warn!(
                "peer {}: ping_url uses http:// - the push token is sent in cleartext, use https",
                peer.id
            );
        }
    }
    Ok(())
}
