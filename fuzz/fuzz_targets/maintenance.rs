//! Planned maintenance against a real (in-memory) database: windows planned
//! through the API's inputs, windows a peer announces on its heartbeats, and
//! what is then read back - the schedule, what is told to the peers.
//!
//! Input: one operation per line, on a clock that starts at
//! `1_800_000_000` and only moves forward.
//!
//! - `A<json>`: an authenticated heartbeat of `peer-x` with this body
//!   (`a<json>`: without its token);
//! - `P<monitors>|<host>|<starts_at>|<duration>|<reason>`: plan a window
//!   (an empty field is an absent one);
//! - `C<id>`: cancel a window;
//! - `T<seconds>`: let time pass.
#![no_main]

use chrono::DateTime;
use hora_core::db::Store;
use hora_core::mesh::wire::HeartbeatBody;
use hora_core::planned::{self, Plan};
use libfuzzer_sys::fuzz_target;

const START: i64 = 1_800_000_000;
const MAX_OPERATIONS: usize = 8;
const DAY: i64 = 86_400;

fuzz_target!(|data: &[u8]| {
    hora_fuzz::block_on(run(data));
});

async fn run(data: &[u8]) {
    let config = hora_fuzz::node();
    let peer = &config.peers[0];
    let store = Store::in_memory().await;
    let mut input = hora_fuzz::Input::new(data);
    let mut now = START;
    for _ in 0..MAX_OPERATIONS {
        if input.is_empty() {
            break;
        }
        let line = input.line();
        let Some(operation) = line.chars().next() else {
            continue;
        };
        let argument = &line[operation.len_utf8()..];
        match operation {
            'A' | 'a' => {
                let Ok(body) = serde_json::from_str::<HeartbeatBody>(argument) else {
                    continue;
                };
                let applied = planned::apply_announcement(
                    &store,
                    peer,
                    operation == 'A',
                    &body.maintenance,
                    now,
                )
                .await
                .expect("the database answers");
                for window in &applied.accepted {
                    assert!(window.starts_at < window.ends_at);
                    assert!(window.ends_at - window.starts_at <= 900);
                }
            }
            'P' => {
                let mut fields = argument.splitn(5, '|');
                let mut field = || fields.next().filter(|field| !field.is_empty());
                let plan = Plan {
                    monitors: field(),
                    host: field(),
                    starts_at: field(),
                    duration: field().unwrap_or(""),
                    reason: field().unwrap_or(""),
                };
                if let Ok(planned) = planned::plan(&store, &config, plan, now).await {
                    assert!(planned.window.ends_at > now);
                    let _ = planned::is_announced(&config, &planned.window);
                }
            }
            'C' => {
                let _ = planned::cancel(&store, argument.trim().parse().unwrap_or(1)).await;
            }
            'T' => now += argument.trim().parse::<u32>().map_or(60, i64::from),
            _ => continue,
        }

        // What every reader then sees.
        let announced = planned::announcements(&store, &config, now)
            .await
            .expect("the database answers");
        assert!(announced.len() <= planned::MAX_ANNOUNCED_WINDOWS);
        let schedule = planned::schedule_between(&store, &config, now - DAY, now + 8 * DAY).await;
        let at = DateTime::from_timestamp(now, 0).expect("a time");
        for window in schedule.windows() {
            assert!(window.start <= window.end);
            let _ = window.is_active(at);
            let _ = window.covered_peers(&config);
        }
        for id in ["web", "intra", "peer-x"] {
            let _ = schedule.in_maintenance(id, at);
        }
    }
}
