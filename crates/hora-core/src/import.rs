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
//! | `accepted_statuscodes`               | `expected_status`               |
//! | `method` `HEAD`                      | `method = "HEAD"`               |
//! | `maxretries`                         | `fail_threshold` (retries + 1)  |
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
//! `resendInterval`, `upsideDown`, `maxredirects`, HTTP
//! `body`/`httpBodyEncoding`, `authMethod`
//! and its credentials (basic/NTLM/mTLS/OAuth2), `proxyId` (Hora proxies are
//! per-monitor URLs), `notificationIDList` (channel routing differs),
//! `tags`, `description`, `ignoreTls`, `packetSize` and per-type extras.
//!
//! Two are said in a comment instead, because the monitor would not check
//! what it checked in Kuma: a `method` other than `GET` or `HEAD` (Hora sends
//! no request body), and a status-code range that is not a whole class
//! (`200-299` is `"2xx"`; `301-302` has no spelling here).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::MIN_TOKEN_CHARS;

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
    /// The HTTP method (`GET` unless the monitor says otherwise).
    method: Option<String>,
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

    // Kuma names need not be unique, Hora ids must be: a clash gets a numeric
    // suffix instead of producing a config that fails validation.
    let mut ids = HashSet::new();
    for (idx, monitor) in backup.monitor_list.iter().enumerate() {
        if monitor.monitor_type == "group" {
            continue; // Folders become `group = "..."` on their children.
        }
        let id = unique_id(&sanitize_id(&monitor.name, idx), &mut ids);
        convert_monitor(&mut out, monitor, &id, &group_names);
    }

    Ok(out)
}

fn convert_monitor(
    out: &mut String,
    monitor: &KumaMonitor,
    id: &str,
    group_names: &HashMap<i64, &str>,
) {
    // Anything we cannot probe becomes a commented stub instead of a half
    // valid entry: emitting `kind = "http"` without a target would make the
    // whole generated file fail validation.
    let Some(kind) = kind_for(&monitor.monitor_type) else {
        // `{:?}` escapes newlines and control characters, which would
        // otherwise end the comment and inject a line into the file.
        let _ = writeln!(
            out,
            "# Skipped {:?}: unsupported Uptime Kuma type {:?}\n",
            monitor.name, monitor.monitor_type
        );
        return;
    };

    let _ = writeln!(out, "[[monitors]]\nid = {}", toml_string(id));
    let _ = writeln!(out, "name = {}", toml_string(&monitor.name));
    let _ = writeln!(out, "kind = \"{kind}\"");
    if let Some(group) = monitor.parent.and_then(|parent| group_names.get(&parent)) {
        let _ = writeln!(out, "group = {}", toml_string(group));
    }
    match kind {
        "http" => convert_http(out, monitor),
        "tcp" => {
            if let (Some(hostname), Some(port)) = (&monitor.hostname, monitor.port) {
                let target = host_port(hostname, port);
                let _ = writeln!(out, "target = {}", toml_string(&target));
            }
        }
        "icmp" => {
            if let Some(hostname) = &monitor.hostname {
                let _ = writeln!(out, "target = {}", toml_string(hostname));
            }
        }
        "dns" => convert_dns(out, monitor),
        "push" => {
            // Hora refuses a push monitor without a token of at least
            // MIN_TOKEN_CHARS: a Kuma token that is too short (or absent)
            // becomes the explicit opt-out, so the result still loads and
            // says what is left to do.
            match &monitor.push_token {
                Some(token) if token.chars().count() >= MIN_TOKEN_CHARS => {
                    let _ = writeln!(out, "push_token = {}", toml_string(token));
                }
                _ => {
                    let _ = writeln!(
                        out,
                        "# push_token = \"...\"  # set a token of {MIN_TOKEN_CHARS}+ characters, \
                         then remove the next line"
                    );
                    let _ = writeln!(out, "allow_unauthenticated = true");
                }
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
        // Kuma marks a monitor down once its retries are spent: the first
        // failure and each retry are that many failed checks in a row. Left
        // out at 0, where Hora's default (three) is the safer reading of a
        // setting nobody chose.
        let _ = writeln!(
            out,
            "fail_threshold = {}  # maxretries was {} in Kuma",
            monitor.max_retries.saturating_add(1),
            monitor.max_retries
        );
    }
    out.push('\n');
}

fn convert_http(out: &mut String, monitor: &KumaMonitor) {
    if let Some(url) = &monitor.url {
        let _ = writeln!(out, "target = {}", toml_string(url));
    }
    if let Some(keyword) = &monitor.keyword {
        let _ = writeln!(out, "keyword = {}", toml_string(keyword));
        if monitor.invert_keyword {
            let _ = writeln!(out, "keyword_invert = true");
        }
    }
    if let Some(path) = &monitor.json_path {
        let _ = writeln!(out, "json_query = {}", toml_string(path));
        if let Some(expected) = &monitor.expected_value {
            let _ = writeln!(out, "json_expected = {}", toml_string(expected));
        }
    }
    // Kuma's default is the 2xx range, which is also Hora's and needs no
    // line. Kuma offers whole classes (`200-299`) and single codes, which is
    // what `expected_status` takes; any other range has no equivalent.
    if let Some(codes) = &monitor.accepted_statuscodes
        && codes.as_slice() != ["200-299"]
        && !codes.is_empty()
    {
        match codes
            .iter()
            .map(|code| kuma_status(code))
            .collect::<Option<Vec<_>>>()
            .as_deref()
        {
            // A single entry keeps the scalar form.
            Some([single]) => {
                let _ = writeln!(out, "expected_status = {single}");
            }
            Some(entries) => {
                let _ = writeln!(out, "expected_status = [{}]", entries.join(", "));
            }
            None => {
                let _ = writeln!(out, "# accepted_statuscodes {codes:?} not supported");
            }
        }
    }
    // HEAD maps; GET is the default and needs no line. Anything else would
    // be sent as a GET, which is another check: said, not done silently.
    if let Some(method) = monitor.method.as_deref().map(str::trim) {
        if method.eq_ignore_ascii_case("HEAD") {
            // Hora refuses a body assertion on a HEAD (its answer has no
            // body): such a monitor stays a GET, which the assertion needs.
            if monitor.keyword.is_some() || monitor.json_path.is_some() {
                let _ = writeln!(
                    out,
                    "# method HEAD not kept: the body assertion above needs a GET"
                );
            } else {
                let _ = writeln!(out, "method = \"HEAD\"");
            }
        } else if !method.is_empty() && !method.eq_ignore_ascii_case("GET") {
            let _ = writeln!(
                out,
                "# method {method:?} not supported (GET or HEAD, no request body): \
                 this monitor sends a GET"
            );
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
            .map(|(key, value)| format!("{} = {}", toml_string(key), toml_string(value)))
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

/// One Kuma `accepted_statuscodes` entry as an `expected_status` entry in
/// TOML: a code as a number, a whole class (`200-299`) as `"2xx"`. `None`
/// for anything else.
fn kuma_status(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if let Ok(code) = entry.parse::<u16>() {
        return (100..=599).contains(&code).then(|| code.to_string());
    }
    let (first, last) = entry.split_once('-')?;
    let (first, last) = (
        first.trim().parse::<u16>().ok()?,
        last.trim().parse::<u16>().ok()?,
    );
    ((1..=5).contains(&(first / 100)) && first % 100 == 0 && last == first + 99)
        .then(|| format!("\"{}xx\"", first / 100))
}

fn convert_dns(out: &mut String, monitor: &KumaMonitor) {
    if let Some(hostname) = &monitor.hostname {
        let _ = writeln!(out, "target = {}", toml_string(hostname));
    }
    if let Some(record) = &monitor.dns_resolve_type {
        let _ = writeln!(out, "dns_record = {}", toml_string(record));
    }
    // Kuma accepts a comma-separated list; Hora takes one resolver, by IP.
    if let Some(server) = monitor
        .dns_resolve_server
        .as_deref()
        .and_then(|servers| servers.split(',').map(str::trim).find(|s| !s.is_empty()))
    {
        let resolver = host_port(server, monitor.port.unwrap_or(53));
        if server.parse::<std::net::IpAddr>().is_ok() {
            let _ = writeln!(out, "dns_resolver = {}", toml_string(&resolver));
        } else {
            let _ = writeln!(
                out,
                "# dns_resolver {resolver:?} not supported: Hora needs the resolver's IP address"
            );
        }
    }
}

/// `host:port`, with an IPv6 literal bracketed (`[2606:4700::1111]:53`), the
/// only form Hora's `host:port` fields accept for it.
fn host_port(host: &str, port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
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

/// `id`, or `id-2`, `id-3`... - the first not already taken.
fn unique_id(id: &str, taken: &mut HashSet<String>) -> String {
    let mut candidate = id.to_owned();
    let mut suffix = 2;
    while taken.contains(&candidate) {
        candidate = format!("{id}-{suffix}");
        suffix += 1;
    }
    taken.insert(candidate.clone());
    candidate
}

/// A TOML string literal, quotes included, rendered by the `toml` serializer:
/// every character a hand-rolled escaper could miss (control characters,
/// lone quotes, backslashes) comes out valid.
fn toml_string(text: &str) -> String {
    toml::Value::String(text.to_owned()).to_string()
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
                    "pushToken": "abc123abc123abc123abc123"
                },
                {
                    "name": "Short token",
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
        assert!(toml.contains("push_token = \"abc123abc123abc123abc123\""));
        // A token Hora would refuse becomes the explicit opt-out, and the
        // result loads.
        assert!(!toml.contains("push_token = \"abc123\""));
        assert_eq!(toml.matches("allow_unauthenticated = true").count(), 1);
        crate::config::parse(&format!("[page]\n[server]\n{toml}"))
            .expect("the converted monitors load");
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

    /// Kuma's accepted status codes, method and retries become the keys
    /// that mean the same here, and the result loads.
    #[test]
    fn statuses_method_and_retries_map_onto_the_monitor() {
        let json = r#"{
            "monitorList": [
                {
                    "name": "Private API",
                    "type": "http",
                    "url": "https://api.example.com/",
                    "method": "HEAD",
                    "maxretries": 1,
                    "accepted_statuscodes": ["200-299", "401"]
                },
                {
                    "name": "Classes",
                    "type": "http",
                    "url": "https://classes.example.com/",
                    "method": "GET",
                    "accepted_statuscodes": ["300-399", "200"]
                },
                {
                    "name": "Form",
                    "type": "http",
                    "url": "https://form.example.com/",
                    "method": "POST",
                    "accepted_statuscodes": ["301-302"]
                },
                {
                    "name": "Keyword",
                    "type": "keyword",
                    "url": "https://kw.example.com/",
                    "method": "HEAD",
                    "keyword": "ok"
                }
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(toml.contains("expected_status = [\"2xx\", 401]"), "{toml}");
        assert!(toml.contains("expected_status = [\"3xx\", 200]"), "{toml}");
        assert_eq!(toml.matches("method = \"HEAD\"").count(), 1, "{toml}");
        assert!(
            toml.contains("fail_threshold = 2  # maxretries was 1 in Kuma"),
            "{toml}"
        );
        // What has no equivalent is said, never approximated.
        assert!(
            toml.contains("# accepted_statuscodes [\"301-302\"] not supported"),
            "{toml}"
        );
        assert!(toml.contains("# method \"POST\" not supported"), "{toml}");
        assert!(toml.contains("# method HEAD not kept"), "{toml}");

        let config = crate::config::parse(&format!("[page]\n[server]\n{toml}"))
            .expect("the converted monitors load");
        assert_eq!(config.fail_threshold("private-api"), 2, "{toml}");
        assert_eq!(config.fail_threshold("classes"), 3);
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
        let parsed: toml::Table = toml::from_str(&toml).expect("valid TOML");
        assert_eq!(
            parsed["monitors"][0]["name"].as_str(),
            Some("He said \"hi\"")
        );
    }

    #[test]
    fn control_characters_and_duplicate_names_still_load() {
        // A name with control characters, a duplicated name, and an unsupported
        // monitor whose name tries to break out of its comment.
        let json = r#"{
            "monitorList": [
                {"name": "Bell\u0007 and\ttab\u001b[0m", "type": "http", "url": "https://a.example"},
                {"name": "Web", "type": "http", "url": "https://b.example"},
                {"name": "Web", "type": "http", "url": "https://c.example"},
                {"name": "x\n[[monitors]]\nid = \"evil\"", "type": "docker"}
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        let config = crate::config::parse_with_exec_dir(&format!("[page]\n[server]\n{toml}"), None)
            .expect("generated config loads");
        let ids: Vec<&str> = config.monitors.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["bell-and-tab-0m", "web", "web-2"]);
        assert_eq!(config.monitors[0].name, "Bell\u{7} and\ttab\u{1b}[0m");
    }

    #[test]
    fn dns_resolvers_are_bracketed_or_commented() {
        let json = r#"{
            "monitorList": [
                {"name": "v6", "type": "dns", "hostname": "example.com",
                 "dns_resolve_server": "2606:4700:4700::1111"},
                {"name": "named", "type": "dns", "hostname": "example.com",
                 "dns_resolve_server": "dns.google"},
                {"name": "list", "type": "dns", "hostname": "example.com",
                 "dns_resolve_server": "9.9.9.9, 1.1.1.1"}
            ]
        }"#;

        let toml = convert_kuma_to_hora(json).unwrap();
        assert!(
            toml.contains(r#"dns_resolver = "[2606:4700:4700::1111]:53""#),
            "{toml}"
        );
        assert!(
            toml.contains("# dns_resolver \"dns.google:53\" not supported"),
            "{toml}"
        );
        assert!(toml.contains(r#"dns_resolver = "9.9.9.9:53""#), "{toml}");
        crate::config::parse_with_exec_dir(&format!("[page]\n[server]\n{toml}"), None)
            .expect("generated config loads");
    }
}
