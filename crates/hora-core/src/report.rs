//! Monthly SLA reports: uptime per monitor and group, incidents, MTTR and
//! error-budget consumption over a calendar month (UTC). Everything is
//! computed from data that already exists - daily check counts (raw and
//! downsampled) and the incident log - so a report works as far back as the
//! one-year aggregate retention.
//!
//! Served as a printable page (`/report/2026-05`) and as text
//! (`hora report 2026-05`) - the "here is your May report, 99.95%" feature
//! for operators hosting other people's services.

use chrono::Datelike as _;
use sqlx::SqlitePool;

use crate::SECONDS_PER_DAY;
use crate::config::{Config, Monitor};
use crate::{db, slo};

/// One monitor's month.
#[derive(Debug)]
pub struct MonitorMonth {
    pub id: String,
    pub name: String,
    pub group: Option<String>,
    /// Whether the monitor is publicly visible (the web report filters on it).
    pub public: bool,
    /// Check counts over the month: up / down / degraded.
    pub up: i64,
    pub down: i64,
    pub degraded: i64,
    /// Availability in basis points (9 997 = 99.97%); `None` without checks.
    pub uptime_bp: Option<i64>,
    /// Incidents that overlapped the month.
    pub incidents: usize,
    /// Confirmed-down time within the month, seconds (incident spans clipped
    /// to the month's bounds; an open incident counts up to `now`).
    pub downtime_secs: i64,
    /// Mean time to repair: average duration of the incidents that *ended*
    /// inside the month. `None` when none did.
    pub mttr_secs: Option<i64>,
    /// The availability SLO in basis points, when configured.
    pub slo_bp: Option<u32>,
    /// Whether the month met the SLO target (only when both are known).
    pub slo_met: Option<bool>,
    /// The month's error budget at the SLO, minutes.
    pub budget_minutes: Option<i64>,
    /// Minutes of that budget consumed (conservative: the covered part of the
    /// month is assumed fully monitored).
    pub budget_consumed_minutes: Option<i64>,
}

/// A calendar month's report, monitors in configuration order.
#[derive(Debug)]
pub struct MonthReport {
    /// `"2026-05"`.
    pub month: String,
    /// `"May 2026"`.
    pub label: String,
    pub rows: Vec<MonitorMonth>,
}

/// Parse `"YYYY-MM"` into `(month_start, month_end)` unix seconds (UTC).
/// Rejects anything else - including a start in the future.
#[must_use]
pub fn month_bounds(month: &str) -> Option<(i64, i64)> {
    let (year, month_no) = month.split_once('-')?;
    if year.len() != 4 || month_no.len() != 2 {
        return None;
    }
    let year: i32 = year.parse().ok()?;
    let month_no: u32 = month_no.parse().ok()?;
    let start = chrono::NaiveDate::from_ymd_opt(year, month_no, 1)?;
    let end = start.checked_add_months(chrono::Months::new(1))?;
    let start = start.and_hms_opt(0, 0, 0)?.and_utc().timestamp();
    (start <= chrono::Utc::now().timestamp())
        .then_some((start, end.and_hms_opt(0, 0, 0)?.and_utc().timestamp()))
}

/// The previous calendar month as `"YYYY-MM"` - `hora report`'s default:
/// "here is your report for last month".
#[must_use]
pub fn previous_month(now: i64) -> String {
    let today = chrono::DateTime::from_timestamp(now, 0)
        .map(|dt| dt.date_naive())
        .unwrap_or_default();
    let first = today.with_day(1).unwrap_or(today);
    let previous = first
        .checked_sub_months(chrono::Months::new(1))
        .unwrap_or(first);
    previous.format("%Y-%m").to_string()
}

/// Build the report for `month` (`"YYYY-MM"`).
///
/// # Errors
///
/// Returns an error if the month is malformed or a database read fails.
pub async fn build(pool: &SqlitePool, config: &Config, month: &str) -> anyhow::Result<MonthReport> {
    let (start, end) = month_bounds(month)
        .ok_or_else(|| anyhow::anyhow!("month must be YYYY-MM and not in the future"))?;
    let now = chrono::Utc::now().timestamp();
    // For the running month, judge against what has elapsed, not the future.
    let covered_end = end.min(now);

    // Daily counts from raw checks and the downsampled buckets, keyed by day
    // string - the month is a prefix match away.
    let daily = db::daily_all(pool, start).await?;
    let incidents = db::recent_incidents(pool, 1000).await?;

    let mut rows = Vec::with_capacity(config.monitors.len());
    for monitor in &config.monitors {
        let (mut up, mut down, mut degraded) = (0_i64, 0_i64, 0_i64);
        if let Some(days) = daily.get(&monitor.id) {
            for day in days.iter().filter(|day| day.day.starts_with(month)) {
                up += day.up;
                down += day.down;
                degraded += day.degraded;
            }
        }
        let total = up + down + degraded;
        let available = up + degraded;
        let uptime_bp = (total > 0).then(|| (available * 10_000 + total / 2) / total);

        let mut count = 0_usize;
        let mut downtime_secs = 0_i64;
        let mut repairs: Vec<i64> = Vec::new();
        for incident in incidents
            .iter()
            .filter(|incident| incident.monitor_id == monitor.id)
        {
            let incident_end = incident.ended_at.unwrap_or(now);
            let overlap = incident_end.min(covered_end) - incident.started_at.max(start);
            if incident.started_at >= covered_end || overlap <= 0 {
                continue;
            }
            count += 1;
            downtime_secs += overlap;
            if let (Some(ended), Some(duration)) = (incident.ended_at, incident.duration_s)
                && ended >= start
                && ended < end
            {
                repairs.push(duration);
            }
        }
        let mttr_secs = (!repairs.is_empty())
            .then(|| repairs.iter().sum::<i64>() / i64::try_from(repairs.len()).unwrap_or(1));

        let (slo_met, budget_minutes, budget_consumed_minutes) =
            slo_columns(monitor, uptime_bp, available, total, start, covered_end);

        rows.push(MonitorMonth {
            id: monitor.id.clone(),
            name: monitor.name.clone(),
            group: monitor.group.clone(),
            public: monitor.public,
            up,
            down,
            degraded,
            uptime_bp,
            incidents: count,
            downtime_secs,
            mttr_secs,
            slo_bp: monitor.slo_uptime,
            slo_met,
            budget_minutes,
            budget_consumed_minutes,
        });
    }

    Ok(MonthReport {
        month: month.to_owned(),
        label: month_label(start),
        rows,
    })
}

/// The SLO columns for one monitor: target met, the month's budget, and the
/// consumed share. The budget is the *month's* allowance at the SLO (the SLO
/// window setting governs alerting, not the calendar report).
fn slo_columns(
    monitor: &Monitor,
    uptime_bp: Option<i64>,
    available: i64,
    total: i64,
    start: i64,
    covered_end: i64,
) -> (Option<bool>, Option<i64>, Option<i64>) {
    let Some(slo_bp) = monitor.slo_uptime else {
        return (None, None, None);
    };
    let month_days =
        u16::try_from(((covered_end - start) + SECONDS_PER_DAY - 1) / SECONDS_PER_DAY).unwrap_or(0);
    let budget = slo::budget_minutes(month_days, slo_bp);
    let covered_minutes = (covered_end - start) / 60;
    let consumed = (total > 0).then(|| slo::consumed_minutes(available, total, covered_minutes));
    let met = uptime_bp.map(|bp| bp >= i64::from(slo_bp));
    (met, Some(budget), consumed)
}

/// `"May 2026"` from the month's start timestamp.
fn month_label(start: i64) -> String {
    chrono::DateTime::from_timestamp(start, 0)
        .map_or_else(String::new, |dt| dt.format("%B %Y").to_string())
}

/// `"99.97%"` from basis points.
#[must_use]
pub fn format_bp(basis_points: i64) -> String {
    format!("{}.{:02}%", basis_points / 100, basis_points % 100)
}

/// `"2h 05m"`, `"12m"`, `"45s"` - downtime and MTTR formatting.
#[must_use]
pub fn format_secs(seconds: i64) -> String {
    if seconds >= 3600 {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn month_bounds_parse_and_reject() {
        let (start, end) = month_bounds("2026-05").expect("valid month");
        assert_eq!(end - start, 31 * SECONDS_PER_DAY);
        // Leap February.
        let (start, end) = month_bounds("2024-02").expect("leap month");
        assert_eq!(end - start, 29 * SECONDS_PER_DAY);

        for bad in ["2026-13", "2026-5", "may", "2026-05-01", "9999-01"] {
            assert!(month_bounds(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn previous_month_wraps_the_year() {
        let mid_january = chrono::DateTime::parse_from_rfc3339("2026-01-15T10:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(previous_month(mid_january), "2025-12");
        let mid_june = chrono::DateTime::parse_from_rfc3339("2026-06-12T10:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(previous_month(mid_june), "2026-05");
    }

    #[test]
    fn formatting_helpers() {
        assert_eq!(format_bp(10_000), "100.00%");
        assert_eq!(format_bp(9_997), "99.97%");
        assert_eq!(format_secs(45), "45s");
        assert_eq!(format_secs(720), "12m");
        assert_eq!(format_secs(7500), "2h 05m");
    }

    #[tokio::test]
    async fn report_counts_checks_incidents_and_budget() {
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(":memory:")
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .expect("pool");
        db::migrator().run(&pool).await.expect("migrate");

        let (start, _end) = month_bounds("2021-01").unwrap();
        // Nine up checks and one down inside January; one up check in February
        // that must not count.
        for i in 0..9_i64 {
            sqlx::query("INSERT INTO checks (time, monitor_id, status) VALUES (?, 'm', 1)")
                .bind(start + i * 3600)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO checks (time, monitor_id, status) VALUES (?, 'm', 0)")
            .bind(start + 10 * 3600)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO checks (time, monitor_id, status) VALUES (?, 'm', 1)")
            .bind(start + 35 * SECONDS_PER_DAY)
            .execute(&pool)
            .await
            .unwrap();
        // One resolved incident fully inside the month (10 minutes).
        sqlx::query(
            "INSERT INTO incidents (monitor_id, started_at, ended_at, duration_s, created_at) \
             VALUES ('m', ?, ?, 600, ?)",
        )
        .bind(start + 10 * 3600)
        .bind(start + 10 * 3600 + 600)
        .bind(start)
        .execute(&pool)
        .await
        .unwrap();

        let config: Config = toml::from_str(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "m"
            name = "M"
            target = "https://example.com"
            interval_secs = 60
            slo_uptime = 99.9
        "#,
        )
        .unwrap();

        let report = build(&pool, &config, "2021-01").await.expect("report");
        assert_eq!(report.label, "January 2021");
        let row = &report.rows[0];
        assert_eq!((row.up, row.down), (9, 1));
        assert_eq!(row.uptime_bp, Some(9000)); // 9/10
        assert_eq!(row.incidents, 1);
        assert_eq!(row.downtime_secs, 600);
        assert_eq!(row.mttr_secs, Some(600));
        assert_eq!(row.slo_met, Some(false)); // 90% < 99.9%
        // 31 days at 99.9% ≈ 44 minutes of budget; 10% of the month consumed
        // is far past it.
        assert_eq!(row.budget_minutes, Some(44));
        assert!(row.budget_consumed_minutes.unwrap() > 44);

        // A malformed or future month is rejected.
        assert!(build(&pool, &config, "garbage").await.is_err());
        assert!(build(&pool, &config, "2999-01").await.is_err());
    }
}
