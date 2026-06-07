//! Configuration: parsed from a TOML file, with environment overrides for secrets.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context as _;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub page: Page,
    pub server: Server,
    #[serde(default)]
    pub telegram: Telegram,
    #[serde(default)]
    pub alerts: Alerts,
    #[serde(default)]
    pub monitors: Vec<Monitor>,
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
    /// Per-IP API rate limit: one request slot is replenished every N seconds.
    #[serde(default = "default_rate_limit_refill")]
    pub rate_limit_refill_secs: u64,
    /// Per-IP API rate limit: maximum burst of requests.
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,
}

/// Telegram channel credentials. Empty values disable the channel.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Telegram {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub chat_id: String,
}

// Manual `Debug` so the bot token never leaks: `Config` derives `Debug`, so any
// `{config:?}` (a log line, a panic message) would otherwise print the token.
impl std::fmt::Debug for Telegram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let token = if self.token.is_empty() {
            "<unset>"
        } else {
            "<redacted>"
        };
        f.debug_struct("Telegram")
            .field("token", &token)
            .field("chat_id", &self.chat_id)
            .finish()
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
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Monitor {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    pub target: String,
    pub interval_secs: u64,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub expected_status: Option<u16>,
    #[serde(default)]
    pub degraded_over_ms: Option<i64>,
    /// Extra HTTP request headers (e.g. `Accept`, `Authorization`). HTTP monitors only.
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// Override TLS certificate checking. Defaults to on for `https://` HTTP monitors.
    #[serde(default)]
    pub check_cert: Option<bool>,
    /// Override how long this monitor's checks are kept before pruning.
    #[serde(default)]
    pub retention_days: Option<u16>,
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
    let mut config: Config = toml::from_str(toml_str).context("parsing config TOML")?;
    apply_env_overrides(&mut config);
    validate(&config)?;
    Ok(config)
}

fn apply_env_overrides(config: &mut Config) {
    if let Ok(token) = std::env::var("HORA_TELEGRAM_TOKEN") {
        config.telegram.token = token;
    }
    if let Ok(chat_id) = std::env::var("HORA_TELEGRAM_CHAT_ID") {
        config.telegram.chat_id = chat_id;
    }
    if let Ok(bind) = std::env::var("HORA_BIND") {
        config.server.bind = bind;
    }
    if let Ok(path) = std::env::var("HORA_DATABASE_PATH") {
        config.server.database_path = path;
    }
}

fn validate(config: &Config) -> anyhow::Result<()> {
    let mut seen = HashSet::new();
    for monitor in &config.monitors {
        anyhow::ensure!(!monitor.id.is_empty(), "monitor id must not be empty");
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
            monitor.timeout_secs > 0,
            "monitor {} timeout_secs must be > 0",
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
        assert_eq!(headers.get("Accept").map(String::as_str), Some("text/html"));
        assert_eq!(headers.get("X-Token").map(String::as_str), Some("abc"));
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
    fn telegram_debug_redacts_token() {
        let telegram = Telegram {
            token: "supersecret".to_owned(),
            chat_id: "42".to_owned(),
        };
        let shown = format!("{telegram:?}");
        assert!(!shown.contains("supersecret"), "token leaked: {shown}");
        assert!(shown.contains("<redacted>"));
        assert!(shown.contains("42"));
    }
}
