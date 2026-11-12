//! The operator's view of the mesh (`/watchers`): this node's health, every
//! configured peer as this node sees it (the `/healthz` data), how the
//! confirmations went over 30 days, and each shared service from each place.
//! Operator only: node names, heartbeats and verdicts never reach a public
//! page.

use std::fmt::Write as _;

use hora_core::config::Config;
use hora_core::db::Incident;
use hora_core::fmt::xml_escape;
use hora_core::mesh::wire::{HealthReport, HealthStatus, PeerState};

use crate::display::DisplayState;
use crate::layout::{Chrome, ms};
use crate::page::{Document, glyph, raw, state};
use crate::render::{OWL_PARTS as OWL, coord, owl};
use crate::summary::Summary;
use maud::{Markup, html};

/// Services listed in the per-place table, disagreements first.
const MATRIX_ROWS: usize = 60;

pub(crate) struct WatchersTemplate {
    pub(crate) chrome: Chrome,
    pub(crate) page: WatchersPage,
}

impl WatchersTemplate {
    /// The mesh as a picture, and the nodes one by one.
    fn mesh(&self) -> Markup {
        let page = &self.page;
        html! {
            div.meshgrid {
                section.panel aria-labelledby="mp" {
                    div.panel-head {
                        h2 id="mp" { "The mesh" }
                        span.aside { "As this node sees it" }
                    }
                    (raw(&page.map))
                    div.key {
                        span { i {} "Heartbeats flowing" }
                        span { i.t {} "No heartbeat lately" }
                        span { i.e {} "Heartbeats sent, not watched back" }
                    }
                }
                section aria-labelledby="nd" {
                    div.section-head { h2 id="nd" { "Nodes" } }
                    ul.nodes {
                        @for node in &page.nodes {
                            li class={ "node " (node.state) } {
                                (node.owl)
                                span.n { (node.name) small { (node.id) } }
                                span.s { (state(node.state, node.word)) }
                                span.d { (node.detail) }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Each shared service as each place sees it.
    fn matrix(&self) -> Markup {
        let page = &self.page;
        html! {
            div.tw.panel.table tabindex="0" role="region" aria-labelledby="lx" {
                table.mx {
                    thead { tr {
                        th scope="col" { "Service" }
                        @for column in &page.columns { th scope="col" { (column) } }
                    } }
                    tbody {
                        @for row in &page.rows {
                            tr class=[row.disagree.then_some("disagree")] {
                                td { (row.name) }
                                @for (state, text) in &row.cells {
                                    td {
                                        (glyph(state))
                                        @if *state == "up" { (text) }
                                        @else { span class={ "word " (state) } { (text) } }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            @if page.rows_more > 0 {
                p.more { "And " (page.rows_more) " more services watched from several places." }
            }
        }
    }

    pub(crate) fn render(&self) -> String {
        let (chrome, page) = (&self.chrome, &self.page);
        let tile = |key: &str, value: usize, note: Markup| {
            html! {
                div.tile { span.k { (key) } span.v { (value) } span.n { (note) } }
            }
        };
        let main = html! {
            main id="main" class="page" {
                nav.crumbs aria-label="Breadcrumb" {
                    a href={ "/" (raw(chrome.q())) } { "Status" }
                    span.sep aria-hidden="true" { "/" }
                    span aria-current="page" { "Watchers" }
                }
                div.title {
                    h1 { "Watchers" }
                    span class={ "state-big " (page.state) } { (state(page.state, &page.state_word)) }
                }
                p.lede { (page.lede) }
                (self.mesh())
                section.section aria-labelledby="cf" {
                    div.section-head {
                        h2 id="cf" { "Confirmations, last 30 days" }
                        span.fact { "When this node sees a service down, it asks the others first" }
                    }
                    div.stats {
                        (tile("Asked", page.asked, html! { "times this node saw a service down" }))
                        (tile("Confirmed", page.confirmed, state("down", "Real outages")))
                        (tile("Local only", page.local, state("degraded", "Network near this node")))
                        (tile("No answer", page.unconfirmed, state("none", "No peer could be asked")))
                    }
                }
                section.section aria-labelledby="lx" {
                    div.section-head {
                        h2 id="lx" { "Each service, from each place" }
                        span.fact { "Typical answer time over 24 h" }
                    }
                    @if page.rows.is_empty() {
                        p.quiet-line {
                            (glyph("none"))
                            "No peer shares its view of these services yet. Peers report what they watch too, once they can be asked ("
                            code { "/api/peer/monitors" }
                            ")."
                        }
                    } @else {
                        (self.matrix())
                    }
                }
            }
        };
        Document::new(
            chrome,
            format!("Watchers · {} status", chrome.site),
            "The Hora mesh: who watches what, and each other.".to_owned(),
            main,
        )
        .render()
    }
}

pub(crate) struct WatchersPage {
    /// The headline chip: `up` "All well", or how many links are broken.
    pub(crate) state: &'static str,
    pub(crate) state_word: String,
    pub(crate) lede: String,
    pub(crate) map: String,
    pub(crate) nodes: Vec<Node>,
    pub(crate) asked: usize,
    pub(crate) confirmed: usize,
    pub(crate) local: usize,
    pub(crate) unconfirmed: usize,
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<MatrixRow>,
    pub(crate) rows_more: usize,
}

pub(crate) struct Node {
    pub(crate) owl: Markup,
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) state: &'static str,
    pub(crate) word: &'static str,
    pub(crate) detail: String,
}

pub(crate) struct MatrixRow {
    pub(crate) name: String,
    pub(crate) disagree: bool,
    pub(crate) cells: Vec<(&'static str, String)>,
}

/// One peer as this node sees it.
struct PeerLink {
    name: String,
    id: String,
    state: &'static str,
    word: &'static str,
    detail: String,
    label: String,
    link: &'static str,
}

/// An age at a human size: `21 s`, `6 min`, `3 h`, `3 d`
/// ([`hora_core::fmt::elapsed`]; a clock skew cannot make it negative).
fn ago(secs: i64) -> String {
    hora_core::fmt::elapsed(secs.max(0).unsigned_abs())
}

// One flat assembly of the page's parts; splitting it would only scatter it.
#[allow(clippy::too_many_lines)]
pub(crate) fn build(
    config: &Config,
    health: &HealthReport,
    summary: &Summary,
    incidents: &[Incident],
) -> WatchersPage {
    let healthy = health.status == HealthStatus::Ok;
    let self_name = health.id.clone().unwrap_or_else(|| "This Hora".to_owned());
    let pinged = config
        .peers
        .iter()
        .filter(|peer| peer.ping_url.is_some())
        .count();
    let mut self_detail = match (health.scheduler_ok, health.db_ok) {
        (true, true) => "Scheduler ticking, database writable.".to_owned(),
        (false, true) => format!(
            "The scheduler has not ticked for {}.",
            ago(health.last_tick_age.max(0))
        ),
        (true, false) => "The database does not answer.".to_owned(),
        (false, false) => "Scheduler stalled and database not answering.".to_owned(),
    };
    if !health.probes_ok {
        self_detail
            .push_str(" Checks are being skipped: this host is out of file descriptors or memory.");
    }
    if pinged > 0 {
        let _ = write!(
            self_detail,
            " Sends heartbeats to {pinged} peer{}.",
            if pinged == 1 { "" } else { "s" }
        );
    }

    let links: Vec<PeerLink> = config
        .peers
        .iter()
        .map(|peer| {
            let seen = health.peers.get(&peer.id);
            let shared = summary
                .monitors
                .iter()
                .filter(|monitor| monitor.vantages.iter().any(|v| v.peer == peer.name))
                .count();
            let mut detail;
            let (state, word, label, link) = match seen {
                Some(seen) if seen.state == PeerState::Up => {
                    detail = format!("Last heartbeat {} ago.", ago(seen.age));
                    (
                        "up",
                        "Well",
                        format!("heartbeat {} ago", ago(seen.age)),
                        "link",
                    )
                }
                Some(seen) if seen.state == PeerState::Down => {
                    detail = if seen.age >= 0 {
                        format!("No heartbeat here for {}. ", ago(seen.age))
                    } else {
                        "No heartbeat here. ".to_owned()
                    };
                    detail.push_str("Down, or the route between the two is broken.");
                    (
                        "down",
                        "Silent",
                        if seen.age >= 0 {
                            format!("no heartbeat for {}", ago(seen.age))
                        } else {
                            "no heartbeat".to_owned()
                        },
                        "link split",
                    )
                }
                Some(_) => {
                    detail = "Never heard from yet.".to_owned();
                    (
                        "none",
                        "Not heard yet",
                        "never heard".to_owned(),
                        "link split",
                    )
                }
                None => {
                    detail = if peer.ping_url.is_some() {
                        "Not watched from here; this node sends it heartbeats.".to_owned()
                    } else {
                        "Not watched from here.".to_owned()
                    };
                    (
                        "none",
                        "Not watched",
                        "heartbeats sent".to_owned(),
                        "link out",
                    )
                }
            };
            if shared > 0 {
                let _ = write!(
                    detail,
                    " Shares its view of {shared} service{}.",
                    if shared == 1 { "" } else { "s" }
                );
            }
            PeerLink {
                name: peer.name.clone(),
                id: peer.id.clone(),
                state,
                word,
                detail,
                label,
                link,
            }
        })
        .collect();

    // A watched peer never heard from is as silent as one that stopped.
    let broken = links
        .iter()
        .filter(|link| link.state == "down" || link.word == "Not heard yet")
        .count();
    let (state, state_word) = if !healthy {
        ("degraded", "This node is not healthy".to_owned())
    } else if broken > 0 {
        (
            "degraded",
            format!("{broken} peer{} silent", if broken == 1 { "" } else { "s" }),
        )
    } else {
        ("up", "All well".to_owned())
    };

    let mut nodes = vec![Node {
        owl: owl(if healthy { "up" } else { "degraded" }, 44),
        name: self_name.clone(),
        id: "this node".to_owned(),
        state: if healthy { "up" } else { "degraded" },
        word: if healthy { "Well" } else { "Unwell" },
        detail: self_detail,
    }];
    nodes.extend(links.iter().map(|link| Node {
        owl: owl(link.state, 44),
        name: link.name.clone(),
        id: link.id.clone(),
        state: link.state,
        word: link.word,
        detail: link.detail.clone(),
    }));

    // The 30-day confirmations: what the peers said when this node saw a down.
    let since = chrono::Utc::now().timestamp() - 30 * hora_core::SECONDS_PER_DAY;
    let verdicts: Vec<&str> = incidents
        .iter()
        .filter(|incident| incident.started_at >= since)
        .filter_map(|incident| incident.vantage.as_deref())
        .collect();
    let confirmed = verdicts
        .iter()
        .filter(|v| v.starts_with("confirmed down"))
        .count();
    let local = verdicts.iter().filter(|v| v.starts_with("seen UP")).count();
    let unconfirmed = verdicts
        .iter()
        .filter(|v| v.starts_with("no peer vantage"))
        .count();

    let (columns, rows, rows_more) = matrix(summary);
    let total = 1 + links.len();
    WatchersPage {
        state,
        state_word,
        lede: format!(
            "{total} Hora node{} watch your services{}. Each one asks the others before it calls \
             an outage.",
            if total == 1 { "" } else { "s" },
            if total > 1 { " and each other" } else { "" }
        ),
        map: map(&self_name, healthy, &links),
        nodes,
        asked: verdicts.len(),
        confirmed,
        local,
        unconfirmed,
        columns,
        rows,
        rows_more,
    }
}

/// The mesh drawn: this node in the middle, each peer around it, the link
/// solid while heartbeats flow, dashed when they stopped. Only this node's
/// own links are known here (a peer's view of another peer is not).
fn map(self_name: &str, healthy: bool, links: &[PeerLink]) -> String {
    // This node on the left, its peers fanned out on an arc to the right,
    // so no label sits on a link.
    const CX: f64 = 110.0;
    const CY: f64 = 190.0;
    let mut edges = String::new();
    let mut nodes = String::new();
    let count = links.len();
    for (index, link) in links.iter().enumerate() {
        let spread = std::f64::consts::FRAC_PI_3;
        let angle = if count > 1 {
            -spread + 2.0 * spread * coord(index) / coord(count - 1)
        } else {
            0.0
        };
        let (x, y) = (CX + 330.0 * angle.cos(), CY + 160.0 * angle.sin());
        let _ = write!(
            edges,
            "<path class=\"{}\" d=\"M{CX:.0} {CY:.0}L{x:.0} {y:.0}\"/>\
             <text class=\"ll{}\" x=\"{:.0}\" y=\"{:.0}\" text-anchor=\"middle\">{}</text>",
            link.link,
            if link.state == "up" { "" } else { " split" },
            f64::midpoint(CX, x) + 20.0,
            f64::midpoint(CY, y) - 10.0,
            xml_escape(&link.label),
        );
        let _ = write!(
            nodes,
            "<g transform=\"translate({x:.0} {y:.0})\"><circle class=\"ring\" r=\"34\"/>\
             <svg class=\"owl\" data-state=\"{}\" viewBox=\"6 6 40 40\" width=\"48\" height=\"48\" \
             x=\"-24\" y=\"-26\">{OWL}</svg>\
             <text class=\"lab\" y=\"52\" text-anchor=\"middle\">{}</text></g>",
            link.state,
            xml_escape(&link.name),
        );
    }
    let label = if links.is_empty() {
        format!("{self_name} watches alone: no peer configured.")
    } else {
        let silent = links.iter().filter(|link| link.state == "down").count();
        format!(
            "{self_name} and {} peer{}; {}",
            links.len(),
            if links.len() == 1 { "" } else { "s" },
            if silent == 0 {
                "heartbeats flow on every watched link.".to_owned()
            } else {
                format!("{silent} sent no heartbeat lately.")
            }
        )
    };
    // As tall as the fan of peers needs, no taller: one peer sits level with
    // this node, and a fixed square left that drawing afloat in an empty card.
    let reach = if count > 1 {
        160.0 * std::f64::consts::FRAC_PI_3.sin()
    } else {
        0.0
    };
    let (top, height) = (CY - reach - 56.0, 2.0 * reach + 140.0);
    format!(
        "<svg class=\"map\" viewBox=\"0 {top:.0} 560 {height:.0}\" role=\"img\" aria-label=\"{}\">\
         {edges}\
         <g transform=\"translate({CX:.0} {CY:.0})\"><circle class=\"ring me\" r=\"34\"/>\
         <svg class=\"owl\" data-state=\"{}\" viewBox=\"6 6 40 40\" width=\"48\" height=\"48\" \
         x=\"-24\" y=\"-26\">{OWL}</svg>\
         <text class=\"lab\" y=\"52\" text-anchor=\"middle\">{}</text>\
         <text class=\"sub\" y=\"67\" text-anchor=\"middle\">this Hora</text></g>{nodes}</svg>",
        xml_escape(&label),
        if healthy { "up" } else { "degraded" },
        xml_escape(self_name),
    )
}

/// Each shared service from each place: this node, then every peer that
/// reported on at least one of them. Disagreements first.
fn matrix(summary: &Summary) -> (Vec<String>, Vec<MatrixRow>, usize) {
    let mut peers: Vec<String> = Vec::new();
    for monitor in &summary.monitors {
        for vantage in &monitor.vantages {
            if !peers.contains(&vantage.peer) {
                peers.push(vantage.peer.clone());
            }
        }
    }
    let mut rows: Vec<MatrixRow> = summary
        .monitors
        .iter()
        .filter(|monitor| !monitor.vantages.is_empty())
        .map(|monitor| {
            let here = DisplayState::from(monitor.status);
            let here_cell = match here {
                DisplayState::Up | DisplayState::Degraded => monitor
                    .p50_ms
                    .or(monitor.last_latency_ms)
                    .map_or_else(|| here.word().to_owned(), ms),
                DisplayState::Down => "Down here".to_owned(),
                DisplayState::Maint | DisplayState::None => here.word().to_owned(),
            };
            // What each peer that watches it sees; `None` where one does not.
            let mut seen: Vec<DisplayState> = Vec::new();
            let mut cells = vec![(here.class(), here_cell)];
            for peer in &peers {
                cells.push(
                    match monitor
                        .vantages
                        .iter()
                        .find(|vantage| &vantage.peer == peer)
                    {
                        Some(vantage) => {
                            let state = DisplayState::from(vantage.status);
                            let text = match vantage.p50_ms {
                                Some(latency) if state.answers() => ms(latency),
                                _ => state.word().to_owned(),
                            };
                            seen.push(state);
                            (state.class(), text)
                        }
                        None => (DisplayState::None.class(), "Not watched there".to_owned()),
                    },
                );
            }
            // A peer without data takes no side.
            let disagree = seen
                .iter()
                .filter(|state| **state != DisplayState::None)
                .any(|state| (*state == DisplayState::Down) != (here == DisplayState::Down));
            MatrixRow {
                name: monitor.name.clone(),
                disagree,
                cells,
            }
        })
        .collect();
    rows.sort_by_key(|row| !row.disagree);
    let more = rows.len().saturating_sub(MATRIX_ROWS);
    rows.truncate(MATRIX_ROWS);
    let mut columns = vec!["Here".to_owned()];
    columns.extend(peers);
    (columns, rows, more)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages_read_at_a_human_size() {
        assert_eq!(ago(21), "21 s");
        assert_eq!(ago(360), "6 min");
        assert_eq!(ago(7200), "2 h");
        assert_eq!(ago(3 * 86_400), "3 d");
    }
}
