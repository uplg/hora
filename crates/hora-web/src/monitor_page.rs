//! One monitor's own page (`/monitor/{id}`): its state, the figures with a
//! unit and a period, the 24 h response time, one link per day of the bar
//! (reachable by keyboard and touch, its detail shown without script), the
//! hour-by-hour heatmap, where it is checked from, and its incidents.
//! Built from the monitor's summary view (no extra summary work) plus its
//! recent incidents.

use std::fmt::Write as _;
use std::sync::Arc;

use chrono::{DateTime, NaiveDate, Utc};
use hora_core::config::{Config, Kind, Monitor};
use hora_core::db::{EventMarker, Incident};

use crate::display::DisplayState;
use crate::layout::{Chrome, join_and, minutes, ms, pct};
use crate::page::{Document, glyph, raw, state};
use crate::render::{Chart, meter, owls, sparkline};
use crate::summary::{BudgetState, CertState, MonitorView};
use maud::{Markup, html};

pub(crate) struct MonitorTemplate {
    pub(crate) chrome: Chrome,
    pub(crate) page: MonitorPage,
}

impl MonitorTemplate {
    /// The name, the state, what is checked, and the figures that sum it up.
    fn heading(&self) -> Markup {
        let (page, q) = (&self.page, raw(self.chrome.q()));
        html! {
            nav.crumbs aria-label="Breadcrumb" {
                a href={ "/" (q) } { "Status" }
                span.sep aria-hidden="true" { "/" }
                @if let Some((name, href)) = &page.group {
                    a href={ (href) (q) } { (name) }
                    span.sep aria-hidden="true" { "/" }
                }
                span aria-current="page" { (page.m.name) }
            }
            div.title {
                h1 { (page.m.name) }
                span class={ "state-big " (page.state) } { (state(page.state, &page.word)) }
            }
            p.target {
                @if let Some(target) = &page.target { code { (target) } " · " }
                (page.checked)
            }
            @if let Some(why) = &page.why { p.lede { (why) } }
            @if !page.tiles.is_empty() {
                div.tiles {
                    @for tile in &page.tiles {
                        div.tile {
                            span.k { (tile.key) }
                            span.v {
                                (tile.value)
                                @if !tile.unit.is_empty() { small { (tile.unit) } }
                            }
                            (tile.meter)
                            @if !tile.note.is_empty() { span.n { (tile.note) } }
                        }
                    }
                }
            }
        }
    }

    /// The last 24 hours of answer times, with what the drawing shows said
    /// in words.
    fn response_time(&self) -> Markup {
        let (page, chart) = (&self.page, &self.page.chart);
        let mut label = format!("Response time of {} over the last 24 hours", page.m.name);
        if !chart.summary.is_empty() {
            let _ = write!(label, ": {}", chart.summary);
        }
        if let Some(limit) = &chart.limit {
            let _ = write!(label, ". Counted slow above {}", limit.text);
        }
        label.push('.');
        html! {
            section.panel.spaced aria-labelledby="rt" {
                div.panel-head {
                    h2 id="rt" { "Response time" }
                    span.aside { "Last 24 h, times in UTC" }
                }
                div role="img" aria-label=(label) { (chart.html) }
                div.key {
                    span { i {} "Response time (averaged over a few minutes)" }
                    @if let Some(limit) = &chart.limit {
                        @if limit.drawn { span { i.t {} "Slow above " (limit.text) } }
                        @else { span { "Slow above " (limit.text) ", off this scale" } }
                    }
                    @if !chart.events.is_empty() { span { i.e.v {} "Deploys and changes" } }
                }
                @if !chart.events.is_empty() {
                    ul.marks aria-label="Deploys and changes on this chart" {
                        @for event in &chart.events {
                            li { time datetime=(event.iso) { (event.at) } span { (event.title) } }
                        }
                    }
                }
            }
        }
    }

    /// The daily bar: a link per day, and each day in words.
    fn days(&self) -> Markup {
        let (page, q) = (&self.page, raw(self.chrome.q()));
        let count = page.days.len();
        let notable = page.notable_days();
        html! {
            section.panel.spaced aria-labelledby="days" {
                div.panel-head {
                    h2 id="days" { "Last " (count) " days" }
                    span.aside { "Pick a day to read it" }
                }
                div.days-wrap {
                    ol.days aria-label={ "Last " (count) " days, one link per day" } {
                        @for day in &page.days {
                            li {
                                a class=(day.class) href={ "#" (day.anchor) } data-tip=(day.tip) aria-label=(day.tip) {}
                            }
                        }
                    }
                }
                div.bar-legend.flush aria-hidden="true" {
                    span.dall { (page.first_day) }
                    span.d30 { (page.day30) }
                    span { "Today" }
                }
                @if !notable.is_empty() {
                    ul.daylinks aria-label="Days with something to report, newest first" {
                        @for day in &notable {
                            li class=[(!day.recent).then_some("old")] {
                                a class={ "chip " (day.state) } href={ "#" (day.anchor) } {
                                    (glyph(day.state)) (day.short) " · " (day.word)
                                }
                            }
                        }
                    }
                }
                @for day in &page.days {
                    div class={ "daydetail " (day.state) } id=(day.anchor) {
                        (glyph(day.state))
                        span.k { (day.title) }
                        p { (day.text) }
                        @if !day.incidents.is_empty() {
                            ul {
                                @for (id, text) in &day.incidents {
                                    li { a href={ "/incident/" (id) (q) } { (text) } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Where it is checked from, and its incidents.
    fn places_and_incidents(&self) -> Markup {
        let (page, q) = (&self.page, raw(self.chrome.q()));
        let all_history = html! { a.aside href={ "/history" (q) } { "All history" } };
        html! {
            section.panel aria-labelledby="vp" {
                @if page.places.is_empty() {
                    div.panel-head { h2 id="vp" { "Incidents" } (all_history) }
                } @else {
                    div.panel-head {
                        h2 id="vp" { "Checked from " (page.places.len()) " places" }
                        span.aside { (state(page.places_state, &page.places_word)) }
                    }
                    div.vpwrap {
                        (page.places_owls)
                        p.muted.small {
                            "Each place checks on its own. An outage is called only when they agree, so a hiccup near one place does not page anyone."
                        }
                    }
                    ul.vlist {
                        @for place in &page.places {
                            li {
                                (glyph(place.state))
                                span.n { (place.name) small { (place.sub) } }
                                span.r {
                                    span class={ "word " (place.state) } { (place.word) }
                                    (place.meter)
                                    (place.latency)
                                }
                            }
                        }
                    }
                    p.muted.small {
                        "Typical answer time over 24 h, from each place. A place further away answers slower; that is distance, not trouble."
                    }
                    div.panel-head.later { h2 id="inc" { "Incidents" } (all_history) }
                }
                @if page.incidents.is_empty() {
                    p.quiet-line { (glyph("up")) "No incident recorded lately." }
                } @else {
                    ul.incl {
                        @for incident in &page.incidents {
                            li {
                                (glyph(incident.state))
                                span.t {
                                    a href={ "/incident/" (incident.id) (q) } { (incident.title) }
                                    @if !incident.small.is_empty() { small { (incident.small) } }
                                }
                                span.r { (incident.date) }
                            }
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn render(&self) -> String {
        let (chrome, page) = (&self.chrome, &self.page);
        let main = html! {
            main id="main" class="page" {
                (self.heading())
                (self.response_time())
                (self.days())
                div.two {
                    @if let Some(heatmap) = &page.heatmap {
                        section.panel aria-labelledby="hm" {
                            div.panel-head { h2 id="hm" { "Hour by hour, 4 weeks" } }
                            (raw(heatmap))
                        }
                    }
                    (self.places_and_incidents())
                }
                @if !page.details.is_empty() {
                    details.ops {
                        summary {
                            svg.chev aria-hidden="true" { use href="#i-chev" {} }
                            "Details "
                            span.muted { "for operators: probe, percentiles, targets" }
                        }
                        div.in { dl.kv {
                            @for (key, value) in &page.details { dt { (key) } dd { (value) } }
                        } }
                    }
                }
            }
        };
        Document::new(
            chrome,
            format!("{} · {} status", page.m.name, chrome.site),
            format!(
                "{}: {}. Uptime, response time and incidents.",
                page.m.name, page.word
            ),
            main,
        )
        .render()
    }
}

pub(crate) struct MonitorPage {
    pub(crate) m: Arc<MonitorView>,
    pub(crate) state: &'static str,
    /// The state's word, with how long for a down: "Down · 9 min".
    pub(crate) word: String,
    /// The group's name and page (`/status/{group}`), for the breadcrumb.
    pub(crate) group: Option<(String, String)>,
    /// "checked every 30 s from 3 places".
    pub(crate) checked: String,
    /// The probed target: operator and group viewers only.
    pub(crate) target: Option<String>,
    pub(crate) why: Option<String>,
    pub(crate) tiles: Vec<Tile>,
    pub(crate) chart: Chart,
    pub(crate) days: Vec<Day>,
    pub(crate) first_day: String,
    pub(crate) day30: String,
    pub(crate) heatmap: Option<String>,
    pub(crate) places: Vec<PlaceRow>,
    pub(crate) places_owls: Markup,
    pub(crate) places_state: &'static str,
    pub(crate) places_word: String,
    pub(crate) incidents: Vec<IncidentLine>,
    /// "For operators": label/value pairs, empty for public visitors.
    pub(crate) details: Vec<(String, String)>,
}

impl MonitorPage {
    /// The days with something to report, newest first: the links listed
    /// under the bar.
    pub(crate) fn notable_days(&self) -> Vec<&Day> {
        self.days.iter().rev().filter(|day| day.notable).collect()
    }
}

/// A figure: what, the number and its unit, the line that explains it.
pub(crate) struct Tile {
    pub(crate) key: String,
    pub(crate) value: String,
    pub(crate) unit: String,
    pub(crate) note: String,
    pub(crate) meter: Markup,
}

/// One day of the bar: a link to its own detail (`#day-2026-09-18`).
pub(crate) struct Day {
    pub(crate) anchor: String,
    pub(crate) class: char,
    pub(crate) state: &'static str,
    pub(crate) tip: String,
    /// "18 Sep" and the state's word, for the list of days worth opening.
    pub(crate) short: String,
    pub(crate) word: &'static str,
    /// Something to report (an outage, slow, maintenance): listed under the
    /// bar as a link of its own, a full-size target where the bar's are thin.
    pub(crate) notable: bool,
    /// Among the last 30 days, the ones a phone's bar shows.
    pub(crate) recent: bool,
    pub(crate) title: String,
    pub(crate) text: String,
    pub(crate) incidents: Vec<(i64, String)>,
}

/// One place's view of this monitor.
pub(crate) struct PlaceRow {
    pub(crate) name: String,
    pub(crate) sub: String,
    pub(crate) state: &'static str,
    pub(crate) word: &'static str,
    pub(crate) latency: String,
    pub(crate) meter: Markup,
}

/// One past incident of this monitor.
pub(crate) struct IncidentLine {
    pub(crate) id: i64,
    pub(crate) state: &'static str,
    pub(crate) title: String,
    pub(crate) small: String,
    pub(crate) date: String,
}

/// Build the page. `events` are the markers this audience sees on the chart
/// (the operator's only); `detailed` reveals the operator's details (the
/// probe, the percentiles, the topology); `show_target` the probed address.
// The page's inputs, each from its own source; a struct would only rename them.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build(
    m: Arc<MonitorView>,
    config: &Config,
    monitor: &Monitor,
    incidents: &[Incident],
    events: &[EventMarker],
    heatmap: Option<String>,
    detailed: bool,
    show_target: bool,
) -> MonitorPage {
    let now = Utc::now().timestamp();
    let state = m.display_state();
    let places_count = 1 + m.vantages.len();
    let every = hora_core::fmt::period(monitor.interval_secs);
    let checked = if monitor.kind() == Kind::Push {
        format!("expects a heartbeat every {every}")
    } else if places_count > 1 {
        format!("checked every {every} from {places_count} places")
    } else {
        format!("checked every {every}")
    };

    let days = days(&m, incidents);
    let first_day = m.bar.first().map_or_default(|cell| short_date(&cell.date));
    let day30 = m
        .bar
        .len()
        .checked_sub(30)
        .and_then(|index| m.bar.get(index))
        .map_or_default(|cell| short_date(&cell.date));

    let (places, places_owls, places_state, places_word) = places(&m);
    let incident_lines = incidents
        .iter()
        .take(8)
        .map(|incident| {
            let ongoing = incident.ended_at.is_none();
            IncidentLine {
                id: incident.id,
                state: if ongoing { "down" } else { "up" },
                title: match incident.duration_s {
                    Some(secs) if !ongoing => {
                        format!("Down for {}", hora_core::fmt::duration(secs))
                    }
                    _ => "Down, ongoing".to_owned(),
                },
                small: incident
                    .cause
                    .as_ref()
                    .map(|cause| format!("Caused by {cause}"))
                    .or_else(|| incident.error.clone())
                    .unwrap_or_default(),
                date: DateTime::from_timestamp(incident.started_at, 0)
                    .map_or_default(|at| at.format("%-d %b").to_string()),
            }
        })
        .collect();

    // The chart is drawn here, for the one monitor asked for (the status
    // page draws none); the series is shared with the snapshot.
    let chart = sparkline(
        &m.spark,
        m.status.as_str(),
        events,
        monitor.degraded_over_ms,
    );
    MonitorPage {
        state: state.class(),
        word: match down_for(&m, now) {
            Some(length) => format!("{} · {}", m.word(), minutes(length)),
            None => m.word().to_owned(),
        },
        group: m.group.as_ref().map(|group| {
            (
                group.clone(),
                format!("/status/{}", hora_core::fmt::percent_encode(group)),
            )
        }),
        checked,
        // A target URL may carry credentials (`https://user:pass@...`,
        // `?api_key=${KEY}`): a group viewer is a client, not the operator.
        target: (show_target && !monitor.target().is_empty())
            .then(|| hora_core::config::redact_url_secrets(monitor.target()).into_owned()),
        why: why_line(&m, now),
        tiles: tiles(&m, monitor, incidents),
        chart,
        days,
        first_day,
        day30,
        heatmap,
        places,
        places_owls,
        places_state,
        places_word,
        incidents: incident_lines,
        details: if detailed {
            details(&m, config, monitor, show_target)
        } else {
            Vec::new()
        },
        m,
    }
}

/// How long a monitor shown down has been down, in whole minutes (its open
/// incident's age); `None` when it is not shown down or the start is unknown.
pub(crate) fn down_for(m: &MonitorView, now: i64) -> Option<i64> {
    let since = m
        .down_since
        .filter(|_| m.display_state() == DisplayState::Down)?;
    Some((now - since).max(0) / 60)
}

/// "Since 14:02 UTC (38 min)": when a down started, the day too when it was
/// not today. `None` when it is not shown down.
pub(crate) fn since_line(m: &MonitorView, now: i64) -> Option<String> {
    let length = down_for(m, now)?;
    let since = DateTime::from_timestamp(m.down_since?, 0)?;
    let today = DateTime::from_timestamp(now, 0)?.date_naive();
    let at = if since.date_naive() == today {
        since.format("%H:%M UTC").to_string()
    } else {
        since.format("%a %-d %b, %H:%M UTC").to_string()
    };
    Some(format!("Since {at} ({}).", minutes(length)))
}

/// The line under the title when the monitor is not plainly up.
fn why_line(m: &MonitorView, now: i64) -> Option<String> {
    if m.local_only() {
        return Some(
            "One of our checkpoints can't reach it; the other places can. Not an outage."
                .to_owned(),
        );
    }
    match m.display_state() {
        DisplayState::Down => {
            let answer = m.last_error.as_ref().map_or_else(
                || "It does not answer.".to_owned(),
                |error| format!("It does not answer as expected: {error}."),
            );
            Some(match since_line(m, now) {
                Some(since) => format!("{since} {answer}"),
                None => answer,
            })
        }
        DisplayState::Degraded => Some(match (&m.last_error, m.last_latency_ms) {
            (Some(error), _) => {
                format!("The last check failed ({error}); checking again before calling it down.")
            }
            (None, Some(latency)) => format!("It answers, slowly: {}.", ms(latency)),
            _ => "It answers, slowly.".to_owned(),
        }),
        DisplayState::Maint => Some(format!(
            "Planned maintenance{}.",
            m.maintenance
                .as_deref()
                .filter(|title| !title.is_empty())
                .map_or_default(|title| format!(": {title}"))
        )),
        DisplayState::None => Some("Waiting for the first check.".to_owned()),
        DisplayState::Up => None,
    }
}

fn tiles(m: &MonitorView, monitor: &Monitor, incidents: &[Incident]) -> Vec<Tile> {
    let mut tiles = Vec::new();
    let days = m.bar.len();
    // Over the bar's days when there is data for them, else the 24 h.
    let (key, permille) = match (m.uptime_long_permille, m.uptime_permille) {
        (Some(long), _) => (format!("Up over {days} days"), Some(long)),
        (None, day) => ("Up over 24 h".to_owned(), day),
    };
    if let Some(permille) = permille {
        tiles.push(Tile {
            key,
            value: pct(permille),
            unit: String::new(),
            note: outage_note(m, incidents),
            meter: Markup::default(),
        });
    }
    if let Some(p50) = m.p50_ms {
        let (value, unit) = split_unit(&ms(p50));
        tiles.push(Tile {
            key: "Response time, 24 h".to_owned(),
            value,
            unit: format!("{unit} typical"),
            note: m
                .p95_ms
                .map_or_default(|p95| format!("Slowest 1 in 20: {}", ms(p95))),
            meter: Markup::default(),
        });
    }
    if let (Some(left), Some(total)) = (m.budget_minutes_left, m.budget_minutes_total) {
        let class = match m.budget_state {
            BudgetState::Breached => "meter out",
            BudgetState::Low => "meter low",
            BudgetState::Met | BudgetState::None => "meter",
        };
        tiles.push(Tile {
            key: "Allowed downtime left".to_owned(),
            // The whole span is the figure: "1" alone, with "h 35 min"
            // small beside it, said nothing.
            value: minutes(left),
            unit: format!("of {}", minutes(total)),
            note: m.slo_uptime_pct.map_or_else(String::new, |target| {
                format!("Target {target} % over {} days", monitor.slo_window_days())
            }),
            meter: meter(left, total, class),
        });
    }
    if let Some(left) = m.cert_secs_left {
        let (value, unit) = cert_left(left);
        tiles.push(Tile {
            key: "Certificate".to_owned(),
            value: value.to_string(),
            unit: unit.to_owned(),
            note: match m.cert_state {
                CertState::Expired => "Expired: browsers refuse it".to_owned(),
                CertState::Warn => "Renew it soon".to_owned(),
                CertState::Ok | CertState::None => "Valid".to_owned(),
            },
            meter: Markup::default(),
        });
    }
    tiles
}

/// A certificate's time left as a figure and its unit, from the seconds until
/// it expires: days, then hours on the last day, then minutes in the last
/// hour. A certificate with 23 hours left is not expired, and the page must
/// not say so; one that has expired reads 0 d.
fn cert_left(secs_left: i64) -> (i64, &'static str) {
    if secs_left < 0 {
        (0, "d left")
    } else if secs_left >= hora_core::SECONDS_PER_DAY {
        (secs_left / hora_core::SECONDS_PER_DAY, "d left")
    } else if secs_left >= 3600 {
        (secs_left / 3600, "h left")
    } else {
        (secs_left / 60, "min left")
    }
}

/// The outages over the bar's days, in words: "One outage, 41 min, on 18
/// Sep", "3 outages, 2 h 5 min in all", "No outage in 90 days". Counted
/// from the incidents when they reach back that far, else from the days.
fn outage_note(m: &MonitorView, incidents: &[Incident]) -> String {
    let days = m.bar.len();
    let first = m.bar.first().map_or_default(|cell| cell.date.as_str());
    let within: Vec<&Incident> = incidents
        .iter()
        .filter(|incident| {
            DateTime::from_timestamp(incident.started_at, 0)
                .is_some_and(|at| at.format("%Y-%m-%d").to_string().as_str() >= first)
        })
        .collect();
    let minutes_of = |incident: &Incident| incident.duration_s.map(|secs| (secs + 59) / 60);
    match within.as_slice() {
        [] => {
            let outage_days = m.bar.iter().filter(|cell| cell.state == "down").count();
            if outage_days == 0 {
                format!("No outage in {days} days")
            } else {
                format!(
                    "{outage_days} day{} with an outage in {days} days",
                    if outage_days == 1 { "" } else { "s" }
                )
            }
        }
        [one] => {
            let on = DateTime::from_timestamp(one.started_at, 0)
                .map_or_default(|at| at.format("%-d %b").to_string());
            match minutes_of(one) {
                Some(length) => format!("One outage, {}, on {on}", minutes(length)),
                None => format!("One outage, ongoing, since {on}"),
            }
        }
        many => {
            let total: i64 = many
                .iter()
                .filter_map(|incident| minutes_of(incident))
                .sum();
            format!("{} outages, {} in all", many.len(), minutes(total))
        }
    }
}

/// `64 ms` → (`64`, `ms`), so the unit sits small beside the figure.
fn split_unit(text: &str) -> (String, String) {
    text.split_once(' ').map_or_else(
        || (text.to_owned(), String::new()),
        |(value, unit)| (value.to_owned(), unit.to_owned()),
    )
}

fn short_date(date: &str) -> String {
    NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map_or_else(|_| date.to_owned(), |day| day.format("%-d %b").to_string())
}

/// The bar's days, each with its words and the incidents that started then.
fn days(m: &MonitorView, incidents: &[Incident]) -> Vec<Day> {
    let last = m.bar.len().saturating_sub(1);
    m.bar
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let now = index == last && m.maintenance.is_some();
            let class = crate::summary::bar_class(cell.state, cell.maint, now);
            // `day_class` gives these five letters only; anything else
            // would be a day without data.
            let state = match class {
                b'u' => DisplayState::Up,
                b'd' => DisplayState::Degraded,
                b'x' => DisplayState::Down,
                b'm' => DisplayState::Maint,
                _ => DisplayState::None,
            };
            let figure = cell.permille.map(hora_core::fmt::permille);
            let date = NaiveDate::parse_from_str(&cell.date, "%Y-%m-%d").ok();
            let short = short_date(&cell.date);
            let tip = match &figure {
                Some(figure) => format!("{short}: {} · {figure} up", state.word()),
                None => format!("{short}: {}", state.word()),
            };
            let text = match (state, figure) {
                (DisplayState::Up, _) => "Up all day.".to_owned(),
                (DisplayState::None, _) => "No checks recorded this day.".to_owned(),
                (DisplayState::Maint, _) if index == last => {
                    "Planned maintenance today.".to_owned()
                }
                (DisplayState::Maint, _) => "Planned maintenance this day.".to_owned(),
                (DisplayState::Down, Some(figure)) => {
                    format!("Outage: answered as expected in {figure} of the day's checks.")
                }
                (DisplayState::Degraded, Some(figure)) => {
                    format!("Slow at times: answered as expected in {figure} of the day's checks.")
                }
                (DisplayState::Down | DisplayState::Degraded, None) => state.word().to_owned(),
            };
            let started: Vec<(i64, String)> = incidents
                .iter()
                .filter(|incident| {
                    DateTime::from_timestamp(incident.started_at, 0)
                        .is_some_and(|at| at.format("%Y-%m-%d").to_string() == cell.date)
                })
                .map(|incident| {
                    let at = DateTime::from_timestamp(incident.started_at, 0)
                        .map_or_default(|at| at.format("%H:%M UTC").to_string());
                    let length = incident
                        .duration_s
                        .map_or_else(|| "ongoing".to_owned(), hora_core::fmt::duration);
                    (incident.id, format!("Down at {at}, {length}"))
                })
                .collect();
            Day {
                anchor: format!("day-{}", cell.date),
                class: char::from(class),
                state: state.class(),
                tip,
                short,
                word: state.word(),
                notable: state.notable(),
                recent: index + 30 >= m.bar.len(),
                title: date.map_or_else(
                    || cell.date.clone(),
                    |day| day.format("%A %-d %B").to_string(),
                ),
                text,
                incidents: started,
            }
        })
        .collect()
}

/// Where it is checked from: this node, then each peer watching the same
/// target, with each one's typical answer time.
fn places(m: &MonitorView) -> (Vec<PlaceRow>, Markup, &'static str, String) {
    if m.vantages.is_empty() {
        return (
            Vec::new(),
            Markup::default(),
            DisplayState::None.class(),
            String::new(),
        );
    }
    let here = DisplayState::from(m.status);
    let seen: Vec<DisplayState> = std::iter::once(here)
        .chain(m.vantages.iter().map(|v| DisplayState::from(v.status)))
        .collect();
    let mut rows = vec![PlaceRow {
        name: "This Hora".to_owned(),
        sub: m.last_checked.as_deref().map_or_else(
            || "this node".to_owned(),
            |at| format!("this node · checked {}", at.get(11..16).unwrap_or(at)),
        ),
        state: here.class(),
        word: here.word(),
        latency: m.p50_ms.or(m.last_latency_ms).map_or_default(ms),
        meter: Markup::default(),
    }];
    for vantage in &m.vantages {
        let state = DisplayState::from(vantage.status);
        rows.push(PlaceRow {
            name: vantage.peer.clone(),
            sub: "peer".to_owned(),
            state: state.class(),
            word: state.word(),
            latency: vantage.p50_ms.map_or_default(ms),
            meter: Markup::default(),
        });
    }
    let slowest = m
        .p50_ms
        .into_iter()
        .chain(m.vantages.iter().filter_map(|v| v.p50_ms))
        .max()
        .unwrap_or(0);
    let latencies: Vec<Option<i64>> = std::iter::once(m.p50_ms.or(m.last_latency_ms))
        .chain(m.vantages.iter().map(|v| v.p50_ms))
        .collect();
    for (row, latency) in rows.iter_mut().zip(latencies) {
        if let Some(latency) = latency.filter(|_| slowest > 0) {
            row.meter = meter(latency, slowest, "lat");
        }
    }
    let up = seen
        .iter()
        .filter(|state| **state == DisplayState::Up)
        .count();
    let total = rows.len();
    let state = if up == total {
        DisplayState::Up
    } else if seen.iter().all(|state| *state == DisplayState::Down) {
        DisplayState::Down
    } else {
        DisplayState::Degraded
    };
    let pictured: Vec<(&str, &str)> = rows.iter().map(|r| (r.state, r.name.as_str())).collect();
    let names: Vec<&str> = rows.iter().map(|r| r.name.as_str()).collect();
    let label = format!(
        "{}: {up} of {total} places see {} up",
        join_and(&names),
        m.name
    );
    let picture = owls(&pictured, &label);
    (
        rows,
        picture,
        state.class(),
        format!("Up from {up} of {total}"),
    )
}

/// The operator's details: the probe, the figures behind the words, the
/// topology, the peers' raw view.
fn details(
    m: &MonitorView,
    config: &Config,
    monitor: &Monitor,
    show_target: bool,
) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    let mut probe = format!(
        "{} every {}",
        monitor.kind().as_str(),
        hora_core::fmt::period(monitor.interval_secs)
    );
    if show_target && !monitor.target().is_empty() {
        probe = format!(
            "{probe}, {}",
            hora_core::config::redact_url_secrets(monitor.target())
        );
    }
    rows.push(("Probe".to_owned(), probe));
    if let Some(at) = &m.last_checked {
        let mut last = at.clone();
        if let Some(error) = &m.last_error {
            last = format!("{last}, failed: {error}");
        }
        rows.push(("Last check".to_owned(), last));
    }
    if let (Some(p50), Some(p95), Some(p99)) = (m.p50_ms, m.p95_ms, m.p99_ms) {
        rows.push((
            "p50 / p95 / p99, 24 h".to_owned(),
            format!("{p50} / {p95} / {p99} ms"),
        ));
    }
    if let Some(target) = m.slo_latency_ms {
        rows.push((
            "Latency SLO".to_owned(),
            format!("p95 under {target} ms: {}", m.slo_state.word()),
        ));
    }
    if let Some(label) = &m.uptime_label {
        rows.push(("Uptime, 24 h".to_owned(), label.clone()));
    }
    if let Some(label) = &m.budget_label {
        rows.push((
            "Error budget".to_owned(),
            format!("{label} ({})", m.budget_title),
        ));
    }
    if let Some(label) = &m.cert_label {
        rows.push((
            "TLS certificate".to_owned(),
            format!("{label} ({})", m.cert_state.as_str()),
        ));
    }
    if let Some(depends) = &monitor.depends_on {
        let names: Vec<&str> = depends.iter().map(|id| config.monitor_name(id)).collect();
        if !names.is_empty() {
            rows.push(("Depends on".to_owned(), join_and(&names)));
        }
    }
    if let Some(cause) = &m.cause {
        rows.push(("Probable cause".to_owned(), cause.clone()));
    }
    if !m.impacted.is_empty() {
        rows.push(("Impacts".to_owned(), m.impacted.join(", ")));
    }
    if !m.vantages.is_empty() {
        let labels: Vec<&str> = m.vantages.iter().map(|v| v.label.as_str()).collect();
        rows.push(("Peers' view".to_owned(), labels.join(", ")));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_read_like_a_person_says_them() {
        assert_eq!(hora_core::fmt::period(30), "30 s");
        assert_eq!(hora_core::fmt::period(300), "5 min");
        assert_eq!(hora_core::fmt::period(90), "1 min 30 s");
        assert_eq!(hora_core::fmt::period(3600), "1 h");
        assert_eq!(hora_core::fmt::period(86_400), "1 d");
    }

    /// The last day reads in hours and the last hour in minutes: truncated to
    /// days, 23 hours left read "0 d left", next to "Expired".
    #[test]
    fn a_certificates_last_day_reads_in_hours() {
        assert_eq!(cert_left(40 * 86_400 + 7), (40, "d left"));
        assert_eq!(cert_left(86_400), (1, "d left"));
        assert_eq!(cert_left(23 * 3600 + 59), (23, "h left"));
        assert_eq!(cert_left(3600), (1, "h left"));
        assert_eq!(cert_left(12 * 60), (12, "min left"));
        assert_eq!(cert_left(-23 * 3600), (0, "d left"));
    }

    #[test]
    fn units_split_off_their_figure() {
        assert_eq!(split_unit("64 ms"), ("64".to_owned(), "ms".to_owned()));
        assert_eq!(split_unit("3 h 7 min").0, "3");
        assert_eq!(short_date("2026-09-18"), "18 Sep");
    }
}
