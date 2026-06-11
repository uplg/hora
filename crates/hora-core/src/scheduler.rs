//! The scheduler: one independent probing loop per monitor, plus alert state.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hora_notify::Event;
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::coalesce::{AlertMsg, DownAlert};
use crate::config::{Config, Kind, Monitor};
use crate::notifications::Notifiers;
use crate::probe::Outcome;
use crate::topology;
use crate::{db, probe, slo};

/// The level a monitor was most recently alerted at, so we never re-alert the
/// same state and can detect transitions (escalation, recovery).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AlertLevel {
    Healthy,
    Degraded,
    Down,
}

/// Edge-triggered burn-rate alert state: each severity fires once when its
/// window pair first exceeds the threshold and re-arms when the long window
/// cools back down.
#[derive(Default)]
struct BurnAlerts {
    fast: bool,
    slow: bool,
}

impl BurnAlerts {
    fn any(&self) -> bool {
        self.fast || self.slow
    }
}

/// Everything a monitor loop borrows from the application: storage, its HTTP
/// client, the notifier set (degraded/burn alerts), the coalescer inbox
/// (down/recovered alerts), and the liveness beacon.
pub struct MonitorDeps {
    pub pool: SqlitePool,
    pub client: Client,
    pub notifier: Notifiers,
    pub alerts: mpsc::UnboundedSender<AlertMsg>,
    pub last_tick: Arc<AtomicU64>,
}

/// Spawn the probing loop for a single monitor. Aborting the returned handle
/// stops the loop (used by the supervisor when a monitor is removed or changed);
/// a shutdown signal lets it finish the current tick and exit cleanly.
#[must_use]
pub fn spawn_monitor(
    monitor: Monitor,
    config: watch::Receiver<Arc<Config>>,
    deps: MonitorDeps,
    shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(run(monitor, config, deps, shutdown))
}

async fn run(
    monitor: Monitor,
    config: watch::Receiver<Arc<Config>>,
    deps: MonitorDeps,
    mut shutdown: watch::Receiver<bool>,
) {
    let MonitorDeps {
        pool,
        client,
        notifier,
        alerts,
        last_tick,
    } = deps;
    // Fixed cadence: the tick interval does not drift by the probe duration.
    let mut ticker = tokio::time::interval(monitor.interval());
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut consecutive_down: u32 = 0;
    let mut consecutive_degraded: u32 = 0;
    let mut alerted = AlertLevel::Healthy;
    let mut burn = BurnAlerts::default();
    // Re-attach to an incident left open by a previous run (restart mid-outage,
    // monitor edited live): it gets closed on the first healthy tick instead of
    // staying open forever, and a still-down monitor keeps its original start.
    let mut open_incident: Option<i64> = db::find_open_incident(&pool, &monitor.id)
        .await
        .ok()
        .flatten();

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

        // No outcome means a push monitor without a heartbeat yet: status
        // stays unknown, nothing to react to this tick.
        let Some(outcome) = tick_outcome(&client, &pool, &monitor).await else {
            continue;
        };

        // Every failure that survived its retries is worth a log line (the
        // page only shows "degraded" until the threshold confirms, so this is
        // where a blip's reason is visible). Quiet once the outage is
        // confirmed - "confirmed down" already said it.
        if !outcome.up && alerted != AlertLevel::Down {
            warn!(
                monitor = %monitor.id,
                error = outcome.error.as_deref().unwrap_or("unknown"),
                "check failed"
            );
        }

        let (muted, threshold, alert_on_degraded) = alert_settings(&config, &monitor.id);
        // Ad-hoc silences (`hora silence`, POST /api/silence) mute exactly like
        // a maintenance window, read fresh each tick so they apply immediately.
        let muted = muted || silenced(&pool, &monitor.id).await;
        // An incident is bound to confirmed-down alerts; any up tick (healthy
        // or merely degraded) ends it, whatever the alert state machine does -
        // including an incident inherited from a previous run, and even during
        // maintenance (the record should reflect the real outage span).
        if outcome.up {
            close_open_incident(&pool, &monitor.id, &mut open_incident).await;
        }

        if muted {
            // Checks are still recorded above; only alert transitions are skipped.
            continue;
        }

        // Burn-rate alerting is orthogonal to the up/down state machine: a
        // monitor flapping every few minutes never confirms down, yet burns
        // its budget - exactly what this catches. Healthy, armed monitors
        // cost nothing; once tripped, evaluation continues on up ticks so the
        // alert can re-arm when the windows cool.
        if let Some(slo_bp) = monitor.slo_uptime
            && (!outcome.up || burn.any())
        {
            evaluate_burn(&pool, &notifier, &monitor, slo_bp, &mut burn).await;
        }

        if !outcome.up {
            // Down resets degraded tracking; alert once `threshold` consecutive
            // failures confirm it (escalating from healthy or degraded).
            consecutive_down = consecutive_down.saturating_add(1);
            consecutive_degraded = 0;
            if consecutive_down >= threshold && alerted != AlertLevel::Down {
                error!(monitor = %monitor.id, failures = consecutive_down, "confirmed down");
                let snapshot = config.borrow().clone();
                confirm_down(
                    &snapshot,
                    &pool,
                    &alerts,
                    &monitor,
                    &outcome,
                    threshold,
                    &mut open_incident,
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
                // Through the coalescer too: the recovery of a folded down
                // alert stays silent (nothing was announced going down).
                let _ = alerts.send(AlertMsg::Recovered {
                    id: monitor.id.clone(),
                    name: monitor.name.clone(),
                    notify: monitor.notify.clone(),
                });
                alerted = AlertLevel::Healthy;
            }
        }
    }
}

/// A monitor just confirmed down: resolve the topology context, open (or
/// resume) the incident record, and hand the alert to the coalescer, which
/// may fold it into its root cause's single notification.
async fn confirm_down(
    config: &Config,
    pool: &SqlitePool,
    alerts: &mpsc::UnboundedSender<AlertMsg>,
    monitor: &Monitor,
    outcome: &Outcome,
    threshold: u32,
    open_incident: &mut Option<i64>,
) {
    let (cause, impacted_names) = down_context(config, pool, monitor, threshold).await;

    // Unless one is already open (resumed from a previous run mid-outage).
    if open_incident.is_none() {
        *open_incident = open_incident_record(
            pool,
            monitor,
            outcome,
            cause.as_ref().map(|(_, name)| name.as_str()),
            &impacted_names,
        )
        .await;
    }

    // The coalescer groups on the *configured* upstreams: in a cascade this
    // monitor often confirms a tick before its upstream is derivably down,
    // so the derived cause alone would lose the race.
    let _ = alerts.send(AlertMsg::Down(DownAlert {
        id: monitor.id.clone(),
        name: monitor.name.clone(),
        error: outcome.error.clone(),
        upstreams: topology::transitive_upstreams(&config.monitors, &monitor.id)
            .into_iter()
            .map(str::to_owned)
            .collect(),
        cause_name: cause.map(|(_, name)| name),
        impacted: impacted_names,
        notify: monitor.notify.clone(),
    }));
}

/// Live alert settings for this tick, read fresh so a maintenance window or a
/// reloaded threshold applies immediately: (muted, fail threshold, degraded
/// alerts enabled).
fn alert_settings(config: &watch::Receiver<Arc<Config>>, monitor_id: &str) -> (bool, u32, bool) {
    let snapshot = config.borrow();
    (
        snapshot.in_maintenance(monitor_id, chrono::Utc::now()),
        snapshot.alerts.fail_threshold.max(1),
        snapshot.alerts.alert_on_degraded,
    )
}

/// Whether an ad-hoc silence covers this monitor right now. A read error fails
/// open (logged, not silenced): a database hiccup must never mute an alert.
async fn silenced(pool: &SqlitePool, monitor_id: &str) -> bool {
    match db::is_silenced(pool, monitor_id, chrono::Utc::now().timestamp()).await {
        Ok(silenced) => silenced,
        Err(err) => {
            error!(monitor = %monitor_id, "failed to read silences: {err:#}");
            false
        }
    }
}

/// Close the open incident, if any. Failures are logged, never fatal.
async fn close_open_incident(pool: &SqlitePool, monitor_id: &str, open_incident: &mut Option<i64>) {
    if let Some(incident_id) = open_incident.take()
        && let Err(err) = db::update_incident_end(pool, incident_id).await
    {
        error!(monitor = %monitor_id, "failed to close incident: {err:#}");
    }
}

/// One tick's outcome: probe active monitors (recording the check), evaluate
/// stored heartbeats for push monitors. `None` means nothing to react to yet.
async fn tick_outcome(client: &Client, pool: &SqlitePool, monitor: &Monitor) -> Option<Outcome> {
    if monitor.kind == Kind::Push {
        return heartbeat_outcome(pool, monitor).await;
    }
    let outcome = probe::run(client, monitor).await;
    if let Err(err) = db::insert_check(pool, &monitor.id, outcome.status_value(), &outcome).await {
        error!(monitor = %monitor.id, "failed to record check: {err:#}");
    }
    Some(outcome)
}

/// Open an incident record for a confirmed-down monitor; `None` (logged) when
/// the insert fails, so a database hiccup never blocks the alert itself.
async fn open_incident_record(
    pool: &SqlitePool,
    monitor: &Monitor,
    outcome: &Outcome,
    cause: Option<&str>,
    impacted: &[String],
) -> Option<i64> {
    match db::insert_incident_start(pool, &monitor.id, outcome.error.as_deref(), cause, impacted)
        .await
    {
        Ok(incident_id) => Some(incident_id),
        Err(err) => {
            error!(monitor = %monitor.id, "failed to record incident: {err:#}");
            None
        }
    }
}

/// Multi-window burn-rate evaluation (Google SRE): page when ~2% of the error
/// budget burns within an hour (confirmed by the last 5 minutes, so a stale
/// spike never pages), warn when ~5% burns within six hours (confirmed by 30
/// minutes). A fast alert subsumes the slow one for the same episode.
async fn evaluate_burn(
    pool: &SqlitePool,
    notifier: &Notifiers,
    monitor: &Monitor,
    slo_bp: u32,
    state: &mut BurnAlerts,
) {
    let window_days = monitor.slo_window_days();
    let now = chrono::Utc::now().timestamp();

    let burn_1h = burn_window(pool, &monitor.id, now - 3600, slo_bp).await;
    let fast_threshold = slo::fast_burn_threshold_x10(window_days);
    let fast_now = burn_1h >= fast_threshold
        && burn_window(pool, &monitor.id, now - 300, slo_bp).await >= fast_threshold;
    if fast_now {
        if !state.fast {
            state.fast = true;
            state.slow = true;
            fire_burn_alert(pool, notifier, monitor, slo_bp, burn_1h, "1h").await;
        }
        return;
    }
    if burn_1h < fast_threshold {
        state.fast = false;
    }

    let burn_6h = burn_window(pool, &monitor.id, now - 6 * 3600, slo_bp).await;
    let slow_threshold = slo::slow_burn_threshold_x10(window_days);
    let slow_now = burn_6h >= slow_threshold
        && burn_window(pool, &monitor.id, now - 1800, slo_bp).await >= slow_threshold;
    if slow_now && !state.slow {
        state.slow = true;
        fire_burn_alert(pool, notifier, monitor, slo_bp, burn_6h, "6h").await;
    } else if burn_6h < slow_threshold {
        state.slow = false;
    }
}

/// The burn rate over one lookback window, in tenths. A read error counts as
/// zero: never alert (or re-arm) off unreadable data.
async fn burn_window(pool: &SqlitePool, id: &str, since: i64, slo_bp: u32) -> i64 {
    match db::availability(pool, id, since).await {
        Ok((available, total)) => slo::burn_rate_x10(available, total, slo_bp),
        Err(err) => {
            error!(monitor = %id, "failed to read availability: {err:#}");
            0
        }
    }
}

/// Dispatch a [`Event::BudgetBurn`], with the exhaustion estimate computed
/// from the full SLO window. The window is assumed fully covered: for a
/// monitor younger than the window this overstates consumption, which only
/// makes the estimate conservative.
async fn fire_burn_alert(
    pool: &SqlitePool,
    notifier: &Notifiers,
    monitor: &Monitor,
    slo_bp: u32,
    burn_x10: i64,
    window: &'static str,
) {
    let window_days = monitor.slo_window_days();
    let now = chrono::Utc::now().timestamp();
    let since = now - i64::from(window_days) * crate::SECONDS_PER_DAY;
    // No estimate beats a wrong one: unreadable history drops the ETA only.
    let exhausted_in_secs = match db::availability(pool, &monitor.id, since).await {
        Ok((available, total)) => {
            let covered = i64::from(window_days) * 24 * 60;
            let remaining = slo::budget_minutes(window_days, slo_bp)
                - slo::consumed_minutes(available, total, covered);
            slo::exhausted_in_secs(remaining, burn_x10, slo_bp)
        }
        Err(_) => None,
    };
    warn!(
        monitor = %monitor.id,
        burn_x10, window, "error budget burning"
    );
    notifier
        .load_full()
        .dispatch(
            Event::BudgetBurn {
                monitor: &monitor.name,
                burn_rate_x10: burn_x10,
                window,
                exhausted_in_secs,
            },
            monitor.notify.as_deref(),
        )
        .await;
}

/// Evaluate a push monitor from its stored heartbeats: down (and record it) when
/// one is overdue, up otherwise. Without a `schedule`, a heartbeat is overdue
/// once it is older than the interval; with one, only once a scheduled run has
/// missed its grace window. `None` means no heartbeat yet (or a read error) -
/// the loop skips this tick, leaving the status unknown.
async fn heartbeat_outcome(pool: &SqlitePool, monitor: &Monitor) -> Option<Outcome> {
    let Some(schedule) = &monitor.schedule else {
        return heartbeat_outcome_for(pool, &monitor.id, monitor.interval_secs).await;
    };
    // Validated at config load; a parse failure here is defensive only.
    let Ok(cron) = schedule.parse::<croner::Cron>() else {
        error!(monitor = %monitor.id, "invalid cron schedule {schedule:?}");
        return None;
    };

    let last = last_heartbeat(pool, &monitor.id).await?;
    let now = chrono::Utc::now().timestamp();
    match cron_missed(&cron, last, monitor.push_grace_secs(), now) {
        Some(due) => {
            let due_label = chrono::DateTime::from_timestamp(due, 0)
                .map_or_else(|| due.to_string(), |dt| dt.format("%H:%M UTC").to_string());
            let reason = format!(
                "missed scheduled heartbeat (was due {due_label} + {}m grace)",
                monitor.push_grace_secs() / 60
            );
            Some(record_missed_heartbeat(pool, &monitor.id, reason).await)
        }
        None => Some(up_heartbeat()),
    }
}

/// With a cron schedule, the heartbeat is overdue once `now` passes the first
/// scheduled run *after* the last heartbeat. Returns that due time when missed
/// (for the alert message), `None` while on time. A schedule with no computable
/// next occurrence never alerts rather than alerting forever.
fn cron_missed(cron: &croner::Cron, last: i64, grace_secs: u64, now: i64) -> Option<i64> {
    let last_at = chrono::DateTime::from_timestamp(last, 0)?;
    let due = cron.find_next_occurrence(&last_at, false).ok()?.timestamp();
    let deadline = due.saturating_add(i64::try_from(grace_secs).unwrap_or(i64::MAX));
    (now > deadline).then_some(due)
}

/// Compute topology annotation for a down alert: the nearest down upstream
/// (`cause`, as config id + display name) if any, or the list of impacted
/// dependents (`impacted`) if this monitor is a root cause. Returns
/// `(None, vec![])` when the monitor has no topology configured.
async fn down_context(
    config: &Config,
    pool: &SqlitePool,
    monitor: &Monitor,
    threshold: u32,
) -> (Option<(String, String)>, Vec<String>) {
    let threshold_i64 = i64::from(threshold.max(1));

    let upstreams = topology::transitive_upstreams(&config.monitors, &monitor.id);
    for up_id in &upstreams {
        let Ok(recent) = db::recent_checks(pool, up_id, threshold_i64).await else {
            continue;
        };
        if db::derive_status(&recent, threshold_i64) == "down"
            && let Some(name) = topology::monitor_name(&config.monitors, up_id)
        {
            return (Some(((*up_id).to_owned(), name.to_owned())), Vec::new());
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
    let last = last_heartbeat(pool, id).await?;
    let now = chrono::Utc::now().timestamp();
    let max_gap = i64::try_from(interval_secs).unwrap_or(i64::MAX);
    if now - last > max_gap {
        Some(record_missed_heartbeat(pool, id, "missing heartbeat".to_owned()).await)
    } else {
        Some(up_heartbeat())
    }
}

/// The last *positive* heartbeat time, or `None` for never/unreadable (logged).
async fn last_heartbeat(pool: &SqlitePool, id: &str) -> Option<i64> {
    match db::last_heartbeat_time(pool, id).await {
        Ok(last) => last,
        Err(err) => {
            error!(monitor = %id, "failed to read last heartbeat: {err:#}");
            None
        }
    }
}

/// Record a missed heartbeat as a down check so the page and alerting react.
/// The up-checks themselves are written by the push endpoint; staleness stays
/// measured from the last positive heartbeat, so this recorded miss does not
/// mask the ongoing outage.
async fn record_missed_heartbeat(pool: &SqlitePool, id: &str, reason: String) -> Outcome {
    let outcome = Outcome::down(reason);
    if let Err(err) = db::insert_check(pool, id, outcome.status_value(), &outcome).await {
        error!(monitor = %id, "failed to record heartbeat miss: {err:#}");
    }
    outcome
}

/// A healthy heartbeat outcome - the up-check is already recorded by the push
/// endpoint, so nothing is written here.
fn up_heartbeat() -> Outcome {
    Outcome {
        up: true,
        degraded: false,
        latency_ms: None,
        status_code: None,
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cron_missed_only_after_due_plus_grace() {
        // Nightly at 03:00 UTC; epoch day boundaries make the math readable.
        let cron: croner::Cron = "0 3 * * *".parse().expect("valid cron");
        let day = 86_400;
        let last = day + 3 * 3600 + 120; // pinged 03:02, day 2
        let due = 2 * day + 3 * 3600; // next run: 03:00, day 3
        let grace: u64 = 1800;
        let deadline = due + 1800; // due + grace, as a timestamp

        // Before the next run, and within the grace window: on time.
        assert_eq!(cron_missed(&cron, last, grace, due - 3600), None);
        assert_eq!(cron_missed(&cron, last, grace, deadline), None);
        // Past due + grace: missed, reporting the due time.
        assert_eq!(cron_missed(&cron, last, grace, deadline + 1), Some(due));
        // A heartbeat long dead stays missed until a fresh ping moves `last`.
        assert_eq!(cron_missed(&cron, last, grace, due + 30 * day), Some(due));
    }
}
