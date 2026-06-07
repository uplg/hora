//! Configuration: parsed from a TOML file, with environment overrides for secrets.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
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
            | Self::Email { name, .. } => name,
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
            Self::Webhook { url, .. } => !url.is_empty(),
            Self::Email { host, from, to, .. } => {
                !host.is_empty() && !from.is_empty() && !to.is_empty()
            }
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
    /// Warn this many days before a TLS certificate expires.
    #[serde(default = "default_cert_expiry_days")]
    pub cert_expiry_days: u16,
    /// Default storage retention, overridable per monitor.
    #[serde(default = "default_retention_days")]
    pub default_retention_days: u16,
}

impl Default for Alerts {
    fn default() -> Self {
        Self {
            fail_threshold: default_threshold(),
            cert_expiry_days: default_cert_expiry_days(),
            default_retention_days: default_retention_days(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Http,
    Tcp,
    /// Passive heartbeat: the monitored job pings `/api/push/{id}`; missing a
    /// heartbeat within the interval marks it down. No active probing.
    Push,
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    /// Probe target (URL for HTTP, `host:port` for TCP). Unused for push monitors.
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
    /// Override TLS certificate checking. Defaults to on for `https://` HTTP monitors.
    #[serde(default)]
    pub check_cert: Option<bool>,
    /// Override how long this monitor's checks are kept before pruning.
    #[serde(default)]
    pub retention_days: Option<u16>,
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
            .field("headers", &self.headers)
            .field("keyword", &self.keyword)
            .field("keyword_invert", &self.keyword_invert)
            .field("json_query", &self.json_query)
            .field("json_expected", &self.json_expected)
            .field("max_body_kb", &self.max_body_kb)
            .field("notify", &self.notify)
            .field("proxy", &self.proxy.as_deref().map(redact_url_credentials))
            .field("push_token", &self.push_token)
            .field("check_cert", &self.check_cert)
            .field("retention_days", &self.retention_days)
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
fn default_retention_days() -> u16 {
    90
}
fn default_rate_limit_refill() -> u64 {
    1
}
fn default_rate_limit_burst() -> u32 {
    30
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
    // `${VAR}` is expanded from the environment first, so secrets (channel
    // tokens/URLs) can stay out of the file: `webhook_url = "${OPS_DISCORD}"`.
    let expanded = expand_env(toml_str);
    let mut config: Config = toml::from_str(&expanded).context("parsing config TOML")?;
    apply_env_overrides(&mut config);
    validate(&config)?;
    Ok(config)
}

/// Substitute `${VAR}` with the environment value (empty if unset). `$$` is a
/// literal `$`, so `$${id}` yields a literal `${id}`.
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
                    // Escape for a TOML basic string (the intended use is
                    // `key = "${VAR}"`), so a value with a quote or newline
                    // can't break parsing or inject config.
                    out.push_str(&toml_escape(&value));
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

/// Escape a value so it is safe inside a TOML basic (double-quoted) string.
fn toml_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
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

fn validate(config: &Config) -> anyhow::Result<()> {
    let mut channel_names = HashSet::new();
    for channel in &config.channels {
        anyhow::ensure!(!channel.name().is_empty(), "channel name must not be empty");
        anyhow::ensure!(
            channel_names.insert(channel.name()),
            "duplicate channel name: {}",
            channel.name()
        );
        // Webhook-style channels carry a token in the URL; warn on cleartext http.
        let url = match channel {
            Channel::Discord { webhook_url, .. } | Channel::Slack { webhook_url, .. } => {
                Some(webhook_url.as_ref())
            }
            Channel::Webhook { url, .. } => Some(url.as_ref()),
            Channel::Telegram { .. } | Channel::Email { .. } => None,
        };
        if url.is_some_and(|url| url.starts_with("http://")) {
            tracing::warn!(
                "channel {}: webhook URL uses http:// - the token is sent in cleartext",
                channel.name()
            );
        }
    }

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
        anyhow::ensure!(
            monitor.kind == Kind::Http
                || (monitor.keyword.is_none()
                    && monitor.json_query.is_none()
                    && monitor.proxy.is_none()),
            "monitor {}: keyword/json_query/proxy require an http monitor",
            monitor.id
        );
        if let Some(query) = &monitor.json_query {
            serde_json_path::JsonPath::parse(query).map_err(|err| {
                anyhow::anyhow!("monitor {}: invalid json_query: {err}", monitor.id)
            })?;
        }
        if let Some(proxy) = &monitor.proxy {
            reqwest::Proxy::all(proxy)
                .map_err(|err| anyhow::anyhow!("monitor {}: invalid proxy: {err}", monitor.id))?;
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
        Kind::Push => {}
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
    fn toml_escape_neutralises_special_chars() {
        assert_eq!(toml_escape("plain-token_123"), "plain-token_123");
        assert_eq!(toml_escape("a\"b"), "a\\\"b");
        assert_eq!(toml_escape("line1\nline2"), "line1\\nline2");
    }

    #[test]
    fn expand_env_dollar_escape() {
        // `$$` is a literal `$`, so `$${id}` is a literal `${id}` (no env lookup).
        assert_eq!(expand_env("$${id}"), "${id}");
        assert_eq!(expand_env("a$$b"), "a$b");
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
}
