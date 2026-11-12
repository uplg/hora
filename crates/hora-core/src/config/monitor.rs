//! A validated monitor: the settings every kind shares, and the kind-specific
//! ones in a [`MonitorKind`], parsed once at load.
//!
//! The file format is the flat `[[monitors]]` table (`config::raw`);
//! [`Monitor`] deserializes through it and through validation, so a setting
//! that does not belong to the monitor's kind has nowhere to live here, and
//! the probes never re-check combinations the loader already refused.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use serde::Deserialize;

use super::Alerts;
use super::parsed::{Compiled, DnsRecord, ExpectedStatus, HttpMethod, Parsed};
use super::raw::RawMonitor;
use super::secret::{Secret, redact_url_secrets};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, serde::Serialize, utoipa::ToSchema,
)]
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
    /// External command probe (`command = [...]`), following the
    /// monitoring-plugins convention: exit 0 = up, 1 = degraded, anything
    /// else = down; the first stdout line is the message. Requires the
    /// `HORA_EXEC_DIR` environment variable - the deployment-level consent -
    /// and only runs executables inside that directory.
    Exec,
    /// Passive heartbeat: the monitored job pings `/api/push/{id}`; missing a
    /// heartbeat within the interval marks it down. No active probing.
    Push,
    /// DNS resolution: resolve a name and assert the expected record type/value.
    /// Target is a hostname; `dns_expected` pins the answer (hijack detection).
    Dns,
}

impl Kind {
    /// Lowercase name, as written in the config and the API.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Tcp => "tcp",
            Self::Icmp => "icmp",
            Self::Exec => "exec",
            Self::Push => "push",
            Self::Dns => "dns",
        }
    }
}

/// `release = { github = "owner/repo", current = "v1.2.3" }` on a monitor:
/// which project is watched, and how Hora knows the version that runs.
///
/// The running version is either written down (`current`, the line to edit
/// on each upgrade) or read from the service itself (`current_url`, with
/// `current_query` when the answer is JSON): the second form cannot drift from
/// what is deployed, and clears the alert by itself once the upgrade is done.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseWatch {
    /// The project on GitHub, `owner/repo`. Its latest published release is
    /// what counts: drafts and prereleases are not.
    pub github: String,
    /// The version that runs, as a literal ("v1.9.1", "1.9.1").
    #[serde(default)]
    pub current: Option<String>,
    /// An address of the service that answers its own version: a JSON document
    /// (give `current_query`) or a bare version as text.
    #[serde(default)]
    pub current_url: Option<String>,
    /// `JSONPath` to the version inside the JSON answered by `current_url`.
    #[serde(default)]
    pub current_query: Option<Parsed<serde_json_path::JsonPath>>,
}

impl std::fmt::Debug for ReleaseWatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReleaseWatch")
            .field("github", &self.github)
            .field("current", &self.current)
            // A version endpoint may sit behind a `?token=`, like a target.
            .field(
                "current_url",
                &self.current_url.as_deref().map(redact_url_secrets),
            )
            .field("current_query", &self.current_query)
            .finish()
    }
}

/// A monitor, validated: the settings every kind shares, plus its kind with
/// that kind's own settings ([`MonitorKind`]).
///
/// Deserializes from a `[[monitors]]` table through the flat file table and
/// the per-monitor validation, so an invalid monitor fails the load with the same
/// `monitor <id>: ...` message as before.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "RawMonitor")]
pub struct Monitor {
    pub id: String,
    pub name: String,
    pub interval_secs: u64,
    pub timeout_secs: u64,
    /// Latency above which a passing check counts as degraded.
    pub degraded_over_ms: Option<i64>,
    /// Latency SLO objective in ms: the 24h p95 is flagged met/breached against it.
    pub slo_latency_ms: Option<i64>,
    /// Availability SLO target in basis points (99.9% = 9990), evaluated over
    /// [`slo_window_days`](Self::slo_window_days).
    pub slo_uptime: Option<u32>,
    /// Window for the availability SLO, in days (default 30).
    pub slo_window_days: Option<u16>,
    /// Immediate re-probes after a failed check before recording it
    /// (default 1, at most 5).
    pub probe_retries: Option<u32>,
    /// Consecutive failed checks before this monitor is confirmed down;
    /// unset = `[alerts] fail_threshold` (see [`Monitor::fail_threshold`]).
    pub fail_threshold: Option<u32>,
    /// Restrict this monitor's alerts to these channel names. Unset = every
    /// configured channel.
    pub notify: Option<Vec<String>>,
    /// Override TLS certificate checking (see [`Monitor::checks_cert`]).
    pub check_cert: Option<bool>,
    /// Override how long this monitor's checks are kept before pruning.
    pub retention_days: Option<u16>,
    /// Display group for the status page.
    pub group: Option<String>,
    /// The machine the monitored service runs on: what a maintenance window
    /// planned for a host (`POST /api/maintenance?host=...`) covers.
    pub host: Option<String>,
    /// Upstream monitor ids this one depends on (validated as a DAG).
    pub depends_on: Option<Vec<String>>,
    /// Whether this monitor appears on the public status page.
    pub public: bool,
    /// Show this monitor's full failure detail to anonymous viewers.
    pub public_error_detail: bool,
    /// TLS certificate pinning: lowercase hex SHA-256 of the leaf public key.
    pub cert_pin: Option<String>,
    /// The registered domain checked against the registry over RDAP.
    pub domain_expiry: Option<String>,
    /// Watch the upstream releases of the software behind this monitor.
    pub release: Option<ReleaseWatch>,
    /// Multi-vantage confirmation override; unset = the `[health]` default.
    pub confirm_with_peers: Option<bool>,
    /// The monitor's own credential: required on `/api/push/{id}` (push
    /// monitors) and accepted on `/api/monitors/{id}/alert` (every kind).
    pub push_token: Option<Secret>,
    /// Push monitor without a `push_token`: the id alone authorizes a
    /// heartbeat (the operator's explicit opt-out).
    pub allow_unauthenticated: bool,
    /// What is checked, and how: the kind with its own settings.
    pub spec: MonitorKind,
}

/// A monitor's kind with the settings only that kind understands, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorKind {
    Http(HttpSpec),
    Tcp(TcpSpec),
    Icmp(IcmpSpec),
    Dns(DnsSpec),
    Push(PushSpec),
    Exec(ExecSpec),
}

/// An HTTP(S) check.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpSpec {
    /// The target as written in the config (the identity peers compare).
    pub target: String,
    /// The same, parsed (always `http` or `https`).
    pub url: reqwest::Url,
    /// The statuses that count as up; unset = any 2xx. A monitor that
    /// accepts a 3xx judges the first answer: redirects are not followed.
    pub expected_status: Option<ExpectedStatus>,
    /// The request sent, on every hop of a redirect chain.
    pub method: HttpMethod,
    /// Extra request headers; values may be credentials.
    pub headers: HashMap<String, Secret>,
    /// The body must (or, inverted, must not) contain this text.
    pub keyword: Option<KeywordAssert>,
    /// A `JSONPath` evaluated against a JSON body.
    pub json: Option<JsonAssert>,
    /// A number extracted from the body by a regex, within bounds.
    pub number: Option<NumberAssert>,
    /// Cap (KiB) on the body read for the assertions (default 1 MiB).
    pub max_body_kb: Option<u32>,
    /// Route the requests through this proxy.
    pub proxy: Option<String>,
    /// Probe IPv4 and IPv6 separately and require both.
    pub dual_stack: bool,
}

/// `keyword` / `keyword_invert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeywordAssert {
    pub text: String,
    /// The body must *not* contain the text.
    pub invert: bool,
}

/// `json_query` / `json_expected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonAssert {
    pub query: Compiled<serde_json_path::JsonPath>,
    /// The value the query result must equal (string compare); unset = the
    /// query only has to match at least one node.
    pub expected: Option<String>,
}

/// `number_regex` / `number_min` / `number_max`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumberAssert {
    /// The first capture group (or the whole match) is the number.
    pub regex: Compiled<regex::Regex>,
    /// Inclusive bounds.
    pub min: Option<i64>,
    pub max: Option<i64>,
}

/// A TCP connect check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpSpec {
    /// `host:port` (IPv6 in brackets).
    pub target: String,
    /// The STARTTLS negotiation the certificate watcher runs first.
    pub starttls: Option<Starttls>,
    pub dual_stack: bool,
}

/// The STARTTLS negotiation to run before the TLS handshake, for services
/// that greet in plaintext first (mail servers on 587/143).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Starttls {
    /// `EHLO` with this name (default: the local address literal), then
    /// `STARTTLS`.
    Smtp {
        ehlo_name: Option<String>,
    },
    Imap,
}

/// An ICMP echo check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcmpSpec {
    /// A bare host or IP.
    pub target: String,
    pub dual_stack: bool,
}

/// A DNS resolution check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DnsSpec {
    /// The name to resolve.
    pub target: String,
    /// The record type queried (default `A`).
    pub record: DnsRecord,
    /// The pinned answer, comma-separated, compared order-insensitively.
    pub expected: Option<String>,
    /// A custom resolver; unset = the system's.
    pub resolver: Option<SocketAddr>,
}

/// A passive heartbeat: the job pings `/api/push/{id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSpec {
    /// Heartbeats expected per cron run rather than every interval.
    pub schedule: Option<PushSchedule>,
}

/// `schedule` / `grace_secs` on a push monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushSchedule {
    /// Five-field cron, UTC.
    pub cron: Compiled<croner::Cron>,
    /// How late a heartbeat may be after its scheduled time (default 1800).
    pub grace_secs: u64,
}

/// An external command, run inside `HORA_EXEC_DIR`.
#[derive(Clone, PartialEq, Eq)]
pub struct ExecSpec {
    /// `command[0]`: a bare file name, resolved inside `HORA_EXEC_DIR`.
    pub program: String,
    /// The rest of `command`.
    pub args: Vec<String>,
}

/// The kinds with a live network probe, borrowed from a [`MonitorKind`].
#[derive(Debug, Clone, Copy)]
pub enum NetworkProbe<'a> {
    Http(&'a HttpSpec),
    Tcp(&'a TcpSpec),
    Icmp(&'a IcmpSpec),
    Dns(&'a DnsSpec),
}

impl MonitorKind {
    /// The kind's name.
    #[must_use]
    pub fn kind(&self) -> Kind {
        match self {
            Self::Http(_) => Kind::Http,
            Self::Tcp(_) => Kind::Tcp,
            Self::Icmp(_) => Kind::Icmp,
            Self::Dns(_) => Kind::Dns,
            Self::Push(_) => Kind::Push,
            Self::Exec(_) => Kind::Exec,
        }
    }

    /// The live network probe, for the kinds that have one.
    #[must_use]
    pub fn network(&self) -> Option<NetworkProbe<'_>> {
        match self {
            Self::Http(spec) => Some(NetworkProbe::Http(spec)),
            Self::Tcp(spec) => Some(NetworkProbe::Tcp(spec)),
            Self::Icmp(spec) => Some(NetworkProbe::Icmp(spec)),
            Self::Dns(spec) => Some(NetworkProbe::Dns(spec)),
            Self::Push(_) | Self::Exec(_) => None,
        }
    }
}

impl HttpSpec {
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

    /// Whether any body assertion is configured.
    #[must_use]
    pub fn asserts_body(&self) -> bool {
        self.keyword.is_some() || self.json.is_some() || self.number.is_some()
    }
}

// Manual `Debug` so credentials never leak: the target and the proxy may embed
// `user:pass@` or a `?token=` query, and header values are `Secret`. A `{:?}`
// of a `Monitor` - or of the whole `Config` - is therefore safe to log.
impl std::fmt::Debug for HttpSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpSpec")
            .field("target", &redact_url_secrets(&self.target))
            .field("expected_status", &self.expected_status)
            .field("method", &self.method)
            .field("headers", &self.headers)
            .field("keyword", &self.keyword)
            .field("json", &self.json)
            .field("number", &self.number)
            .field("max_body_kb", &self.max_body_kb)
            .field("proxy", &self.proxy.as_deref().map(redact_url_secrets))
            .field("dual_stack", &self.dual_stack)
            // The parsed form of `target`, redacted above.
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ExecSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Arguments may carry credentials (a plugin's API key): name only.
        f.debug_struct("ExecSpec")
            .field("program", &self.program)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Monitor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Monitor")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("interval_secs", &self.interval_secs)
            .field("timeout_secs", &self.timeout_secs)
            .field("degraded_over_ms", &self.degraded_over_ms)
            .field("slo_latency_ms", &self.slo_latency_ms)
            .field("slo_uptime", &self.slo_uptime)
            .field("slo_window_days", &self.slo_window_days)
            .field("probe_retries", &self.probe_retries)
            .field("fail_threshold", &self.fail_threshold)
            .field("notify", &self.notify)
            .field("check_cert", &self.check_cert)
            .field("retention_days", &self.retention_days)
            .field("group", &self.group)
            .field("host", &self.host)
            .field("depends_on", &self.depends_on)
            .field("public", &self.public)
            .field("public_error_detail", &self.public_error_detail)
            .field("cert_pin", &self.cert_pin)
            .field("domain_expiry", &self.domain_expiry)
            .field("release", &self.release)
            .field("confirm_with_peers", &self.confirm_with_peers)
            .field("push_token", &self.push_token)
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .field("spec", &self.spec)
            .finish()
    }
}

impl TryFrom<RawMonitor> for Monitor {
    type Error = String;

    fn try_from(raw: RawMonitor) -> Result<Self, Self::Error> {
        super::validate::monitor_from_raw(raw).map_err(|err| format!("{err:#}"))
    }
}

impl Monitor {
    /// The kind's name.
    #[must_use]
    pub fn kind(&self) -> Kind {
        self.spec.kind()
    }

    /// The probe target as written in the config: a URL, `host:port`, a host
    /// or a name. Empty for push and exec monitors, which have none.
    #[must_use]
    pub fn target(&self) -> &str {
        match &self.spec {
            MonitorKind::Http(spec) => &spec.target,
            MonitorKind::Tcp(spec) => &spec.target,
            MonitorKind::Icmp(spec) => &spec.target,
            MonitorKind::Dns(spec) => &spec.target,
            MonitorKind::Push(_) | MonitorKind::Exec(_) => "",
        }
    }

    /// The HTTP proxy this monitor's requests go through, if any.
    #[must_use]
    pub fn proxy(&self) -> Option<&str> {
        match &self.spec {
            MonitorKind::Http(spec) => spec.proxy.as_deref(),
            _ => None,
        }
    }

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
        match &self.spec {
            MonitorKind::Http(spec) => spec.dual_stack,
            MonitorKind::Tcp(spec) => spec.dual_stack,
            MonitorKind::Icmp(spec) => spec.dual_stack,
            MonitorKind::Dns(_) | MonitorKind::Push(_) | MonitorKind::Exec(_) => false,
        }
    }

    /// Whether this monitor should have its TLS certificate expiry checked:
    /// on by default for `https://` HTTP monitors and for tcp monitors with
    /// `starttls` set, overridable either way with `check_cert`.
    #[must_use]
    pub fn checks_cert(&self) -> bool {
        self.check_cert.unwrap_or_else(|| match &self.spec {
            MonitorKind::Http(spec) => spec.url.scheme() == "https",
            MonitorKind::Tcp(spec) => spec.starttls.is_some(),
            _ => false,
        })
    }

    /// Effective storage retention in days, falling back to the global default.
    #[must_use]
    pub fn retention_days(&self, default: u16) -> u16 {
        self.retention_days.unwrap_or(default)
    }

    /// Immediate re-probes after a failure before recording it (default 1).
    #[must_use]
    pub fn probe_retries(&self) -> u32 {
        self.probe_retries.unwrap_or(1)
    }

    /// Consecutive failed checks that confirm this monitor down (and, with
    /// `alert_on_degraded`, slow ones that confirm it degraded): its own
    /// `fail_threshold`, else the global one. Never 0, which would confirm
    /// nothing. The one place that decides it, so the alert, the status page
    /// and the badge cannot disagree on when a monitor is down.
    ///
    /// A missed heartbeat does not wait for it: see
    /// [`FailureKind::is_missed_heartbeat`](crate::probe::FailureKind::is_missed_heartbeat).
    #[must_use]
    pub fn fail_threshold(&self, alerts: &Alerts) -> u32 {
        self.fail_threshold.unwrap_or(alerts.fail_threshold).max(1)
    }

    /// Availability SLO window in days (default 30).
    #[must_use]
    pub fn slo_window_days(&self) -> u16 {
        self.slo_window_days.unwrap_or(30)
    }

    /// A throwaway monitor for a one-shot `hora probe`: a target and a kind,
    /// every other setting at its default, validated like a configured one
    /// (so a malformed target is refused here, not at probe time). It never
    /// reaches the scheduler, the database or the config file, so its id,
    /// name and interval are cosmetic.
    ///
    /// # Errors
    ///
    /// The validation message, for a target that does not suit the kind (or
    /// a kind without a target: push, exec).
    pub fn ad_hoc(kind: Kind, target: String) -> anyhow::Result<Self> {
        super::validate::monitor_from_raw(RawMonitor {
            id: "probe".to_owned(),
            name: target.clone(),
            kind,
            target,
            // A one-shot ad-hoc probe wants the honest first result, not the
            // anti-flap retry (the peer responder does the same for confirm).
            probe_retries: Some(0),
            ..RawMonitor::default()
        })
    }
}

/// Split `host:port` or `[ipv6]:port` into a non-empty host (brackets
/// stripped) and a numeric port - the one parser behind every `host:port`
/// field, so they all agree on IPv6. An unbracketed address with more than
/// one colon (`::1:80`) is ambiguous - is `80` a port or the last group of the
/// address? - and returns `None`, as does a bracketed host that is not an IPv6
/// literal.
#[must_use]
pub fn split_host_port(target: &str) -> Option<(&str, u16)> {
    let (host, port) = if let Some(rest) = target.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return None;
        }
        (host, after.strip_prefix(':')?)
    } else {
        let (host, port) = target.rsplit_once(':')?;
        if host.contains(':') {
            return None;
        }
        (host, port)
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port.parse().ok()?))
}
