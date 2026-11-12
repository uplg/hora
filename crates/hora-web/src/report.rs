//! The monthly service report (`/report/2026-05`): a printable sheet per
//! month - the month at a glance, then a table per group with uptime, the
//! target and whether it was met, incidents, downtime, time to recover and
//! the allowed downtime left. Pre-formatted here; maud does the escaping.

use chrono::{Datelike, NaiveDate, Utc};
use hora_core::fmt;
use hora_core::planned::Window;
use hora_core::report::{DayTally, MonitorMonth, MonthReport};

use crate::layout::{Chrome, join_and, minutes};
use crate::page::{Document, icon, raw, state};
use maud::{Markup, html};

pub(crate) struct ReportTemplate {
    pub(crate) chrome: Chrome,
    /// Whose report this is (the page title, `· group` when scoped).
    pub(crate) title: String,
    /// `"May 2026"`.
    pub(crate) label: String,
    /// "1 to 31 May 2026, times in UTC. 14 services, 8 with a target."
    pub(crate) lede: String,
    pub(crate) generated: String,
    pub(crate) kpis: Kpis,
    pub(crate) groups: Vec<ReportGroup>,
    /// The months around this one (`2026-04`, label), when there is one.
    pub(crate) prev: Option<(String, String)>,
    pub(crate) next: Option<(String, String)>,
    /// The month links' own query: `?group=X` on a scoped report, else
    /// empty (the viewer's token and theme follow it, spliced per request).
    pub(crate) group_query: String,
}

/// The month at a glance.
impl ReportTemplate {
    /// The screen-only bar above the sheet: the neighbouring months, the way
    /// back to the status page, the theme, and how to keep a copy.
    fn toolbar(&self) -> Markup {
        let chrome = &self.chrome;
        let month_link = |month: &str, name: &str| {
            html! {
                a.btn.quiet href={
                    "/report/" (month)
                    @if self.group_query.is_empty() { (raw(chrome.q())) }
                    @else { (self.group_query) (raw(chrome.q_more())) }
                } { (name) }
            }
        };
        html! {
            div.toolbar {
                nav.nav aria-label="Months" {
                    @if let Some((month, name)) = &self.prev { (month_link(month, name)) }
                    span aria-current="page" { (self.label) }
                    @if let Some((month, name)) = &self.next { (month_link(month, name)) }
                }
                span.tools {
                    a.btn.quiet href={ "/" (raw(chrome.q())) } { "Status page" }
                    a.btn.theme-btn href=(raw(chrome.theme_href()))
                        aria-label={ "Theme: " (raw(chrome.theme_label())) ". Change theme" } {
                        (icon("sun"))
                        span.lbl { (raw(chrome.theme_label())) }
                    }
                }
                p.hint {
                    (icon("print"))
                    "To keep a copy, print this page or save it as a PDF from your browser."
                }
            }
        }
    }

    /// The sheet's letterhead: whose report, and by what.
    fn letterhead(&self) -> Markup {
        let chrome = &self.chrome;
        html! {
            header.rh {
                @match (&chrome.logo, &chrome.logo_dark) {
                    (Some(logo), Some(dark)) => {
                        img.op-logo.for-light src=(logo) alt="";
                        img.op-logo.for-dark src=(dark) alt="";
                    }
                    (Some(logo), None) => { img.op-logo src=(logo) alt=""; }
                    (None, _) => { span.op-logo aria-hidden="true" { (chrome.initial) } }
                }
                span.op { (self.title) }
                " "
                span.who {
                    img src={ "/assets/mark.svg?v=" (chrome.v) } alt="" width="20" height="20";
                    "Report by "
                    svg.wm viewBox="0 0 89 26" role="img" aria-label="Hora" { use href="#w-hora" {} }
                }
            }
        }
    }

    /// The month's headline figures.
    fn figures(&self) -> Markup {
        let kpis = &self.kpis;
        let targeted = !kpis.targets.is_empty();
        let lead = if !kpis.all_met {
            "kpi lead missed"
        } else if !kpis.untargeted_out.is_empty() && targeted {
            "kpi lead plain"
        } else {
            "kpi lead"
        };
        let kpi = |key: &str, value: &dyn maud::Render, note: &str| {
            html! {
                div.kpi { div.k { (key) } div.v { (value) } div.n.muted { (note) } }
            }
        };
        html! {
            div.kpis {
                div class=(lead) {
                    div.k { "Up overall" }
                    div.v { (kpis.uptime) }
                    div.n {
                        @if !targeted { span.muted { "all services" } }
                        @else if kpis.all_met && kpis.untargeted_out.is_empty() { (state("up", "Targets met")) }
                        @else if kpis.all_met { span.muted { (kpis.untargeted_out) } }
                        @else { (state("down", "A target missed")) }
                    }
                }
                div class=(if targeted { "kpi-rest" } else { "kpi-rest three" }) {
                    (kpi("Incidents", &kpis.incidents, &kpis.incidents_note))
                    (kpi("Downtime", &kpis.downtime, &kpis.downtime_note))
                    (kpi("To recover", &kpis.mttr, "median, outages only"))
                    @if targeted {
                        div.kpi {
                            div.k { "Targets met" }
                            div.v { (kpis.targets) }
                            div.n {
                                @if kpis.all_met { (state("up", "All met")) }
                                @else { (state("down", &kpis.missed)) }
                            }
                        }
                    }
                }
            }
        }
    }

    /// One group's table: a service a row.
    fn group(group: &ReportGroup, index: usize) -> Markup {
        let id = format!("rg{index}");
        let column = |name: &str| html! { th role="columnheader" scope="col" { (name) } };
        let explained = |title: &str, name: &str| {
            html! { th role="columnheader" scope="col" { abbr title=(title) { (name) } } }
        };
        html! {
            section.rg aria-labelledby=(id) {
                div.rg-h {
                    h2 id=(id) {
                        @if group.name.is_empty() { "Other services" } @else { (group.name) }
                    }
                    span {
                        (group.len()) " " (if group.len() == 1 { "service" } else { "services" })
                        " · " (group.uptime) " up"
                    }
                }
                table role="table" {
                    thead role="rowgroup" { tr role="row" {
                        (column("Service")) (column("Up")) (column("Target")) (column("Met"))
                        (column("Incidents")) (column("Downtime"))
                        (explained("Time to recover, the typical outage", "Recovery"))
                        (explained("Allowed downtime left at the target", "Budget"))
                    } }
                    tbody role="rowgroup" {
                        @for row in &group.rows {
                            tr role="row" class=[(row.met_state == "down").then_some("missed")] {
                                th role="rowheader" scope="row" {
                                    span.svc {
                                        span { (row.name) }
                                        span.bar.strip role="img" aria-label=(row.days_label) { (raw(&row.days)) }
                                    }
                                }
                                td role="cell" data-l="Up" { (row.uptime) }
                                td role="cell" data-l="Target" { (row.target) }
                                td.met role="cell" {
                                    @if row.met_state.is_empty() { span.muted { "no target" } }
                                    @else { (state(row.met_state, row.met_word)) }
                                }
                                td role="cell" data-l="Incidents" { (row.incidents) }
                                td role="cell" data-l="Downtime" { (row.downtime) }
                                td role="cell" data-l="Recovery" { (row.mttr) }
                                td role="cell" data-l="Budget" { (row.left) }
                            }
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn render(&self) -> String {
        let chrome = &self.chrome;
        let main = html! {
            main id="main" class="sheet" {
                (self.letterhead())
                div.rt {
                    p.eyebrow { "Service report" }
                    h1 { (self.label) }
                    p { (self.lede) }
                }
                @if self.groups.is_empty() {
                    p.quiet-line { "Nothing was watched this month." }
                } @else {
                    (self.figures())
                    @for (index, group) in self.groups.iter().enumerate() {
                        (Self::group(group, index + 1))
                    }
                }
                div.method {
                    p {
                        b { "How this is counted." }
                        " A service is up when it answers as expected; slow answers count as up. Downtime is the time of confirmed outages within the month. Recovery is the typical length of an outage, over the outages that ended this month. Budget is the downtime still allowed by the target."
                    }
                    p {
                        "Generated " (self.generated) " by Hora. "
                        a.noprint href={ "/" (raw(chrome.q())) } { "Live status" }
                    }
                }
            }
        };
        Document::new(
            chrome,
            format!("{} · {} service report", self.label, self.title),
            format!("{} service report for {}.", self.title, self.label),
            main,
        )
        .framed("report", self.toolbar(), html! {})
        .render()
    }
}

pub(crate) struct Kpis {
    pub(crate) uptime: String,
    pub(crate) incidents: usize,
    pub(crate) incidents_note: String,
    pub(crate) downtime: String,
    /// What the downtime figure adds up: it is a sum over services, so on
    /// a bad month it can exceed the month's own length.
    pub(crate) downtime_note: String,
    /// Set when every target was met and yet services *without* a target
    /// had outages: the lead figure is then not the targets' to vouch for.
    pub(crate) untargeted_out: String,
    pub(crate) mttr: String,
    /// "7 of 8"; empty without any target.
    pub(crate) targets: String,
    /// The services that missed their target, named.
    pub(crate) missed: String,
    pub(crate) all_met: bool,
}

pub(crate) struct ReportGroup {
    /// Display group name; empty for ungrouped monitors.
    name: String,
    /// Aggregate uptime across the group's monitors.
    uptime: String,
    rows: Vec<ReportRow>,
}

pub(crate) struct ReportRow {
    name: String,
    uptime: String,
    target: String,
    /// `up` (met), `down` (missed) or empty (no target or no data).
    met_state: &'static str,
    met_word: &'static str,
    incidents: String,
    downtime: String,
    mttr: String,
    left: String,
    /// The month's days as a strip (an inline SVG), and its words.
    days: String,
    days_label: String,
}

impl ReportGroup {
    /// How many services the group lists.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Group the report rows (already in configuration order) and format them.
pub(crate) fn group_rows(
    report: &MonthReport,
    visible: impl Fn(&MonitorMonth) -> bool,
) -> Vec<ReportGroup> {
    let mut groups: Vec<(String, Vec<&MonitorMonth>)> = Vec::new();
    for row in report.rows.iter().filter(|row| visible(row)) {
        let key = row.group.clone().unwrap_or_default();
        match groups.iter_mut().find(|(name, _)| *name == key) {
            Some((_, rows)) => rows.push(row),
            None => groups.push((key, vec![row])),
        }
    }
    // Ungrouped monitors render last, like on the status page.
    groups.sort_by_key(|(name, _)| name.is_empty());

    groups
        .into_iter()
        .map(|(name, rows)| ReportGroup {
            name,
            uptime: uptime_of(&rows),
            rows: rows
                .into_iter()
                .map(|row| format_row(row, &report.month, &report.maintenance))
                .collect(),
        })
        .collect()
}

/// The aggregate uptime of some rows: degraded checks count as available.
fn uptime_of(rows: &[&MonitorMonth]) -> String {
    let (available, total) = rows.iter().fold((0_i64, 0_i64), |(a, t), row| {
        (
            a + row.up + row.degraded,
            t + row.up + row.down + row.degraded,
        )
    });
    fmt::basis_points(available, total).map_or_else(|| "no data".to_owned(), fmt::pct_bp)
}

/// The month at a glance, over the rows this viewer may see.
pub(crate) fn kpis(report: &MonthReport, visible: impl Fn(&MonitorMonth) -> bool) -> Kpis {
    let rows: Vec<&MonitorMonth> = report.rows.iter().filter(|row| visible(row)).collect();
    let incidents: usize = rows.iter().map(|row| row.incidents).sum();
    let downtime: i64 = rows.iter().map(|row| row.downtime_secs).sum();
    let mut mttrs: Vec<i64> = rows.iter().filter_map(|row| row.mttr_secs).collect();
    mttrs.sort_unstable();
    let targeted: Vec<&&MonitorMonth> = rows.iter().filter(|row| row.slo_met.is_some()).collect();
    let met = targeted
        .iter()
        .filter(|row| row.slo_met == Some(true))
        .count();
    let missed: Vec<&str> = targeted
        .iter()
        .filter(|row| row.slo_met == Some(false))
        .map(|row| row.name.as_str())
        .collect();
    Kpis {
        uptime: uptime_of(&rows),
        incidents,
        incidents_note: if incidents == 0 {
            "A quiet month".to_owned()
        } else {
            format!(
                "across {} service{}",
                rows.iter().filter(|row| row.incidents > 0).count(),
                if rows.iter().filter(|row| row.incidents > 0).count() == 1 {
                    ""
                } else {
                    "s"
                }
            )
        },
        downtime: minutes((downtime + 59) / 60),
        downtime_note: match rows.iter().filter(|row| row.downtime_secs > 0).count() {
            0 => "no confirmed outage".to_owned(),
            1 => "confirmed outages, one service".to_owned(),
            out => format!("added up over {out} services"),
        },
        untargeted_out: match rows
            .iter()
            .filter(|row| row.slo_met.is_none() && row.downtime_secs > 0)
            .count()
        {
            0 => String::new(),
            1 => "Targets met; 1 service without a target had outages".to_owned(),
            out => format!("Targets met; {out} services without a target had outages"),
        },
        mttr: mttrs
            .get(mttrs.len() / 2)
            .map_or_else(|| "n/a".to_owned(), |secs| minutes((secs + 59) / 60)),
        targets: if targeted.is_empty() {
            String::new()
        } else {
            format!("{met} of {}", targeted.len())
        },
        all_met: missed.is_empty(),
        missed: if missed.is_empty() {
            String::new()
        } else {
            format!("{} missed", join_and(&missed))
        },
    }
}

/// A month link: its path segment (`2026-04`) and its name (`April`).
pub(crate) type MonthLink = Option<(String, String)>;

/// The month before and after `month` (`YYYY-MM`); the next one only once it
/// has started.
pub(crate) fn neighbours(month: &str) -> (MonthLink, MonthLink) {
    let Ok(first) = NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d") else {
        return (None, None);
    };
    let prev = first.pred_opt().map(|day| {
        (
            day.format("%Y-%m").to_string(),
            day.format("%B").to_string(),
        )
    });
    let next = first
        .checked_add_months(chrono::Months::new(1))
        .filter(|day| *day <= Utc::now().date_naive())
        .map(|day| {
            (
                day.format("%Y-%m").to_string(),
                day.format("%B").to_string(),
            )
        });
    (prev, next)
}

/// "1 to 31 May 2026".
pub(crate) fn span(month: &str) -> String {
    let Ok(first) = NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d") else {
        return month.to_owned();
    };
    let last = first
        .checked_add_months(chrono::Months::new(1))
        .and_then(|next| next.pred_opt())
        .map_or(28, |day| day.day());
    format!("1 to {last} {}", first.format("%B %Y"))
}

/// A report day's class in the strip, by the status bar's rule
/// (`summary::bar_class`): slow, or briefly down, stays amber; red once the
/// day was mostly an outage. A planned window turns a day that was not an
/// outage into maintenance.
fn day_class(day: DayTally, maintenance: bool) -> u8 {
    let state = crate::summary::day_state(day.up, day.down, day.degraded);
    crate::summary::bar_class(
        state.map_or("empty", |(state, _)| state),
        maintenance,
        false,
    )
}

/// How many days `month` (`2026-05`) has; 31 when it does not parse.
fn month_length(month: &str) -> usize {
    NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d")
        .ok()
        .and_then(|first| first.checked_add_months(chrono::Months::new(1)))
        .and_then(|next| next.pred_opt())
        .map_or(31, |last| last.day() as usize)
}

/// The days of `month` (`2026-05`) a planned window covering `id` touched,
/// as 1-based day numbers. The windows are the config's and the ones planned
/// at runtime alike (see `hora_core::planned`).
fn maintenance_days(month: &str, id: &str, windows: &[Window]) -> Vec<u32> {
    let Some(first) = NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d").ok() else {
        return Vec::new();
    };
    let mut days = Vec::new();
    for window in windows.iter().filter(|window| window.covers(id)) {
        let mut day = window.start.date_naive();
        while day <= window.end.date_naive() {
            if day.year() == first.year() && day.month() == first.month() {
                days.push(day.day());
            }
            let Some(next) = day.succ_opt() else { break };
            day = next;
        }
    }
    days
}

fn format_row(row: &MonitorMonth, month: &str, maintenance: &[Window]) -> ReportRow {
    let planned = maintenance_days(month, &row.id, maintenance);
    let mut classes = Vec::with_capacity(row.days.len());
    let mut outages = 0;
    for (day, number) in row.days.iter().zip(1_u32..) {
        let class = day_class(*day, planned.contains(&number));
        outages += usize::from(class == b'x');
        classes.push(class);
    }
    let days_label = match outages {
        0 => "No day with an outage".to_owned(),
        1 => "1 day with an outage".to_owned(),
        n => format!("{n} days with an outage"),
    };
    let dash = || "\u{2014}".to_owned();
    let (met_state, met_word) = match row.slo_met {
        Some(true) => ("up", "Met"),
        Some(false) => ("down", "Missed"),
        None => ("", ""),
    };
    ReportRow {
        name: row.name.clone(),
        uptime: row
            .uptime_bp
            .map_or_else(|| "no data".to_owned(), fmt::pct_bp),
        target: row
            .slo_bp
            .map_or_else(dash, |slo| fmt::pct_bp(i64::from(slo))),
        met_state,
        met_word,
        incidents: row.incidents.to_string(),
        downtime: minutes((row.downtime_secs + 59) / 60),
        mttr: row
            .mttr_secs
            .map_or_else(|| "n/a".to_owned(), |secs| minutes((secs + 59) / 60)),
        left: match (row.budget_consumed_minutes, row.budget_minutes) {
            (Some(consumed), Some(budget)) => minutes((budget - consumed).max(0)),
            _ => dash(),
        },
        days: crate::render::day_strip(&classes, month_length(month)),
        days_label,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn months_around_and_their_span() {
        let (prev, _) = neighbours("2026-03");
        assert_eq!(prev, Some(("2026-02".to_owned(), "February".to_owned())));
        // A month in the future has no "next".
        assert_eq!(neighbours("2999-01").1, None);
        assert_eq!(span("2026-02"), "1 to 28 February 2026");
        assert_eq!(span("2024-02"), "1 to 29 February 2024");
        assert_eq!(fmt::pct_bp(9997), "99.97 %");
    }

    fn month_row(name: &str, slo_met: Option<bool>, downtime_secs: i64) -> MonitorMonth {
        MonitorMonth {
            id: name.to_lowercase(),
            name: name.to_owned(),
            group: None,
            public: true,
            up: 100,
            down: 0,
            degraded: 0,
            uptime_bp: Some(10_000),
            incidents: usize::from(downtime_secs > 0),
            downtime_secs,
            mttr_secs: None,
            slo_bp: slo_met.map(|_| 9_990),
            slo_met,
            budget_minutes: None,
            budget_consumed_minutes: None,
            days: Vec::new(),
        }
    }

    /// The downtime figure is a sum over services, and says so; and "targets
    /// met" does not vouch for the month when a service without a target
    /// was out.
    #[test]
    fn headline_notes_say_what_the_figures_cover() {
        let report = |rows| MonthReport {
            month: "2026-06".to_owned(),
            label: "June 2026".to_owned(),
            rows,
            maintenance: Vec::new(),
        };
        let quiet = kpis(&report(vec![month_row("Web", Some(true), 0)]), |_| true);
        assert_eq!(quiet.downtime, "0 min");
        assert_eq!(quiet.downtime_note, "no confirmed outage");
        assert!(quiet.all_met && quiet.untargeted_out.is_empty());

        let hosts = kpis(
            &report(vec![
                month_row("Web", Some(true), 60),
                month_row("Host A", None, 21 * 86_400),
                month_row("Host B", None, 21 * 86_400),
            ]),
            |_| true,
        );
        // Longer than the month itself: three services' outages, added up.
        assert_eq!(hosts.downtime, "42 d");
        assert_eq!(hosts.downtime_note, "added up over 3 services");
        assert!(hosts.all_met);
        assert_eq!(
            hosts.untargeted_out,
            "Targets met; 2 services without a target had outages"
        );
    }

    #[test]
    fn strip_days_follow_the_bar_rule() {
        let tally = |up, down, degraded| DayTally { up, down, degraded };
        assert_eq!(day_class(tally(0, 0, 0), false), b'n');
        assert_eq!(day_class(tally(100, 0, 0), false), b'u');
        assert_eq!(day_class(tally(95, 5, 0), false), b'd');
        assert_eq!(day_class(tally(50, 50, 0), false), b'x');
        // A planned window marks the day, unless it was still an outage.
        assert_eq!(day_class(tally(95, 5, 0), true), b'm');
        assert_eq!(day_class(tally(50, 50, 0), true), b'x');
    }

    #[test]
    fn maintenance_days_are_clipped_to_the_month() {
        // One window as the API stores it, read back through the schedule
        // every reader shares: the report marks its days like a config one's.
        let config = hora_core::config::parse(
            "[page]\n[server]\n[[monitors]]\nid = \"api\"\nname = \"API\"\n\
             target = \"https://example.com\"\ninterval_secs = 60\n",
        )
        .expect("config");
        let at = |text: &str| {
            text.parse::<chrono::DateTime<chrono::Utc>>()
                .expect("time")
                .timestamp()
        };
        let stored = hora_core::db::MaintenanceWindow {
            id: 1,
            starts_at: at("2026-04-29T22:00:00Z"),
            ends_at: at("2026-05-02T01:00:00Z"),
            reason: "storage move".to_owned(),
            selector: "ids:api".to_owned(),
            origin: "api".to_owned(),
            remote_id: None,
            created_at: 0,
        };
        let schedule = hora_core::planned::Schedule::merged(&config, &[stored]);
        let windows = schedule.windows();
        assert_eq!(maintenance_days("2026-05", "api", windows), [1, 2]);
        assert_eq!(maintenance_days("2026-04", "api", windows), [29, 30]);
        assert_eq!(
            maintenance_days("2026-05", "web", windows),
            Vec::<u32>::new()
        );
        assert_eq!(month_length("2026-02"), 28);
        assert_eq!(month_length("2024-02"), 29);
    }
}
