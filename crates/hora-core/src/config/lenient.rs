//! Lenient loading, for the daemon: a faulty entry is left out, named and
//! explained, instead of failing the whole configuration.
//!
//! Ten monitors with one typo start nine, not none. [`parse`](super::parse)
//! (and `hora check`'s verdict) stay strict; the daemon loads through
//! [`parse_lenient`], which deserializes every monitor, channel and
//! maintenance window on its own and drops the ones that do not, then runs
//! the strict validation on what is left and drops the entry each error
//! names, until the rest validates. References to what is gone are fixed
//! rather than failed: a route loses the channel, a `depends_on` the monitor,
//! a group token its group. Every removal becomes a [`ConfigIssue`], and the
//! built-in monitor
//! [`CONFIG_MONITOR_ID`] reports them like any outage. Only what has no
//! partial form - the TOML syntax, the global sections (`[server]`,
//! `[alerts]`, `[[peers]]`, ...) - still fails.
//!
//! The same rules apply to a live reload: an entry edited into an invalid
//! state stops until it is fixed; an older version of it is never kept.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::Context as _;
use serde::Deserialize;

use super::env::{Lookup, expand_env_table};
use super::{Channel, Config, Maintenance, Monitor, MonitorKind};

/// Id of the built-in monitor that is down while the configuration has
/// issues. Hora alone feeds it, never `/api/push`, and no configured monitor
/// or peer may take the id.
pub const CONFIG_MONITOR_ID: &str = "hora-config";

/// Id of the built-in self-check monitor (see `crate::self_check`), reserved
/// like [`CONFIG_MONITOR_ID`].
pub const SELF_MONITOR_ID: &str = "hora-self";

/// `[config_monitor]` / `[self_monitor]`: how a built-in monitor shows and
/// alerts. Every key is optional; the monitor exists without the section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuiltinMonitor {
    /// Display name; absent = "Hora configuration" / "Hora self-check".
    #[serde(default)]
    pub name: Option<String>,
    /// Status-page group.
    #[serde(default)]
    pub group: Option<String>,
    /// Channels to alert; absent = every channel, like any monitor.
    #[serde(default)]
    pub notify: Option<Vec<String>>,
    /// Shown to everyone on the status page; absent = `false`: the
    /// operator's view only. What Hora itself is doing is not what a visitor
    /// came to read, and the alert does not need the card.
    #[serde(default)]
    pub public: Option<bool>,
    /// "Still down" reminders while it stays down, like a monitor's
    /// `repeat_alert_secs`; absent = `[alerts] repeat_alert_secs`, 0 = none.
    #[serde(default)]
    pub repeat_alert_secs: Option<u64>,
}

/// One entry a lenient load left out or changed, or a standing problem with
/// the configuration file, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigIssue {
    /// What it concerns: `monitor api`, `channel ops`, `config file`, ...
    pub item: String,
    /// What is wrong, and what was done about it.
    pub reason: String,
}

impl ConfigIssue {
    #[must_use]
    pub fn new(item: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            item: item.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.item, self.reason)
    }
}

/// Read the config file at `path` and [`parse_lenient_at`] it.
///
/// # Errors
///
/// Only for what cannot be loaded in part: an unreadable file, a TOML syntax
/// error, invalid global settings.
pub fn load_lenient_from(path: &Path) -> anyhow::Result<Config> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    parse_lenient_at(&raw, path)
}

/// [`parse_lenient`], then the logos (relative to `path`): one that cannot be
/// read is an issue, and the page shows none.
///
/// # Errors
///
/// See [`parse_lenient`].
pub fn parse_lenient_at(raw: &str, path: &Path) -> anyhow::Result<Config> {
    let mut config = parse_lenient(raw)?;
    if let Err(err) = super::read_logos(&mut config.page, path) {
        let page = &mut config.page;
        page.logo = None;
        page.logo_dark = None;
        page.logo_image = None;
        page.logo_dark_image = None;
        config.issues.push(ConfigIssue::new(
            "page.logo",
            format!("{err:#}; no logo shown"),
        ));
    }
    Ok(config)
}

/// The lenient counterpart of [`parse`](super::parse): what does not
/// validate is left out and listed in [`Config::issues`], and the built-in
/// monitors are added.
///
/// # Errors
///
/// Only for what cannot be loaded in part: a TOML syntax error, invalid
/// global settings.
pub fn parse_lenient(raw: &str) -> anyhow::Result<Config> {
    parse_lenient_with(
        raw,
        std::env::var_os("HORA_EXEC_DIR").map(Into::into),
        &super::env::process_env,
    )
}

pub(super) fn parse_lenient_with(
    raw: &str,
    exec_dir: Option<std::path::PathBuf>,
    lookup: Lookup<'_>,
) -> anyhow::Result<Config> {
    let mut table: toml::Table = toml::from_str(raw).context("parsing config TOML")?;
    for warning in expand_env_table(&mut table, lookup) {
        tracing::warn!("{warning}");
    }
    let mut issues = Vec::new();
    // Each entry on its own: one that does not deserialize (a type error, an
    // unknown key, a monitor's own checks) goes, the rest stays.
    let monitors = keep_valid::<Monitor>(&mut table, "monitors", "id", &mut issues);
    let channels = keep_valid::<Channel>(&mut table, "channels", "name", &mut issues);
    keep_valid::<Maintenance>(&mut table, "maintenance", "title", &mut issues);

    let mut config: Config = table.try_into().map_err(super::read_error)?;
    super::apply_env_overrides(&mut config);
    config.exec_dir = exec_dir.map(|dir| dir.canonicalize().unwrap_or(dir));

    // The strict validation on what is left, dropping the entry each error
    // names. Its warnings are said once, for the attempt that held.
    let mut attempts = monitors + channels + config.maintenance.len() + 2;
    loop {
        prune(&mut config, &mut issues);
        let (validated, warnings) = super::captured_warnings(|| super::validate(&config));
        let err = match validated {
            Ok(()) => {
                for warning in warnings {
                    tracing::warn!("{warning}");
                }
                break;
            }
            Err(err) => err,
        };
        let message = format!("{err:#}");
        if attempts == 0 || !drop_blamed(&mut config, &message, &mut issues) {
            for warning in warnings {
                tracing::warn!("{warning}");
            }
            return Err(err);
        }
        attempts -= 1;
    }

    config.cannot_alert = (channels > 0 && config.channels.is_empty())
        || (monitors > 0 && config.monitors.is_empty());
    add_builtins(&mut config, &mut issues);
    config.issues = issues;
    Ok(config)
}

/// Keep the entries of the array `key` that deserialize; report the others
/// by their `label` field. Returns how many entries were configured.
fn keep_valid<T: serde::de::DeserializeOwned>(
    table: &mut toml::Table,
    key: &str,
    label: &str,
    issues: &mut Vec<ConfigIssue>,
) -> usize {
    let Some(toml::Value::Array(entries)) = table.remove(key) else {
        return 0;
    };
    let configured = entries.len();
    let mut kept = Vec::with_capacity(configured);
    for (index, entry) in entries.into_iter().enumerate() {
        // Its warnings are said when the whole file is read, after this.
        let (parsed, _) = super::captured_warnings(|| entry.clone().try_into::<T>());
        match parsed {
            Ok(_) => kept.push(entry),
            Err(err) => {
                let name = entry.get(label).and_then(toml::Value::as_str);
                let item = entry_item(key, name, index);
                let reason = err.message().trim_end();
                // A monitor's own checks name it already ("monitor api: ...").
                let reason = name
                    .and_then(|name| reason.strip_prefix(&format!("monitor {name}: ")))
                    .unwrap_or(reason);
                issues.push(ConfigIssue::new(item, format!("{reason}; not loaded")));
            }
        }
    }
    table.insert(key.to_owned(), toml::Value::Array(kept));
    configured
}

/// How an issue names the `index`th entry of the array `key`.
fn entry_item(key: &str, name: Option<&str>, index: usize) -> String {
    let kind = match key {
        "monitors" => "monitor",
        "channels" => "channel",
        _ => "maintenance",
    };
    match name {
        Some(name) if kind == "maintenance" => format!("{kind} {name:?}"),
        Some(name) if !name.is_empty() => format!("{kind} {name}"),
        _ => format!("{kind} #{}", index + 1),
    }
}

/// Fix what refers to something that is not loaded, and drop what no error
/// message could name (an empty name, an id that is not URL-safe). Running
/// it again changes nothing.
fn prune(config: &mut Config, issues: &mut Vec<ConfigIssue>) {
    prune_unnamed(config, issues);
    prune_dependencies(config, issues);

    let channels: HashSet<String> = config
        .channels
        .iter()
        .map(|channel| channel.name().to_owned())
        .collect();
    // Who lost a route, per channel: one issue each, not one per route.
    let mut removed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for monitor in &mut config.monitors {
        let owner = format!("monitor {}", monitor.id);
        drop_routes(&owner, &mut monitor.notify, &channels, &mut removed);
    }
    let groups: HashSet<&str> = config
        .monitors
        .iter()
        .filter_map(|monitor| monitor.group.as_deref())
        .collect();
    config.server.group_tokens.retain(|group, _| {
        groups.contains(group.as_str()) || {
            issues.push(ConfigIssue::new(
                format!("server.group_tokens[{group:?}]"),
                "no loaded monitor belongs to this group; token ignored",
            ));
            false
        }
    });
    for peer in &mut config.peers {
        let owner = format!("peer {}", peer.id);
        drop_routes(&owner, &mut peer.notify, &channels, &mut removed);
    }
    drop_routes(
        "updates",
        &mut config.updates.notify,
        &channels,
        &mut removed,
    );
    drop_routes(
        "alerts.notify_unconfirmed",
        &mut config.alerts.notify_unconfirmed,
        &channels,
        &mut removed,
    );
    if let Some(digest) = &mut config.digest {
        drop_routes("digest", &mut digest.notify, &channels, &mut removed);
    }
    if let Some(builtin) = &mut config.config_monitor {
        drop_routes(
            "config_monitor",
            &mut builtin.notify,
            &channels,
            &mut removed,
        );
    }
    if let Some(builtin) = &mut config.self_monitor {
        drop_routes("self_monitor", &mut builtin.notify, &channels, &mut removed);
    }
    for (channel, owners) in removed {
        let named = owners.iter().take(MAX_NAMED).cloned().collect::<Vec<_>>();
        let more = match owners.len().saturating_sub(MAX_NAMED) {
            0 => String::new(),
            n => format!(" and {n} more"),
        };
        issues.push(ConfigIssue::new(
            format!("notify {channel:?}"),
            format!(
                "the channel is not loaded; route removed from {}{more}",
                named.join(", ")
            ),
        ));
    }
}

/// Drop the entries no error message could name (an empty name, an id that
/// is not URL-safe) and the exec monitors without a place to run from.
fn prune_unnamed(config: &mut Config, issues: &mut Vec<ConfigIssue>) {
    let mut index = 0;
    config.channels.retain(|channel| {
        index += 1;
        !channel.name().is_empty() || {
            issues.push(ConfigIssue::new(
                format!("channel #{index}"),
                "channel name must not be empty; not loaded",
            ));
            false
        }
    });
    let exec_ok = config.exec_dir.as_ref().is_some_and(|dir| dir.is_dir());
    config.monitors.retain(|monitor| {
        let problem = if monitor.id.is_empty() || !super::validate::is_url_safe_id(&monitor.id) {
            Some("monitor id must be non-empty and alphanumeric, '-' or '_'")
        } else if matches!(monitor.spec, MonitorKind::Exec(_)) && !exec_ok {
            Some("exec monitors require HORA_EXEC_DIR to name a directory")
        } else {
            None
        };
        problem.is_none_or(|reason| {
            issues.push(ConfigIssue::new(
                format!("monitor {:?}", monitor.id),
                format!("{reason}; not loaded"),
            ));
            false
        })
    });
}

/// Ignore a `depends_on` that names no loaded monitor; in a cycle, every one.
fn prune_dependencies(config: &mut Config, issues: &mut Vec<ConfigIssue>) {
    let ids: HashSet<String> = config.monitors.iter().map(|m| m.id.clone()).collect();
    for monitor in &mut config.monitors {
        let owner = format!("monitor {}", monitor.id);
        if let Some(deps) = &mut monitor.depends_on {
            deps.retain(|dep| {
                ids.contains(dep) || {
                    issues.push(ConfigIssue::new(
                        owner.clone(),
                        format!("depends_on {dep:?} names no loaded monitor; dependency ignored"),
                    ));
                    false
                }
            });
        }
    }
    if crate::topology::validate_dag(&config.monitors).is_err() {
        for monitor in &mut config.monitors {
            if monitor
                .depends_on
                .take()
                .is_some_and(|deps| !deps.is_empty())
            {
                issues.push(ConfigIssue::new(
                    format!("monitor {}", monitor.id),
                    "part of a dependency cycle; depends_on ignored",
                ));
            }
        }
    }
}

/// How many entries an issue names before it only counts the rest.
const MAX_NAMED: usize = 5;

/// Remove the routes to channels that are not loaded, noting `owner` under
/// each such channel.
fn drop_routes(
    owner: &str,
    routes: &mut Option<Vec<String>>,
    channels: &HashSet<String>,
    removed: &mut BTreeMap<String, Vec<String>>,
) {
    if let Some(routes) = routes {
        routes.retain(|route| {
            channels.contains(route) || {
                removed
                    .entry(route.clone())
                    .or_default()
                    .push(owner.to_owned());
                false
            }
        });
    }
}

/// Drop the entry the validation error `message` names, and say so. False
/// when it names none: the error is about the file as a whole.
fn drop_blamed(config: &mut Config, message: &str, issues: &mut Vec<ConfigIssue>) -> bool {
    // A duplicate keeps the first: the error is about a later one.
    let duplicate = message.contains("duplicate");
    if message.starts_with("digest:") {
        config.digest = None;
        issues.push(ConfigIssue::new(
            "digest",
            format!("{message}; digest not loaded"),
        ));
        return true;
    }
    for (section, builtin) in [
        ("config_monitor", &mut config.config_monitor),
        ("self_monitor", &mut config.self_monitor),
    ] {
        if message.starts_with(&format!("{section}.")) {
            *builtin = None;
            issues.push(ConfigIssue::new(
                section,
                format!("{message}; defaults used"),
            ));
            return true;
        }
    }
    let blamed = |prefixes: &[String]| prefixes.iter().any(|p| message.starts_with(p.as_str()));
    let pick = |matches: Vec<usize>| {
        if duplicate {
            matches.last().copied()
        } else {
            matches.first().copied()
        }
    };

    let monitor = pick(
        config
            .monitors
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                let id = &m.id;
                blamed(&[
                    format!("monitor {id}:"),
                    format!("monitor {id} "),
                    format!("duplicate monitor id: {id}"),
                ])
            })
            .map(|(index, _)| index)
            .collect(),
    );
    if let Some(index) = monitor {
        let monitor = config.monitors.remove(index);
        let owner = format!("monitor {}", monitor.id);
        let reason = without_owner(message, &owner);
        issues.push(ConfigIssue::new(owner, reason));
        return true;
    }
    let channel = pick(
        config
            .channels
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                let name = c.name();
                blamed(&[
                    format!("channel {name}:"),
                    format!("duplicate channel name: {name}"),
                ])
            })
            .map(|(index, _)| index)
            .collect(),
    );
    if let Some(index) = channel {
        let owner = format!("channel {}", config.channels.remove(index).name());
        let reason = without_owner(message, &owner);
        issues.push(ConfigIssue::new(owner, reason));
        return true;
    }
    let window = config
        .maintenance
        .iter()
        .position(|w| blamed(&[format!("maintenance window {:?}:", w.title)]));
    if let Some(index) = window {
        let title = config.maintenance.remove(index).title;
        let reason = without_owner(message, &format!("maintenance window {title:?}"));
        issues.push(ConfigIssue::new(format!("maintenance {title:?}"), reason));
        return true;
    }
    false
}

/// `message` without the `owner` it starts with (`monitor api: ...`,
/// `monitor api interval_secs ...`), and what was done.
fn without_owner(message: &str, owner: &str) -> String {
    let reason = message
        .strip_prefix(owner)
        .and_then(|rest| rest.strip_prefix(": ").or_else(|| rest.strip_prefix(' ')))
        .unwrap_or(message);
    format!("{reason}; not loaded")
}

/// Append the built-in monitors (configuration, then self-check): push
/// monitors fed by the daemon itself (never by `/api/push`, which they have
/// no token for), down while there is something to report, alerting at the
/// first failure. Their deadline is the longest period allowed; their feeders
/// record far more often. They are private unless their section makes them
/// public: the operator's view shows them, and where the page has no token
/// nobody's does - the alert and the log speak for them. (A configured
/// monitor may not be private there; these are added after the validation,
/// on purpose.) `[config_monitor]` / `[self_monitor]` name, group and route
/// them; a section a monitor cannot be built from is an issue, and the
/// defaults are used.
fn add_builtins(config: &mut Config, issues: &mut Vec<ConfigIssue>) {
    let builtins = [
        (
            CONFIG_MONITOR_ID,
            "config_monitor",
            "Hora configuration",
            config.config_monitor.clone(),
        ),
        (
            SELF_MONITOR_ID,
            "self_monitor",
            "Hora self-check",
            config.self_monitor.clone(),
        ),
    ];
    for (id, section, default_name, settings) in builtins {
        let settings = settings.unwrap_or_default();
        let monitor = builtin(id, default_name, &settings).or_else(|err| {
            issues.push(ConfigIssue::new(section, format!("{err}; defaults used")));
            builtin(id, default_name, &BuiltinMonitor::default())
        });
        match monitor {
            Ok(monitor) => config.monitors.push(monitor),
            // The defaults are a constant: covered by the tests.
            Err(err) => tracing::error!("built-in monitor {id} cannot be built: {err}"),
        }
    }
}

fn builtin(id: &str, default_name: &str, settings: &BuiltinMonitor) -> Result<Monitor, String> {
    let period = i64::try_from(super::validate::MAX_PERIOD_SECS).unwrap_or(i64::MAX);
    let mut monitor = toml::Table::new();
    monitor.insert("id".into(), id.into());
    let name = settings.name.as_deref().unwrap_or(default_name);
    monitor.insert("name".into(), name.into());
    monitor.insert("kind".into(), "push".into());
    monitor.insert("interval_secs".into(), period.into());
    monitor.insert("fail_threshold".into(), 1.into());
    monitor.insert("public".into(), settings.public.unwrap_or(false).into());
    if let Some(group) = &settings.group {
        monitor.insert("group".into(), group.as_str().into());
    }
    if let Some(notify) = &settings.notify {
        let routes = notify.iter().map(|route| route.as_str().into()).collect();
        monitor.insert("notify".into(), toml::Value::Array(routes));
    }
    if let Some(secs) = settings.repeat_alert_secs {
        monitor.insert(
            "repeat_alert_secs".into(),
            i64::try_from(secs).unwrap_or(i64::MAX).into(),
        );
    }
    toml::Value::Table(monitor)
        .try_into::<Monitor>()
        .map_err(|err| err.message().trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEAD: &str = "[page]\n[server]\n";

    fn load(body: &str) -> Config {
        let lookup = |_: &str| Err(std::env::VarError::NotPresent);
        parse_lenient_with(&format!("{HEAD}{body}"), None, &lookup).expect("lenient load")
    }

    fn ids(config: &Config) -> Vec<&str> {
        config.monitors.iter().map(|m| m.id.as_str()).collect()
    }

    fn has_issue(config: &Config, item: &str, needle: &str) -> bool {
        config
            .issues
            .iter()
            .any(|issue| issue.item == item && issue.reason.contains(needle))
    }

    fn http(id: &str) -> String {
        format!(
            "[[monitors]]\nid = \"{id}\"\nname = \"{id}\"\ntarget = \"https://example.com\"\n\
             interval_secs = 60\n"
        )
    }

    const WEBHOOK: &str =
        "[[channels]]\nname = \"ok\"\ntype = \"webhook\"\nurl = \"https://hooks.example.com/x\"\n";
    /// A webhook without its URL: does not deserialize.
    const BROKEN: &str = "[[channels]]\nname = \"broken\"\ntype = \"webhook\"\n";

    #[test]
    fn a_faulty_monitor_is_dropped_and_the_rest_starts() {
        let config = load(&format!(
            "{}{}[[monitors]]\nid = \"bad\"\nname = \"Bad\"\ntarget = \"https://example.com\"\n\
             interval_secs = 0\n",
            http("a"),
            http("b")
        ));
        assert_eq!(ids(&config), ["a", "b", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(
            has_issue(
                &config,
                "monitor bad",
                "interval_secs must be > 0; not loaded"
            ),
            "{:?}",
            config.issues
        );
        assert!(!config.cannot_alert);
    }

    #[test]
    fn an_unknown_key_drops_only_its_monitor() {
        let config = load(&format!(
            "{}[[monitors]]\nid = \"typo\"\nname = \"T\"\ntarget = \"https://example.com\"\n\
             intervall_secs = 60\n",
            http("a")
        ));
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(
            has_issue(&config, "monitor typo", "not loaded"),
            "{:?}",
            config.issues
        );
    }

    #[test]
    fn duplicate_ids_keep_the_first() {
        let config = load(&format!(
            "{}{}",
            http("a"),
            http("a").replace("interval_secs = 60", "interval_secs = 30")
        ));
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert_eq!(config.monitors[0].interval_secs, 60);
        assert!(has_issue(&config, "monitor a", "duplicate"));
    }

    #[test]
    fn a_faulty_channel_is_dropped_and_routes_to_it_removed() {
        let config = load(&format!(
            "{WEBHOOK}{BROKEN}{}notify = [\"ok\", \"broken\"]\n",
            http("a")
        ));
        let names: Vec<_> = config.channels.iter().map(Channel::name).collect();
        assert_eq!(names, ["ok"]);
        assert_eq!(
            config.monitors[0].notify.as_deref(),
            Some(&["ok".to_owned()][..])
        );
        assert!(
            has_issue(&config, "channel broken", "missing field `url`; not loaded"),
            "{:?}",
            config.issues
        );
        assert!(
            has_issue(&config, "notify \"broken\"", "route removed from monitor a"),
            "{:?}",
            config.issues
        );
        assert!(!config.cannot_alert);
    }

    /// A channel many monitors route to is one issue, not one per route.
    #[test]
    fn routes_to_a_dropped_channel_are_one_issue() {
        let mut monitors = String::new();
        for n in 1..=7 {
            monitors.push_str(&http(&format!("m{n}")));
            monitors.push_str("notify = [\"ok\", \"broken\"]\n");
        }
        let config = load(&format!("{WEBHOOK}{BROKEN}{monitors}"));
        assert_eq!(config.issues.len(), 2, "{:?}", config.issues);
        assert_eq!(
            config.issues[1].to_string(),
            "notify \"broken\": the channel is not loaded; route removed from monitor m1, \
             monitor m2, monitor m3, monitor m4, monitor m5 and 2 more"
        );
    }

    #[test]
    fn a_duplicate_channel_keeps_the_first() {
        let config = load(&format!("{WEBHOOK}{}", WEBHOOK.replace("/x", "/y")));
        assert_eq!(config.channels.len(), 1);
        assert!(has_issue(&config, "channel ok", "duplicate channel name"));
    }

    #[test]
    fn losing_every_channel_or_every_monitor_means_hora_cannot_alert() {
        let config = load(&format!("{BROKEN}{}", http("a")));
        assert!(config.channels.is_empty());
        assert!(config.cannot_alert);
        let config = load(&format!(
            "{WEBHOOK}[[monitors]]\nid = \"bad\"\nname = \"B\"\ntarget = \"https://e.com\"\n\
             interval_secs = 0\n"
        ));
        assert_eq!(ids(&config), [CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(config.cannot_alert);
        // Nothing configured is a choice, not a loss.
        assert!(!load("").cannot_alert);
    }

    #[test]
    fn a_missing_or_cyclic_dependency_is_ignored_not_fatal() {
        let config = load(&format!("{}depends_on = [\"gone\"]\n", http("a")));
        assert_eq!(config.monitors[0].depends_on.as_deref(), Some(&[][..]));
        assert!(has_issue(&config, "monitor a", "depends_on \"gone\""));

        let config = load(&format!(
            "{}depends_on = [\"b\"]\n{}depends_on = [\"a\"]\n",
            http("a"),
            http("b")
        ));
        assert_eq!(ids(&config), ["a", "b", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(config.monitors.iter().all(|m| m.depends_on.is_none()));
        assert!(has_issue(&config, "monitor b", "dependency cycle"));
    }

    #[test]
    fn a_faulty_maintenance_window_is_dropped() {
        let config = load(&format!(
            "{}[[maintenance]]\ntitle = \"Backwards\"\nstart = \"2026-01-02T00:00:00Z\"\n\
             end = \"2026-01-01T00:00:00Z\"\n\
             [[maintenance]]\ntitle = \"Nightly\"\nstart = \"2026-01-01T03:00:00Z\"\n\
             end = \"2026-01-01T04:00:00Z\"\nmonitors = [\"a\"]\n\
             [[maintenance]]\ntitle = \"Typo\"\nstart = \"2026-01-01T03:00:00Z\"\n\
             ende = \"2026-01-01T04:00:00Z\"\n",
            http("a")
        ));
        let titles: Vec<_> = config
            .maintenance
            .iter()
            .map(|w| w.title.as_str())
            .collect();
        assert_eq!(titles, ["Nightly"]);
        assert!(
            has_issue(
                &config,
                "maintenance \"Backwards\"",
                "start must be before end"
            ),
            "{:?}",
            config.issues
        );
        assert!(has_issue(&config, "maintenance \"Typo\"", "not loaded"));
    }

    #[test]
    fn a_group_token_without_a_loaded_group_is_ignored() {
        let config = load(&format!(
            "{}group = \"ops\"\n",
            http("a").replace("interval_secs = 60", "interval_secs = 0")
        ));
        let config_with_token = load(&format!(
            "[server.group_tokens]\nops = \"0123456789abcdef0123\"\n{}group = \"ops\"\n",
            http("a").replace("interval_secs = 60", "interval_secs = 0")
        ));
        assert!(config.server.group_tokens.is_empty());
        assert!(config_with_token.server.group_tokens.is_empty());
        assert!(has_issue(
            &config_with_token,
            "server.group_tokens[\"ops\"]",
            "token ignored"
        ));
    }

    #[test]
    fn the_config_monitor_is_added_and_its_id_reserved() {
        let config = load(&format!("{}{}", http("a"), http(CONFIG_MONITOR_ID)));
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(has_issue(
            &config,
            &format!("monitor {CONFIG_MONITOR_ID}"),
            "reserved"
        ));
        let builtin = config
            .find_monitor(CONFIG_MONITOR_ID)
            .expect("config monitor");
        assert_eq!(builtin.name, "Hora configuration");
        assert_eq!(builtin.kind(), super::super::Kind::Push);
        assert_eq!(builtin.fail_threshold, Some(1));
        assert!(builtin.push_token.is_none() && !builtin.allow_unauthenticated);
        assert!(builtin.notify.is_none(), "every channel");
        assert!(builtin.repeat_alert_secs.is_none(), "[alerts]' reminders");
        assert!(!builtin.public, "the operator's view only, token or not");
        let shown = load(&format!("{}[config_monitor]\npublic = true\n", http("a")));
        let builtin = shown
            .find_monitor(CONFIG_MONITOR_ID)
            .expect("config monitor");
        assert!(builtin.public, "shown to everyone when asked for");
        assert!(shown.issues.is_empty(), "{:?}", shown.issues);
    }

    #[test]
    fn config_monitor_names_groups_and_routes_the_built_in() {
        let config = load(&format!(
            "{WEBHOOK}{}[config_monitor]\nname = \"Config valid\"\ngroup = \"Watchdog\"\n\
             notify = [\"ok\"]\nrepeat_alert_secs = 3600\n{}",
            WEBHOOK.replace("\"ok\"", "\"other\""),
            http("a")
        ));
        assert_eq!(config.issues, []);
        let builtin = config
            .find_monitor(CONFIG_MONITOR_ID)
            .expect("config monitor");
        assert_eq!(builtin.name, "Config valid");
        assert_eq!(builtin.group.as_deref(), Some("Watchdog"));
        assert_eq!(builtin.notify.as_deref(), Some(&["ok".to_owned()][..]));
        assert_eq!(builtin.repeat_alert_secs, Some(3600));
        assert_eq!(builtin.fail_threshold, Some(1), "the rest stays built in");
    }

    #[test]
    fn a_config_monitor_route_to_a_missing_channel_is_removed() {
        let config = load(&format!(
            "{WEBHOOK}{BROKEN}[config_monitor]\nnotify = [\"broken\", \"ok\", \"nowhere\"]\n{}",
            http("a")
        ));
        let builtin = config
            .find_monitor(CONFIG_MONITOR_ID)
            .expect("config monitor");
        assert_eq!(builtin.notify.as_deref(), Some(&["ok".to_owned()][..]));
        for channel in ["broken", "nowhere"] {
            assert!(
                has_issue(
                    &config,
                    &format!("notify {channel:?}"),
                    "route removed from config_monitor"
                ),
                "{:?}",
                config.issues
            );
        }
    }

    /// A reminder period out of bounds is an issue; the defaults are used.
    #[test]
    fn a_config_monitor_section_out_of_bounds_falls_back_to_the_defaults() {
        let config = load(&format!(
            "[config_monitor]\nname = \"Config valid\"\nrepeat_alert_secs = 2592001\n{}",
            http("a")
        ));
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(
            has_issue(&config, "config_monitor", "defaults used"),
            "{:?}",
            config.issues
        );
        let builtin = config
            .find_monitor(CONFIG_MONITOR_ID)
            .expect("config monitor");
        assert_eq!(builtin.name, "Hora configuration");
        assert!(builtin.repeat_alert_secs.is_none());
    }

    /// `[self_monitor]` takes the same keys as `[config_monitor]`, and the
    /// self-check's id is reserved like the configuration monitor's.
    #[test]
    fn self_monitor_names_groups_and_routes_the_self_check() {
        let config = load(&format!(
            "{WEBHOOK}{BROKEN}[self_monitor]\nname = \"Self\"\ngroup = \"Watchdog\"\n\
             notify = [\"broken\", \"ok\"]\npublic = true\nrepeat_alert_secs = 0\n{}{}",
            http("a"),
            http(SELF_MONITOR_ID)
        ));
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(has_issue(
            &config,
            &format!("monitor {SELF_MONITOR_ID}"),
            "reserved"
        ));
        assert!(has_issue(
            &config,
            "notify \"broken\"",
            "route removed from self_monitor"
        ));
        let builtin = config.find_monitor(SELF_MONITOR_ID).expect("self monitor");
        assert_eq!(builtin.name, "Self");
        assert_eq!(builtin.group.as_deref(), Some("Watchdog"));
        assert_eq!(builtin.notify.as_deref(), Some(&["ok".to_owned()][..]));
        assert_eq!(builtin.repeat_alert_secs, Some(0), "no reminders");
        assert!(builtin.public, "shown to everyone when asked for");
        assert_eq!(builtin.kind(), super::super::Kind::Push);
        assert_eq!(builtin.fail_threshold, Some(1));
        assert!(builtin.push_token.is_none() && !builtin.allow_unauthenticated);
        let plain = load(&http("a"));
        let builtin = plain.find_monitor(SELF_MONITOR_ID).expect("self monitor");
        assert_eq!(builtin.name, "Hora self-check");
        assert!(builtin.group.is_none() && builtin.notify.is_none());
        assert!(builtin.repeat_alert_secs.is_none(), "[alerts]' reminders");
        assert!(!builtin.public, "the operator's view only, token or not");
    }

    /// A reminder period out of bounds is an issue; the defaults are used.
    #[test]
    fn a_self_monitor_section_out_of_bounds_falls_back_to_the_defaults() {
        let config = load(&format!(
            "[self_monitor]\nname = \"Self\"\nrepeat_alert_secs = 2592001\n{}",
            http("a")
        ));
        assert_eq!(ids(&config), ["a", CONFIG_MONITOR_ID, SELF_MONITOR_ID]);
        assert!(
            has_issue(&config, "self_monitor", "defaults used"),
            "{:?}",
            config.issues
        );
        let builtin = config.find_monitor(SELF_MONITOR_ID).expect("self monitor");
        assert_eq!(builtin.name, "Hora self-check");
        assert!(builtin.repeat_alert_secs.is_none());
        let other = config
            .find_monitor(CONFIG_MONITOR_ID)
            .expect("config monitor");
        assert_eq!(other.name, "Hora configuration", "the other is untouched");
    }

    /// The strict parse checks the section like any monitor's routes, and
    /// knows its keys.
    #[test]
    fn the_strict_parse_checks_the_built_in_sections() {
        for name in ["config_monitor", "self_monitor"] {
            let section = |body: &str| format!("{HEAD}{WEBHOOK}[{name}]\n{body}\n");
            let config = super::super::parse(&section("name = \"Config valid\"")).unwrap();
            let settings = match name {
                "config_monitor" => config.config_monitor,
                _ => config.self_monitor,
            };
            assert_eq!(
                settings.expect("section").name.as_deref(),
                Some("Config valid")
            );
            assert!(config.monitors.is_empty(), "no built-in on a strict load");
            assert!(super::super::parse(&section("repeat_alert_secs = 0")).is_ok());
            for bad in [
                "repeat_alert_secs = 2592001",
                "notify = [\"nowhere\"]",
                "nmae = \"typo\"",
                "public = \"yes\"",
            ] {
                assert!(super::super::parse(&section(bad)).is_err(), "{name}: {bad}");
            }
        }
    }

    #[test]
    fn a_clean_file_has_no_issues_and_warns_once() {
        let body = format!(
            "{}{}",
            http("a"),
            http("slow").replace("interval_secs = 60", "interval_secs = 5\ntimeout_secs = 10")
        );
        let body = format!(
            "{body}[[monitors]]\nid = \"bad\"\nname = \"B\"\ntarget = \"https://e.com\"\n\
             interval_secs = 0\n"
        );
        let (config, warnings) = super::super::captured_warnings(|| load(&body));
        assert_eq!(config.issues.len(), 1, "{:?}", config.issues);
        assert_eq!(
            warnings.len(),
            1,
            "said once, not per attempt: {warnings:?}"
        );
        assert!(warnings[0].contains("timeout_secs"), "{warnings:?}");
        let config = load(&http("a"));
        assert_eq!(config.issues, []);
    }

    /// What a monitor warns about as it is read is said once, though each
    /// entry is read on its own first.
    #[test]
    fn a_monitor_warning_is_said_once() {
        let body = format!("{}keyword_invert = true\n", http("a"));
        let (config, warnings) = super::super::captured_warnings(|| load(&body));
        assert_eq!(config.issues, []);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("keyword_invert"), "{warnings:?}");
    }

    #[test]
    fn what_has_no_partial_form_still_fails() {
        let lookup = |_: &str| Err(std::env::VarError::NotPresent);
        assert!(
            parse_lenient_with("[[monitors]\nid = \"a\"", None, &lookup).is_err(),
            "TOML syntax"
        );
        assert!(
            parse_lenient_with("[page]\n[server]\nbind = 5\n", None, &lookup).is_err(),
            "global type error"
        );
        assert!(
            parse_lenient_with("[page]\n[server]\nauth_token = \"short\"\n", None, &lookup)
                .is_err(),
            "global validation error"
        );
    }

    #[test]
    fn the_strict_parse_is_unchanged() {
        let toml = format!(
            "{HEAD}{}[[monitors]]\nid = \"bad\"\nname = \"Bad\"\ntarget = \"https://example.com\"\n\
             interval_secs = 0\n",
            http("a")
        );
        assert!(super::super::parse(&toml).is_err());
        let clean = super::super::parse(&format!("{HEAD}{}", http("a"))).unwrap();
        assert_eq!(clean.monitors.len(), 1, "no built-ins on a strict load");
        assert!(super::super::parse(&format!("{HEAD}{}", http(CONFIG_MONITOR_ID))).is_err());
        assert!(super::super::parse(&format!("{HEAD}{}", http(SELF_MONITOR_ID))).is_err());
    }

    #[test]
    fn an_unreadable_logo_is_an_issue_not_a_failure() {
        let config = parse_lenient_at(
            &format!("[page]\nlogo = \"missing.svg\"\n[server]\n{}", http("a")),
            Path::new("/nonexistent/config.toml"),
        )
        .expect("loads");
        assert!(config.page.logo.is_none());
        assert!(has_issue(&config, "page.logo", "no logo shown"));
    }
}
