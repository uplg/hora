//! Plain-text status rendering for curl and other text-based clients: the
//! same sentence as the page's hero, then every service with its state's
//! shape and word (never a symbol alone).

use std::fmt::Write as _;

use crate::display::DisplayState;
use crate::summary::Summary;

/// Render the summary as text. `operator` adds the watched peers (node names
/// stay off public pages).
pub(crate) fn render(summary: &Summary, operator: bool) -> String {
    let mut out = String::with_capacity(2048);
    let _ = writeln!(out, "{}", summary.title);
    // Bytes would over-shoot on accented titles; chars is close enough.
    let _ = writeln!(out, "{}", "=".repeat(summary.title.chars().count()));
    let _ = writeln!(out);
    if summary.monitors.is_empty() {
        let _ = writeln!(out, "Nothing to watch yet.");
        let _ = writeln!(out, "{}", summary.when());
        return out;
    }
    let _ = writeln!(out, "{}", crate::status_page::summary_sentence(summary));
    let _ = writeln!(out, "{}", summary.when());
    let _ = writeln!(out);

    if !summary.incidents.is_empty() {
        let _ = writeln!(out, "Announcements:");
        for incident in &summary.incidents {
            let _ = writeln!(
                out,
                "  [{}] {}",
                incident.severity.to_uppercase(),
                incident.title
            );
            if !incident.body.is_empty() {
                let _ = writeln!(out, "    {}", incident.body);
            }
        }
        let _ = writeln!(out);
    }

    if !summary.maintenances.is_empty() {
        let _ = writeln!(out, "Planned maintenance:");
        for m in &summary.maintenances {
            let _ = writeln!(out, "  {} ({})", m.reason, m.monitors);
        }
        let _ = writeln!(out);
    }

    for group in &summary.groups {
        if !group.name.is_empty() {
            let _ = writeln!(out, "{}", group.name);
            let _ = writeln!(out, "{}", "-".repeat(group.name.chars().count()));
        }

        // Pad names to the group's longest so the figures line up
        // (chars, not bytes: names may be accented).
        let width = group
            .monitors
            .iter()
            .map(|monitor| monitor.name.chars().count())
            .max()
            .unwrap_or(0);
        for monitor in &group.monitors {
            let uptime = monitor
                .uptime_label
                .clone()
                .unwrap_or_else(|| "-".to_owned());
            let latency = monitor
                .last_latency_ms
                .map_or_else(|| "-".to_owned(), hora_core::fmt::latency);
            let state = monitor.display_state();

            let _ = writeln!(
                out,
                "  {} {:<width$}  {:<11}  {:>8}  {:>8}",
                state.symbol(),
                monitor.name,
                monitor.word(),
                uptime,
                latency
            );

            if monitor.local_only() {
                let _ = writeln!(
                    out,
                    "      one checkpoint can't reach it; the others can (not an outage)"
                );
            }
            if let Some(cause) = &monitor.cause {
                let _ = writeln!(out, "      probably caused by {cause}");
            }
            if !monitor.impacted.is_empty() {
                let _ = writeln!(out, "      {} depend on it", monitor.impacted.join(", "));
            }
        }
        let _ = writeln!(out);
    }

    if operator && !summary.peers.is_empty() {
        let _ = writeln!(out, "Watchers");
        let _ = writeln!(out, "--------");
        for peer in &summary.peers {
            let state = DisplayState::from(peer.status);
            let _ = writeln!(out, "  {} {}", state.symbol(), peer.name);
        }
    }

    out
}
