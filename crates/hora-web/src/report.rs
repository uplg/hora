//! The monthly SLA report page (`/report/2026-05`): a printable, light-theme
//! table per group - uptime, incidents, downtime, MTTR, SLO verdict and
//! error-budget consumption. Pre-formatted here; Askama does the escaping.

use askama::Template;
use hora_core::report::{MonitorMonth, MonthReport, format_bp, format_secs};

#[derive(Template)]
#[template(path = "report.html")]
pub(crate) struct ReportTemplate {
    /// The status page title ("whose report is this").
    pub(crate) title: String,
    /// `"May 2026"`.
    pub(crate) label: String,
    pub(crate) generated: String,
    pub(crate) groups: Vec<ReportGroup>,
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
    incidents: String,
    downtime: String,
    mttr: String,
    slo: String,
    /// `met` / `missed` / `none`, for the verdict colour.
    slo_state: &'static str,
    budget: String,
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
        .map(|(name, rows)| {
            let (available, total) = rows.iter().fold((0_i64, 0_i64), |(a, t), row| {
                (
                    a + row.up + row.degraded,
                    t + row.up + row.down + row.degraded,
                )
            });
            let uptime = if total > 0 {
                format_bp((available * 10_000 + total / 2) / total)
            } else {
                "no data".to_owned()
            };
            ReportGroup {
                name,
                uptime,
                rows: rows.into_iter().map(format_row).collect(),
            }
        })
        .collect()
}

fn format_row(row: &MonitorMonth) -> ReportRow {
    let dash = || "\u{2014}".to_owned();
    let (slo, slo_state) = match (row.slo_bp, row.slo_met) {
        (Some(slo_bp), Some(met)) => (
            format!(
                "{} \u{b7} {}",
                format_bp(i64::from(slo_bp)),
                if met { "met" } else { "missed" }
            ),
            if met { "met" } else { "missed" },
        ),
        (Some(slo_bp), None) => (format_bp(i64::from(slo_bp)), "none"),
        _ => (dash(), "none"),
    };
    let budget = match (row.budget_consumed_minutes, row.budget_minutes) {
        (Some(consumed), Some(budget)) => format!("{consumed}m of {budget}m"),
        _ => dash(),
    };
    ReportRow {
        name: row.name.clone(),
        uptime: row
            .uptime_bp
            .map_or_else(|| "no data".to_owned(), format_bp),
        incidents: if row.incidents > 0 {
            row.incidents.to_string()
        } else {
            dash()
        },
        downtime: if row.downtime_secs > 0 {
            format_secs(row.downtime_secs)
        } else {
            dash()
        },
        mttr: row.mttr_secs.map_or_else(dash, format_secs),
        slo,
        slo_state,
        budget,
    }
}
