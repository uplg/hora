//! Configuration: parsed from a TOML file, with environment overrides for secrets.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use chrono::{DateTime, Utc};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub page: Page,
    pub server: Server,
    /// Named notification channels. Monitors route to them by name.
    #[serde(default)]
    pub channels: Vec<Channel>,
    /// Scheduled maintenance windows that mute alerts.
    #[serde(default)]
    pub maintenance: Vec<Maintenance>,
    /// Incidents / announcements shown as a banner on the status page.
    #[serde(default)]
    pub incidents: Vec<Incident>,
    #[serde(default)]
    pub alerts: Alerts,
    #[serde(default)]
    pub monitors: Vec<Monitor>,
    /// Outbound dead-man heartbeat + peer-watch settings. Absent = this node
    /// neither emits heartbeats nor watches peers.
    #[serde(default)]
    pub health: Option<Health>,
    /// Peers in a mutual-surveillance mesh. Each entry pings the peer (OUT) and/or
    /// watches it for missed pings (IN); requires a [`Health`] section.
    #[serde(default)]
    pub peers: Vec<Peer>,
    /// Periodic recap through the notification channels ("this week: 99.97%,
    /// 2 incidents, budget 18m of 43m left"). Absent = no digest.
    #[serde(default)]
    pub digest: Option<Digest>,
}

impl Config {
    /// The maintenance window covering `monitor_id` at `now`, if any. A window
    /// with no `monitors` list covers all of them.
    #[must_use]
    pub fn active_maintenance(&self, monitor_id: &str, now: DateTime<Utc>) -> Option<&Maintenance> {
        self.maintenance.iter().find(|window| {
            now >= window.start
                && now <= window.end
                && (window.monitors.is_empty() || window.monitors.iter().any(|id| id == monitor_id))
        })
    }

    /// Whether `monitor_id` is inside an active maintenance window (alerts muted).
    #[must_use]
    pub fn in_maintenance(&self, monitor_id: &str, now: DateTime<Utc>) -> bool {
        self.active_maintenance(monitor_id, now).is_some()
    }
}

/// The periodic digest: a recap of the last seven days per monitor (uptime,
/// incidents, error budget), sent through the notification channels on a cron
/// schedule. Zero false-positive risk by construction - it reports, it never
/// alerts.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Digest {
    /// When to send, five-field cron, UTC. Default: Monday 08:00.
    #[serde(default = "default_digest_schedule")]
    pub schedule: String,
    /// Channels to send through; absent = every configured channel.
    #[serde(default)]
    pub notify: Option<Vec<String>>,
}

fn default_digest_schedule() -> String {
    "0 8 * * 1".to_owned()
}

/// A scheduled maintenance window: alerts for the affected monitors are muted
/// between `start` and `end` (RFC 3339 timestamps, quoted in TOML).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Maintenance {
    #[serde(default)]
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Monitors covered by the window; empty = all monitors.
    #[serde(default)]
    pub monitors: Vec<String>,
}

/// Severity of an [`Incident`], controlling its banner colour.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    #[default]
    Info,
    Warning,
    Critical,
    Resolved,
}

impl Severity {
    /// Lowercase name, used as a CSS class and in the API.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Critical => "critical",
            Self::Resolved => "resolved",
        }
    }
}

/// A posted incident or announcement shown on the status page.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Incident {
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub severity: Severity,
    /// When it was posted (RFC 3339, quoted in TOML); shown if present.
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Page {
    #[serde(default = "default_title")]
    pub title: String,
    #[serde(default = "default_history_days")]
    pub history_days: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_database_path")]
    pub database_path: String,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Trust this request header for the client IP when rate limiting (e.g.
    /// `cf-connecting-ip` behind Cloudflare). Only safe when a proxy you control
    /// sets it and direct access to the origin is blocked - otherwise clients can
    /// forge it. Unset = smart detection (x-forwarded-for / x-real-ip / peer).
    #[serde(default)]
    pub client_ip_header: Option<String>,
    /// Per-IP API rate limit: one request slot is replenished every N seconds.
    #[serde(default = "default_rate_limit_refill")]
    pub rate_limit_refill_secs: u64,
    /// Per-IP API rate limit: maximum burst of requests.
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,
    /// Token required to view private monitors (those with `public = false`).
    /// Sent as `Authorization: Bearer <token>` or `?token=` query parameter.
    #[serde(default)]
    pub auth_token: Option<Secret>,
}

/// A named notification channel. Several channels may share a `type` (e.g. two
/// Discord webhooks), and a monitor routes to specific ones by `name`.
#[derive(Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Channel {
    Telegram {
        name: String,
        token: Secret,
        chat_id: String,
    },
    Discord {
        name: String,
        webhook_url: Secret,
    },
    Slack {
        name: String,
        webhook_url: Secret,
    },
    Webhook {
        name: String,
        url: Secret,
    },
    Matrix {
        name: String,
        /// Homeserver base URL, e.g. `https://matrix.org`.
        homeserver: String,
        /// Access token of the sending (bot) user.
        token: Secret,
        /// Target room id, e.g. `!abcdef:matrix.org`.
        room_id: String,
    },
    FreeMobile {
        name: String,
        /// Free Mobile account identifier (the login from the SMS-notifications option).
        user: String,
        /// API key generated in the Free Mobile "Notifications par SMS" option.
        pass: Secret,
    },
    Email {
        name: String,
        host: String,
        #[serde(default = "default_smtp_port")]
        port: u16,
        #[serde(default)]
        username: String,
        #[serde(default)]
        password: Secret,
        from: String,
        to: String,
        /// Implicit TLS (port 465) instead of STARTTLS (the default, port 587).
        #[serde(default)]
        implicit_tls: bool,
    },
    Ntfy {
        name: String,
        /// ntfy topic URL, e.g. `https://ntfy.sh/my-topic`.
        url: Secret,
        /// Optional access token for private ntfy servers.
        #[serde(default)]
        token: Option<Secret>,
    },
    Gotify {
        name: String,
        /// Gotify server URL, e.g. `https://gotify.example.com`.
        url: Secret,
        /// Application token.
        token: Secret,
    },
    Pushover {
        name: String,
        /// Pushover API token (application key).
        token: Secret,
        /// Pushover user key (recipient).
        user: Secret,
    },
}

impl Channel {
    /// The routing name a monitor refers to.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Telegram { name, .. }
            | Self::Discord { name, .. }
            | Self::Slack { name, .. }
            | Self::Webhook { name, .. }
            | Self::Matrix { name, .. }
            | Self::FreeMobile { name, .. }
            | Self::Email { name, .. }
            | Self::Ntfy { name, .. }
            | Self::Gotify { name, .. }
            | Self::Pushover { name, .. } => name,
        }
    }

    /// Whether the channel's required secret is present (an empty one - e.g. an
    /// unset `${VAR}` - disables the channel rather than erroring at send time).
    #[must_use]
    pub fn is_configured(&self) -> bool {
        match self {
            Self::Telegram { token, chat_id, .. } => !token.is_empty() && !chat_id.is_empty(),
            Self::Discord { webhook_url, .. } | Self::Slack { webhook_url, .. } => {
                !webhook_url.is_empty()
            }
            Self::Webhook { url, .. } | Self::Ntfy { url, .. } => !url.is_empty(),
            Self::Matrix {
                homeserver,
                token,
                room_id,
                ..
            } => !homeserver.is_empty() && !token.is_empty() && !room_id.is_empty(),
            Self::FreeMobile { user, pass, .. } => !user.is_empty() && !pass.is_empty(),
            Self::Email { host, from, to, .. } => {
                !host.is_empty() && !from.is_empty() && !to.is_empty()
            }
            Self::Gotify { url, token, .. } => !url.is_empty() && !token.is_empty(),
            Self::Pushover { token, user, .. } => !token.is_empty() && !user.is_empty(),
        }
    }
}

// Manual `Debug` so channel secrets (tokens, webhook URLs) never leak through a
// `{config:?}` in a log line or panic message.
impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Telegram {
                name,
                token,
                chat_id,
            } => f
                .debug_struct("Telegram")
                .field("name", name)
                .field("token", token)
                .field("chat_id", chat_id)
                .finish(),
            Self::Discord { name, webhook_url } => f
                .debug_struct("Discord")
                .field("name", name)
                .field("webhook_url", webhook_url)
                .finish(),
            Self::Slack { name, webhook_url } => f
                .debug_struct("Slack")
                .field("name", name)
                .field("webhook_url", webhook_url)
                .finish(),
            Self::Webhook { name, url } => f
                .debug_struct("Webhook")
                .field("name", name)
                .field("url", url)
                .finish(),
            Self::Matrix {
                name,
                homeserver,
                token,
                room_id,
            } => f
                .debug_struct("Matrix")
                .field("name", name)
                .field("homeserver", homeserver)
                .field("token", token)
                .field("room_id", room_id)
                .finish(),
            Self::FreeMobile { name, user, pass } => f
                .debug_struct("FreeMobile")
                .field("name", name)
                .field("user", user)
                .field("pass", pass)
                .finish(),
            Self::Email {
                name,
                host,
                port,
                username,
                password,
                from,
                to,
                implicit_tls,
            } => f
                .debug_struct("Email")
                .field("name", name)
                .field("host", host)
                .field("port", port)
                .field("username", username)
                .field("password", password)
                .field("from", from)
                .field("to", to)
                .field("implicit_tls", implicit_tls)
                .finish(),
            Self::Ntfy { name, url, token } => f
                .debug_struct("Ntfy")
                .field("name", name)
                .field("url", url)
                .field("token", token)
                .finish(),
            Self::Gotify { name, url, token } => f
                .debug_struct("Gotify")
                .field("name", name)
                .field("url", url)
                .field("token", token)
                .finish(),
            Self::Pushover { name, token, user } => f
                .debug_struct("Pushover")
                .field("name", name)
                .field("token", token)
                .field("user", user)
                .finish(),
        }
    }
}

fn default_smtp_port() -> u16 {
    587
}

/// Render a secret for `Debug`: `<unset>` when empty, `<redacted>` otherwise.
fn redacted(secret: &str) -> &'static str {
    if secret.is_empty() {
        "<unset>"
    } else {
        "<redacted>"
    }
}

/// A configuration string that is redacted from `Debug` output, so a secret in a
/// `Debug`-derived struct (e.g. a monitor's push token) never reaches the logs.
#[derive(Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(redacted(&self.0))
    }
}

// `AsRef`, not `Deref`: reading the secret must be explicit (`.as_ref()`), so it
// can't be coerced into a `Display` context (e.g. `info!("{}", *secret)`) by
// accident.
impl AsRef<str> for Secret {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Secret {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Alerting and retention policy, independent of any notification channel.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alerts {
    /// Consecutive failed checks before a monitor is alerted as down.
    #[serde(default = "default_threshold")]
    pub fail_threshold: u32,
    /// Also alert when a monitor is *degraded* - up, but slower than its
    /// `degraded_over_ms` - not only when it is down. Uses the same
    /// `fail_threshold` (consecutive degraded checks). Off by default.
    #[serde(default)]
    pub alert_on_degraded: bool,
    /// Warn this many days before a TLS certificate expires.
    #[serde(default = "default_cert_expiry_days")]
    pub cert_expiry_days: u16,
    /// Warn this many days before a registered domain expires (RDAP); applies
    /// to monitors that opt in with `domain_expiry = "example.com"`.
    #[serde(default = "default_domain_expiry_days")]
    pub domain_expiry_days: u16,
    /// Default storage retention, overridable per monitor.
    #[serde(default = "default_retention_days")]
    pub default_retention_days: u16,
    /// Root-cause alert grouping window, seconds. A monitor confirmed down
    /// with a down upstream waits this long before alerting; if the upstream
    /// alerts first (or already has), the symptom is folded into that single
    /// notification. 0 disables grouping (every monitor alerts on its own,
    /// the pre-0.5 behaviour). Default 30.
    #[serde(default = "default_group_window")]
    pub group_window_secs: u64,
}

impl Default for Alerts {
    fn default() -> Self {
        Self {
            fail_threshold: default_threshold(),
            alert_on_degraded: false,
            cert_expiry_days: default_cert_expiry_days(),
            domain_expiry_days: default_domain_expiry_days(),
            default_retention_days: default_retention_days(),
            group_window_secs: default_group_window(),
        }
    }
}

/// Node-level dead-man / peer settings. A node with a `[health]` section emits an
/// outbound heartbeat to its peers (and any `heartbeat_url`) whenever it is
/// locally healthy, and exposes its view of those peers on `/healthz`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Health {
    /// This node's global identity. Peers refer to it by this id, and it is the
    /// key under which other nodes report this node in their `/healthz` `peers`.
    pub id: String,
    /// Cadence of outbound heartbeats, in seconds.
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    /// After startup, a peer that has never been seen is not alerted down until
    /// this many seconds have passed (it stays `unknown`).
    #[serde(default = "default_grace")]
    pub grace_secs: u64,
    /// Before alerting a peer down, poll the other peers' `/healthz`: if any still
    /// sees it up, treat it as a partition (`PeerLinkDegraded`) instead of an
    /// outage. With fewer than two other peers this is a no-op (honest mode).
    #[serde(default)]
    pub quorum: bool,
    /// An extra dead-man target (e.g. a healthchecks.io ping URL) pinged every
    /// `interval_secs` when locally healthy, alongside the peers' `ping_url`.
    #[serde(default)]
    pub heartbeat_url: Option<Secret>,
}

impl Health {
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }
}

/// A peer in a mutual-surveillance mesh. The two halves are independent: `ping_url`
/// is the OUT side (I heartbeat the peer when healthy); `expect_every_secs` enables
/// the IN side (I mark the peer down if it stops heartbeating me). Either or both
/// may be set; each half can terminate at another Hora or at an external service.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// The peer's global identity (its `[health].id`). Witnesses key their view of
    /// the peer by this id, so it must match across the mesh.
    pub id: String,
    pub name: String,
    /// OUT: where I `POST` my heartbeat when healthy - the peer's
    /// `/api/push/{my-id}`, or an external receiver. Absent = I don't ping it.
    #[serde(default)]
    pub ping_url: Option<Secret>,
    /// Token sent as the `X-Push-Token` header on the outbound ping (kept out of
    /// the peer's access logs, unlike a `?token=` query).
    #[serde(default)]
    pub ping_token: Option<Secret>,
    /// IN: the local push id the peer heartbeats (`/api/push/{listen_id}`).
    /// Defaults to `id`. Only meaningful when `expect_every_secs` is set.
    #[serde(default)]
    pub listen_id: Option<String>,
    /// Token required on the inbound `/api/push/{listen_id}` (the peer's
    /// `ping_token` for me).
    #[serde(default)]
    pub listen_token: Option<Secret>,
    /// Watch the peer: mark it down if no inbound heartbeat arrives within this
    /// window. Setting it enables the IN side; leaving it unset makes this an
    /// OUT-only peer (a plain dead-man to an external receiver).
    #[serde(default)]
    pub expect_every_secs: Option<u64>,
    /// Where other peers poll this peer's `/healthz` for quorum. Defaults to the
    /// origin (`scheme://host[:port]`) of `ping_url` plus `/healthz`.
    #[serde(default)]
    pub witness_url: Option<String>,
    /// Restrict this peer's alerts to these channel names. Unset = every channel.
    #[serde(default)]
    pub notify: Option<Vec<String>>,
}

impl Peer {
    /// The local push id this peer heartbeats (falls back to the peer's `id`).
    #[must_use]
    pub fn listen_id(&self) -> &str {
        self.listen_id.as_deref().unwrap_or(&self.id)
    }

    /// Whether the IN side is active (I watch this peer for missed heartbeats).
    #[must_use]
    pub fn is_watched(&self) -> bool {
        self.expect_every_secs.is_some()
    }

    /// The inbound staleness window, defaulting to the global health interval.
    #[must_use]
    pub fn expect_every(&self) -> Option<Duration> {
        self.expect_every_secs.map(Duration::from_secs)
    }

    /// The peer's `/healthz` URL for quorum: the explicit `witness_url`, else the
    /// origin (`scheme://host[:port]`) of `ping_url` with `/healthz` appended.
    #[must_use]
    pub fn effective_witness_url(&self) -> Option<String> {
        if let Some(url) = &self.witness_url {
            return Some(url.clone());
        }
        let ping = self.ping_url.as_ref()?;
        let url = reqwest::Url::parse(ping.as_ref()).ok()?;
        let origin = url.origin();
        origin
            .is_tuple()
            .then(|| format!("{}/healthz", origin.ascii_serialization()))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Http,
    Tcp,
    /// ICMP echo (ping). Target is a host or IP (no port). Uses an unprivileged
    /// datagram socket, so it works in rootless Docker without `CAP_NET_RAW`
    /// (the kernel's `net.ipv4.ping_group_range` must cover the process, which is
    /// the Docker default). IPv4 and IPv6 are both supported.
    Icmp,
    /// Passive heartbeat: the monitored job pings `/api/push/{id}`; missing a
    /// heartbeat within the interval marks it down. No active probing.
    Push,
    /// DNS resolution: resolve a name and assert the expected record type/value.
    /// Target is a hostname; `dns_expected` pins the answer (hijack detection).
    Dns,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    /// Probe target (URL for HTTP, `host:port` for TCP, host or IP for ICMP,
    /// hostname for DNS). Unused for push monitors.
    #[serde(default)]
    pub target: String,
    pub interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub expected_status: Option<u16>,
    #[serde(default)]
    pub degraded_over_ms: Option<i64>,
    /// Latency SLO objective in ms: the 24h p95 is flagged met/breached against it.
    #[serde(default)]
    pub slo_latency_ms: Option<i64>,
    /// Availability SLO target in percent (e.g. `99.9`), evaluated over
    /// [`slo_window_days`](Self::slo_window_days). Drives the error-budget
    /// display on the status page and the burn-rate alerts. Stored in basis
    /// points (99.9% = 9990) so the config stays `Eq`-comparable.
    #[serde(default, deserialize_with = "de_slo_uptime")]
    pub slo_uptime: Option<u32>,
    /// Window for the availability SLO, in days (default 30).
    #[serde(default)]
    pub slo_window_days: Option<u16>,
    /// Extra HTTP request headers (e.g. `Accept`, `Authorization`). HTTP monitors
    /// only. Values are redacted in `Debug` (a header may carry a credential).
    #[serde(default)]
    pub headers: HashMap<String, Secret>,
    /// HTTP body assertion: the response body must contain this text.
    #[serde(default)]
    pub keyword: Option<String>,
    /// Invert the [`keyword`](Self::keyword) check: the body must *not* contain it.
    #[serde(default)]
    pub keyword_invert: bool,
    /// HTTP body assertion: a `JSONPath` (RFC 9535) evaluated against a JSON response.
    #[serde(default)]
    pub json_query: Option<String>,
    /// Value the [`json_query`](Self::json_query) result must equal (string compare).
    /// When unset, the query only has to match at least one node.
    #[serde(default)]
    pub json_expected: Option<String>,
    /// Cap (KiB) on the response body read for keyword/JSON assertions
    /// (default 1024 = 1 MiB). Raise for large JSON endpoints, with care.
    #[serde(default)]
    pub max_body_kb: Option<u32>,
    /// Immediate re-probes after a failed check before recording it (default
    /// 1, max 5). A single network blip between the prober and the target
    /// thus never lands in the history - so the page, the uptime numbers and
    /// the error budget only count failures that survived a retry. Set 0 to
    /// record every raw probe result. Not applicable to push monitors.
    #[serde(default)]
    pub probe_retries: Option<u32>,
    /// Probe both address families and require both to pass - the classic
    /// silent failure is a service whose IPv6 has been dead for weeks behind a
    /// healthy IPv4. One broken family marks the monitor down, naming it in
    /// the reason ("IPv6 failing: connection timed out (IPv4 ok)"). For http,
    /// tcp and icmp monitors with a hostname target; not combinable with
    /// `proxy`. The probing host itself needs working IPv4 *and* IPv6.
    #[serde(default)]
    pub dual_stack: Option<bool>,
    /// Restrict this monitor's alerts to these channel names (e.g. `["ops"]`).
    /// Unset = every configured channel.
    #[serde(default)]
    pub notify: Option<Vec<String>>,
    /// Route this monitor's HTTP requests through a proxy (`http(s)://…` or
    /// `socks5://…`). HTTP monitors only.
    #[serde(default)]
    pub proxy: Option<String>,
    /// Push monitor only: secret required as `?token=` on `/api/push/{id}`.
    /// Unset = no token check (anyone who knows the id can heartbeat).
    #[serde(default)]
    pub push_token: Option<Secret>,
    /// Push monitor only: a five-field cron expression (UTC) for when heartbeats
    /// are expected (e.g. `"0 3 * * *"` for a nightly job). The monitor goes
    /// down only when a scheduled run misses its [`grace_secs`](Self::grace_secs)
    /// window - `interval_secs` then merely sets how often Hora re-evaluates.
    #[serde(default)]
    pub schedule: Option<String>,
    /// Push monitor with a [`schedule`](Self::schedule): how late (seconds) a
    /// heartbeat may arrive after its scheduled time before alerting.
    /// Default 1800 (30 minutes).
    #[serde(default)]
    pub grace_secs: Option<u64>,
    /// Override TLS certificate checking. Defaults to on for `https://` HTTP monitors.
    #[serde(default)]
    pub check_cert: Option<bool>,
    /// Override how long this monitor's checks are kept before pruning.
    #[serde(default)]
    pub retention_days: Option<u16>,
    /// Display group for the status page (monitors sharing a group are rendered
    /// together under a section header). Ungrouped monitors appear last.
    #[serde(default)]
    pub group: Option<String>,
    /// Upstream monitor ids this one depends on. When set, a down alert is
    /// annotated with the probable upstream cause (if any upstream is also
    /// down) or with the dependents it impacts (if this is a root cause).
    /// Validated as a DAG at load time.
    #[serde(default)]
    pub depends_on: Option<Vec<String>>,
    /// Whether this monitor appears on the public status page. `true` by default;
    /// set `false` to hide it from unauthenticated viewers (requires a token).
    #[serde(default = "default_true")]
    pub public: bool,
    /// Show this monitor's full failure detail to anonymous viewers. By default
    /// the public page collapses failure reasons to a safe category ("HTTP 500",
    /// "content check failed"), since the stored detail can carry response-body
    /// snippets, DNS answers or asserted keywords. Opt in per monitor when that
    /// detail is fine to publish (e.g. a push monitor whose `msg` is meant for
    /// the page). Authenticated viewers always see the full reason.
    #[serde(default)]
    pub public_error_detail: bool,
    /// DNS monitor: the record type to query (e.g. `"A"`, `"AAAA"`, `"CNAME"`).
    #[serde(default)]
    pub dns_record: Option<String>,
    /// DNS monitor: the expected answer, comma-separated when several records
    /// are pinned, compared order-insensitively (hijack detection). When unset,
    /// any non-empty answer counts as up - no change detection, since answers
    /// rotate freely behind CDNs and round-robin records.
    #[serde(default)]
    pub dns_expected: Option<String>,
    /// DNS monitor: custom resolver address (`host:port`). Unset = system default.
    #[serde(default)]
    pub dns_resolver: Option<String>,
    /// TLS certificate pinning: hex-encoded SHA-256 of the leaf public key. When
    /// set, an alert fires if the key changes (MITM / unexpected renewal detection).
    #[serde(default)]
    pub cert_pin: Option<String>,
    /// The monitor's *registered* domain ("example.com", not the subdomain the
    /// target uses), checked daily against the registry over RDAP; an alert
    /// fires `alerts.domain_expiry_days` before it expires. Explicit rather
    /// than derived from `target`: registrable-domain extraction needs a
    /// public-suffix list, and the operator already knows the answer.
    #[serde(default)]
    pub domain_expiry: Option<String>,
}

// Manual `Debug` (rather than derived) so credentials never leak: `target` and
// `proxy` may embed `user:pass@`, and `headers`/`push_token` are `Secret` (which
// self-redact). A `{:?}` of a `Monitor` - or of the whole `Config`, which derives
// `Debug` and holds a `Vec<Monitor>` - is therefore safe to log.
impl std::fmt::Debug for Monitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Monitor")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("target", &redact_url_credentials(&self.target))
            .field("interval_secs", &self.interval_secs)
            .field("timeout_secs", &self.timeout_secs)
            .field("expected_status", &self.expected_status)
            .field("degraded_over_ms", &self.degraded_over_ms)
            .field("slo_latency_ms", &self.slo_latency_ms)
            .field("slo_uptime", &self.slo_uptime)
            .field("slo_window_days", &self.slo_window_days)
            .field("headers", &self.headers)
            .field("keyword", &self.keyword)
            .field("keyword_invert", &self.keyword_invert)
            .field("json_query", &self.json_query)
            .field("json_expected", &self.json_expected)
            .field("max_body_kb", &self.max_body_kb)
            .field("probe_retries", &self.probe_retries)
            .field("dual_stack", &self.dual_stack)
            .field("notify", &self.notify)
            .field("proxy", &self.proxy.as_deref().map(redact_url_credentials))
            .field("push_token", &self.push_token)
            .field("schedule", &self.schedule)
            .field("grace_secs", &self.grace_secs)
            .field("check_cert", &self.check_cert)
            .field("retention_days", &self.retention_days)
            .field("group", &self.group)
            .field("depends_on", &self.depends_on)
            .field("public", &self.public)
            .field("public_error_detail", &self.public_error_detail)
            .field("dns_record", &self.dns_record)
            .field("dns_expected", &self.dns_expected)
            .field("dns_resolver", &self.dns_resolver)
            .field("cert_pin", &self.cert_pin)
            .field("domain_expiry", &self.domain_expiry)
            .finish()
    }
}

/// Mask any `user:pass@` credentials in a URL-like string for `Debug`, keeping
/// the host and path so logs stay useful. Inputs that don't parse as a URL (e.g.
/// a TCP `host:port` target) or carry no credentials are returned unchanged.
fn redact_url_credentials(raw: &str) -> std::borrow::Cow<'_, str> {
    match reqwest::Url::parse(raw) {
        Ok(mut url) if !url.username().is_empty() || url.password().is_some() => {
            // These setters only fail for cannot-be-a-base URLs, which never
            // carry credentials, so the guard above already excludes them.
            let _ = url.set_username("***");
            if url.password().is_some() {
                let _ = url.set_password(Some("***"));
            }
            std::borrow::Cow::Owned(url.to_string())
        }
        _ => std::borrow::Cow::Borrowed(raw),
    }
}

impl Monitor {
    #[must_use]
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs)
    }

    /// Whether this monitor probes both address families (default off).
    #[must_use]
    pub fn dual_stack(&self) -> bool {
        self.dual_stack.unwrap_or(false)
    }

    /// Whether this monitor should have its TLS certificate expiry checked.
    #[must_use]
    pub fn checks_cert(&self) -> bool {
        self.check_cert
            .unwrap_or_else(|| self.kind == Kind::Http && self.target.starts_with("https://"))
    }

    /// Effective storage retention in days, falling back to the global default.
    #[must_use]
    pub fn retention_days(&self, default: u16) -> u16 {
        self.retention_days.unwrap_or(default)
    }

    /// Byte cap for assertion body reads (falls back to the 1 MiB default).
    #[must_use]
    pub fn assertion_body_cap(&self) -> usize {
        const DEFAULT: usize = 1 << 20; // 1 MiB
        self.max_body_kb.map_or(DEFAULT, |kb| {
            usize::try_from(kb)
                .unwrap_or(usize::MAX)
                .saturating_mul(1024)
        })
    }

    /// Grace period for a scheduled push monitor (default 30 minutes).
    #[must_use]
    pub fn push_grace_secs(&self) -> u64 {
        self.grace_secs.unwrap_or(1800)
    }

    /// Immediate re-probes after a failure before recording it (default 1).
    #[must_use]
    pub fn probe_retries(&self) -> u32 {
        self.probe_retries.unwrap_or(1)
    }

    /// Availability SLO window in days (default 30).
    #[must_use]
    pub fn slo_window_days(&self) -> u16 {
        self.slo_window_days.unwrap_or(30)
    }
}

/// Accept the availability SLO as a percent float (`99.9`) and store basis
/// points (9990): the config structs stay `Eq` and no precision is lost for
/// the targets that occur in practice (two decimals).
fn de_slo_uptime<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let Some(percent) = Option::<f64>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if !percent.is_finite() || percent <= 0.0 || percent >= 100.0 {
        return Err(serde::de::Error::custom(
            "slo_uptime must be a percentage between 0 and 100 (exclusive), e.g. 99.9",
        ));
    }
    // Lossless float-to-int without a cast: the value is bounded (0, 10_000)
    // by the guard above, and this runs once at config load.
    let basis_points = format!("{:.0}", percent * 100.0)
        .parse::<u32>()
        .map_err(serde::de::Error::custom)?;
    Ok(Some(basis_points.clamp(1, 9999)))
}

fn default_title() -> String {
    "Status".to_owned()
}
fn default_history_days() -> u16 {
    90
}
fn default_bind() -> String {
    "127.0.0.1:8787".to_owned()
}
fn default_database_path() -> String {
    "hora.db".to_owned()
}
fn default_threshold() -> u32 {
    3
}
fn default_timeout() -> u64 {
    10
}
fn default_cert_expiry_days() -> u16 {
    14
}
fn default_domain_expiry_days() -> u16 {
    14
}
fn default_retention_days() -> u16 {
    90
}
fn default_rate_limit_refill() -> u64 {
    1
}
fn default_health_interval() -> u64 {
    60
}
fn default_grace() -> u64 {
    180
}
fn default_group_window() -> u64 {
    30
}
fn default_rate_limit_burst() -> u32 {
    30
}
fn default_true() -> bool {
    true
}

/// The configuration file path, from `$HORA_CONFIG` (default `./config.toml`).
#[must_use]
pub fn path() -> PathBuf {
    std::env::var_os("HORA_CONFIG").map_or_else(|| PathBuf::from("config.toml"), PathBuf::from)
}

/// Load the configuration from the default [`path`].
///
/// # Errors
///
/// Returns an error if the file cannot be read, parsed, or validated.
pub fn load() -> anyhow::Result<Config> {
    load_from(&path())
}

/// Load `path`, apply environment overrides, and validate.
///
/// # Errors
///
/// Returns an error if the file cannot be read, the TOML is invalid, or a
/// monitor is misconfigured (empty/duplicate id, zero interval).
pub fn load_from(path: &Path) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    parse(&raw)
}

/// Parse, env-override and validate a configuration from TOML text.
///
/// # Errors
///
/// Returns an error if the TOML is invalid or a monitor is misconfigured
/// (empty/duplicate id, zero interval).
pub fn parse(toml_str: &str) -> anyhow::Result<Config> {
    // Parse first, expand `${VAR}` afterwards - in string values only - so
    // secrets (channel tokens/URLs) can stay out of the file:
    // `webhook_url = "${OPS_DISCORD}"`. Expanding the raw text instead would
    // also hydrate comments (warning about unset variables in commented-out
    // examples) and let a syntax error echo an already-expanded secret back,
    // since TOML errors quote the offending line.
    let mut table: toml::Table = toml::from_str(toml_str).context("parsing config TOML")?;
    expand_env_table(&mut table);
    let mut config: Config = table.try_into().context("reading config values")?;
    apply_env_overrides(&mut config);
    // Canonicalize cert pins to lowercase hex so the comparison in
    // `cert::pin_alert` (against the lowercase `sha256_hex`) matches whatever
    // case the operator typed; `validate` then rejects a malformed pin outright.
    for monitor in &mut config.monitors {
        if let Some(pin) = &mut monitor.cert_pin {
            pin.make_ascii_lowercase();
        }
        // Domain names are case-insensitive; RDAP servers expect lowercase.
        if let Some(domain) = &mut monitor.domain_expiry {
            domain.make_ascii_lowercase();
        }
    }
    validate(&config)?;
    Ok(config)
}

/// Expand `${VAR}` in every string value of the document, recursively. Keys,
/// comments and non-string values are never touched, so a `${VAR}` in a
/// commented-out example neither warns nor pulls a secret into the file's
/// parse context.
fn expand_env_table(table: &mut toml::Table) {
    for (_key, value) in table.iter_mut() {
        expand_env_value(value);
    }
}

fn expand_env_value(value: &mut toml::Value) {
    match value {
        toml::Value::String(text) => {
            if text.contains('$') {
                *text = expand_env(text);
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                expand_env_value(item);
            }
        }
        toml::Value::Table(table) => expand_env_table(table),
        _ => {}
    }
}

/// Substitute `${VAR}` with the environment value (empty if unset). `$$` is a
/// literal `$`, so `$${id}` yields a literal `${id}`. Runs on already-parsed
/// string values, so the result needs no TOML escaping - a value with quotes
/// or newlines is stored verbatim and can't break parsing or inject config.
fn expand_env(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        if let Some(stripped) = after.strip_prefix('$') {
            out.push('$'); // `$$` escape.
            rest = stripped;
        } else if let Some(body) = after.strip_prefix('{') {
            if let Some(end) = body.find('}') {
                let name = &body[..end];
                if let Ok(value) = std::env::var(name) {
                    out.push_str(&value);
                } else {
                    tracing::warn!("config references unset environment variable {name:?}");
                }
                rest = &body[end + 1..];
            } else {
                out.push_str("${"); // No closing brace: emit literally.
                rest = body;
            }
        } else {
            out.push('$'); // A lone `$`.
            rest = after;
        }
    }
    out.push_str(rest);
    out
}

/// Infrastructure overrides honoured by the Docker image; secrets come from the
/// config (optionally via `${VAR}`), not from fixed variables.
fn apply_env_overrides(config: &mut Config) {
    if let Ok(bind) = std::env::var("HORA_BIND") {
        config.server.bind = bind;
    }
    if let Ok(path) = std::env::var("HORA_DATABASE_PATH") {
        config.server.database_path = path;
    }
}

/// A configured access token must be non-empty: an empty one (often the result
/// of an unset `${VAR}` expanding to "") would authorize a blank `?token=`, since
/// the constant-time compare treats `"" == ""` as a match. Short-but-set tokens
/// are the operator's call, so they only warn.
fn validate_token(label: &str, token: Option<&Secret>) -> anyhow::Result<()> {
    if let Some(token) = token {
        anyhow::ensure!(!token.is_empty(), "{label} must not be empty");
        if token.as_ref().len() < 16 {
            tracing::warn!("{label} is short (under 16 chars); prefer a long random token");
        }
    }
    Ok(())
}

fn validate(config: &Config) -> anyhow::Result<()> {
    validate_token("server.auth_token", config.server.auth_token.as_ref())?;
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
            monitor
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "monitor id {:?} must be alphanumeric, '-' or '_'",
            monitor.id
        );
        anyhow::ensure!(
            seen.insert(monitor.id.as_str()),
            "duplicate monitor id: {}",
            monitor.id
        );
        anyhow::ensure!(
            monitor.interval_secs > 0,
            "monitor {} interval_secs must be > 0",
            monitor.id
        );
        anyhow::ensure!(
            monitor.kind == Kind::Push || !monitor.target.is_empty(),
            "monitor {}: target must not be empty",
            monitor.id
        );
        anyhow::ensure!(
            monitor.timeout_secs > 0,
            "monitor {} timeout_secs must be > 0",
            monitor.id
        );
        // A private monitor without a configured token would be visible to no
        // one at all - fail fast rather than silently hide it. (Emptiness is
        // already rejected by validate_token above, so presence is enough.)
        anyhow::ensure!(
            monitor.public || config.server.auth_token.is_some(),
            "monitor {}: public = false requires server.auth_token to be set",
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
        // Without a token the id alone authorizes /api/push/{id}, and ids are
        // not secrets (a public monitor's id is served on the page and API):
        // anyone could forge heartbeats to mask an outage. Warn, don't fail -
        // a bare id may be acceptable on a private network.
        if monitor.kind == Kind::Push && monitor.push_token.is_none() {
            tracing::warn!(
                "monitor {}: push monitor has no push_token - its id appears on the \
                 status page/API, so anyone who can reach /api/push can forge heartbeats",
                monitor.id
            );
        }
        validate_monitor_io(monitor)?;
        if let Some(routes) = &monitor.notify {
            for route in routes {
                anyhow::ensure!(
                    channel_names.contains(route.as_str()),
                    "monitor {}: notify references unknown channel {route:?}",
                    monitor.id
                );
            }
        }
    }

    crate::topology::validate_dag(&config.monitors)?;

    validate_peers(config, &seen, &channel_names)?;

    // Digest: the cron must parse at load (not at the first missed send), and
    // routes must name real channels, like a monitor's `notify`.
    if let Some(digest) = &config.digest {
        digest.schedule.parse::<croner::Cron>().map_err(|err| {
            anyhow::anyhow!("digest: invalid schedule {:?}: {err}", digest.schedule)
        })?;
        if let Some(routes) = &digest.notify {
            for route in routes {
                anyhow::ensure!(
                    channel_names.contains(route.as_str()),
                    "digest: notify references unknown channel {route:?}"
                );
            }
        }
    }
    Ok(())
}

/// Validate the notification channels (unique non-empty names, cleartext-URL
/// warnings) and return the set of valid channel names for route checks.
fn validate_channels(config: &Config) -> anyhow::Result<HashSet<&str>> {
    let mut channel_names = HashSet::new();
    for channel in &config.channels {
        anyhow::ensure!(!channel.name().is_empty(), "channel name must not be empty");
        anyhow::ensure!(
            channel_names.insert(channel.name()),
            "duplicate channel name: {}",
            channel.name()
        );
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
        anyhow::ensure!(health.interval_secs > 0, "health.interval_secs must be > 0");
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
        validate_token(
            &format!("peer {}: ping_token", peer.id),
            peer.ping_token.as_ref(),
        )?;
        if let Some(every) = peer.expect_every_secs {
            anyhow::ensure!(every > 0, "peer {}: expect_every_secs must be > 0", peer.id);
            // Same reasoning as the push-monitor warning: peer ids are exposed
            // on the unauthenticated /healthz (witnesses need them), so an
            // unprotected listen id lets anyone forge the peer's heartbeats.
            if peer.listen_token.is_none() {
                tracing::warn!(
                    "peer {}: watched without a listen_token - its id appears in /healthz, \
                     so anyone who can reach /api/push can forge its heartbeats",
                    peer.id
                );
            }
            // The id appears in `/api/push/{id}`, so keep it URL-safe.
            let listen_id = peer.listen_id();
            anyhow::ensure!(
                listen_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
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
        if let Some(routes) = &peer.notify {
            for route in routes {
                anyhow::ensure!(
                    channel_names.contains(route.as_str()),
                    "peer {}: notify references unknown channel {route:?}",
                    peer.id
                );
            }
        }
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

/// Validate a monitor's target, latency thresholds and headers (split out of
/// [`validate`] to keep it small).
fn validate_monitor_io(monitor: &Monitor) -> anyhow::Result<()> {
    // Parse the target now, so a typo fails at load instead of at probe time.
    match monitor.kind {
        Kind::Http => anyhow::ensure!(
            reqwest::Url::parse(&monitor.target)
                .is_ok_and(|url| matches!(url.scheme(), "http" | "https")),
            "monitor {}: target must be an http(s) URL",
            monitor.id
        ),
        Kind::Tcp => anyhow::ensure!(
            monitor
                .target
                .rsplit_once(':')
                .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok()),
            "monitor {}: tcp target must be host:port",
            monitor.id
        ),
        Kind::Icmp => {
            // A stray `:port` is a common mistake; an IPv6 literal parses as an IP
            // and is exempt, so its colons are fine.
            let has_port = monitor.target.parse::<std::net::IpAddr>().is_err()
                && monitor
                    .target
                    .rsplit_once(':')
                    .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok());
            anyhow::ensure!(
                !monitor.target.contains("://")
                    && !monitor.target.contains('/')
                    && !monitor.target.chars().any(char::is_whitespace)
                    && !has_port,
                "monitor {}: icmp target must be a bare host or IP (no scheme, path, or port)",
                monitor.id
            );
        }
        Kind::Push => {}
        Kind::Dns => validate_dns_io(monitor)?,
    }
    // A negative latency threshold would mark every check degraded/breached.
    for (label, value) in [
        ("degraded_over_ms", monitor.degraded_over_ms),
        ("slo_latency_ms", monitor.slo_latency_ms),
    ] {
        if let Some(ms) = value {
            anyhow::ensure!(ms > 0, "monitor {}: {label} must be > 0", monitor.id);
        }
    }
    // Each retry can cost a full timeout; an unbounded count would let one
    // tick outlive several intervals.
    anyhow::ensure!(
        monitor.probe_retries.is_none_or(|retries| retries <= 5),
        "monitor {}: probe_retries must be at most 5",
        monitor.id
    );
    anyhow::ensure!(
        monitor.max_body_kb != Some(0),
        "monitor {}: max_body_kb must be > 0",
        monitor.id
    );
    // Catch header typos / CR-LF injection at load rather than at send time.
    for (name, value) in &monitor.headers {
        anyhow::ensure!(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).is_ok(),
            "monitor {}: invalid header name {name:?}",
            monitor.id
        );
        anyhow::ensure!(
            reqwest::header::HeaderValue::from_str(value.as_ref()).is_ok(),
            "monitor {}: invalid value for header {name:?}",
            monitor.id
        );
    }
    anyhow::ensure!(
        monitor.kind == Kind::Http
            || (monitor.keyword.is_none()
                && monitor.json_query.is_none()
                && monitor.proxy.is_none()),
        "monitor {}: keyword/json_query/proxy require an http monitor",
        monitor.id
    );
    if let Some(query) = &monitor.json_query {
        serde_json_path::JsonPath::parse(query)
            .map_err(|err| anyhow::anyhow!("monitor {}: invalid json_query: {err}", monitor.id))?;
    }
    if let Some(proxy) = &monitor.proxy {
        reqwest::Proxy::all(proxy)
            .map_err(|err| anyhow::anyhow!("monitor {}: invalid proxy: {err}", monitor.id))?;
    }
    anyhow::ensure!(
        monitor.kind == Kind::Dns
            || (monitor.dns_record.is_none()
                && monitor.dns_expected.is_none()
                && monitor.dns_resolver.is_none()),
        "monitor {}: dns_record/dns_expected/dns_resolver require a dns monitor",
        monitor.id
    );
    // A malformed pin (wrong length, non-hex) can never match the observed
    // fingerprint, which silently disables pinning after one spurious alert.
    validate_pins(monitor)?;
    if monitor.dual_stack() {
        validate_dual_stack(monitor)?;
    }
    validate_schedule_and_slo(monitor)?;
    Ok(())
}

/// Validate the identity assertions: the certificate pin and the RDAP domain.
fn validate_pins(monitor: &Monitor) -> anyhow::Result<()> {
    if let Some(pin) = &monitor.cert_pin {
        anyhow::ensure!(
            pin.len() == 64 && pin.bytes().all(|b| b.is_ascii_hexdigit()),
            "monitor {}: cert_pin must be 64 hex chars (SHA-256 of the leaf public key)",
            monitor.id
        );
    }
    // A bare registrable name ("example.com"): an URL or a path would query
    // RDAP for garbage, and a name without a dot is a TLD, not a domain.
    if let Some(domain) = &monitor.domain_expiry {
        anyhow::ensure!(
            domain.contains('.')
                && !domain.contains('/')
                && !domain.contains(':')
                && !domain.contains(char::is_whitespace),
            "monitor {}: domain_expiry must be a bare domain name like \"example.com\"",
            monitor.id
        );
    }
    Ok(())
}

/// `dual_stack` needs a probe that can be steered per address family: an
/// active kind, no proxy in the way, and a hostname target (an IP literal has
/// a single family by construction). Runs after the per-kind target checks,
/// so the target is already known to be well-shaped.
fn validate_dual_stack(monitor: &Monitor) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(monitor.kind, Kind::Http | Kind::Tcp | Kind::Icmp),
        "monitor {}: dual_stack requires an http, tcp or icmp monitor",
        monitor.id
    );
    anyhow::ensure!(
        monitor.proxy.is_none(),
        "monitor {}: dual_stack cannot go through a proxy (only the proxy hop's family would be tested)",
        monitor.id
    );
    let host = match monitor.kind {
        Kind::Http => reqwest::Url::parse(&monitor.target)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned)),
        Kind::Tcp => monitor
            .target
            .rsplit_once(':')
            .map(|(host, _port)| host.to_owned()),
        _ => Some(monitor.target.clone()),
    };
    // URL and tcp hosts may carry an IPv6 literal in brackets.
    let host = host.unwrap_or_default();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    anyhow::ensure!(
        bare.parse::<std::net::IpAddr>().is_err(),
        "monitor {}: dual_stack requires a hostname target (an IP literal has a single address family)",
        monitor.id
    );
    Ok(())
}

/// Validate the push `schedule`/`grace_secs` pair and the availability SLO
/// fields. The cron expression is parsed here so a typo fails at load, not at
/// the first missed heartbeat.
fn validate_schedule_and_slo(monitor: &Monitor) -> anyhow::Result<()> {
    if let Some(schedule) = &monitor.schedule {
        anyhow::ensure!(
            monitor.kind == Kind::Push,
            "monitor {}: schedule requires a push monitor",
            monitor.id
        );
        schedule.parse::<croner::Cron>().map_err(|err| {
            anyhow::anyhow!(
                "monitor {}: invalid schedule {schedule:?}: {err}",
                monitor.id
            )
        })?;
    }
    anyhow::ensure!(
        monitor.schedule.is_some() || monitor.grace_secs.is_none(),
        "monitor {}: grace_secs requires a schedule",
        monitor.id
    );
    if let Some(window) = monitor.slo_window_days {
        anyhow::ensure!(
            window > 0,
            "monitor {}: slo_window_days must be > 0",
            monitor.id
        );
        anyhow::ensure!(
            monitor.slo_uptime.is_some(),
            "monitor {}: slo_window_days requires slo_uptime",
            monitor.id
        );
    }
    Ok(())
}

/// The DNS-specific half of [`validate_monitor_io`]: target shape, record
/// type, resolver address.
fn validate_dns_io(monitor: &Monitor) -> anyhow::Result<()> {
    anyhow::ensure!(
        !monitor.target.is_empty()
            && !monitor.target.contains("://")
            && !monitor.target.chars().any(char::is_whitespace),
        "monitor {}: dns target must be a hostname (no scheme or whitespace)",
        monitor.id
    );
    if let Some(record) = &monitor.dns_record {
        anyhow::ensure!(
            matches!(
                record.to_uppercase().as_str(),
                "A" | "AAAA" | "CNAME" | "MX" | "NS" | "TXT" | "SRV" | "SOA" | "PTR"
            ),
            "monitor {}: unsupported dns_record type {record:?}",
            monitor.id
        );
    }
    if let Some(resolver) = &monitor.dns_resolver {
        anyhow::ensure!(
            resolver
                .rsplit_once(':')
                .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok()),
            "monitor {}: dns_resolver must be host:port",
            monitor.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_src: &str) -> Config {
        toml::from_str(toml_src).expect("valid config")
    }

    const MINIMAL: &str = r#"
        [page]
        [server]
        [[monitors]]
        id = "web"
        name = "Web"
        target = "https://example.com"
        interval_secs = 60
    "#;

    #[test]
    fn applies_defaults() {
        let config = parse(MINIMAL);
        assert_eq!(config.page.history_days, 90);
        assert_eq!(config.server.bind, "127.0.0.1:8787");
        assert_eq!(config.alerts.fail_threshold, 3);
        assert_eq!(config.alerts.cert_expiry_days, 14);
        assert_eq!(config.alerts.default_retention_days, 90);

        let monitor = &config.monitors[0];
        assert_eq!(monitor.kind, Kind::Http);
        assert_eq!(monitor.timeout_secs, 10);
        assert_eq!(monitor.retention_days(90), 90);
    }

    #[test]
    fn cert_checking_auto_detects_https() {
        let config = parse(MINIMAL);
        assert!(config.monitors[0].checks_cert());

        let plain = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "tcp"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 60
        "#,
        );
        assert!(!plain.monitors[0].checks_cert());
    }

    #[test]
    fn respects_overrides() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            check_cert = false
            retention_days = 7
        "#,
        );
        assert!(!config.monitors[0].checks_cert());
        assert_eq!(config.monitors[0].retention_days(90), 7);
    }

    #[test]
    fn parses_custom_headers() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            target = "https://crates.io"
            interval_secs = 60
            headers = { Accept = "text/html", "X-Token" = "abc" }
        "#,
        );
        let headers = &config.monitors[0].headers;
        assert_eq!(headers.get("Accept").map(AsRef::as_ref), Some("text/html"));
        assert_eq!(headers.get("X-Token").map(AsRef::as_ref), Some("abc"));
    }

    #[test]
    fn rejects_duplicate_ids() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "dup"
            name = "One"
            target = "https://a.example"
            interval_secs = 60
            [[monitors]]
            id = "dup"
            name = "Two"
            target = "https://b.example"
            interval_secs = 60
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("duplicate monitor id"), "got: {error}");
    }

    #[test]
    fn rejects_zero_timeout() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            target = "https://example.com"
            interval_secs = 60
            timeout_secs = 0
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("timeout_secs"), "got: {error}");
    }

    #[test]
    fn channel_debug_redacts_secrets() {
        let telegram = Channel::Telegram {
            name: "ops".to_owned(),
            token: Secret("supersecret".to_owned()),
            chat_id: "42".to_owned(),
        };
        let shown = format!("{telegram:?}");
        assert!(!shown.contains("supersecret"), "token leaked: {shown}");
        assert!(shown.contains("<redacted>"));
        assert!(shown.contains("ops") && shown.contains("42"));

        let discord = Channel::Discord {
            name: "web".to_owned(),
            webhook_url: Secret("https://discord.com/api/webhooks/123/supersecret".to_owned()),
        };
        let shown = format!("{discord:?}");
        assert!(!shown.contains("supersecret"), "webhook leaked: {shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn parses_http_assertions() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com/health"
            interval_secs = 60
            keyword = "operational"
            json_query = "$.status"
            json_expected = "ok"
        "#,
        );
        let monitor = &config.monitors[0];
        assert_eq!(monitor.keyword.as_deref(), Some("operational"));
        assert_eq!(monitor.json_query.as_deref(), Some("$.status"));
        assert_eq!(monitor.json_expected.as_deref(), Some("ok"));
        validate(&config).expect("valid assertions");
    }

    #[test]
    fn rejects_assertions_on_tcp() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 60
            keyword = "nope"
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("require an http monitor"), "got: {error}");
    }

    #[test]
    fn rejects_invalid_json_query() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            json_query = "not a path"
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("invalid json_query"), "got: {error}");
    }

    #[test]
    fn parses_push_monitor_without_target() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "cron"
            name = "Nightly backup"
            kind = "push"
            interval_secs = 90000
            push_token = "secret"
        "#,
        );
        assert_eq!(config.monitors[0].kind, Kind::Push);
        assert!(config.monitors[0].target.is_empty());
        assert_eq!(
            config.monitors[0].push_token.as_ref().map(AsRef::as_ref),
            Some("secret")
        );
        validate(&config).expect("push monitor valid without target");
    }

    #[test]
    fn rejects_empty_target_on_http() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            interval_secs = 60
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("target must not be empty"), "got: {error}");
    }

    #[test]
    fn parses_and_validates_proxy() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "via"
            name = "Via"
            target = "https://example.com"
            interval_secs = 60
            proxy = "socks5://127.0.0.1:9050"
        "#,
        );
        assert_eq!(
            config.monitors[0].proxy.as_deref(),
            Some("socks5://127.0.0.1:9050")
        );
        validate(&config).expect("valid proxy");
    }

    #[test]
    fn rejects_proxy_on_tcp() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 60
            proxy = "http://127.0.0.1:8080"
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("require an http monitor"), "got: {error}");
    }

    #[test]
    fn dual_stack_validation() {
        let base = |body: &str| {
            format!(
                r#"
                [page]
                [server]
                [[monitors]]
                id = "x"
                name = "X"
                interval_secs = 60
                dual_stack = true
                {body}
            "#
            )
        };

        // Hostname targets pass for the three active kinds.
        for body in [
            "target = \"https://example.com\"",
            "kind = \"tcp\"\ntarget = \"db.example.com:5432\"",
            "kind = \"icmp\"\ntarget = \"gw.example.com\"",
        ] {
            validate(&parse(&base(body))).expect("hostname target valid");
        }

        // An IP literal has a single family - rejected, brackets included.
        for body in [
            "target = \"https://192.0.2.1/health\"",
            "target = \"https://[2001:db8::1]/health\"",
            "kind = \"tcp\"\ntarget = \"192.0.2.1:5432\"",
            "kind = \"icmp\"\ntarget = \"2001:db8::1\"",
        ] {
            let error = validate(&parse(&base(body))).unwrap_err().to_string();
            assert!(error.contains("requires a hostname"), "got: {error}");
        }

        // Wrong kind, and proxy combination.
        let error = validate(&parse(&base("kind = \"dns\"\ntarget = \"example.com\"")))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("requires an http, tcp or icmp monitor"),
            "got: {error}"
        );
        let error = validate(&parse(&base(
            "target = \"https://example.com\"\nproxy = \"http://127.0.0.1:8080\"",
        )))
        .unwrap_err()
        .to_string();
        assert!(error.contains("cannot go through a proxy"), "got: {error}");
    }

    #[test]
    fn maintenance_window_mutes_selected_monitor() {
        let config = parse(
            r#"
            [page]
            [server]
            [[maintenance]]
            title = "DB upgrade"
            start = "2026-06-08T00:00:00Z"
            end = "2026-06-08T02:00:00Z"
            monitors = ["db"]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 60
        "#,
        );
        let parse_dt = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
        assert!(config.in_maintenance("db", parse_dt("2026-06-08T01:00:00Z")));
        // Outside the window, or a monitor not listed.
        assert!(!config.in_maintenance("db", parse_dt("2026-06-08T03:00:00Z")));
        assert!(!config.in_maintenance("web", parse_dt("2026-06-08T01:00:00Z")));
        validate(&config).expect("valid maintenance");
    }

    #[test]
    fn rejects_inverted_maintenance_window() {
        let config = parse(
            r#"
            [page]
            [server]
            [[maintenance]]
            start = "2026-06-08T02:00:00Z"
            end = "2026-06-08T00:00:00Z"
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("start must be before end"), "got: {error}");
    }

    #[test]
    fn parses_incidents() {
        let config = parse(
            r#"
            [page]
            [server]
            [[incidents]]
            title = "Investigating elevated latency"
            body = "We are looking into it."
            severity = "warning"
            at = "2026-06-07T12:00:00Z"
        "#,
        );
        assert_eq!(config.incidents.len(), 1);
        assert_eq!(config.incidents[0].severity, Severity::Warning);
        assert!(config.incidents[0].at.is_some());
    }

    #[test]
    fn rejects_non_url_safe_id() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "a/b"
            name = "X"
            target = "https://example.com"
            interval_secs = 60
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("alphanumeric"), "got: {error}");
    }

    #[test]
    fn secret_is_redacted_in_debug() {
        assert_eq!(
            format!("{:?}", Secret("supersecret".to_owned())),
            "<redacted>"
        );
        assert_eq!(format!("{:?}", Secret(String::new())), "<unset>");
    }

    #[test]
    fn monitor_debug_redacts_url_credentials() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://user:s3cret@example.com/health"
            interval_secs = 60
            proxy = "socks5://puser:ppass@proxy.internal:1080"
        "#,
        );
        let dump = format!("{:?}", config.monitors[0]);
        // Credentials gone, hosts kept (so logs stay useful).
        assert!(!dump.contains("s3cret"), "target password leaked: {dump}");
        assert!(!dump.contains("ppass"), "proxy password leaked: {dump}");
        assert!(!dump.contains("puser"), "proxy username leaked: {dump}");
        assert!(dump.contains("example.com"), "target host lost: {dump}");
        assert!(dump.contains("proxy.internal"), "proxy host lost: {dump}");
    }

    #[test]
    fn expand_env_dollar_escape() {
        // `$$` is a literal `$`, so `$${id}` is a literal `${id}` (no env lookup).
        assert_eq!(expand_env("$${id}"), "${id}");
        assert_eq!(expand_env("a$$b"), "a$b");
    }

    #[test]
    fn env_expansion_only_touches_string_values() {
        // Expansion runs on the parsed document, so a `${VAR}` inside a comment
        // is never looked up or hydrated (raw-text expansion used to splice
        // values - and unset-variable warnings - into commented-out examples).
        let toml = r#"
            [page]
            [server]
            # auth_token = "${HORA_SURELY_UNSET_COMMENTED_EXAMPLE}"
            [[monitors]]
            id = "x"
            name = "$${literal}"
            target = "https://example.com"
            interval_secs = 60
        "#;
        let config = super::parse(toml).expect("a commented ${VAR} must not affect parsing");
        // `$$` in a real string value still unescapes to a literal `$`.
        assert_eq!(config.monitors[0].name, "${literal}");
    }

    #[test]
    fn rejects_negative_latency_threshold() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            target = "https://example.com"
            interval_secs = 60
            degraded_over_ms = -1
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(
            error.contains("degraded_over_ms must be > 0"),
            "got: {error}"
        );
    }

    #[test]
    fn rejects_malformed_http_target() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "x"
            name = "X"
            target = "https//example.com"
            interval_secs = 60
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("http(s) URL"), "got: {error}");
    }

    #[test]
    fn parses_named_channels_and_routing() {
        let config = parse(
            r#"
            [page]
            [server]
            client_ip_header = "cf-connecting-ip"

            [[channels]]
            name = "ops"
            type = "telegram"
            token = "t"
            chat_id = "42"

            [[channels]]
            name = "web-discord"
            type = "discord"
            webhook_url = "https://discord.com/api/webhooks/1/a"

            [[channels]]
            name = "alerts-discord"
            type = "discord"
            webhook_url = "https://discord.com/api/webhooks/2/b"

            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            notify = ["web-discord", "ops"]
        "#,
        );
        assert_eq!(
            config.server.client_ip_header.as_deref(),
            Some("cf-connecting-ip")
        );
        // Two channels share the discord type, routed independently by name.
        assert_eq!(config.channels.len(), 3);
        assert_eq!(config.channels[1].name(), "web-discord");
        assert_eq!(config.channels[2].name(), "alerts-discord");
        let notify = config.monitors[0].notify.as_ref().expect("notify set");
        assert_eq!(notify, &["web-discord", "ops"]);
        validate(&config).expect("valid config");
    }

    #[test]
    fn parses_matrix_and_freemobile_channels() {
        let config = parse(
            r#"
            [page]
            [server]

            [[channels]]
            name = "ops-matrix"
            type = "matrix"
            homeserver = "https://matrix.example.org"
            token = "syt_secret"
            room_id = "!room:matrix.example.org"

            [[channels]]
            name = "oncall-sms"
            type = "freemobile"
            user = "12345678"
            pass = "apikey"
        "#,
        );
        assert_eq!(config.channels.len(), 2);
        assert_eq!(config.channels[0].name(), "ops-matrix");
        assert_eq!(config.channels[1].name(), "oncall-sms");
        assert!(config.channels[0].is_configured() && config.channels[1].is_configured());
        validate(&config).expect("valid");
        // Neither secret may surface through Debug.
        let dump = format!("{:?}", config.channels);
        assert!(!dump.contains("syt_secret"), "matrix token leaked: {dump}");
        assert!(!dump.contains("apikey"), "freemobile pass leaked: {dump}");
    }

    #[test]
    fn alert_on_degraded_defaults_off_and_parses() {
        let off = parse("[page]\n[server]\n");
        assert!(!off.alerts.alert_on_degraded);

        let on = parse("[page]\n[server]\n[alerts]\nalert_on_degraded = true\n");
        assert!(on.alerts.alert_on_degraded);
    }

    #[test]
    fn parses_email_channel_with_default_port() {
        let config = parse(
            r#"
            [page]
            [server]
            [[channels]]
            name = "mail"
            type = "email"
            host = "smtp.example.com"
            username = "u"
            password = "${SMTP_PASSWORD}"
            from = "Hora <hora@example.com>"
            to = "ops@example.com"
        "#,
        );
        match &config.channels[0] {
            Channel::Email {
                host, port, from, ..
            } => {
                assert_eq!(host, "smtp.example.com");
                assert_eq!(*port, 587);
                assert_eq!(from, "Hora <hora@example.com>");
            }
            other => panic!("expected email channel, got {other:?}"),
        }
        validate(&config).expect("valid email channel");
    }

    #[test]
    fn rejects_duplicate_channel_names() {
        let config = parse(
            r#"
            [page]
            [server]
            [[channels]]
            name = "dup"
            type = "discord"
            webhook_url = "https://x/1"
            [[channels]]
            name = "dup"
            type = "slack"
            webhook_url = "https://y/2"
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("duplicate channel name"), "got: {error}");
    }

    #[test]
    fn digest_validates_schedule_and_routes() {
        let base = r#"
            [page]
            [server]
            [[channels]]
            name = "ops"
            type = "slack"
            webhook_url = "https://x/1"
            [digest]
            {digest}
        "#;

        // The default schedule is Monday 08:00 UTC; routes must exist.
        let config = super::parse(&base.replace("{digest}", "")).expect("valid digest");
        assert_eq!(config.digest.as_ref().unwrap().schedule, "0 8 * * 1");

        let err = super::parse(&base.replace("{digest}", "schedule = \"not cron\""))
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid schedule"), "{err}");

        let err = super::parse(&base.replace("{digest}", "notify = [\"typo\"]"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown channel"), "{err}");
    }

    #[test]
    fn rejects_notify_unknown_channel() {
        let config = parse(
            r#"
            [page]
            [server]
            [[channels]]
            name = "ops"
            type = "slack"
            webhook_url = "https://x/1"
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            notify = ["typo"]
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("unknown channel"), "got: {error}");
    }

    #[test]
    fn parses_health_and_peers() {
        let config = parse(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            quorum = true
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "https://b.example/api/push/hora-a"
            ping_token = "tok"
            listen_token = "in"
            expect_every_secs = 90
        "#,
        );
        let health = config.health.as_ref().expect("health");
        assert_eq!(health.id, "hora-a");
        assert_eq!(health.interval_secs, 60); // default
        assert!(health.quorum);
        let peer = &config.peers[0];
        assert!(peer.is_watched());
        assert_eq!(peer.listen_id(), "hora-b"); // defaults to id
        assert_eq!(
            peer.effective_witness_url().as_deref(),
            Some("https://b.example/healthz")
        );
        validate(&config).expect("valid health + peers");
    }

    #[test]
    fn rejects_peers_without_health() {
        let config = parse(
            r#"
            [page]
            [server]
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "https://b.example/api/push/hora-a"
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("require a [health] section"), "got: {error}");
    }

    #[test]
    fn rejects_peer_that_does_nothing() {
        let config = parse(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[peers]]
            id = "hora-b"
            name = "Hora B"
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(
            error.contains("ping_url") && error.contains("expect_every_secs"),
            "got: {error}"
        );
    }

    #[test]
    fn rejects_peer_listen_id_clashing_with_monitor() {
        let config = parse(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[monitors]]
            id = "shared"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            listen_id = "shared"
            expect_every_secs = 90
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("clashes with a monitor id"), "got: {error}");
    }

    #[test]
    fn out_only_peer_is_valid_without_watch() {
        let config = parse(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[peers]]
            id = "hc"
            name = "healthchecks.io"
            ping_url = "https://hc-ping.com/abc"
        "#,
        );
        assert!(!config.peers[0].is_watched());
        validate(&config).expect("OUT-only peer valid");
    }

    #[test]
    fn explicit_witness_url_overrides_derivation() {
        let config = parse(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "https://b.example:9000/api/push/hora-a"
            witness_url = "https://b.internal/healthz"
            expect_every_secs = 90
        "#,
        );
        assert_eq!(
            config.peers[0].effective_witness_url().as_deref(),
            Some("https://b.internal/healthz")
        );
    }

    #[test]
    fn peer_debug_redacts_secrets() {
        let config = parse(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            heartbeat_url = "https://hc-ping.com/sup3rsecret"
            [[peers]]
            id = "hora-b"
            name = "Hora B"
            ping_url = "https://b.example/api/push/hora-a?tok=sup3rsecret"
            ping_token = "tok3n"
            expect_every_secs = 90
        "#,
        );
        let dump = format!("{:?}", config.health) + &format!("{:?}", config.peers);
        assert!(!dump.contains("sup3rsecret"), "url secret leaked: {dump}");
        assert!(!dump.contains("tok3n"), "ping token leaked: {dump}");
    }

    #[test]
    fn parses_group_and_depends_on() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "DB"
            kind = "tcp"
            target = "db:5432"
            interval_secs = 30
            group = "infra"
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://api.example"
            interval_secs = 30
            group = "app"
            depends_on = ["db"]
        "#,
        );
        assert_eq!(config.monitors[0].group.as_deref(), Some("infra"));
        assert_eq!(
            config.monitors[1].depends_on.as_deref(),
            Some(&vec!["db".to_owned()][..])
        );
        validate(&config).expect("valid topology");
    }

    #[test]
    fn rejects_depends_on_unknown_monitor() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://api.example"
            interval_secs = 30
            depends_on = ["ghost"]
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("unknown monitor"), "got: {error}");
    }

    #[test]
    fn rejects_dependency_cycle() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "a"
            name = "A"
            target = "https://a.example"
            interval_secs = 30
            depends_on = ["b"]
            [[monitors]]
            id = "b"
            name = "B"
            target = "https://b.example"
            interval_secs = 30
            depends_on = ["a"]
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("cycle"), "got: {error}");
    }

    #[test]
    fn parses_icmp_monitor() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "host"
            name = "Host"
            kind = "icmp"
            target = "192.0.2.1"
            interval_secs = 30
        "#,
        );
        assert_eq!(config.monitors[0].kind, Kind::Icmp);
        // ICMP is not HTTPS, so no certificate check.
        assert!(!config.monitors[0].checks_cert());
        validate(&config).expect("valid icmp monitor");
    }

    #[test]
    fn rejects_icmp_target_with_scheme_or_port() {
        for bad in ["https://example.com", "1.2.3.4:443", "a/b"] {
            let config = parse(&format!(
                r#"
                [page]
                [server]
                [[monitors]]
                id = "host"
                name = "Host"
                kind = "icmp"
                target = "{bad}"
                interval_secs = 30
            "#
            ));
            let error = validate(&config).unwrap_err().to_string();
            assert!(error.contains("bare host or IP"), "{bad} -> {error}");
        }
    }

    #[test]
    fn accepts_dns_monitor() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "dns"
            name = "DNS"
            kind = "dns"
            target = "example.com"
            interval_secs = 300
            dns_record = "A"
            dns_expected = "1.2.3.4, 5.6.7.8"
            dns_resolver = "9.9.9.9:53"
        "#,
        );
        validate(&config).expect("valid dns monitor");
    }

    #[test]
    fn rejects_invalid_dns_monitor() {
        for (field, message) in [
            ("dns_record = \"WHATEVER\"", "unsupported dns_record"),
            ("dns_resolver = \"no-port\"", "must be host:port"),
            ("dns_resolver = \"host:notaport\"", "must be host:port"),
        ] {
            let config = parse(&format!(
                r#"
                [page]
                [server]
                [[monitors]]
                id = "dns"
                name = "DNS"
                kind = "dns"
                target = "example.com"
                interval_secs = 300
                {field}
            "#
            ));
            let error = validate(&config).unwrap_err().to_string();
            assert!(error.contains(message), "{field} -> {error}");
        }
    }

    #[test]
    fn rejects_dns_target_with_scheme() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "dns"
            name = "DNS"
            kind = "dns"
            target = "https://example.com"
            interval_secs = 300
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("must be a hostname"), "{error}");
    }

    #[test]
    fn rejects_dns_fields_on_http() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            dns_record = "A"
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("require a dns monitor"), "{error}");
    }

    #[test]
    fn private_monitor_requires_auth_token() {
        let private = r#"
            [page]
            [server]
            {token}
            [[monitors]]
            id = "internal"
            name = "Internal"
            target = "https://internal.example"
            interval_secs = 60
            public = false
        "#;

        let config = parse(&private.replace("{token}", ""));
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("requires server.auth_token"), "{error}");

        let config = parse(&private.replace("{token}", "auth_token = \"s3cret\""));
        validate(&config).expect("private monitor with token is valid");

        // An interpolated-but-unset token expands to "" - now rejected outright
        // (an empty token would authorize a blank `?token=`), not silently treated
        // as "no token".
        let config = parse(&private.replace("{token}", "auth_token = \"\""));
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("auth_token must not be empty"), "{error}");
    }

    #[test]
    fn cert_pin_must_be_hex_and_is_lowercased() {
        let cfg = |pin: &str| {
            format!(
                r#"
                [page]
                [server]
                [[monitors]]
                id = "web"
                name = "Web"
                target = "https://example.com"
                interval_secs = 60
                cert_pin = "{pin}"
            "#
            )
        };

        // Wrong length / non-hex is rejected at load.
        let err = super::parse(&cfg("abc")).unwrap_err().to_string();
        assert!(err.contains("cert_pin must be 64 hex"), "{err}");

        // A valid pin typed in upper case is normalized to lowercase, so it
        // matches the lowercase fingerprint Hora computes from the cert.
        let config = super::parse(&cfg(&"A".repeat(64))).expect("valid pin");
        let lower = "a".repeat(64);
        assert_eq!(config.monitors[0].cert_pin.as_deref(), Some(lower.as_str()));
    }

    #[test]
    fn domain_expiry_must_be_a_bare_domain_and_is_lowercased() {
        let cfg = |domain: &str| {
            format!(
                r#"
                [page]
                [server]
                [[monitors]]
                id = "web"
                name = "Web"
                target = "https://api.example.com"
                interval_secs = 60
                domain_expiry = "{domain}"
            "#
            )
        };

        // URLs, host:port, paths and TLD-only names are rejected at load.
        for bad in [
            "https://example.com",
            "example.com/x",
            "example.com:443",
            "com",
        ] {
            let err = super::parse(&cfg(bad)).unwrap_err().to_string();
            assert!(err.contains("bare domain name"), "{bad}: {err}");
        }

        // Domain names are case-insensitive; canonicalized for RDAP.
        let config = super::parse(&cfg("Example.COM")).expect("valid domain");
        assert_eq!(
            config.monitors[0].domain_expiry.as_deref(),
            Some("example.com")
        );

        // The default warning window parallels the certificate one.
        assert_eq!(config.alerts.domain_expiry_days, 14);
    }

    #[test]
    fn public_error_detail_parses_and_defaults_off() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "a"
            name = "A"
            target = "https://example.com"
            interval_secs = 60
            [[monitors]]
            id = "b"
            name = "B"
            target = "https://example.org"
            interval_secs = 60
            public_error_detail = true
        "#,
        );
        assert!(!config.monitors[0].public_error_detail);
        assert!(config.monitors[1].public_error_detail);
    }

    #[test]
    fn empty_access_token_is_rejected() {
        let toml = r#"
            [page]
            [server]
            [[monitors]]
            id = "beat"
            name = "Beat"
            kind = "push"
            interval_secs = 60
            push_token = ""
        "#;
        let config = parse(toml);
        let err = validate(&config).unwrap_err().to_string();
        assert!(err.contains("push_token must not be empty"), "{err}");
    }

    #[test]
    fn accepts_scheduled_push_monitor() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "backup"
            name = "Backup"
            kind = "push"
            interval_secs = 60
            schedule = "0 3 * * *"
            grace_secs = 1800
        "#,
        );
        validate(&config).expect("valid scheduled push monitor");
        assert_eq!(config.monitors[0].push_grace_secs(), 1800);
    }

    #[test]
    fn rejects_bad_schedule_combinations() {
        for (fields, message) in [
            ("schedule = \"0 3 * * *\"", "requires a push monitor"),
            (
                "kind = \"push\"\nschedule = \"99 99 * * *\"",
                "invalid schedule",
            ),
            (
                "kind = \"push\"\ngrace_secs = 600",
                "grace_secs requires a schedule",
            ),
        ] {
            let config = parse(&format!(
                r#"
                [page]
                [server]
                [[monitors]]
                id = "m"
                name = "M"
                target = "https://example.com"
                interval_secs = 60
                {fields}
            "#
            ));
            let error = validate(&config).unwrap_err().to_string();
            assert!(error.contains(message), "{fields} -> {error}");
        }
    }

    #[test]
    fn slo_uptime_parses_to_basis_points() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            slo_uptime = 99.9
            slo_window_days = 7
        "#,
        );
        validate(&config).expect("valid slo");
        assert_eq!(config.monitors[0].slo_uptime, Some(9990));
        assert_eq!(config.monitors[0].slo_window_days(), 7);

        // Out-of-range targets are rejected at parse time.
        let bad = toml::from_str::<Config>(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            slo_uptime = 150.0
        "#,
        );
        assert!(bad.is_err());
    }

    #[test]
    fn probe_retries_default_and_bounds() {
        let config = parse(MINIMAL);
        assert_eq!(config.monitors[0].probe_retries(), 1);

        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "raw"
            name = "Raw"
            target = "https://example.com"
            interval_secs = 60
            probe_retries = 0
        "#,
        );
        validate(&config).expect("0 disables retries");
        assert_eq!(config.monitors[0].probe_retries(), 0);

        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "m"
            name = "M"
            target = "https://example.com"
            interval_secs = 60
            probe_retries = 6
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("at most 5"), "{error}");
    }

    #[test]
    fn rejects_slo_window_without_target() {
        let config = parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            slo_window_days = 30
        "#,
        );
        let error = validate(&config).unwrap_err().to_string();
        assert!(error.contains("requires slo_uptime"), "{error}");
    }
}
