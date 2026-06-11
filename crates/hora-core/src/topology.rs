//! Dependency graph helpers: validation (DAG via Kahn's algorithm) and traversal
//! (transitive upstreams / dependents via BFS). Pure functions operating on the
//! monitor list; no I/O.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::config::Monitor;

/// Validate the dependency graph: every `depends_on` id must reference an existing
/// monitor, and the graph must be acyclic (Kahn's algorithm).
///
/// # Errors
///
/// Returns an error if a `depends_on` references an unknown monitor id, or if the
/// graph contains a cycle.
pub fn validate_dag(monitors: &[Monitor]) -> anyhow::Result<()> {
    let ids: HashSet<&str> = monitors.iter().map(|m| m.id.as_str()).collect();

    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();

    for monitor in monitors {
        in_degree.entry(monitor.id.as_str()).or_insert(0);
        if let Some(deps) = &monitor.depends_on {
            for dep in deps {
                anyhow::ensure!(
                    ids.contains(dep.as_str()),
                    "monitor {}: depends_on references unknown monitor {dep:?}",
                    monitor.id
                );
                adj.entry(dep.as_str())
                    .or_default()
                    .push(monitor.id.as_str());
                *in_degree.entry(monitor.id.as_str()).or_insert(0) += 1;
            }
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(&id, _)| id)
        .collect();
    let mut visited = 0usize;

    while let Some(node) = queue.pop_front() {
        visited += 1;
        if let Some(neighbors) = adj.get(node) {
            for &next in neighbors {
                if let Some(deg) = in_degree.get_mut(next) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        queue.push_back(next);
                    }
                }
            }
        }
    }

    anyhow::ensure!(
        visited == ids.len(),
        "dependency cycle detected among monitors"
    );

    Ok(())
}

/// Collect every transitive upstream of `id` (BFS over `depends_on`), in
/// discovery order. Returns an empty slice when the monitor has no dependencies.
#[must_use]
pub fn transitive_upstreams<'a>(monitors: &'a [Monitor], id: &str) -> Vec<&'a str> {
    let upstreams: HashMap<&str, &[String]> = monitors
        .iter()
        .filter_map(|m| {
            m.depends_on
                .as_ref()
                .map(|deps| (m.id.as_str(), deps.as_slice()))
        })
        .collect();

    let mut visited = HashSet::new();
    let mut result = Vec::new();
    let mut queue = VecDeque::new();

    if let Some(deps) = upstreams.get(id) {
        for dep in *deps {
            queue.push_back(dep.as_str());
        }
    }

    while let Some(dep) = queue.pop_front() {
        if visited.insert(dep) {
            result.push(dep);
            if let Some(further) = upstreams.get(dep) {
                for next in *further {
                    queue.push_back(next.as_str());
                }
            }
        }
    }
    result
}

/// Collect every transitive dependent of `id` (reverse BFS: who depends on `id`,
/// directly or transitively), in discovery order.
#[must_use]
pub fn transitive_dependents<'a>(monitors: &'a [Monitor], id: &str) -> Vec<&'a str> {
    let mut downstreams: HashMap<&str, Vec<&str>> = HashMap::new();
    for monitor in monitors {
        if let Some(deps) = &monitor.depends_on {
            for dep in deps {
                downstreams
                    .entry(dep.as_str())
                    .or_default()
                    .push(monitor.id.as_str());
            }
        }
    }

    let mut visited = HashSet::new();
    let mut result = Vec::new();
    let mut queue = VecDeque::new();

    if let Some(deps) = downstreams.get(id) {
        for &dep in deps {
            queue.push_back(dep);
        }
    }

    while let Some(dep) = queue.pop_front() {
        if visited.insert(dep) {
            result.push(dep);
            if let Some(further) = downstreams.get(dep) {
                for &next in further {
                    queue.push_back(next);
                }
            }
        }
    }
    result
}

/// Look up a monitor's display name by id.
#[must_use]
pub fn monitor_name<'a>(monitors: &'a [Monitor], id: &str) -> Option<&'a str> {
    monitors
        .iter()
        .find(|m| m.id == id)
        .map(|m| m.name.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(id: &str, depends_on: Option<Vec<&str>>) -> Monitor {
        Monitor {
            id: id.to_owned(),
            name: id.to_uppercase(),
            kind: crate::config::Kind::Http,
            target: format!("https://{id}.example"),
            interval_secs: 60,
            timeout_secs: 10,
            expected_status: None,
            degraded_over_ms: None,
            slo_latency_ms: None,
            headers: HashMap::new(),
            keyword: None,
            keyword_invert: false,
            json_query: None,
            json_expected: None,
            max_body_kb: None,
            probe_retries: None,
            dual_stack: None,
            notify: None,
            proxy: None,
            push_token: None,
            check_cert: None,
            retention_days: None,
            group: None,
            depends_on: depends_on.map(|v| v.into_iter().map(String::from).collect()),
            public: true,
            public_error_detail: false,
            dns_record: None,
            dns_expected: None,
            dns_resolver: None,
            cert_pin: None,
            domain_expiry: None,
            slo_uptime: None,
            slo_window_days: None,
            schedule: None,
            grace_secs: None,
        }
    }

    #[test]
    fn accepts_empty_and_linear_graphs() {
        let monitors = vec![
            monitor("db", None),
            monitor("api", Some(vec!["db"])),
            monitor("web", Some(vec!["api"])),
        ];
        validate_dag(&monitors).expect("linear DAG is valid");
    }

    #[test]
    fn rejects_unknown_upstream() {
        let monitors = vec![monitor("api", Some(vec!["ghost"]))];
        let err = validate_dag(&monitors).unwrap_err().to_string();
        assert!(err.contains("unknown monitor"), "got: {err}");
    }

    #[test]
    fn rejects_cycle() {
        let monitors = vec![monitor("a", Some(vec!["b"])), monitor("b", Some(vec!["a"]))];
        let err = validate_dag(&monitors).unwrap_err().to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn rejects_self_loop() {
        let monitors = vec![monitor("a", Some(vec!["a"]))];
        let err = validate_dag(&monitors).unwrap_err().to_string();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn upstreams_are_transitive() {
        let monitors = vec![
            monitor("db", None),
            monitor("cache", None),
            monitor("api", Some(vec!["db", "cache"])),
            monitor("web", Some(vec!["api"])),
        ];
        let ups = transitive_upstreams(&monitors, "web");
        assert_eq!(ups, vec!["api", "db", "cache"]);
    }

    #[test]
    fn upstreams_empty_for_root() {
        let monitors = vec![monitor("db", None)];
        assert!(transitive_upstreams(&monitors, "db").is_empty());
    }

    #[test]
    fn dependents_are_transitive() {
        let monitors = vec![
            monitor("db", None),
            monitor("api", Some(vec!["db"])),
            monitor("web", Some(vec!["api"])),
            monitor("worker", Some(vec!["db"])),
        ];
        let deps = transitive_dependents(&monitors, "db");
        assert!(deps.contains(&"api"));
        assert!(deps.contains(&"worker"));
        assert!(deps.contains(&"web"));
    }

    #[test]
    fn dependents_empty_for_leaf() {
        let monitors = vec![monitor("db", None), monitor("api", Some(vec!["db"]))];
        assert!(transitive_dependents(&monitors, "api").is_empty());
    }

    #[test]
    fn name_lookup() {
        let monitors = vec![monitor("db", None)];
        assert_eq!(monitor_name(&monitors, "db"), Some("DB"));
        assert_eq!(monitor_name(&monitors, "nope"), None);
    }
}
