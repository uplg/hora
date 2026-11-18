//! Background maintenance: the periodic task that keeps the database within its
//! retention (see [`crate::db`] for the pruning itself).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use crate::config::Config;
use crate::db::{Store, prune, roll_up_recent};

const PRUNE_INTERVAL: Duration = Duration::from_hours(6);
/// How long after startup the first prune tick runs. Boot is already the
/// busiest write window (every monitor's first probe, the cert sweep, possibly
/// a schema migration); maintenance can wait until the burst has settled.
const PRUNE_STARTUP_DELAY: Duration = Duration::from_mins(5);
/// How often ended hours are rolled up between prunes. The status page reads
/// ended hours from the roll-ups and only the raw rows above the newest one,
/// so the frontier must trail the clock by about an hour, not a prune
/// interval. A tick with no newly ended hour is one indexed `MAX(hour)`.
const ROLLUP_INTERVAL: Duration = Duration::from_mins(5);
/// How soon a prune tick that ran out of its budget (a retention backlog)
/// comes back for the rest. The roll-ups get their turn in between.
const PRUNE_CATCH_UP_DELAY: Duration = Duration::from_mins(5);

/// Background task: periodically prune each monitor's history to its retention,
/// and drop any data left behind by monitors removed from the config (after a
/// grace period, see `db::retention::delete_orphans`); in between, roll the
/// ended hours up every few minutes (from startup on: after an upgrade or a
/// downtime the status page reads raw rows until the roll-ups catch up). A
/// shutdown signal lets it stop between ticks instead of being aborted.
#[must_use]
pub fn spawn_pruner(
    store: &Store,
    config: watch::Receiver<Arc<Config>>,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    let store = store.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + PRUNE_STARTUP_DELAY,
            PRUNE_INTERVAL,
        );
        let mut rollup = tokio::time::interval(ROLLUP_INTERVAL);
        rollup.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    let config = config.borrow().clone();
                    // Each sliced DELETE is its own statement, so dropping the
                    // prune between two of them on shutdown loses nothing.
                    let finished = tokio::select! {
                        result = prune(&store, &config) => result,
                        _ = shutdown.changed() => break,
                    };
                    match finished {
                        Ok(true) => {}
                        Ok(false) => {
                            tracing::info!(
                                "retention backlog: more history to prune, continuing in {} min",
                                PRUNE_CATCH_UP_DELAY.as_secs() / 60
                            );
                            ticker.reset_after(PRUNE_CATCH_UP_DELAY);
                        }
                        Err(err) => tracing::warn!("pruning failed: {err}"),
                    }
                }
                _ = rollup.tick() => {
                    let now = jiff::Timestamp::now().as_second();
                    if let Err(err) = roll_up_recent(&store, now).await {
                        tracing::warn!("hourly roll-up failed: {err}");
                    }
                }
                _ = shutdown.changed() => break,
            }
        }
    })
}
