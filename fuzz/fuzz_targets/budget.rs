//! A peer's maintenance budget, without the database: what an authenticated
//! heartbeat announces (any `i64` in every field: the peer chooses them) is
//! filtered and cut as `apply_announcement` does, against the rows that peer
//! was granted before.
//!
//! The granted rows are what this node stored earlier, so they lie where it
//! can have put them: around `now`, at most a week long.
#![no_main]

use hora_core::db::MaintenanceWindow;
use hora_core::mesh::wire::AnnouncedWindow;
use hora_core::planned::{MAX_ANNOUNCED_WINDOWS, fuzz};
use libfuzzer_sys::fuzz_target;

const DAY: i64 = 86_400;
/// `accept_maintenance_secs` of the fuzz node's `peer-x`.
const ACCEPTED_SECS: i64 = 900;

fuzz_target!(|data: &[u8]| {
    let config = hora_fuzz::node();
    let peer = &config.peers[0];
    let mut input = hora_fuzz::Input::new(data);
    let now = input.time();

    let granted: Vec<MaintenanceWindow> = (0..input.byte() % 8)
        .map(|index| {
            let starts_at = now + i64::from(input.u32() % 2_000_000) - 10 * DAY;
            MaintenanceWindow {
                id: i64::from(index),
                starts_at,
                ends_at: starts_at + 1 + i64::from(input.u32() % 604_800),
                reason: String::new(),
                selector: "peer".to_owned(),
                origin: "peer:peer-x".to_owned(),
                remote_id: Some(i64::from(input.byte() % 8)),
                created_at: starts_at,
            }
        })
        .collect();

    let mut announced = Vec::new();
    while !input.is_empty() && announced.len() < 2 * MAX_ANNOUNCED_WINDOWS {
        // Near `now`, where a window is accepted, or anywhere at all.
        let (starts_at, ends_at) = if input.flag() {
            let starts_at = now + i64::from(input.u32() % 2_000_000) - 10 * DAY;
            (starts_at, starts_at + i64::from(input.u32() % 100_000) - 10)
        } else {
            (input.i64(), input.i64())
        };
        announced.push(AnnouncedWindow {
            id: if input.flag() {
                i64::from(input.byte() % 8)
            } else {
                input.i64()
            },
            starts_at,
            ends_at,
            reason: String::new(),
            host: match input.byte() % 3 {
                0 => None,
                1 => Some("beta".to_owned()),
                _ => Some("elsewhere".to_owned()),
            },
        });
    }

    let kept = fuzz::budgeted(peer, &announced, &granted, now);
    assert!(kept.len() <= MAX_ANNOUNCED_WINDOWS);
    for &(_, starts_at, ends_at) in &kept {
        assert!(starts_at < ends_at, "an empty window was kept");
        assert!(ends_at > now, "a window already over was kept");
        assert!(
            ends_at - starts_at <= ACCEPTED_SECS,
            "a window longer than accept_maintenance_secs was kept"
        );
    }
    // What is kept covers a measurable span.
    let spans: Vec<(i64, i64)> = kept.iter().map(|&(_, start, end)| (start, end)).collect();
    if let (Some(first), Some(last)) = (
        spans.iter().map(|span| span.0).min(),
        spans.iter().map(|span| span.1).max(),
    ) {
        let muted = fuzz::measure(&spans, first, last);
        assert!((0..=last - first).contains(&muted));
    }
});
