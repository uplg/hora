//! Availability SLO arithmetic: error budgets and burn rates.
//!
//! Burn rate is the Google SRE measure: 1x means consuming the error budget
//! exactly as fast as the SLO window replenishes it; 14.4x means a 30-day
//! budget is gone in ~2 days. Alerts pair a long lookback (is the burn
//! sustained?) with a short one (is it still happening *now*?) so a stale
//! spike never pages.
//!
//! Everything here is integer arithmetic - burn rates travel in tenths
//! (144 = 14.4x) and SLO targets in basis points (9990 = 99.9%) - so the
//! results are exact and the config stays `Eq`.

/// Fast-burn threshold in tenths for a window: 2% of the budget consumed in
/// one hour. 144 (14.4x) for the canonical 30-day window.
#[must_use]
pub fn fast_burn_threshold_x10(window_days: u16) -> i64 {
    // 0.02 * days * 24 * 10
    i64::from(window_days) * 24 / 5
}

/// Slow-burn threshold in tenths: 5% of the budget consumed in six hours.
/// 60 (6.0x) for the canonical 30-day window.
#[must_use]
pub fn slow_burn_threshold_x10(window_days: u16) -> i64 {
    // 0.05 * days * 24 / 6 * 10
    i64::from(window_days) * 24 / 12
}

/// The burn rate over a lookback, in tenths: observed down-ratio divided by
/// the budget fraction (`1 - slo`). 0 when there are no checks (no data is
/// not an outage).
#[must_use]
pub fn burn_rate_x10(available: i64, total: i64, slo_basis_points: u32) -> i64 {
    if total <= 0 {
        return 0;
    }
    let down = total.saturating_sub(available).max(0);
    let budget_bp = i64::from(10_000 - slo_basis_points.min(9_999));
    // (down / total) / (budget_bp / 10_000) * 10
    down.saturating_mul(100_000) / (total * budget_bp)
}

/// Minutes of allowed downtime over the whole window for an SLO target.
#[must_use]
pub fn budget_minutes(window_days: u16, slo_basis_points: u32) -> i64 {
    let window_minutes = i64::from(window_days) * 24 * 60;
    window_minutes * i64::from(10_000 - slo_basis_points.min(9_999)) / 10_000
}

/// Estimated downtime minutes already consumed, from the window's check counts
/// and the span actually covered by data (a young monitor has burned little).
#[must_use]
pub fn consumed_minutes(available: i64, total: i64, covered_minutes: i64) -> i64 {
    if total <= 0 {
        return 0;
    }
    let down = total.saturating_sub(available).max(0);
    down.saturating_mul(covered_minutes) / total
}

/// Estimated seconds until the budget is exhausted at the current burn rate:
/// downtime accrues at `burn × (1 - slo)` seconds per second, so the remaining
/// budget divided by that rate. `None` when the burn is not positive (no
/// exhaustion in sight); `Some(0)` when the budget is already gone.
#[must_use]
pub fn exhausted_in_secs(
    remaining_budget_minutes: i64,
    burn_x10: i64,
    slo_basis_points: u32,
) -> Option<i64> {
    if burn_x10 <= 0 {
        return None;
    }
    if remaining_budget_minutes <= 0 {
        return Some(0);
    }
    let budget_bp = i64::from(10_000 - slo_basis_points.min(9_999));
    // remaining_secs / (burn * budget_fraction)
    Some(
        remaining_budget_minutes.saturating_mul(60 * 10 * 10_000)
            / burn_x10.saturating_mul(budget_bp),
    )
}

/// `"14.4x"` from tenths, for alert messages.
#[must_use]
pub fn format_burn_x10(burn_x10: i64) -> String {
    if burn_x10 % 10 == 0 {
        format!("{}x", burn_x10 / 10)
    } else {
        format!("{}.{}x", burn_x10 / 10, burn_x10 % 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_match_the_sre_book_for_30_days() {
        assert_eq!(fast_burn_threshold_x10(30), 144); // 14.4x
        assert_eq!(slow_burn_threshold_x10(30), 60); // 6.0x
    }

    #[test]
    fn burn_rate_normalises_by_budget() {
        // 99.9% SLO: budget fraction 0.001. Fully down = 1000x burn.
        assert_eq!(burn_rate_x10(0, 60, 9990), 10_000);
        // 10% down at 99.9% = 100x.
        assert_eq!(burn_rate_x10(54, 60, 9990), 1000);
        // No checks or all up: zero.
        assert_eq!(burn_rate_x10(0, 0, 9990), 0);
        assert_eq!(burn_rate_x10(60, 60, 9990), 0);
    }

    #[test]
    fn budget_and_consumption_in_minutes() {
        // 99.9% over 30d = 43.2 minutes, truncated to 43.
        assert_eq!(budget_minutes(30, 9990), 43);
        // 99% over 30d = 432 minutes.
        assert_eq!(budget_minutes(30, 9900), 432);
        // 1% of checks down across 30 covered days = 432 minutes down.
        assert_eq!(consumed_minutes(9900, 10_000, 30 * 1440), 432);
        assert_eq!(consumed_minutes(0, 0, 30 * 1440), 0);
    }

    #[test]
    fn exhaustion_estimates() {
        // 20 minutes left at 14.4x on 99.9%: 1200s / 0.0144 ≈ 23h09.
        assert_eq!(exhausted_in_secs(20, 144, 9990), Some(83_333));
        // Already exhausted / not burning.
        assert_eq!(exhausted_in_secs(0, 144, 9990), Some(0));
        assert_eq!(exhausted_in_secs(20, 0, 9990), None);
    }

    #[test]
    fn burn_formats_in_tenths() {
        assert_eq!(format_burn_x10(144), "14.4x");
        assert_eq!(format_burn_x10(60), "6x");
        assert_eq!(format_burn_x10(10_000), "1000x");
    }
}
