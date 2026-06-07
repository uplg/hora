//! The status summary: turning stored checks into the page/API view model.

use std::collections::HashMap;

use askama::Template;
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;
use sqlx::SqlitePool;
use utoipa::ToSchema;

use hora_core::SECONDS_PER_DAY;
use hora_core::config::{Config, Monitor};
use hora_core::db::{self, DayRow, Latest, Point};

use crate::SPARK_BUCKETS;
use crate::render::sparkline;

// --- View model ----------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub(crate) struct Summary {
    title: String,
    overall: &'static str,
    overall_label: &'static str,
    generated_at: String,
    /// Human-readable UTC timestamp for the footer (the API uses `generated_at`).
    #[serde(skip)]
    updated_utc: String,
    incidents: Vec<IncidentView>,
    maintenances: Vec<MaintenanceView>,
    pub(crate) monitors: Vec<MonitorView>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct IncidentView {
    title: String,
    body: String,
    severity: &'static str,
    at: Option<String>,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MaintenanceView {
    reason: String,
    monitors: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct MonitorView {
    pub(crate) id: String,
    name: String,
    pub(crate) status: &'static str,
    last_latency_ms: Option<i64>,
    last_checked: Option<String>,
    #[serde(rename = "uptime_24h_permille")]
    pub(crate) uptime_permille: Option<i64>,
    #[serde(skip)]
    uptime_label: Option<String>,
    #[serde(rename = "latency_p50_ms")]
    p50_ms: Option<i64>,
    #[serde(rename = "latency_p95_ms")]
    p95_ms: Option<i64>,
    #[serde(rename = "latency_p99_ms")]
    p99_ms: Option<i64>,
    #[serde(rename = "slo_latency_ms")]
    slo_latency_ms: Option<i64>,
    #[serde(skip)]
    slo_state: &'static str,
    /// Active maintenance window title (alerts muted); `None` outside a window.
    maintenance: Option<String>,
    #[serde(rename = "cert_expiry_days")]
    cert_days: Option<i64>,
    #[serde(skip)]
    cert_label: Option<String>,
    #[serde(skip)]
    cert_state: &'static str,
    #[serde(rename = "history")]
    bar: Vec<DayCell>,
    #[serde(skip)]
    chart_svg: String,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct DayCell {
    date: String,
    state: &'static str,
    #[serde(skip)]
    title: String,
}

#[derive(Template)]
#[template(path = "status.html")]
pub(crate) struct StatusTemplate<'a> {
    pub(crate) summary: &'a Summary,
}

/// Shared, read-only inputs for building each monitor's view.
pub(crate) struct SummaryCtx {
    now: DateTime<Utc>,
    timestamp: i64,
    since_24h: i64,
    since_history: i64,
    threshold: i64,
    cert_threshold: i64,
    history_days: u16,
}

pub(crate) async fn build_summary(pool: &SqlitePool, config: &Config) -> Summary {
    let now = Utc::now();
    let timestamp = now.timestamp();
    let ctx = SummaryCtx {
        now,
        timestamp,
        since_24h: timestamp - SECONDS_PER_DAY,
        since_history: timestamp - i64::from(config.page.history_days) * SECONDS_PER_DAY,
        threshold: i64::from(config.alerts.fail_threshold.max(1)),
        cert_threshold: i64::from(config.alerts.cert_expiry_days),
        history_days: config.page.history_days,
    };

    // The 24h/90d aggregates batch into one query each. A failed query degrades
    // to empty data ("no data" cards) rather than blacking out the page.
    let availability = or_empty(
        db::availability_all(pool, ctx.since_24h).await,
        "availability",
    );
    let daily = or_empty(db::daily_all(pool, ctx.since_history).await, "daily");
    // Latency is summarised in SQL: exact percentiles, plus a bucket-averaged
    // series for the sparkline. The raw 24h samples never enter memory or the
    // page, so both stay bounded by the monitor count, not the check frequency.
    let percentiles = percentile_map(or_empty(
        db::latency_percentiles_all(pool, ctx.since_24h).await,
        "latency percentiles",
    ));
    let bucket_secs = (SECONDS_PER_DAY / SPARK_BUCKETS).max(1);
    let sparklines = or_empty(
        db::latency_sparkline_all(pool, ctx.since_24h, bucket_secs).await,
        "latency sparklines",
    );
    let certs = or_empty(db::cert_all(pool).await, "certificates");
    let recent = recent_checks_map(pool, &config.monitors, ctx.threshold.max(1)).await;

    let data = MonitorData {
        recent: &recent,
        availability: &availability,
        daily: &daily,
        percentiles: &percentiles,
        sparklines: &sparklines,
        certs: &certs,
    };

    let monitors: Vec<MonitorView> = config
        .monitors
        .iter()
        .map(|monitor| {
            let mut view = build_monitor_view(monitor, &ctx, &data);
            view.maintenance = config
                .active_maintenance(&monitor.id, now)
                .map(|window| window.title.clone());
            view
        })
        .collect();

    let overall = monitors
        .iter()
        .fold("up", |worst, m| worse(worst, m.status));

    let incidents = config
        .incidents
        .iter()
        .map(|incident| IncidentView {
            title: incident.title.clone(),
            body: incident.body.clone(),
            severity: incident.severity.as_str(),
            at: incident
                .at
                .map(|at| at.format("%Y-%m-%d %H:%M UTC").to_string()),
        })
        .collect();

    // Active maintenance windows shown as a top banner (so a long reason never
    // changes a card's height and disturbs the grid).
    let maintenances = config
        .maintenance
        .iter()
        .filter(|window| now >= window.start && now <= window.end)
        .map(|window| {
            let monitors = if window.monitors.is_empty() {
                "all monitors".to_owned()
            } else {
                window
                    .monitors
                    .iter()
                    .map(|id| {
                        config
                            .monitors
                            .iter()
                            .find(|monitor| &monitor.id == id)
                            .map_or(id.as_str(), |monitor| monitor.name.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            MaintenanceView {
                reason: window.title.clone(),
                monitors,
            }
        })
        .collect();

    Summary {
        title: config.page.title.clone(),
        overall,
        overall_label: overall_label(overall),
        generated_at: now.to_rfc3339(),
        updated_utc: now.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        incidents,
        maintenances,
        monitors,
    }
}

/// Unwrap a batch query, logging and using empty data on error so one failed
/// query degrades to "no data" cards instead of failing the whole page.
pub(crate) fn or_empty<T: Default>(result: sqlx::Result<T>, what: &str) -> T {
    result.unwrap_or_else(|err| {
        tracing::error!("summary: {what} query failed: {err:#}");
        T::default()
    })
}

/// Convert the raw `(p50, p95, p99)` tuples from SQL into [`Percentiles`].
pub(crate) fn percentile_map(
    raw: HashMap<String, (i64, i64, i64)>,
) -> HashMap<String, Percentiles> {
    raw.into_iter()
        .map(|(id, (p50, p95, p99))| (id, Percentiles { p50, p95, p99 }))
        .collect()
}

/// Fetch each monitor's recent checks. Deliberately per-monitor: the query is an
/// indexed `ORDER BY time DESC LIMIT N` (tiny), and unlike a single windowed query
/// it is correct for any interval - a monitor checked less than once a day (e.g. a
/// weekly push heartbeat) would be dropped by a 24h batch window and shown as
/// "unknown". On embedded `SQLite` these N statements cost microseconds each, far
/// less than scanning the whole history table to rank rows.
pub(crate) async fn recent_checks_map(
    pool: &SqlitePool,
    monitors: &[Monitor],
    limit: i64,
) -> HashMap<String, Vec<Latest>> {
    let mut recent: HashMap<String, Vec<Latest>> = HashMap::new();
    for monitor in monitors {
        let checks = or_empty(
            db::recent_checks(pool, &monitor.id, limit).await,
            "recent checks",
        );
        recent.insert(monitor.id.clone(), checks);
    }
    recent
}

/// The pre-fetched batch maps a monitor's view is assembled from, keyed by
/// monitor id. Grouped so [`build_monitor_view`] stays a small, pure function.
pub(crate) struct MonitorData<'a> {
    recent: &'a HashMap<String, Vec<Latest>>,
    availability: &'a HashMap<String, (i64, i64)>,
    daily: &'a HashMap<String, Vec<DayRow>>,
    percentiles: &'a HashMap<String, Percentiles>,
    sparklines: &'a HashMap<String, Vec<Point>>,
    certs: &'a HashMap<String, i64>,
}

/// Build a monitor's view from the pre-fetched batch maps. Pure: a monitor with
/// no data simply renders an empty ("no data yet") card.
pub(crate) fn build_monitor_view(
    monitor: &Monitor,
    ctx: &SummaryCtx,
    data: &MonitorData,
) -> MonitorView {
    let recent = data
        .recent
        .get(&monitor.id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let status = derive_status(recent, ctx.threshold);

    let uptime_permille = data
        .availability
        .get(&monitor.id)
        .and_then(|&(available, total)| {
            (total > 0).then(|| available.saturating_mul(1000) / total)
        });

    let daily = data
        .daily
        .get(&monitor.id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let bar = build_bar(daily, ctx.now, ctx.history_days);

    let spark_points = data
        .sparklines
        .get(&monitor.id)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let chart_svg = sparkline(spark_points, status);
    let pct = data.percentiles.get(&monitor.id).copied();
    let slo_state = slo_state(monitor.slo_latency_ms, pct.map(|p| p.p95));

    let cert_days = data
        .certs
        .get(&monitor.id)
        .map(|&not_after| (not_after - ctx.timestamp) / SECONDS_PER_DAY);

    let latest = recent.first();
    MonitorView {
        id: monitor.id.clone(),
        name: monitor.name.clone(),
        status,
        last_latency_ms: latest.and_then(|l| l.latency_ms),
        last_checked: latest.and_then(|l| iso(l.time)),
        uptime_permille,
        uptime_label: uptime_permille.map(format_permille),
        p50_ms: pct.map(|p| p.p50),
        p95_ms: pct.map(|p| p.p95),
        p99_ms: pct.map(|p| p.p99),
        slo_latency_ms: monitor.slo_latency_ms,
        slo_state,
        // Overridden in `build_summary`, which has the live config and `now`.
        maintenance: None,
        cert_days,
        cert_label: cert_days.map(cert_label),
        cert_state: cert_state_for(cert_days, ctx.cert_threshold),
        bar,
        chart_svg,
    }
}

/// 24h latency percentiles for a monitor, computed in SQL by
/// [`db::latency_percentiles_all`] (nearest-rank).
#[derive(Clone, Copy)]
pub(crate) struct Percentiles {
    p50: i64,
    p95: i64,
    p99: i64,
}

/// Whether the measured p95 meets the configured latency objective.
pub(crate) fn slo_state(target: Option<i64>, p95: Option<i64>) -> &'static str {
    match (target, p95) {
        (Some(target), Some(p95)) if p95 <= target => "met",
        (Some(_), Some(_)) => "breached",
        _ => "none",
    }
}

/// Current status from the recent checks (newest first): a single failure only
/// counts as `degraded` until `threshold` consecutive failures confirm `down`.
pub(crate) fn derive_status(recent: &[Latest], threshold: i64) -> &'static str {
    let Some(latest) = recent.first() else {
        return "unknown";
    };
    match latest.status {
        1 => "up",
        2 => "degraded",
        _ => {
            let needed = usize::try_from(threshold).unwrap_or(usize::MAX);
            if recent.len() >= needed && recent.iter().all(|check| check.status == 0) {
                "down"
            } else {
                "degraded"
            }
        }
    }
}

pub(crate) fn worse(current: &'static str, candidate: &'static str) -> &'static str {
    if rank(candidate) > rank(current) {
        candidate
    } else {
        current
    }
}

pub(crate) fn rank(status: &str) -> u8 {
    match status {
        "up" => 0,
        "degraded" => 2,
        "down" => 3,
        _ => 1,
    }
}

pub(crate) fn overall_label(status: &str) -> &'static str {
    match status {
        "up" => "All systems operational",
        "degraded" => "Degraded performance",
        "down" => "Major outage",
        _ => "Awaiting data",
    }
}

pub(crate) fn cert_state_for(days: Option<i64>, threshold: i64) -> &'static str {
    match days {
        None => "none",
        Some(remaining) if remaining <= 0 => "expired",
        Some(remaining) if remaining <= threshold => "warn",
        Some(_) => "ok",
    }
}

pub(crate) fn cert_label(days: i64) -> String {
    if days <= 0 {
        "expired".to_owned()
    } else {
        format!("{days}d")
    }
}

/// Format permille (0..=1000) as a percentage with one decimal, e.g. `99.9%`.
pub(crate) fn format_permille(permille: i64) -> String {
    let permille = permille.clamp(0, 1000);
    format!("{}.{}%", permille / 10, permille % 10)
}

pub(crate) fn iso(timestamp: i64) -> Option<String> {
    DateTime::from_timestamp(timestamp, 0).map(|dt| dt.to_rfc3339())
}

/// Build the daily uptime bar (oldest to newest), zero-filling missing days.
pub(crate) fn build_bar(daily: &[DayRow], now: DateTime<Utc>, days: u16) -> Vec<DayCell> {
    let by_day: HashMap<&str, &DayRow> = daily.iter().map(|row| (row.day.as_str(), row)).collect();
    let mut cells = Vec::with_capacity(usize::from(days));

    for offset in (0..days).rev() {
        let date = (now - TimeDelta::days(i64::from(offset)))
            .format("%Y-%m-%d")
            .to_string();
        let cell = by_day.get(date.as_str()).map_or_else(
            || DayCell {
                title: format!("{date}: no data"),
                date: date.clone(),
                state: "empty",
            },
            |row| day_cell(date.clone(), row),
        );
        cells.push(cell);
    }
    cells
}

/// A day stays amber as long as it was mostly up; it only turns red once the day
/// was a real outage (majority down).
pub(crate) const DAY_OUTAGE_BELOW_PERMILLE: i64 = 900; // < 90% availability over the day

pub(crate) fn day_cell(date: String, row: &DayRow) -> DayCell {
    let total = row.up + row.down + row.degraded;
    if total == 0 {
        let title = format!("{date}: no data");
        return DayCell {
            date,
            state: "empty",
            title,
        };
    }
    let permille = (row.up + row.degraded).saturating_mul(1000) / total;
    let state = if row.down == 0 && row.degraded == 0 {
        "up"
    } else if permille >= DAY_OUTAGE_BELOW_PERMILLE {
        "degraded"
    } else {
        "down"
    };
    let title = format!("{date}: {}", format_permille(permille));
    DayCell { date, state, title }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn check(status: i64) -> Latest {
        Latest {
            time: 0,
            latency_ms: None,
            status,
        }
    }
    #[test]
    fn permille_formats_one_decimal() {
        assert_eq!(format_permille(1000), "100.0%");
        assert_eq!(format_permille(999), "99.9%");
        assert_eq!(format_permille(0), "0.0%");
    }

    #[test]
    fn worse_picks_higher_severity() {
        assert_eq!(worse("up", "degraded"), "degraded");
        assert_eq!(worse("down", "degraded"), "down");
        assert_eq!(worse("up", "unknown"), "unknown");
        assert_eq!(worse("degraded", "up"), "degraded");
    }

    #[test]
    fn derive_status_confirms_down_only_after_threshold() {
        assert_eq!(derive_status(&[check(0), check(0), check(0)], 3), "down");
        assert_eq!(derive_status(&[check(0)], 3), "degraded");
        assert_eq!(derive_status(&[], 3), "unknown");
        assert_eq!(derive_status(&[check(1)], 3), "up");
    }

    #[test]
    fn cert_state_thresholds() {
        assert_eq!(cert_state_for(None, 14), "none");
        assert_eq!(cert_state_for(Some(-1), 14), "expired");
        assert_eq!(cert_state_for(Some(10), 14), "warn");
        assert_eq!(cert_state_for(Some(40), 14), "ok");
    }

    #[test]
    fn build_bar_zero_fills_to_requested_days() {
        let now = DateTime::from_timestamp(1_609_459_200, 0).unwrap();
        let rows = vec![DayRow {
            day: "2021-01-01".to_owned(),
            up: 10,
            down: 0,
            degraded: 0,
        }];
        let bar = build_bar(&rows, now, 7);
        assert_eq!(bar.len(), 7);
        assert_eq!(bar.last().unwrap().state, "up");
        assert_eq!(bar[0].state, "empty");
    }

    #[test]
    fn day_cell_reds_only_real_outages() {
        let row = |up, down| DayRow {
            day: "2021-01-01".to_owned(),
            up,
            down,
            degraded: 0,
        };
        assert_eq!(day_cell("d".to_owned(), &row(100, 0)).state, "up"); // 100%
        assert_eq!(day_cell("d".to_owned(), &row(1439, 1)).state, "degraded"); // ~99.9% blip
        assert_eq!(day_cell("d".to_owned(), &row(1400, 40)).state, "degraded"); // ~97% - mostly up
        assert_eq!(day_cell("d".to_owned(), &row(800, 640)).state, "down"); // ~56% - real outage
    }
    #[test]
    fn slo_state_compares_p95() {
        assert_eq!(slo_state(Some(200), Some(150)), "met");
        assert_eq!(slo_state(Some(200), Some(250)), "breached");
        assert_eq!(slo_state(None, Some(150)), "none");
        assert_eq!(slo_state(Some(200), None), "none");
    }
}
