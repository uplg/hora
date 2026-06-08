//! The scheduler: one independent probing loop per monitor, plus alert state.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hora_notify::Event;
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::config::{Config, Kind, Monitor};
use crate::notifications::Notifiers;
use crate::probe::Outcome;
use crate::topology;
use crate::{db, probe};

/// The level a monitor was most recently alerted at, so we never re-alert the
/// same state and can detect transitions (escalation, recovery).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AlertLevel {
    Healthy,
    Degraded,
    Down,
}

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
    last_tick: Arc<AtomicU64>,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(run(
        monitor, config, pool, client, notifier, last_tick, shutdown,
    ))
}

async fn run(
    monitor: Monitor,
    config: watch::Receiver<Arc<Config>>,
    pool: SqlitePool,
    client: Client,
    notifier: Notifiers,
    last_tick: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
) {
    // Fixed cadence: the tick interval does not drift by the probe duration.
    let mut ticker = tokio::time::interval(monitor.interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut consecutive_down: u32 = 0;
    let mut consecutive_degraded: u32 = 0;
    let mut alerted = AlertLevel::Healthy;

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = shutdown.changed() => break,
        }
        // Liveness beacon: record that this scheduler loop iterated, independent of
        // the probe outcome (a hung probe never reaches here, so the timestamp goes
        // stale - exactly what the dead-man heartbeat and /healthz want to detect).
        // `fetch_max` across all monitors tracks the most recent tick of any of them.
        last_tick.fetch_max(
            u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0),
            Ordering::Relaxed,
        );

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
        let (muted, threshold, alert_on_degraded) = {
            let snapshot = config.borrow();
            (
                snapshot.in_maintenance(&monitor.id, now),
                snapshot.alerts.fail_threshold.max(1),
                snapshot.alerts.alert_on_degraded,
            )
        };
        if muted {
            // Checks are still recorded above; only alert transitions are skipped.
            continue;
        }

        if !outcome.up {
            // Down resets degraded tracking; alert once `threshold` consecutive
            // failures confirm it (escalating from healthy or degraded).
            consecutive_down = consecutive_down.saturating_add(1);
            consecutive_degraded = 0;
            if consecutive_down >= threshold && alerted != AlertLevel::Down {
                error!(monitor = %monitor.id, failures = consecutive_down, "confirmed down");
                let snapshot = config.borrow().clone();
                let (cause, impacted_names) =
                    down_context(&snapshot, &pool, &monitor, threshold).await;
                let impacted_refs: Vec<&str> = impacted_names.iter().map(String::as_str).collect();
                notifier
                    .load_full()
                    .dispatch(
                        Event::Down {
                            monitor: &monitor.name,
                            error: outcome.error.as_deref(),
                            cause: cause.as_deref(),
                            impacted: &impacted_refs,
                        },
                        monitor.notify.as_deref(),
                    )
                    .await;
                alerted = AlertLevel::Down;
            }
        } else if outcome.degraded && alert_on_degraded {
            // Up but slow: same anti-flap threshold as down, separate state.
            consecutive_degraded = consecutive_degraded.saturating_add(1);
            consecutive_down = 0;
            if consecutive_degraded >= threshold && alerted != AlertLevel::Degraded {
                warn!(monitor = %monitor.id, latency_ms = ?outcome.latency_ms, "degraded");
                notifier
                    .load_full()
                    .dispatch(
                        Event::Degraded {
                            monitor: &monitor.name,
                            latency_ms: outcome.latency_ms,
                        },
                        monitor.notify.as_deref(),
                    )
                    .await;
                alerted = AlertLevel::Degraded;
            }
        } else {
            // Fully healthy (or degraded with the option off, treated as up).
            consecutive_down = 0;
            consecutive_degraded = 0;
            if alerted != AlertLevel::Healthy {
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
                alerted = AlertLevel::Healthy;
            }
        }
    }
}

/// Evaluate a push monitor from its stored heartbeats: down (and record it) when
/// none arrived within the interval, up when one did. `None` means no heartbeat
/// yet (or a read error) - the loop skips this tick, leaving the status unknown.
async fn heartbeat_outcome(pool: &SqlitePool, monitor: &Monitor) -> Option<Outcome> {
    heartbeat_outcome_for(pool, &monitor.id, monitor.interval_secs).await
}

/// Compute topology annotation for a down alert: the nearest down upstream
/// (`cause`) if any, or the list of impacted dependents (`impacted`) if this
/// monitor is a root cause. Returns `(None, vec![])` when the monitor has no
/// topology configured.
async fn down_context(
    config: &Config,
    pool: &SqlitePool,
    monitor: &Monitor,
    threshold: u32,
) -> (Option<String>, Vec<String>) {
    let threshold_i64 = i64::from(threshold.max(1));

    let upstreams = topology::transitive_upstreams(&config.monitors, &monitor.id);
    for up_id in &upstreams {
        let Ok(recent) = db::recent_checks(pool, up_id, threshold_i64).await else {
            continue;
        };
        if db::derive_status(&recent, threshold_i64) == "down"
            && let Some(name) = topology::monitor_name(&config.monitors, up_id)
        {
            return (Some(name.to_owned()), Vec::new());
        }
    }

    let dependents = topology::transitive_dependents(&config.monitors, &monitor.id);
    let impacted: Vec<String> = dependents
        .iter()
        .filter_map(|dep_id| topology::monitor_name(&config.monitors, dep_id).map(String::from))
        .collect();

    (None, impacted)
}

/// Evaluate a heartbeat from the stored pings for `id` against `interval_secs`:
/// down (and record it) when none arrived within the interval, up when one did.
/// `None` means no heartbeat yet (or a read error), leaving the status unknown -
/// which is also the startup grace, since a peer that has never pinged is unknown,
/// not down. Shared by push monitors and peer watches.
///
/// Staleness is measured from the last *positive* heartbeat, not the last check:
/// the misses recorded below carry a fresh timestamp, so measuring from the latter
/// would reset the clock each tick and the monitor would flap instead of
/// confirming down (see [`db::last_heartbeat_time`]).
pub(crate) async fn heartbeat_outcome_for(
    pool: &SqlitePool,
    id: &str,
    interval_secs: u64,
) -> Option<Outcome> {
    let last = match db::last_heartbeat_time(pool, id).await {
        Ok(Some(last)) => last,
        Ok(None) => return None,
        Err(err) => {
            error!(monitor = %id, "failed to read last heartbeat: {err:#}");
            return None;
        }
    };

    let now = chrono::Utc::now().timestamp();
    let max_gap = i64::try_from(interval_secs).unwrap_or(i64::MAX);
    if now - last > max_gap {
        // Heartbeat missed: record the down so the page and alerting react. The
        // up-checks themselves are written by the push endpoint. Staleness is
        // tracked from the last positive heartbeat above, so this recorded miss
        // does not mask the ongoing outage.
        let outcome = Outcome::down("missing heartbeat".to_owned());
        if let Err(err) = db::insert_check(pool, id, outcome.status_value(), &outcome).await {
            error!(monitor = %id, "failed to record heartbeat miss: {err:#}");
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
