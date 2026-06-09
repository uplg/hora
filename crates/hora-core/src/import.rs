//! Import monitors from an Uptime Kuma backup JSON.
//!
//! Uptime Kuma exports monitors as a JSON file with a `monitorList` array.
//! This module converts those monitors into Hora TOML configuration format,
//! printed to stdout by `hora import kuma <file>` for review.
//!
//! # What maps
//!
//! | Uptime Kuma                          | Hora                            |
//! |--------------------------------------|---------------------------------|
//! | `http`, `keyword`, `json-query`      | `kind = "http"` (+ assertions)  |
//! | `port`                               | `kind = "tcp"`                  |
//! | `ping`                               | `kind = "icmp"`                 |
//! | `dns`                                | `kind = "dns"`                  |
//! | `push`                               | `kind = "push"`                 |
//! | `group` type + `parent`              | `group = "<name>"` (display)    |
//! | `keyword` / `invertKeyword`          | `keyword` / `keyword_invert`    |
//! | `jsonPath` / `expectedValue`         | `json_query` / `json_expected`  |
//! | `interval`, `timeout`                | `interval_secs`, `timeout_secs` |
//! | `headers` (JSON string)              | `headers = { ... }`             |
//! | single `accepted_statuscodes` entry  | `expected_status`               |
//! | `expiryNotification = false`         | `check_cert = false`            |
//! | `dnsResolveType` / `dnsResolveServer`| `dns_record` / `dns_resolver`   |
//! | `pushToken`                          | `push_token`                    |
//!
//! # What does not
//!
//! Monitor types with no Hora equivalent come out as commented stubs:
//! `grpc-keyword`, `docker`, `real-browser`, `steam`, `gamedig`, `mqtt`,
//! `sqlserver`, `postgres`, `mysql`, `mongodb`, `radius`, `redis`, `snmp`,
//! `tailscale-ping`, `kafka-producer` and `rabbitmq`. Fields that are
//! dropped (silently - they have no Hora counterpart): `retryInterval`,
//! `resendInterval`, `upsideDown`, `maxredirects`, status-code *ranges*
//! beyond the default, HTTP `method`/`body`/`httpBodyEncoding`, `authMethod`
//! and its credentials (basic/NTLM/mTLS/OAuth2), `proxyId` (Hora proxies are
//! per-monitor URLs), `notificationIDList` (channel routing differs),
//! `tags`, `description`, `ignoreTls`, `packetSize` and per-type extras.
//! `maxretries` becomes a comment: Hora's equivalent is the global
//! `[alerts].fail_threshold`.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KumaBackup {
    #[serde(default)]
    monitor_list: Vec<KumaMonitor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KumaMonitor {
    id: Option<i64>,
    name: String,
    #[serde(rename = "type")]
    monitor_type: String,
    parent: Option<i64>,
    url: Option<String>,
    hostname: Option<String>,
    port: Option<u16>,
    interval: Option<u64>,
    timeout: Option<u64>,
    /// Kuma's backups spell this `maxretries` (a database column name);
    /// accept the camelCase form too.
    #[serde(default, alias = "maxretries")]
    max_retries: u32,
    keyword: Option<String>,
    /// Kuma's "keyword should NOT be present" toggle.
    #[serde(default)]
    invert_keyword: bool,
    json_path: Option<String>,
    expected_value: Option<String>,
    /// A JSON object *as a string* in Kuma backups.
    headers: Option<String>,
    #[serde(alias = "accepted_statuscodes")]
    accepted_statuscodes: Option<Vec<String>>,
    expiry_notification: Option<bool>,
    push_token: Option<String>,
    #[serde(alias = "dns_resolve_type")]
    dns_resolve_type: Option<String>,
    #[serde(alias = "dns_resolve_server")]
    dns_resolve_server: Option<String>,
}

/// Convert an Uptime Kuma backup JSON to Hora TOML configuration.
///
/// # Errors
///
/// Returns an error if the JSON is invalid.
pub fn convert_kuma_to_hora(json_str: &str) -> Result<String> {
    let backup: KumaBackup =
        serde_json::from_str(json_str).context("failed to parse Uptime Kuma backup JSON")?;

    // First pass: Kuma "group" monitors are folders; children reference them
    // through `parent` and map onto Hora's display groups.
    let group_names: HashMap<i64, &str> = backup
        .monitor_list
        .iter()
        .filter(|monitor| monitor.monitor_type == "group")
        .filter_map(|monitor| monitor.id.map(|id| (id, monitor.name.as_str())))
        .collect();

    let mut out = String::new();
    out.push_str("# Imported from Uptime Kuma\n# Review and adjust as needed\n\n");

    for (idx, monitor) in backup.monitor_list.iter().enumerate() {
        if monitor.monitor_type == "group" {
            continue; // Folders become `group = "..."` on their children.
        }
        convert_monitor(&mut out, monitor, idx, &group_names);
    }

    Ok(out)
}

fn convert_monitor(
    out: &mut String,
    monitor: &KumaMonitor,
    idx: usize,
    group_names: &HashMap<i64, &str>,
) {
    let id = sanitize_id(&monitor.name, idx);
    let name = escape_toml_string(&monitor.name);

    // Anything we cannot probe becomes a commented stub instead of a half
    // valid entry: emitting `kind = "http"` without a target would make the
    // whole generated file fail validation.
    let Some(kind) = kind_for(&monitor.monitor_type) else {
        let _ = writeln!(
            out,
            "# Skipped \"{name}\": unsupported Uptime Kuma type {:?}\n",
            monitor.monitor_type
        );
        return;
    };

    let _ = writeln!(out, "[[monitors]]\nid = \"{id}\"\nname = \"{name}\"");
    let _ = writeln!(out, "kind = \"{kind}\"");
    if let Some(group) = monitor.parent.and_then(|parent| group_names.get(&parent)) {
        let _ = writeln!(out, "group = \"{}\"", escape_toml_string(group));
    }
    match kind {
        "http" => convert_http(out, monitor),
        "tcp" => {
            if let (Some(hostname), Some(port)) = (&monitor.hostname, monitor.port) {
                let _ = writeln!(out, "target = \"{}:{port}\"", escape_toml_string(hostname));
            }
        }
        "icmp" => {
            if let Some(hostname) = &monitor.hostname {
                let _ = writeln!(out, "target = \"{}\"", escape_toml_string(hostname));
            }
        }
        "dns" => convert_dns(out, monitor),
        "push" => {
            if let Some(token) = &monitor.push_token {
                let _ = writeln!(out, "push_token = \"{}\"", escape_toml_string(token));
            } else {
                let _ = writeln!(out, "# push_token = \"...\"  # set a token");
            }
            let _ = writeln!(out, "# heartbeats go to /api/push/{id}");
        }
        _ => {}
    }

    let _ = writeln!(out, "interval_secs = {}", monitor.interval.unwrap_or(60));
    if let Some(timeout) = monitor.timeout.filter(|&t| t > 0) {
        let _ = writeln!(out, "timeout_secs = {timeout}");
    }
    if monitor.max_retries > 0 {
        // Hora's equivalent is the global [alerts].fail_threshold.
        let _ = writeln!(out, "# max_retries was {} in Kuma", monitor.max_retries);
    }
    out.push('\n');
}

fn convert_http(out: &mut String, monitor: &KumaMonitor) {
    if let Some(url) = &monitor.url {
        let _ = writeln!(out, "target = \"{}\"", escape_toml_string(url));
    }
    if let Some(keyword) = &monitor.keyword {
        let _ = writeln!(out, "keyword = \"{}\"", escape_toml_string(keyword));
        if monitor.invert_keyword {
            let _ = writeln!(out, "keyword_invert = true");
        }
    }
    if let Some(path) = &monitor.json_path {
        let _ = writeln!(out, "json_query = \"{}\"", escape_toml_string(path));
        if let Some(expected) = &monitor.expected_value {
            let _ = writeln!(out, "json_expected = \"{}\"", escape_toml_string(expected));
        }
    }
    // Kuma's default is the 2xx range, which is also Hora's; a single exact
    // code maps, anything fancier has no equivalent.
    if let Some(codes) = &monitor.accepted_statuscodes
        && codes.as_slice() != ["200-299"]
    {
        if let [single] = codes.as_slice()
            && let Ok(code) = single.parse::<u16>()
        {
            let _ = writeln!(out, "expected_status = {code}");
        } else {
            let _ = writeln!(out, "# accepted_statuscodes {codes:?} not supported");
        }
    }
    if let Some(headers) = monitor
        .headers
        .as_deref()
        .and_then(|raw| serde_json::from_str::<HashMap<String, String>>(raw).ok())
        && !headers.is_empty()
    {
        let mut pairs: Vec<_> = headers.iter().collect();
        pairs.sort();
        let rendered: Vec<String> = pairs
            .iter()
            .map(|(key, value)| {
                format!(
                    "\"{}\" = \"{}\"",
                    escape_toml_string(key),
                    escape_toml_string(value)
                )
            })
            .collect();
        let _ = writeln!(out, "headers = {{ {} }}", rendered.join(", "));
    }
    if monitor.expiry_notification == Some(false)
        && monitor
            .url
            .as_deref()
            .is_some_and(|u| u.starts_with("https://"))
    {
        let _ = writeln!(out, "check_cert = false");
    }
}

fn convert_dns(out: &mut String, monitor: &KumaMonitor) {
    if let Some(hostname) = &monitor.hostname {
        let _ = writeln!(out, "target = \"{}\"", escape_toml_string(hostname));
    }
    if let Some(record) = &monitor.dns_resolve_type {
        let _ = writeln!(out, "dns_record = \"{}\"", escape_toml_string(record));
    }
    if let Some(server) = &monitor.dns_resolve_server {
        let port = monitor.port.unwrap_or(53);
        let _ = writeln!(
            out,
            "dns_resolver = \"{}:{port}\"",
            escape_toml_string(server)
        );
    }
}

/// Map an Uptime Kuma monitor type onto a Hora monitor kind.
fn kind_for(kuma_type: &str) -> Option<&'static str> {
    match kuma_type {
        "http" | "keyword" | "json-query" => Some("http"),
        "port" => Some("tcp"),
        "ping" => Some("icmp"),
        "dns" => Some("dns"),
        "push" => Some("push"),
        _ => None,
    }
}

fn sanitize_id(name: &str, idx: usize) -> String {
    let mut id = String::with_capacity(name.len());
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c);
        } else if !id.ends_with('-') {
            // Collapse every run of non-alphanumerics into a single dash.
            id.push('-');
        }
    }
    let id = id.trim_matches('-');
    if id.is_empty() {
        format!("monitor-{idx}")
    } else {
        id.to_owned()
    }
}

fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_id() {
        assert_eq!(sanitize_id("My Monitor", 0), "my-monitor");
        assert_eq!(sanitize_id("API / Health", 1), "api-health");
        assert_eq!(sanitize_id("", 2), "monitor-2");
        assert_eq!(sanitize_id("!!!", 3), "monitor-3");
    }

    #[test]
    fn test_convert_basic_http() {
        let json = r#"{
            "monitorList": [
                {
                    "name": "Example",
                    "type": "http",
                    "url": "https://example.com",
                    "interval": 60,
                    "timeout": 20
                }
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(toml.contains("id = \"example\""));
        assert!(toml.contains("kind = \"http\""));
        assert!(toml.contains("target = \"https://example.com\""));
        assert!(toml.contains("timeout_secs = 20"));
    }

    #[test]
    fn test_convert_tcp() {
        let json = r#"{
            "monitorList": [
                {
                    "name": "Database",
                    "type": "port",
                    "hostname": "db.example.com",
                    "port": 5432,
                    "interval": 30
                }
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(toml.contains("kind = \"tcp\""));
        assert!(toml.contains("target = \"db.example.com:5432\""));
    }

    #[test]
    fn test_convert_dns_and_push() {
        let json = r#"{
            "monitorList": [
                {
                    "name": "DNS",
                    "type": "dns",
                    "hostname": "example.com",
                    "port": 53,
                    "dns_resolve_type": "A",
                    "dns_resolve_server": "1.1.1.1"
                },
                {
                    "name": "Backup job",
                    "type": "push",
                    "pushToken": "abc123"
                }
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(toml.contains("kind = \"dns\""));
        assert!(toml.contains("dns_record = \"A\""));
        assert!(toml.contains("dns_resolver = \"1.1.1.1:53\""));
        assert!(toml.contains("kind = \"push\""));
        assert!(toml.contains("push_token = \"abc123\""));
    }

    #[test]
    fn test_convert_json_query_and_status() {
        let json = r#"{
            "monitorList": [
                {
                    "name": "API health",
                    "type": "json-query",
                    "url": "https://api.example.com/health",
                    "jsonPath": "$.status",
                    "expectedValue": "ok",
                    "accepted_statuscodes": ["503"],
                    "headers": "{\"Authorization\": \"Bearer x\"}"
                }
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(toml.contains("kind = \"http\""));
        assert!(toml.contains("json_query = \"$.status\""));
        assert!(toml.contains("json_expected = \"ok\""));
        assert!(toml.contains("expected_status = 503"));
        assert!(toml.contains(r#"headers = { "Authorization" = "Bearer x" }"#));
    }

    #[test]
    fn test_groups_map_to_display_groups() {
        let json = r#"{
            "monitorList": [
                {"id": 7, "name": "Backend", "type": "group"},
                {
                    "id": 8,
                    "name": "API",
                    "type": "http",
                    "parent": 7,
                    "url": "https://api.example.com"
                }
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(toml.contains("group = \"Backend\""), "{toml}");
        // The folder itself is not emitted as a monitor.
        assert!(!toml.contains("name = \"Backend\""));
    }

    #[test]
    fn test_unsupported_type_is_commented_out() {
        let json = r#"{
            "monitorList": [
                {"name": "Container", "type": "docker"}
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(toml.contains("# Skipped \"Container\""));
        assert!(!toml.contains("\nkind = "));
    }

    #[test]
    fn test_names_are_toml_escaped() {
        let json = r#"{
            "monitorList": [
                {"name": "He said \"hi\"", "type": "http", "url": "https://example.com"}
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(toml.contains(r#"name = "He said \"hi\"""#));
    }
}
