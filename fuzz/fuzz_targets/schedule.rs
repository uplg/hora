//! Schedules and durations: a push monitor's cron `schedule` (parsed, then
//! asked when the next heartbeat is due, as the scheduler does on each
//! tick), the durations of the API and the CLI, a maintenance window's
//! `starts_at`.
//!
//! Input: the text on the first line, then the clock (when the last
//! heartbeat came, how long ago, the grace).
#![no_main]

use hora_core::fuzzing::heartbeat;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut input = hora_fuzz::Input::new(data);
    let text = input.line();
    let since = input.time();
    // Up to a few years without a heartbeat.
    let now = since + i64::from(input.u32() % 100_000_000);
    let grace_secs = input.u64();
    let seen = input.flag();

    let _ = hora_core::parse_duration(&text);
    let _ = hora_core::planned::parse_start(&text);
    if let Ok(cron) = hora_core::config::parse_cron(&text) {
        let _ = heartbeat::cron_overdue(&cron, grace_secs, since, seen, now);
    }
    let _ = heartbeat::interval_overdue(grace_secs, since, now);
});
