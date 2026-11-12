//! The `[[monitors]]` table as written in the file.

use std::collections::HashMap;

use serde::Deserialize;

use super::monitor::{Kind, ReleaseWatch};
use super::parsed::{DnsRecord, HttpMethod, Parsed, RawStatuses};
use super::secret::Secret;

/// A `[[monitors]]` entry exactly as the TOML spells it: one flat table, every
/// kind-specific setting optional. This is the file format, kept stable; the
/// daemon never probes from it. Validation turns it into a [`Monitor`], where
/// each kind carries only the settings it understands (see
/// [`MonitorKind`]).
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawMonitor {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) kind: Kind,
    /// Probe target (URL for HTTP, `host:port` for TCP, host or IP for ICMP,
    /// hostname for DNS). Unused for push monitors.
    #[serde(default)]
    pub(crate) target: String,
    pub(crate) interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub(crate) timeout_secs: u64,
    /// HTTP monitor: what counts as up instead of any 2xx - a status
    /// (`200`), a class (`"2xx"`) or a list of them (`[200, 401]`,
    /// `["2xx", 401]`).
    #[serde(default)]
    pub(crate) expected_status: Option<RawStatuses>,
    /// HTTP monitor: the request method, `"GET"` (the default) or `"HEAD"`.
    #[serde(default)]
    pub(crate) method: Option<Parsed<HttpMethod>>,
    /// Consecutive failed checks before this monitor is confirmed down,
    /// instead of `[alerts] fail_threshold`: `1` for a service whose first
    /// failure is already worth a page, more for one that is known to flap.
    #[serde(default)]
    pub(crate) fail_threshold: Option<u32>,
    #[serde(default)]
    pub(crate) degraded_over_ms: Option<i64>,
    /// Latency SLO objective in ms: the 24h p95 is flagged met/breached against it.
    #[serde(default)]
    pub(crate) slo_latency_ms: Option<i64>,
    /// Availability SLO target in percent (e.g. `99.9`), evaluated over
    /// `slo_window_days`. Drives the error-budget
    /// display on the status page and the burn-rate alerts. Stored in basis
    /// points (99.9% = 9990) so the config stays `Eq`-comparable.
    #[serde(default, deserialize_with = "de_slo_uptime")]
    pub(crate) slo_uptime: Option<u32>,
    /// Window for the availability SLO, in days (default 30).
    #[serde(default)]
    pub(crate) slo_window_days: Option<u16>,
    /// Extra HTTP request headers (e.g. `Accept`, `Authorization`). HTTP monitors
    /// only. Values are redacted in `Debug` (a header may carry a credential).
    #[serde(default)]
    pub(crate) headers: HashMap<String, Secret>,
    /// HTTP body assertion: the response body must contain this text.
    #[serde(default)]
    pub(crate) keyword: Option<String>,
    /// Invert the `keyword` check: the body must *not* contain it.
    #[serde(default)]
    pub(crate) keyword_invert: bool,
    /// HTTP body assertion: a `JSONPath` (RFC 9535) evaluated against a JSON response.
    #[serde(default)]
    pub(crate) json_query: Option<Parsed<serde_json_path::JsonPath>>,
    /// Value the `json_query` result must equal (string compare).
    /// When unset, the query only has to match at least one node.
    #[serde(default)]
    pub(crate) json_expected: Option<String>,
    /// HTTP body assertion: a regex extracting a number from the response body -
    /// the first capture group when the pattern has one, the whole match
    /// otherwise. Fails when nothing matches, the extracted text is not a
    /// number, or the value falls outside `number_min` /
    /// `number_max`. Made for pages that render a gauge
    /// inline ("11 unique ships") with no JSON endpoint behind them.
    #[serde(default)]
    pub(crate) number_regex: Option<Parsed<regex::Regex>>,
    /// Lowest value (inclusive) the `number_regex`
    /// extraction may report before the monitor goes down.
    #[serde(default)]
    pub(crate) number_min: Option<i64>,
    /// Highest value (inclusive) the `number_regex`
    /// extraction may report before the monitor goes down.
    #[serde(default)]
    pub(crate) number_max: Option<i64>,
    /// Cap (KiB) on the response body read for keyword/JSON assertions
    /// (default 1024 = 1 MiB). Raise for large JSON endpoints, with care.
    #[serde(default)]
    pub(crate) max_body_kb: Option<u32>,
    /// Immediate re-probes after a failed check before recording it (default
    /// 1, max 5). A single network blip between the prober and the target
    /// thus never lands in the history - so the page, the uptime numbers and
    /// the error budget only count failures that survived a retry. Set 0 to
    /// record every raw probe result. Not applicable to push monitors.
    #[serde(default)]
    pub(crate) probe_retries: Option<u32>,
    /// Probe both address families and require both to pass - the classic
    /// silent failure is a service whose IPv6 has been dead for weeks behind a
    /// healthy IPv4. One broken family marks the monitor down, naming it in
    /// the reason ("IPv6 failing: connection timed out (IPv4 ok)"). For http,
    /// tcp and icmp monitors with a hostname target; not combinable with
    /// `proxy`. The probing host itself needs working IPv4 *and* IPv6.
    #[serde(default)]
    pub(crate) dual_stack: Option<bool>,
    /// Restrict this monitor's alerts to these channel names (e.g. `["ops"]`).
    /// Unset = every configured channel.
    #[serde(default)]
    pub(crate) notify: Option<Vec<String>>,
    /// Route this monitor's HTTP requests through a proxy (`http(s)://…` or
    /// `socks5://…`). HTTP monitors only.
    #[serde(default)]
    pub(crate) proxy: Option<String>,
    /// The monitor's own secret, sent as `X-Push-Token`: required on
    /// `/api/push/{id}` (a push monitor must set one, or opt out with
    /// `allow_unauthenticated`) and accepted on `/api/monitors/{id}/alert`.
    #[serde(default)]
    pub(crate) push_token: Option<Secret>,
    /// Push monitor only: accept heartbeats on the id alone, with no
    /// `push_token`. The explicit opt-out for a private network; ids are not
    /// secrets, so anyone who can reach `/api/push` can then forge them.
    #[serde(default)]
    pub(crate) allow_unauthenticated: Option<bool>,
    /// Push monitor only: a five-field cron expression (UTC) for when heartbeats
    /// are expected (e.g. `"0 3 * * *"` for a nightly job). The monitor goes
    /// down only when a scheduled run misses its `grace_secs`
    /// window - `interval_secs` then merely sets how often Hora re-evaluates.
    #[serde(default)]
    pub(crate) schedule: Option<String>,
    /// Push monitor with a `schedule`: how late (seconds) a
    /// heartbeat may arrive after its scheduled time before alerting.
    /// Default 1800 (30 minutes).
    #[serde(default)]
    pub(crate) grace_secs: Option<u64>,
    /// Override TLS certificate checking. Defaults to on for `https://` HTTP
    /// monitors, and for tcp monitors with `starttls` set.
    #[serde(default)]
    pub(crate) check_cert: Option<bool>,
    /// TCP monitor: negotiate `STARTTLS` (`"smtp"` or `"imap"`) before the
    /// certificate check, so the expiry/pin machinery covers mail servers on
    /// 587/143 - the certificate nobody ever looks at until it expires. The
    /// regular tcp probe is unchanged (a connect is still the up/down signal);
    /// only the certificate watcher upgrades the connection.
    #[serde(default)]
    pub(crate) starttls: Option<String>,
    /// `starttls = "smtp"`: the name announced in `EHLO`. RFC 5321 wants a
    /// fully qualified domain or an address literal, and MX servers enforce it
    /// on port 25 (`550 Invalid EHLO domain`). Default: the address literal of
    /// the connection's local end (`[192.0.2.10]`), valid everywhere,
    /// including inside a container whose hostname is a bare id.
    #[serde(default)]
    pub(crate) ehlo_name: Option<String>,
    /// Override how long this monitor's checks are kept before pruning.
    #[serde(default)]
    pub(crate) retention_days: Option<u16>,
    /// Display group for the status page (monitors sharing a group are rendered
    /// together under a section header). Ungrouped monitors appear last.
    #[serde(default)]
    pub(crate) group: Option<String>,
    /// The machine the monitored service runs on (`host = "frank"`), a free
    /// label. A maintenance window planned for that host covers every
    /// monitor that carries its name; unset, nothing changes.
    #[serde(default)]
    pub(crate) host: Option<String>,
    /// Upstream monitor ids this one depends on. When set, a down alert is
    /// annotated with the probable upstream cause (if any upstream is also
    /// down) or with the dependents it impacts (if this is a root cause).
    /// Validated as a DAG at load time.
    #[serde(default)]
    pub(crate) depends_on: Option<Vec<String>>,
    /// Whether this monitor appears on the public status page. `true` by default;
    /// set `false` to hide it from unauthenticated viewers (requires a token).
    #[serde(default = "default_true")]
    pub(crate) public: bool,
    /// Show this monitor's full failure detail to anonymous viewers. By default
    /// the public page collapses failure reasons to a safe category ("HTTP 500",
    /// "content check failed"), since the stored detail can carry response-body
    /// snippets, DNS answers or asserted keywords. Opt in per monitor when that
    /// detail is fine to publish (e.g. a push monitor whose `msg` is meant for
    /// the page). Authenticated viewers always see the full reason.
    #[serde(default)]
    pub(crate) public_error_detail: bool,
    /// DNS monitor: the record type to query (e.g. `"A"`, `"AAAA"`, `"CNAME"`),
    /// case-insensitive. Unset = `A`.
    #[serde(default)]
    pub(crate) dns_record: Option<Parsed<DnsRecord>>,
    /// DNS monitor: the expected answer, comma-separated when several records
    /// are pinned, compared order-insensitively (hijack detection). When unset,
    /// any non-empty answer counts as up - no change detection, since answers
    /// rotate freely behind CDNs and round-robin records.
    #[serde(default)]
    pub(crate) dns_expected: Option<String>,
    /// DNS monitor: custom resolver address, an IP and a port (`9.9.9.9:53`,
    /// `[2620:fe::fe]:53`). Unset = system default. A hostname is refused: the
    /// resolver's own address can't be looked up through itself.
    #[serde(default)]
    pub(crate) dns_resolver: Option<Parsed<std::net::SocketAddr>>,
    /// TLS certificate pinning: hex-encoded SHA-256 of the leaf public key. When
    /// set, an alert fires if the key changes (MITM / unexpected renewal detection).
    #[serde(default)]
    pub(crate) cert_pin: Option<String>,
    /// The monitor's *registered* domain ("example.com", not the subdomain the
    /// target uses), checked daily against the registry over RDAP; an alert
    /// fires `alerts.domain_expiry_days` before it expires. Explicit rather
    /// than derived from `target`: registrable-domain extraction needs a
    /// public-suffix list, and the operator already knows the answer.
    #[serde(default)]
    pub(crate) domain_expiry: Option<String>,
    /// Watch the upstream releases of the software behind this monitor: an
    /// alert fires, once per release, when one newer than the version that
    /// runs is published. See [`ReleaseWatch`].
    #[serde(default)]
    pub(crate) release: Option<ReleaseWatch>,
    /// Multi-vantage confirmation override for this monitor; unset = the
    /// `[health].confirm_with_peers` default. See that field for semantics.
    #[serde(default)]
    pub(crate) confirm_with_peers: Option<bool>,
    /// Exec monitors: the argv to run - `["check_raid", "--no-sudo"]`. No
    /// shell is involved, and `command[0]` is resolved strictly inside
    /// `HORA_EXEC_DIR` (a bare file name, no path separators).
    #[serde(default)]
    pub(crate) command: Vec<String>,
}

/// Every field at the value an omitted TOML key gets (`interval_secs`, which
/// the file must spell out, at a minute). The base for ad-hoc monitors and
/// test fixtures, so a new field is added in one place.
impl Default for RawMonitor {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            kind: Kind::default(),
            target: String::new(),
            interval_secs: 60,
            timeout_secs: default_timeout(),
            expected_status: None,
            method: None,
            fail_threshold: None,
            degraded_over_ms: None,
            slo_latency_ms: None,
            slo_uptime: None,
            slo_window_days: None,
            headers: HashMap::new(),
            keyword: None,
            keyword_invert: false,
            json_query: None,
            json_expected: None,
            number_regex: None,
            number_min: None,
            number_max: None,
            max_body_kb: None,
            probe_retries: None,
            dual_stack: None,
            notify: None,
            proxy: None,
            push_token: None,
            allow_unauthenticated: None,
            schedule: None,
            grace_secs: None,
            check_cert: None,
            starttls: None,
            ehlo_name: None,
            retention_days: None,
            group: None,
            host: None,
            depends_on: None,
            public: default_true(),
            public_error_detail: false,
            dns_record: None,
            dns_expected: None,
            dns_resolver: None,
            cert_pin: None,
            domain_expiry: None,
            release: None,
            confirm_with_peers: None,
            command: Vec::new(),
        }
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

pub(crate) fn default_timeout() -> u64 {
    10
}
use super::default_true;
