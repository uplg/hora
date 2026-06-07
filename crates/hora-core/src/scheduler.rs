//! The scheduler: one independent probing loop per monitor, plus alert state.

use std::sync::Arc;

use hora_notify::Event;
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info};

use crate::config::{Config, Kind, Monitor};
use crate::notifications::Notifiers;
use crate::probe::Outcome;
use crate::{db, probe};

/// Spawn the probing loop for a single monitor. Aborting the returned handle
/// stops the loop (used by the supervisor when a monitor is removed or changed);
/// a shutdown signal lets it finish the current tick and exit cleanly.
#[must_use]
pub fn spawn_monitor(
    monitor: Monitor,
    config: watch::Receiver<Arc<Config>>,
    pool: SqlitePool,
    client: Client,
    notifier: Notifiers,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(run(monitor, config, pool, client, notifier, shutdown))
}

async fn run(
    monitor: Monitor,
    config: watch::Receiver<Arc<Config>>,
    pool: SqlitePool,
    client: Client,
    notifier: Notifiers,
    mut shutdown: watch::Receiver<bool>,
) {
    // Fixed cadence: the tick interval does not drift by the probe duration.
    let mut ticker = tokio::time::interval(monitor.interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut consecutive_failures: u32 = 0;
    let mut alerted_down = false;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => break,
        }
        let outcome = if monitor.kind == Kind::Push {
            match heartbeat_outcome(&pool, &monitor).await {
                Some(outcome) => outcome,
                // No heartbeat recorded yet: stay unknown, nothing to react to.
                None => continue,
            }
        } else {
            let outcome = probe::run(&client, &monitor).await;
            if let Err(err) =
                db::insert_check(&pool, &monitor.id, outcome.status_value(), &outcome).await
            {
                error!(monitor = %monitor.id, "failed to record check: {err:#}");
            }
            outcome
        };

        // Read live: a maintenance window mutes alerts; a reloaded threshold
        // applies immediately.
        let now = chrono::Utc::now();
        let (muted, threshold) = {
            let snapshot = config.borrow();
            (
                snapshot.in_maintenance(&monitor.id, now),
                snapshot.alerts.fail_threshold.max(1),
            )
        };
        if muted {
            // Checks are still recorded above; only alert transitions are skipped.
            continue;
        }

        if outcome.up {
            if alerted_down {
                info!(monitor = %monitor.id, "recovered");
                notifier
                    .load_full()
                    .dispatch(
                        Event::Recovered {
                            monitor: &monitor.name,
                        },
                        monitor.notify.as_deref(),
                    )
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
                    .dispatch(
                        Event::Down {
                            monitor: &monitor.name,
                            error: outcome.error.as_deref(),
                        },
                        monitor.notify.as_deref(),
                    )
                    .await;
                alerted_down = true;
            }
        }
    }
}

/// Evaluate a push monitor from its stored heartbeats: down (and record it) when
/// none arrived within the interval, up when one did. `None` means no heartbeat
/// yet (or a read error) - the loop skips this tick, leaving the status unknown.
async fn heartbeat_outcome(pool: &SqlitePool, monitor: &Monitor) -> Option<Outcome> {
    let last = match db::last_check_time(pool, &monitor.id).await {
        Ok(Some(last)) => last,
        Ok(None) => return None,
        Err(err) => {
            error!(monitor = %monitor.id, "failed to read last heartbeat: {err:#}");
            return None;
        }
    };

    let now = chrono::Utc::now().timestamp();
    let max_gap = i64::try_from(monitor.interval_secs).unwrap_or(i64::MAX);
    if now - last > max_gap {
        // Heartbeat missed: record the down so the page and alerting react. The
        // up-checks themselves are written by the push endpoint.
        let outcome = Outcome::down("missing heartbeat".to_owned());
        if let Err(err) =
            db::insert_check(pool, &monitor.id, outcome.status_value(), &outcome).await
        {
            error!(monitor = %monitor.id, "failed to record heartbeat miss: {err:#}");
        }
        Some(outcome)
    } else {
        // Recent heartbeat - up, already recorded by the endpoint.
        Some(Outcome {
            up: true,
            degraded: false,
            latency_ms: None,
            status_code: None,
            error: None,
        })
    }
}
