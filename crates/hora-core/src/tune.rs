//! `hora tune`: replay a monitor's stored check history against alternative
//! anti-flap settings, so an operator can *see* the trade-off before touching a
//! knob. Nobody helps tune a monitor; this is the project's thesis, quantified
//! on the user's own data.
//!
//! Everything is pure analytics over the raw `checks` table (which survives the
//! full retention window, default 90 days), so the command never probes, never
//! writes, and carries no risk. Two knobs are replayable from history:
//!
//! - **`fail_threshold`** - the recorded per-check status sequence (0 down, 1
//!   up, 2 degraded) is exactly what the scheduler's down state machine sees, so
//!   we can count how many down alerts each candidate threshold would fire and
//!   the detection delay it costs. This is the headline.
//! - **`degraded_over_ms`** - every up check carries its latency sample, so the
//!   latency distribution recommends a threshold that flags genuine slowness
//!   rather than normal traffic.
//!
//! **`probe_retries` is deliberately *not* replayed**: only a probe's final
//! attempt reaches the database (see `probe::run`), so retries are invisible
//! here. The closest available signal is the count of single-check failures -
//! the cross-tick equivalent lever is `fail_threshold`, which this *does*
//! replay.

use crate::db::CheckSample;

/// The widest tier ratio below which the failure-run lengths are treated as one
/// continuous cluster (no flap/outage split, hence no threshold recommendation):
/// a tier must be at least twice the one below it to count as a real gap.
const GAP_RATIO: f64 = 2.0;

/// Never recommend a `fail_threshold` above this. A genuine flap clears in a
/// handful of checks; if the "flap" cluster is longer than this, the situation
/// is unusual enough that the operator should read the table and decide.
const MAX_RECOMMENDED_THRESHOLD: u32 = 6;

/// Status code stored for a down check (mirrors `Outcome::status_value`).
const STATUS_DOWN: i64 = 0;

/// Lengths of every maximal run of consecutive *down* checks (status 0). A
/// degraded or up check resets the run, exactly as the scheduler resets
/// `consecutive_down` on any `outcome.up` tick.
#[must_use]
pub fn failure_runs(samples: &[CheckSample]) -> Vec<u32> {
    let mut runs = Vec::new();
    let mut current: u32 = 0;
    for sample in samples {
        if sample.status == STATUS_DOWN {
            current = current.saturating_add(1);
        } else if current > 0 {
            runs.push(current);
            current = 0;
        }
    }
    if current > 0 {
        runs.push(current);
    }
    runs
}

/// Down alerts that would fire at `threshold`: one per failure run long enough
/// to confirm it (the scheduler alerts once `consecutive_down >= threshold`).
#[must_use]
pub fn alerts_at(runs: &[u32], threshold: u32) -> usize {
    let threshold = threshold.max(1);
    runs.iter().filter(|&&len| len >= threshold).count()
}

/// A `fail_threshold` recommendation derived from the shape of the failure runs.
#[derive(Debug, PartialEq, Eq)]
pub struct ThresholdAdvice {
    /// The recommended threshold, or `None` when the runs show no clear
    /// flap/outage separation (too few, or a continuous spread) to justify one.
    pub recommended: Option<u32>,
    /// Top of the flap cluster: runs this short are noise the threshold filters.
    pub flap_max: Option<u32>,
    /// Shortest run in the outage cluster: the recommendation stays at or below
    /// it, so every real outage is still caught.
    pub smallest_outage: Option<u32>,
    /// Longest failure run seen - the worst outage the data contains.
    pub longest_run: u32,
}

/// Split the failure runs into a flap cluster and an outage cluster at the
/// widest multiplicative gap in their sorted lengths, and recommend the
/// smallest threshold that drops every flap while still catching every real
/// outage (`flap_max + 1`, which minimises the added detection delay).
///
/// Returns no recommendation when there is too little signal: fewer than two
/// distinct run lengths, no gap of at least [`GAP_RATIO`], or a flap cluster
/// longer than [`MAX_RECOMMENDED_THRESHOLD`].
#[must_use]
pub fn recommend_threshold(runs: &[u32]) -> ThresholdAdvice {
    let longest_run = runs.iter().copied().max().unwrap_or(0);

    // Distinct lengths, ascending: the tiers a threshold can sit between.
    let mut tiers: Vec<u32> = runs.to_vec();
    tiers.sort_unstable();
    tiers.dedup();

    let mut advice = ThresholdAdvice {
        recommended: None,
        flap_max: None,
        smallest_outage: None,
        longest_run,
    };
    if tiers.len() < 2 {
        return advice;
    }

    // The widest jump between adjacent tiers is the flap/outage boundary.
    let mut best_ratio = 0.0_f64;
    let mut split_at = 0; // index of the top of the flap cluster
    for window in tiers.windows(2).enumerate() {
        let (index, pair) = window;
        let ratio = f64::from(pair[1]) / f64::from(pair[0]);
        if ratio > best_ratio {
            best_ratio = ratio;
            split_at = index;
        }
    }
    if best_ratio < GAP_RATIO {
        return advice;
    }

    let flap_max = tiers[split_at];
    let smallest_outage = tiers[split_at + 1];
    advice.flap_max = Some(flap_max);
    advice.smallest_outage = Some(smallest_outage);
    let recommended = flap_max.saturating_add(1);
    if recommended <= MAX_RECOMMENDED_THRESHOLD {
        advice.recommended = Some(recommended);
    }
    advice
}

/// Latency percentiles over the up (and degraded) checks that carry a sample.
#[derive(Debug, PartialEq, Eq)]
pub struct LatencyStats {
    pub p50: i64,
    pub p95: i64,
    pub p99: i64,
    pub max: i64,
    /// Number of samples the percentiles were computed from.
    pub count: usize,
}

/// Latency distribution of the up checks (status 1 or 2) that recorded a
/// sample. `None` when none did - a push monitor, or a window with no data.
#[must_use]
pub fn latency_stats(samples: &[CheckSample]) -> Option<LatencyStats> {
    let mut latencies: Vec<i64> = samples
        .iter()
        .filter(|sample| sample.status != STATUS_DOWN)
        .filter_map(|sample| sample.latency_ms)
        .collect();
    if latencies.is_empty() {
        return None;
    }
    latencies.sort_unstable();
    Some(LatencyStats {
        p50: percentile(&latencies, 50),
        p95: percentile(&latencies, 95),
        p99: percentile(&latencies, 99),
        max: *latencies.last().unwrap_or(&0),
        count: latencies.len(),
    })
}

/// Nearest-rank percentile of an ascending slice (`p` in 1..=100).
fn percentile(sorted: &[i64], p: u32) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    let n = sorted.len();
    // ceil(p/100 * n), 1-based, clamped into range.
    let rank = (usize::try_from(p).unwrap_or(100) * n).div_ceil(100).max(1);
    sorted[rank.min(n) - 1]
}

/// A latency threshold should sit above normal operation so it flags genuine
/// slowness, not the daily noise: recommend p99, rounded up to a round number.
#[must_use]
pub fn recommend_degraded(stats: &LatencyStats) -> i64 {
    round_up_nice(stats.p99.max(1))
}

/// Round up to a readable step sized to the magnitude (237 -> 250, 1840 ->
/// 2000, 73 -> 80). Keeps the recommendation memorable rather than precise.
#[must_use]
pub fn round_up_nice(ms: i64) -> i64 {
    if ms <= 0 {
        return 0;
    }
    let step = if ms < 100 {
        10
    } else if ms < 1_000 {
        50
    } else if ms < 10_000 {
        100
    } else {
        500
    };
    ((ms + step - 1) / step) * step
}

/// One row of the `fail_threshold` replay table.
#[derive(Debug, PartialEq, Eq)]
pub struct ThresholdRow {
    pub threshold: u32,
    /// Down alerts this threshold would have fired over the window.
    pub alerts: usize,
    /// Time from a run's first failed check to its alert: `threshold * interval`.
    pub detect_after_secs: i64,
}

/// The full tuning analysis for one monitor, ready for the CLI to format.
#[derive(Debug)]
pub struct MonitorTuning {
    pub id: String,
    pub name: String,
    pub group: Option<String>,
    pub kind: &'static str,
    pub interval_secs: u64,
    /// Bounds of the replayed data (`None` when the monitor had no checks).
    pub window: Option<(i64, i64)>,
    pub checks: usize,
    pub down_checks: usize,
    pub current_threshold: u32,
    /// Failure-run lengths, longest first (for display).
    pub runs: Vec<u32>,
    pub single_check_failures: usize,
    pub current_alerts: usize,
    pub table: Vec<ThresholdRow>,
    pub advice: ThresholdAdvice,
    pub latency: Option<LatencyStats>,
    pub current_degraded_over_ms: Option<i64>,
    /// Up checks already over the current `degraded_over_ms` (when one is set).
    pub currently_degraded: Option<usize>,
    pub recommended_degraded_over_ms: Option<i64>,
}

impl MonitorTuning {
    /// Whether the current threshold is so high no recorded outage ever reached
    /// it - the monitor would never have alerted (a real misconfiguration).
    #[must_use]
    pub fn never_alerts(&self) -> bool {
        !self.runs.is_empty() && self.current_alerts == 0
    }

    /// Up checks the latency distribution was computed from.
    #[must_use]
    pub fn latency_count(&self) -> usize {
        self.latency.as_ref().map_or(0, |stats| stats.count)
    }
}

/// Identifying context for one monitor, so [`analyze`] stays a pure function of
/// (settings, samples) and the orchestration is unit-testable without a config.
pub struct MonitorContext<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub group: Option<&'a str>,
    pub kind: &'static str,
    pub interval_secs: u64,
    pub current_threshold: u32,
    pub current_degraded_over_ms: Option<i64>,
}

/// Replay one monitor's samples and assemble its [`MonitorTuning`].
#[must_use]
pub fn analyze(ctx: &MonitorContext, samples: &[CheckSample]) -> MonitorTuning {
    let current_threshold = ctx.current_threshold.max(1);
    let runs = failure_runs(samples);
    let advice = recommend_threshold(&runs);
    let interval = i64::try_from(ctx.interval_secs).unwrap_or(i64::MAX);

    // Table spans 1 up through whatever covers the current value, the
    // recommendation and the worst outage, capped so it stays readable.
    let hi = advice
        .longest_run
        .min(8)
        .max(current_threshold + 2)
        .max(advice.recommended.unwrap_or(1));
    let table = (1..=hi)
        .map(|threshold| ThresholdRow {
            threshold,
            alerts: alerts_at(&runs, threshold),
            detect_after_secs: i64::from(threshold).saturating_mul(interval),
        })
        .collect();

    let latency = latency_stats(samples);
    let currently_degraded = ctx.current_degraded_over_ms.map(|threshold| {
        samples
            .iter()
            .filter(|sample| sample.status != STATUS_DOWN)
            .filter_map(|sample| sample.latency_ms)
            .filter(|&latency| latency > threshold)
            .count()
    });
    let recommended_degraded_over_ms = latency.as_ref().map(recommend_degraded);

    let single_check_failures = runs.iter().filter(|&&len| len == 1).count();
    let down_checks = samples
        .iter()
        .filter(|sample| sample.status == STATUS_DOWN)
        .count();
    let window = match (samples.first(), samples.last()) {
        (Some(first), Some(last)) => Some((first.time, last.time)),
        _ => None,
    };
    let mut runs_desc = runs;
    runs_desc.sort_unstable_by(|a, b| b.cmp(a));

    MonitorTuning {
        id: ctx.id.to_owned(),
        name: ctx.name.to_owned(),
        group: ctx.group.map(str::to_owned),
        kind: ctx.kind,
        interval_secs: ctx.interval_secs,
        window,
        checks: samples.len(),
        down_checks,
        current_threshold,
        single_check_failures,
        current_alerts: alerts_at(&runs_desc, current_threshold),
        table,
        advice,
        latency,
        current_degraded_over_ms: ctx.current_degraded_over_ms,
        currently_degraded,
        recommended_degraded_over_ms,
        runs: runs_desc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(time: i64, status: i64, latency_ms: Option<i64>) -> CheckSample {
        CheckSample {
            time,
            status,
            latency_ms,
        }
    }

    /// Build a status sequence at 60s spacing from a compact spec: 0 down, 1 up,
    /// 2 degraded. Latency is fixed so latency stats stay predictable.
    fn seq(statuses: &[i64]) -> Vec<CheckSample> {
        statuses
            .iter()
            .enumerate()
            .map(|(i, &status)| {
                let latency = (status != 0).then_some(100);
                sample(i64::try_from(i).unwrap_or(0) * 60, status, latency)
            })
            .collect()
    }

    #[test]
    fn failure_runs_are_maximal_down_streaks() {
        // up, down×2, up, degraded, down×1, down ends at the tail (length 3).
        let s = seq(&[1, 0, 0, 1, 2, 0, 1, 0, 0, 0]);
        assert_eq!(failure_runs(&s), vec![2, 1, 3]);
    }

    #[test]
    fn degraded_does_not_extend_a_down_run() {
        // A degraded check (status 2) is "up" to the down state machine.
        let s = seq(&[0, 2, 0]);
        assert_eq!(failure_runs(&s), vec![1, 1]);
    }

    #[test]
    fn alerts_count_runs_meeting_the_threshold() {
        let runs = vec![1, 1, 2, 6, 23];
        assert_eq!(alerts_at(&runs, 1), 5);
        assert_eq!(alerts_at(&runs, 3), 2); // 6 and 23
        assert_eq!(alerts_at(&runs, 7), 1); // 23 only
        // A zero threshold is clamped to 1, never "everything plus the gaps".
        assert_eq!(alerts_at(&runs, 0), 5);
    }

    #[test]
    fn recommendation_sits_just_above_the_flap_cluster() {
        // Flaps of 1-2 checks, real outages of 6+; the widest gap is 2 -> 6.
        let advice = recommend_threshold(&[1, 1, 1, 2, 2, 6, 9, 15]);
        assert_eq!(advice.recommended, Some(3));
        assert_eq!(advice.flap_max, Some(2));
        assert_eq!(advice.smallest_outage, Some(6));
        assert_eq!(advice.longest_run, 15);
    }

    #[test]
    fn single_check_flaps_recommend_threshold_two() {
        let advice = recommend_threshold(&[1, 1, 1, 6, 6]);
        assert_eq!(advice.recommended, Some(2));
    }

    #[test]
    fn no_recommendation_without_a_clear_gap() {
        // A continuous spread of substantial outages: nothing to trim.
        assert_eq!(recommend_threshold(&[5, 6, 8, 9]).recommended, None);
        // A single run is no signal at all.
        assert_eq!(recommend_threshold(&[4]).recommended, None);
        // No failures: no recommendation, longest run zero.
        let empty = recommend_threshold(&[]);
        assert_eq!(empty.recommended, None);
        assert_eq!(empty.longest_run, 0);
    }

    #[test]
    fn absurdly_long_flap_cluster_is_not_recommended() {
        // The lower cluster tops out at 50: a "threshold = 51" is nonsense, so
        // the gap is found but no recommendation is emitted.
        let advice = recommend_threshold(&[50, 50, 200, 200]);
        assert_eq!(advice.flap_max, Some(50));
        assert_eq!(advice.recommended, None);
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let sorted: Vec<i64> = (1..=100).collect();
        assert_eq!(percentile(&sorted, 50), 50);
        assert_eq!(percentile(&sorted, 95), 95);
        assert_eq!(percentile(&sorted, 99), 99);
        assert_eq!(percentile(&sorted, 100), 100);
    }

    #[test]
    fn latency_stats_ignore_down_checks() {
        let mut s = seq(&[1, 1, 1, 1]);
        s[0].latency_ms = Some(10);
        s[1].latency_ms = Some(20);
        s[2].latency_ms = Some(30);
        s[3].latency_ms = Some(40);
        // A down check with a (stale) latency must not enter the distribution.
        s.push(sample(999, 0, Some(99_999)));
        let stats = latency_stats(&s).expect("samples present");
        assert_eq!(stats.count, 4);
        assert_eq!(stats.max, 40);
        assert_eq!(stats.p50, 20);
    }

    #[test]
    fn latency_stats_none_without_samples() {
        // Push-style: up checks but no latency recorded.
        let s = vec![sample(0, 1, None), sample(60, 1, None)];
        assert!(latency_stats(&s).is_none());
    }

    #[test]
    fn round_up_nice_scales_with_magnitude() {
        assert_eq!(round_up_nice(73), 80);
        assert_eq!(round_up_nice(237), 250);
        assert_eq!(round_up_nice(1_840), 1_900);
        assert_eq!(round_up_nice(12_300), 12_500);
        assert_eq!(round_up_nice(250), 250); // already round
        assert_eq!(round_up_nice(0), 0);
    }

    #[test]
    fn analyze_fills_the_table_and_flags_a_never_alerting_monitor() {
        // One outage of 2 checks, current threshold 5: never reaches it.
        let s = seq(&[1, 1, 0, 0, 1, 1]);
        let ctx = MonitorContext {
            id: "api",
            name: "API",
            group: Some("edge"),
            kind: "http",
            interval_secs: 60,
            current_threshold: 5,
            current_degraded_over_ms: Some(50),
        };
        let tuning = analyze(&ctx, &s);
        assert_eq!(tuning.checks, 6);
        assert_eq!(tuning.down_checks, 2);
        assert_eq!(tuning.runs, vec![2]);
        assert_eq!(tuning.current_alerts, 0);
        assert!(tuning.never_alerts());
        // Latency is 100ms on every up check; over the 50ms current threshold.
        assert_eq!(tuning.currently_degraded, Some(4));
        assert_eq!(tuning.window, Some((0, 5 * 60)));
        // Table reaches the current threshold + 2.
        assert_eq!(tuning.table.last().map(|row| row.threshold), Some(7));
    }
}
