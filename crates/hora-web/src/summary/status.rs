//! Status ranking, labels and the daily uptime bar, shared by the summary and
//! the other views.

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};

use hora_core::SECONDS_PER_DAY;
use hora_core::db::DayRow;

use super::view::{DayCell, MonitorView};
use crate::display::DisplayState;

/// Whether the measured p95 meets the configured latency objective.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SloState {
    /// No objective, or nothing measured yet.
    None,
    Met,
    Breached,
}

impl SloState {
    /// As the operator's details say it.
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Met => "met",
            Self::Breached => "breached",
            Self::None => "no data",
        }
    }
}

/// Whether the measured p95 meets the configured latency objective.
pub(crate) fn slo_state(target: Option<i64>, p95: Option<i64>) -> SloState {
    match (target, p95) {
        (Some(target), Some(p95)) if p95 <= target => SloState::Met,
        (Some(_), Some(_)) => SloState::Breached,
        (None, _) | (_, None) => SloState::None,
    }
}

/// How much of a monitor's error budget is left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BudgetState {
    /// No availability objective.
    None,
    Met,
    /// A quarter of the budget or less is left.
    Low,
    /// Nothing is left.
    Breached,
}

/// Where a monitor's TLS certificate stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CertState {
    /// No certificate recorded (not an `https://` monitor, or not read yet).
    None,
    Ok,
    /// Valid, but inside the warning threshold (`alerts.cert_expiry_days`).
    Warn,
    Expired,
}

impl CertState {
    /// As the operator's details say it.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Expired => "expired",
        }
    }
}

/// The state of a whole view, as the API names it. It is the page's own
/// reading ([`DisplayState::overall`]): a monitor inside a planned
/// maintenance window counts as maintenance, not as the outage its checks
/// report, and a down seen from this node only counts as up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Overall {
    Up,
    Degraded,
    Down,
    /// Nothing is slow or down outside a planned maintenance window.
    Maintenance,
    /// No monitor has data yet.
    Unknown,
}

impl Overall {
    pub(crate) fn of<'a>(monitors: impl IntoIterator<Item = &'a MonitorView>) -> Self {
        match DisplayState::overall(monitors.into_iter().map(MonitorView::display_state)) {
            DisplayState::Up => Self::Up,
            DisplayState::Degraded => Self::Degraded,
            DisplayState::Down => Self::Down,
            DisplayState::Maint => Self::Maintenance,
            DisplayState::None => Self::Unknown,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Up => "All systems operational",
            Self::Degraded => "Degraded performance",
            Self::Down => "Major outage",
            Self::Maintenance => "Planned maintenance",
            Self::Unknown => "Awaiting data",
        }
    }
}

/// The days left on a certificate that expires `secs_left` seconds from now
/// (negative once it has), as a whole number that never lies about which
/// side of the expiry it is on: at least 1 while the certificate is valid,
/// at most -1 once it has expired. Truncating to days said 0 for both 23
/// hours left and 23 hours gone, and 0 read as "expired".
pub(crate) fn cert_days_left(secs_left: i64) -> i64 {
    if secs_left >= 0 {
        (secs_left / SECONDS_PER_DAY).max(1)
    } else {
        // Floors: one second past is a day gone, like one second short of a
        // day left is no day left.
        secs_left.div_euclid(SECONDS_PER_DAY)
    }
}

/// Where a certificate stands, from the seconds until it expires (negative
/// once it has; its last second of validity is still valid). `threshold` is
/// the warning's lead, in days.
pub(crate) fn cert_state_for(secs_left: Option<i64>, threshold: i64) -> CertState {
    match secs_left {
        None => CertState::None,
        Some(left) if left < 0 => CertState::Expired,
        Some(left) if left / SECONDS_PER_DAY <= threshold => CertState::Warn,
        Some(_) => CertState::Ok,
    }
}

/// The certificate's time left, short, for the operator's details: `40 d`,
/// `23 h` under a day, `12 min` under an hour - one unit, rounded down.
pub(crate) fn cert_label(secs_left: i64) -> String {
    if secs_left < 0 {
        return "expired".to_owned();
    }
    let unit = match secs_left {
        SECONDS_PER_DAY.. => SECONDS_PER_DAY,
        3600.. => 3600,
        _ => 60,
    };
    hora_core::fmt::duration(secs_left - secs_left % unit)
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
                date: date.clone(),
                state: "empty",
                permille: None,
                maint: false,
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

/// A day's state from its counts, with its availability in permille: up
/// without a failure, slow as long as it was mostly up, down below
/// [`DAY_OUTAGE_BELOW_PERMILLE`]. `None` for a day without checks.
pub(crate) fn day_state(up: i64, down: i64, degraded: i64) -> Option<(&'static str, i64)> {
    let permille = hora_core::fmt::permille_of(up + degraded, up + down + degraded)?;
    let state = if down == 0 && degraded == 0 {
        "up"
    } else if permille >= DAY_OUTAGE_BELOW_PERMILLE {
        "degraded"
    } else {
        "down"
    };
    Some((state, permille))
}

/// A day's class in a bar (see `render::day_class`), maintenance applied:
/// a day a planned window touched (`maint`) is maintenance unless it still
/// was an outage, and a window open now (`maintenance_now`, for today's
/// cell) marks a day that is up so far. The one rule of the status page's
/// bar, the monitor page's and the monthly report's strip.
pub(crate) fn bar_class(state: &str, maint: bool, maintenance_now: bool) -> u8 {
    match crate::render::day_class(state) {
        b'u' | b'd' if maint => b'm',
        b'u' if maintenance_now => b'm',
        class => class,
    }
}

pub(crate) fn day_cell(date: String, row: &DayRow) -> DayCell {
    let state = day_state(row.up, row.down, row.degraded);
    DayCell {
        date,
        state: state.map_or("empty", |(state, _)| state),
        permille: state.map(|(_, permille)| permille),
        maint: false,
    }
}
