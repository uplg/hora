//! The arithmetic on numbers that come from outside, built with overflow
//! checks: a latency (a push monitor's `ping`, a peer's `p50_ms`: any `i64`),
//! a timestamp or a span of time, and the SLO figures drawn from check
//! counts.
//!
//! One function per input, chosen by the first byte, so that a crash names
//! it. Counts of checks stay within what a database can hold (2^40 rows);
//! everything a remote party chooses is unbounded.
#![no_main]

use hora_core::histogram::LatencyHistogram;
use hora_core::{fmt, slo, tune};
use libfuzzer_sys::fuzz_target;

/// More checks than any database holds.
const MAX_COUNT: u64 = 1 << 40;

fuzz_target!(|data: &[u8]| {
    let mut input = hora_fuzz::Input::new(data);
    let op = input.byte();
    let mut count = || i64::try_from(input.u64() % MAX_COUNT).unwrap_or(0);
    let (total, part, minutes) = (count(), count(), count());
    let part = part.min(total);
    let (a, b) = (input.i64(), input.i64());
    let unsigned = input.u64();
    // What validation leaves of an SLO: a share in basis points, a window.
    let slo_bp = 1 + input.u32() % 9_999;
    let days = u16::try_from(input.u32() % 3651).unwrap_or(30);
    match op % 20 {
        0 => drop(fmt::duration(a)),
        1 => drop(fmt::period(unsigned)),
        2 => drop((fmt::utc(a), fmt::date(a), fmt::short_date(a))),
        3 => drop(fmt::latency(a)),
        4 => drop(fmt::bytes(unsigned)),
        5 => drop(fmt::elapsed(unsigned)),
        6 => drop((fmt::pct_bp(a), fmt::permille(a))),
        7 => drop((
            fmt::basis_points(part, total),
            fmt::pct(part, total),
            fmt::permille_of(part, total),
        )),
        8 => drop(tune::round_up_nice(a)),
        9 => drop((
            slo::fast_burn_threshold_x10(days),
            slo::slow_burn_threshold_x10(days),
            slo::budget_minutes(days, slo_bp),
        )),
        10 => drop(slo::burn_rate_x10(part, total, slo_bp)),
        11 => drop(slo::consumed_minutes(part, total, minutes)),
        12 => drop(slo::remaining_minutes(days, slo_bp, part, total)),
        13 => drop(slo::exhausted_in_secs(a, b, slo_bp)),
        14 => drop(slo::format_burn_x10(a)),
        _ => {
            // A day of latencies, each any `i64`, binned and ranked.
            let mut histogram = LatencyHistogram::default();
            histogram.record(a, 1 + unsigned % MAX_COUNT);
            histogram.record(b, 1 + u64::from(slo_bp));
            while !input.is_empty() {
                histogram.record(input.i64(), 1);
            }
            let _ = histogram.len();
            let _ = histogram.p50_p95_p99();
            let again = LatencyHistogram::decode(&histogram.encode());
            assert_eq!(again.as_ref(), Some(&histogram));
            histogram.unrecord(a, unsigned);
        }
    }
});
