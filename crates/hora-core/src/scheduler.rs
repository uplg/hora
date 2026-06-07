//! The scheduler: one independent probing loop per monitor, plus alert state.

use std::sync::Arc;

use hora_notify::Event;
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::config::{Config, Monitor};
use crate::notifications::Notifiers;
use crate::{db, probe};

/// Spawn the probing loop for a single monitor. Aborting the returned handle
/// stops the loop (used by the supervisor when a monitor is removed or changed).
#[must_use]
pub fn spawn_monitor(
    monitor: Monitor,
    config: watch::Receiver<Arc<Config>>,
    pool: SqlitePool,
    client: Client,
    notifier: Notifiers,
) -> JoinHandle<()> {
    tokio::spawn(run(monitor, config, pool, client, notifier))
}

async fn run(
    monitor: Monitor,
    config: watch::Receiver<Arc<Config>>,
    pool: SqlitePool,
    client: Client,
    notifier: Notifiers,
) {
    // Fixed cadence: the tick interval does not drift by the probe duration.
    let mut ticker = tokio::time::interval(monitor.interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut consecutive_failures: u32 = 0;
    let mut alerted_down = false;

    loop {
        ticker.tick().await;
        let outcome = probe::run(&client, &monitor).await;

        if let Err(err) =
            db::insert_check(&pool, &monitor.id, outcome.status_value(), &outcome).await
        {
            error!(monitor = %monitor.id, "failed to record check: {err:#}");
        }

        // Read live so a reloaded threshold applies without restarting the loop.
        let threshold = config.borrow().alerts.fail_threshold.max(1);

        if outcome.up {
            if alerted_down {
                info!(monitor = %monitor.id, "recovered");
                notifier
                    .load_full()
                    .dispatch(Event::Recovered {
                        monitor: &monitor.name,
                    })
                    .await;
                alerted_down = false;
            }
            consecutive_failures = 0;
        } else {
            consecutive_failures = consecutive_failures.saturating_add(1);
            if consecutive_failures >= threshold && !alerted_down {
                error!(monitor = %monitor.id, failures = consecutive_failures, "confirmed down");
                notifier
                    .load_full()
                    .dispatch(Event::Down {
                        monitor: &monitor.name,
                        error: outcome.error.as_deref(),
                    })
                    .await;
                alerted_down = true;
            }
        }
    }
}
