//! The latency histogram blob stored with each hourly roll-up, read back
//! from the database: decoded, merged as a day of them is, ranked and
//! encoded again.
#![no_main]

use hora_core::histogram::LatencyHistogram;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some(histogram) = LatencyHistogram::decode(data) else {
        // A blob that does not decode adds nothing when merged.
        let mut day = LatencyHistogram::default();
        assert_eq!(day.merge_encoded(data), None);
        assert!(day.is_empty());
        return;
    };
    let _ = histogram.len();
    let _ = histogram.p50_p95_p99();
    let _ = histogram.percentile(100);
    // What was read is written back the same.
    assert_eq!(
        LatencyHistogram::decode(&histogram.encode()).as_ref(),
        Some(&histogram)
    );
    // A day is the sum of its hours.
    let mut day = histogram.clone();
    let _ = day.merge_encoded(data);
    day.merge(&histogram);
    let _ = day.len();
    let _ = day.p50_p95_p99();
});
