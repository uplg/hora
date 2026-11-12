//! The scheduler: one independent probing loop per monitor, plus alert state.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::db::Store;
use hora_notify::Event;
use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::coalesce::{AlertMsg, DownAlert};
use crate::config::{Config, Monitor, MonitorKind, NetworkProbe};
use crate::heartbeat::{HeartbeatWatch, heartbeat_outcome_for};
use crate::notifications::Notifiers;
use crate::probe::Outcome;
use crate::status::MonitorState;
use crate::topology;
use crate::{db, probe, slo};

/// The level a monitor (or watched peer) was most recently alerted at, so we
/// never re-alert the same state and can detect transitions (escalation,
/// recovery).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum AlertLevel {
    #[default]
    Healthy,
    Degraded,
    Down,
    /// Peer watches only: down locally but a witness still sees the peer, a
    /// partition reported once as a link-degraded event, not an outage.
    Partition,
    /// Peer watches only: down locally and no witness reachable, so probably
    /// *this* node is isolated - nothing was announced.
    Isolated,
}

/// Edge-triggered burn-rate alert state: each severity fires once when its
/// window pair first exceeds the threshold and re-arms when the long window
/// cools back down.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct BurnAlerts {
    fast: bool,
    slow: bool,
}

impl BurnAlerts {
    fn any(self) -> bool {
        self.fast || self.slow
    }
}

/// The anti-flap alert state machine, shared by monitor loops and peer
/// watches: consecutive-failure counters against the threshold, plus the level
/// last announced. Pure (no I/O), so the transitions are unit-testable; the
/// callers perform the side effect and then record the new [`AlertLevel`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AlertState {
    pub(crate) level: AlertLevel,
    consecutive_down: u32,
    consecutive_degraded: u32,
    burn: BurnAlerts,
    /// The current down was announced as local-only (every peer that
    /// answered saw the target up): when the peers were last asked, unix
    /// seconds. They are asked again every [`LOCAL_ONLY_REASK_SECS`] while
    /// the down lasts, until they confirm it.
    local_only_asked: Option<i64>,
    /// The announced outage ended (an up tick closed its incident) while the
    /// level stayed [`AlertLevel::Down`]: the monitor was muted, or only
    /// degraded and not yet confirmed so. A later confirmed down is then a
    /// new outage and is announced, instead of passing for the old one.
    rearmed: bool,
    /// The current down was handed to the channels at this time (unix
    /// seconds) and has not been seen delivered since: its incident is
    /// looked at again every [`REANNOUNCE_SECS`], and the down sent again
    /// while no channel has taken it.
    announce_checked: Option<i64>,
}

/// How often the peers are asked again about a local-only down.
const LOCAL_ONLY_REASK_SECS: i64 = 300;

/// How long a confirmed down may stay unannounced - every channel failed,
/// ran into its deadline, or had its queue full - before it is sent again.
/// Long next to any delivery, so a down still waiting its turn on a slow
/// channel is not doubled.
const REANNOUNCE_SECS: i64 = 600;

impl AlertState {
    /// The state of a monitor found mid-outage at startup (an incident is
    /// still open): its down was already announced by the previous run, so a
    /// still-down monitor must not alert again, and its recovery must be.
    fn resumed_down(threshold: u32) -> Self {
        Self {
            level: AlertLevel::Down,
            consecutive_down: threshold,
            ..Self::default()
        }
    }

    /// Count a down tick. `true` when this tick confirms the outage: the
    /// threshold is reached and the down has not been announced yet.
    pub(crate) fn observe_down(&mut self, threshold: u32) -> bool {
        self.consecutive_down = self.consecutive_down.saturating_add(1);
        self.consecutive_degraded = 0;
        let confirmed =
            self.consecutive_down >= threshold && (self.level != AlertLevel::Down || self.rearmed);
        if confirmed {
            self.rearmed = false;
        }
        confirmed
    }

    /// An up tick ended the outage's incident, whatever the alert transitions
    /// below do with it (they are skipped while muted). The failure streak is
    /// over, and a down still standing as announced is re-armed: the next
    /// confirmed down opens a new incident and alerts. The recovery itself
    /// stays owed, through [`Self::observe_up`].
    fn outage_ended(&mut self) {
        self.consecutive_down = 0;
        self.local_only_asked = None;
        self.announce_checked = None;
        self.rearmed = self.level == AlertLevel::Down;
    }

    /// Count an up-but-slow tick (only when degraded alerts are on). `true`
    /// when this tick confirms the degradation, same threshold as down.
    fn observe_degraded(&mut self, threshold: u32) -> bool {
        self.consecutive_degraded = self.consecutive_degraded.saturating_add(1);
        self.consecutive_down = 0;
        self.consecutive_degraded >= threshold && self.level != AlertLevel::Degraded
    }

    /// Count a healthy tick. `true` when something was alerted before (the
    /// caller decides what a recovery from [`Self::level`] announces, then
    /// resets it to [`AlertLevel::Healthy`]).
    pub(crate) fn observe_up(&mut self) -> bool {
        self.consecutive_down = 0;
        self.consecutive_degraded = 0;
        self.local_only_asked = None;
        self.rearmed = false;
        self.announce_checked = None;
        self.level != AlertLevel::Healthy
    }

    /// Whether the down's delivery is due another look.
    fn reannounce_due(&self, now: i64) -> bool {
        self.level == AlertLevel::Down
            && self
                .announce_checked
                .is_some_and(|checked| now - checked >= REANNOUNCE_SECS)
    }

    /// Whether a local-only down is due for another round of peer questions.
    fn reask_due(&self, now: i64) -> bool {
        self.level == AlertLevel::Down
            && self
                .local_only_asked
                .is_some_and(|asked| now - asked >= LOCAL_ONLY_REASK_SECS)
    }
}

/// One monitor's (or peer's) alert state, owned by the supervisor and handed to
/// each task it spawns for that id. A task restarted by a config edit, or
/// after it stopped on its own, picks up where the previous one stopped: a
/// still-down monitor is not announced twice, and its recovery is not lost.
/// Empty until the first task seeds it (from the open incident).
///
/// It lives in memory, so it does not outlive the process: a panic in a
/// release build (`panic = "abort"`) ends the daemon, and the task spawned
/// after the restart seeds a new cell from the database - the open incident,
/// and its `notified` mark telling whether the down alert had gone out.
///
/// The task writes it back synchronously right after each transition's side
/// effect, so an abort (always at an `.await`) can at worst repeat an alert
/// whose delivery it interrupted, never lose one.
#[derive(Clone, Debug, Default)]
pub struct AlertCell(Arc<Mutex<Option<AlertState>>>);

impl AlertCell {
    pub(crate) fn get(&self) -> Option<AlertState> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn set(&self, state: AlertState) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(state);
    }
}

/// Everything a monitor loop borrows from the application: storage, its HTTP
/// client, the notifier set (degraded/burn alerts), the coalescer inbox
/// (down/recovered alerts), the liveness beacon, and its alert state.
pub struct MonitorDeps {
    pub store: Store,
    pub client: Client,
    /// The shared plain client for multi-vantage confirmation requests to the
    /// peers. Distinct from `client` on purpose: that one may be bound to the
    /// monitor's proxy, and a peer request must never ride a monitor's proxy.
    pub confirm_client: Client,
    pub notifier: Notifiers,
    pub alerts: mpsc::UnboundedSender<AlertMsg>,
    pub last_tick: Arc<AtomicU64>,
    /// Survives this task: see [`AlertCell`].
    pub alert_state: AlertCell,
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

/// The phase shift for a monitor's first tick: a stable hash of its id spread
/// over the interval, capped at one minute. FNV-1a rather than std's
/// `DefaultHasher`, whose algorithm is unspecified and may change with the
/// toolchain: a monitor keeps its phase across restarts, reloads and upgrades.
/// How often a monitor's loop runs. A probe runs at its interval. A push
/// monitor's interval is something else - the longest silence it tolerates -
/// and judging a stored heartbeat costs one indexed read, so its loop runs
/// at least every minute: ticking once per interval let a nightly job's
/// missed heartbeat go unnoticed for up to a whole interval after its
/// deadline, and then wait `fail_threshold` more of them - days.
fn evaluation_interval(monitor: &Monitor) -> std::time::Duration {
    match monitor.spec {
        MonitorKind::Push(_) => monitor.interval().min(PUSH_EVALUATION_MAX),
        _ => monitor.interval(),
    }
}

/// The longest a push monitor goes between two looks at its heartbeats.
const PUSH_EVALUATION_MAX: std::time::Duration = std::time::Duration::from_mins(1);

fn stagger_offset(id: &str, interval: std::time::Duration) -> std::time::Duration {
    let span = interval.min(std::time::Duration::from_mins(1));
    span.mul_f64(f64::from(u32::try_from(fnv1a(id.as_bytes()) % 1000).unwrap_or(0)) / 1000.0)
}

/// 64-bit FNV-1a: tiny, fixed forever, good enough to spread ids over phases.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    bytes.iter().fold(OFFSET_BASIS, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(PRIME)
    })
}

/// Send an alert to the coalescer. It only fails once the coalescer is gone
/// (it returned, or shutdown is under way; a panic would have aborted a
/// release build before this), and then the alert is lost: say so
/// loudly rather than dropping it in silence.
fn send_alert(alerts: &mpsc::UnboundedSender<AlertMsg>, monitor_id: &str, message: AlertMsg) {
    if alerts.send(message).is_err() {
        error!(monitor = %monitor_id, "alert dropped: the alert coalescer is not running");
    }
}

#[allow(clippy::too_many_lines)]
async fn run(
    monitor: Monitor,
    config: watch::Receiver<Arc<Config>>,
    deps: MonitorDeps,
    mut shutdown: watch::Receiver<bool>,
) {
    let MonitorDeps {
        store,
        client,
        confirm_client,
        notifier,
        alerts,
        last_tick,
        alert_state,
    } = deps;
    // Fixed cadence: the tick interval does not drift by the probe duration.
    // The first tick is phase-shifted per monitor so a fleet sharing the same
    // interval doesn't probe - and insert - in lockstep at boot and then on
    // every aligned tick forever. The offset is a stable hash of the id (same
    // phase across restarts), capped so a long-interval monitor still gets its
    // first check within a minute of starting.
    let every = evaluation_interval(&monitor);
    let offset = stagger_offset(&monitor.id, every);
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + offset, every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The exec directory comes from the environment (immutable for the process
    // lifetime), so one read outlives every config reload.
    let exec_dir = config.borrow().exec_dir.clone();
    // Re-attach to an incident left open by a previous run (restart mid-outage,
    // monitor edited live): it gets closed on the first healthy tick instead of
    // staying open forever, and a still-down monitor keeps its original start.
    let mut open_incident: Option<i64> = db::find_open_incident(&store, &monitor.id)
        .await
        .unwrap_or_else(|err| {
            // Carrying on as if there were none: said out loud, because a
            // down confirmed now opens a second incident beside it (both
            // are closed together on recovery).
            error!(monitor = %monitor.id, "cannot read the open incident: {err:#}");
            None
        });
    // The alert state of the previous task for this id, if the supervisor
    // restarted it; otherwise (daemon start) an open incident says the down
    // was already announced - unless its record says it never was: the daemon
    // stopped between recording the incident and delivering the alert. That
    // down is confirmed again from scratch and announced on the same incident.
    let mut state = match alert_state.get() {
        Some(state) => state,
        None => match open_incident {
            Some(incident_id) if !announced(&store, &monitor.id, incident_id).await => {
                warn!(monitor = %monitor.id, "open incident was never announced: confirming it again");
                AlertState::default()
            }
            Some(incident_id) => {
                let mut state = AlertState::resumed_down(alert_settings(&config, &monitor.id).0);
                resume_local_only(&store, &alerts, &monitor.id, incident_id, &mut state).await;
                state
            }
            None => AlertState::default(),
        },
    };
    alert_state.set(state);
    // Push monitors are judged from stored heartbeats against their cadence,
    // counted from the first heartbeat expected (persisted, so a monitor that
    // never pinged still alerts across restarts).
    let heartbeat = match &monitor.spec {
        MonitorKind::Push(push) => Some(HeartbeatWatch::for_monitor(&store, &monitor, push).await),
        _ => None,
    };

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

        // No outcome means a push monitor still inside its first expected
        // window (or a read error): status stays unknown, nothing to react to.
        let Some(outcome) = tick_outcome(
            &client,
            &store,
            &monitor,
            exec_dir.as_deref(),
            heartbeat.as_ref(),
        )
        .await
        else {
            continue;
        };

        // Every failure that survived its retries is worth a log line (the
        // page only shows "degraded" until the threshold confirms, so this is
        // where a blip's reason is visible). Quiet once the outage is
        // confirmed - "confirmed down" already said it.
        if !outcome.is_up() && state.level != AlertLevel::Down {
            warn!(
                monitor = %monitor.id,
                error = outcome.error.as_deref().unwrap_or("unknown"),
                "check failed"
            );
        }

        let (threshold, alert_on_degraded) = alert_settings(&config, &monitor.id);
        // A maintenance window (the config's, one planned through the API,
        // one a peer announced) or an ad-hoc silence (`hora silence`, POST
        // /api/silence): both read fresh each tick, so they apply at once and
        // stop muting the moment they end.
        let muted = muted(&store, &config, &monitor.id).await;
        // A heartbeat that did not come is confirmed as it is: its interval
        // (or its schedule's grace) was the tolerance. The anti-flap
        // threshold is for checks that ran and failed, and still applies to
        // a job that pushes `status=down`.
        let threshold = if outcome
            .reason
            .is_some_and(probe::FailureKind::is_missed_heartbeat)
        {
            1
        } else {
            threshold
        };
        // An incident is bound to confirmed-down alerts; any up tick (healthy
        // or merely degraded) ends it, whatever the alert state machine does -
        // including an incident inherited from a previous run, and even during
        // maintenance (the record should reflect the real outage span).
        if outcome.is_up() {
            if open_incident.is_some() {
                state.outage_ended();
                alert_state.set(state);
            }
            close_open_incident(&store, &monitor.id, &mut open_incident).await;
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
            && (!outcome.is_up() || state.burn.any())
        {
            evaluate_burn(&store, &notifier, &monitor, slo_bp, &mut state.burn).await;
            alert_state.set(state);
        }

        if !outcome.is_up() {
            // Down resets degraded tracking; alert once `threshold` consecutive
            // failures confirm it (escalating from healthy or degraded).
            if state.observe_down(threshold) {
                error!(monitor = %monitor.id, failures = state.consecutive_down, "confirmed down");
                let snapshot = config.borrow().clone();
                let local_only = confirm_down(
                    &snapshot,
                    &store,
                    &confirm_client,
                    &alerts,
                    &monitor,
                    &outcome,
                    &mut open_incident,
                )
                .await;
                state.level = AlertLevel::Down;
                let now = chrono::Utc::now().timestamp();
                state.local_only_asked = local_only.then_some(now);
                state.announce_checked = Some(now);
            } else if state.reannounce_due(chrono::Utc::now().timestamp()) {
                // Delivered since: nothing more to watch. Still not: every
                // channel failed, or dropped it - said again, on the same
                // incident, for as long as the outage lasts.
                state.announce_checked = match open_incident {
                    Some(incident) if !announced(&store, &monitor.id, incident).await => {
                        warn!(monitor = %monitor.id, "down still unannounced: sending it again");
                        let snapshot = config.borrow().clone();
                        let local_only = confirm_down(
                            &snapshot,
                            &store,
                            &confirm_client,
                            &alerts,
                            &monitor,
                            &outcome,
                            &mut open_incident,
                        )
                        .await;
                        let now = chrono::Utc::now().timestamp();
                        state.local_only_asked = local_only.then_some(now);
                        Some(now)
                    }
                    _ => None,
                };
            } else if state.reask_due(chrono::Utc::now().timestamp()) {
                let snapshot = config.borrow().clone();
                let still_local = reconfirm_local_down(
                    &snapshot,
                    &store,
                    &confirm_client,
                    &alerts,
                    &monitor,
                    &outcome,
                    open_incident,
                )
                .await;
                state.local_only_asked = still_local.then(|| chrono::Utc::now().timestamp());
            }
        } else if outcome.is_degraded() && alert_on_degraded {
            // Up but slow: same anti-flap threshold as down, separate state.
            if state.observe_degraded(threshold) {
                alert_degraded(&notifier, &monitor, &outcome).await;
                state.level = AlertLevel::Degraded;
            }
        } else if state.observe_up() {
            // Fully healthy (or degraded with the option off, treated as up)
            // after an alert.
            info!(monitor = %monitor.id, "recovered");
            // Through the coalescer too: the recovery of a folded down
            // alert stays silent (nothing was announced going down).
            send_alert(
                &alerts,
                &monitor.id,
                AlertMsg::Recovered {
                    id: monitor.id.clone(),
                    name: monitor.name.clone(),
                    notify: monitor.notify.clone(),
                },
            );
            state.level = AlertLevel::Healthy;
        }
        // Written back right after the side effect, with no `.await` in
        // between: see [`AlertCell`].
        alert_state.set(state);
    }
}

/// A monitor just confirmed down: resolve the topology context, open (or
/// resume) the incident record, ask the peers for a multi-vantage verdict,
/// and hand the alert to the coalescer, which may fold it into its root
/// cause's single notification. Returns whether the down is local-only (the
/// peers that answered all see the target up): the coalescer then sends it
/// to the quiet channels only, and the caller asks the peers again later.
async fn confirm_down(
    config: &Config,
    store: &Store,
    confirm_client: &Client,
    alerts: &mpsc::UnboundedSender<AlertMsg>,
    monitor: &Monitor,
    outcome: &Outcome,
    open_incident: &mut Option<i64>,
) -> bool {
    let (cause, impacted_names) = down_context(config, store, monitor).await;

    // "What changed?": the most recent event marker (`hora event`) within the
    // lookback, phrased relative to now ("deploy api v2.3, 3 min before"). Read
    // errors just drop the annotation - it must never delay the alert.
    let event = correlated_event(store, chrono::Utc::now().timestamp()).await;

    // Unless one is already open (resumed from a previous run mid-outage).
    // Recorded *before* the peers are consulted, so the incident history
    // never waits on the network.
    if open_incident.is_none() {
        *open_incident = open_incident_record(
            store,
            monitor,
            outcome,
            cause.as_ref().map(|(_, name)| name.as_str()),
            &impacted_names,
            event.as_deref(),
        )
        .await;
    }

    // Multi-vantage confirmation: bounded (one concurrent round, hard
    // deadline) and strictly fail-open - `None` means the alert reads exactly
    // as it would without the feature.
    let verdict = ask_peers(store, confirm_client, config, monitor, *open_incident).await;
    let local_only = verdict.as_ref().is_some_and(|verdict| verdict.local_only);
    if local_only {
        record_routing(store, monitor, *open_incident, true).await;
    }

    // The coalescer groups on the *configured* upstreams: in a cascade this
    // monitor often confirms a tick before its upstream is derivably down,
    // so the derived cause alone would lose the race.
    send_alert(
        alerts,
        &monitor.id,
        AlertMsg::Down(DownAlert {
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
            vantage: verdict.map(|verdict| verdict.text),
            event,
            local_only,
            incident: *open_incident,
        }),
    );
    local_only
}

/// Whether an open incident's down alert went out before the restart. A read
/// error keeps the previous behaviour (announced): alerting twice on a
/// database hiccup is the lesser promise to break only when we know.
async fn announced(store: &Store, monitor_id: &str, incident_id: i64) -> bool {
    db::incident_notified(store, incident_id)
        .await
        .unwrap_or_else(|err| {
            warn!(monitor = %monitor_id, "cannot tell whether the open incident was announced: {err:#}");
            true
        })
}

/// A daemon start that finds an open incident whose down was local-only
/// (recorded in its `local_only` column): tell the coalescer, whose routing
/// memory the restart wiped, so the recovery goes only to whoever received
/// the down; and, while the peers have not confirmed it, keep asking them.
async fn resume_local_only(
    store: &Store,
    alerts: &mpsc::UnboundedSender<AlertMsg>,
    monitor_id: &str,
    incident_id: i64,
    state: &mut AlertState,
) {
    let local_only = match db::incident_local_only(store, incident_id).await {
        Ok(local_only) => local_only,
        Err(err) => {
            error!(monitor = %monitor_id, "failed to read the incident's routing: {err:#}");
            None
        }
    };
    let Some(still_local) = local_only else {
        return;
    };
    send_alert(
        alerts,
        monitor_id,
        AlertMsg::ResumedLocalOnly {
            id: monitor_id.to_owned(),
            confirmed: !still_local,
        },
    );
    if still_local {
        state.local_only_asked = Some(chrono::Utc::now().timestamp());
    }
}

/// Multi-vantage confirmation: bounded (one concurrent round, hard deadline)
/// and strictly fail-open - `None` means the alert reads exactly as it would
/// without the feature. The verdict is recorded on the incident too (best
/// effort), so the post-mortem can replay what the mesh saw, not just what
/// this node saw.
async fn ask_peers(
    store: &Store,
    confirm_client: &Client,
    config: &Config,
    monitor: &Monitor,
    open_incident: Option<i64>,
) -> Option<crate::mesh::confirm::DownVerdict> {
    let verdict = crate::mesh::confirm::confirm_with_peers(confirm_client, config, monitor).await;
    if let Some(verdict) = &verdict {
        info!(monitor = %monitor.id, verdict = %verdict.text, "multi-vantage verdict");
        if let Some(incident_id) = open_incident
            && let Err(err) = db::update_incident_vantage(store, incident_id, &verdict.text).await
        {
            error!(monitor = %monitor.id, "failed to record vantage verdict: {err:#}");
        }
    }
    verdict
}

/// Persist how the incident's down was routed (see
/// [`db::set_incident_local_only`]), so a restart routes its recovery the
/// same way. Best effort: a failed write only costs that after a restart.
async fn record_routing(
    store: &Store,
    monitor: &Monitor,
    open_incident: Option<i64>,
    local_only: bool,
) {
    if let Some(incident_id) = open_incident
        && let Err(err) = db::set_incident_local_only(store, incident_id, local_only).await
    {
        error!(monitor = %monitor.id, "failed to record the incident's routing: {err:#}");
    }
}

/// A local-only down that lasts: ask the peers again. Once they no longer
/// all see the target up - they see it down too, or can no longer answer,
/// which is no contradiction either - the down goes out to the usual
/// channels (once: the caller stops asking). Returns whether it is still
/// local-only.
async fn reconfirm_local_down(
    config: &Config,
    store: &Store,
    confirm_client: &Client,
    alerts: &mpsc::UnboundedSender<AlertMsg>,
    monitor: &Monitor,
    outcome: &Outcome,
    open_incident: Option<i64>,
) -> bool {
    let verdict = ask_peers(store, confirm_client, config, monitor, open_incident).await;
    if verdict.as_ref().is_some_and(|verdict| verdict.local_only) {
        return true;
    }
    warn!(monitor = %monitor.id, "local-only down now confirmed: alerting");
    record_routing(store, monitor, open_incident, false).await;
    let (cause, impacted_names) = down_context(config, store, monitor).await;
    send_alert(
        alerts,
        &monitor.id,
        AlertMsg::Down(DownAlert {
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
            vantage: verdict.map(|verdict| verdict.text),
            event: correlated_event(store, chrono::Utc::now().timestamp()).await,
            local_only: false,
            incident: open_incident,
        }),
    );
    false
}

/// How far back a recorded event still counts as "what changed" for a down
/// that confirms now. An hour: long enough for a slow rollout to bite, short
/// enough that yesterday's deploy is not blamed for today's outage.
const EVENT_LOOKBACK_SECS: i64 = 3600;

/// The correlated-event phrase for a down confirming at `now` ("deploy api
/// v2.3, 3 min before"), or `None` when no event was recorded within the
/// lookback. A read error drops the annotation (logged), never the alert.
async fn correlated_event(store: &Store, now: i64) -> Option<String> {
    match db::latest_event_before(store, now, EVENT_LOOKBACK_SECS).await {
        Ok(found) => found.map(|event| event_phrase(&event.title, now - event.created_at)),
        Err(err) => {
            error!("failed to read event markers: {err:#}");
            None
        }
    }
}

/// `"deploy api v2.3, 3 min 10 s before"` - the phrase stored on the incident and
/// appended to the alert (each channel prefixes its own "recent change:").
fn event_phrase(title: &str, age_secs: i64) -> String {
    format!("{title}, {} before", crate::fmt::duration(age_secs))
}

/// Live alert settings for this tick, read fresh so a reloaded threshold
/// applies immediately: (this monitor's fail threshold, degraded alerts
/// enabled).
fn alert_settings(config: &watch::Receiver<Arc<Config>>, monitor_id: &str) -> (u32, bool) {
    let snapshot = config.borrow();
    (
        snapshot.fail_threshold(monitor_id),
        snapshot.alerts.alert_on_degraded,
    )
}

/// Whether this id's alerts are muted right now: inside a maintenance window
/// (see [`crate::planned`]) or under an ad-hoc silence. The same question for
/// a monitor and for a watched peer (by its listen id).
pub(crate) async fn muted(store: &Store, config: &watch::Receiver<Arc<Config>>, id: &str) -> bool {
    let snapshot = config.borrow().clone();
    muted_under(store, &snapshot, id).await
}

/// [`muted`] against a config already in hand: the one answer every alert
/// asks for - a down, a certificate or a domain about to expire, a new
/// release - so that a silence mutes whatever a maintenance window does.
pub(crate) async fn muted_under(store: &Store, config: &Config, id: &str) -> bool {
    crate::planned::in_maintenance(store, config, id, chrono::Utc::now()).await
        || silenced(store, id).await
}

/// Announce a confirmed-degraded monitor (up, but over its latency budget).
async fn alert_degraded(notifier: &Notifiers, monitor: &Monitor, outcome: &Outcome) {
    let latency_ms = outcome.latency_ms;
    warn!(monitor = %monitor.id, ?latency_ms, "degraded");
    notifier
        .load_full()
        .dispatch(
            Event::Degraded {
                monitor: &monitor.name,
                latency_ms,
                // Only a push carries one: the job's own `msg`.
                detail: outcome.error.as_deref(),
            },
            monitor.notify.as_deref(),
        )
        .await;
}

/// Whether an ad-hoc silence covers this monitor right now. A read error fails
/// open (logged, not silenced): a database hiccup must never mute an alert.
pub(crate) async fn silenced(store: &Store, monitor_id: &str) -> bool {
    match db::is_silenced(store, monitor_id, chrono::Utc::now().timestamp()).await {
        Ok(silenced) => silenced,
        Err(err) => {
            error!(monitor = %monitor_id, "failed to read silences: {err:#}");
            false
        }
    }
}

/// Close the open incident, if any. Failures are logged, never fatal.
async fn close_open_incident(store: &Store, monitor_id: &str, open_incident: &mut Option<i64>) {
    if open_incident.take().is_some()
        && let Err(err) = db::end_open_incidents(store, monitor_id).await
    {
        error!(monitor = %monitor_id, "failed to close incident: {err:#}");
    }
}

/// One tick's outcome: probe active monitors (recording the check), run exec
/// checks, evaluate stored heartbeats for push monitors. `None` means nothing
/// to react to yet.
async fn tick_outcome(
    client: &Client,
    store: &Store,
    monitor: &Monitor,
    exec_dir: Option<&std::path::Path>,
    heartbeat: Option<&HeartbeatWatch>,
) -> Option<Outcome> {
    let outcome = match &monitor.spec {
        MonitorKind::Push(_) => {
            // Built with the task for every push monitor.
            let watch = heartbeat?;
            return heartbeat_outcome_for(
                store,
                &monitor.id,
                &watch.cadence,
                watch.expected_since,
                i64::try_from(monitor.interval_secs).unwrap_or(i64::MAX),
                chrono::Utc::now().timestamp(),
            )
            .await;
        }
        MonitorKind::Exec(spec) => match exec_dir {
            Some(dir) => crate::exec::run(dir, monitor, spec)
                .await
                .excusing_exhaustion(),
            // Config validation guarantees the directory; defensive only.
            None => crate::probe::Outcome::down(
                crate::probe::FailureKind::Plugin,
                "HORA_EXEC_DIR is not set".to_owned(),
            ),
        },
        MonitorKind::Http(spec) => probe::run(client, monitor, NetworkProbe::Http(spec)).await,
        MonitorKind::Tcp(spec) => probe::run(client, monitor, NetworkProbe::Tcp(spec)).await,
        MonitorKind::Icmp(spec) => probe::run(client, monitor, NetworkProbe::Icmp(spec)).await,
        MonitorKind::Dns(spec) => probe::run(client, monitor, NetworkProbe::Dns(spec)).await,
    };
    // A check this host could not run (out of descriptors, of memory) is no
    // verdict on the target: nothing recorded, nothing counted, and the node
    // reports itself degraded instead (see `probe::local`).
    if outcome.is_local_fault() {
        probe::local::note(
            &monitor.id,
            outcome.error.as_deref().unwrap_or("out of resources"),
            chrono::Utc::now().timestamp(),
        );
        return None;
    }
    if let Err(err) = db::insert_check(store, &monitor.id, &outcome).await {
        error!(monitor = %monitor.id, "failed to record check: {err:#}");
    }
    Some(outcome)
}

/// Open an incident record for a confirmed-down monitor; `None` (logged) when
/// the insert fails, so a database hiccup never blocks the alert itself.
async fn open_incident_record(
    store: &Store,
    monitor: &Monitor,
    outcome: &Outcome,
    cause: Option<&str>,
    impacted: &[String],
    event: Option<&str>,
) -> Option<i64> {
    match db::insert_incident_start(
        store,
        &monitor.id,
        outcome.error.as_deref(),
        outcome.reason,
        cause,
        impacted,
        outcome.snapshot.as_deref(),
        event,
    )
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
    store: &Store,
    notifier: &Notifiers,
    monitor: &Monitor,
    slo_bp: u32,
    state: &mut BurnAlerts,
) {
    let window_days = monitor.slo_window_days();
    let now = chrono::Utc::now().timestamp();

    let burn_1h = burn_window(store, &monitor.id, now - 3600, slo_bp).await;
    let fast_threshold = slo::fast_burn_threshold_x10(window_days);
    let fast_now = burn_1h >= fast_threshold
        && burn_window(store, &monitor.id, now - 300, slo_bp).await >= fast_threshold;
    if fast_now {
        if !state.fast {
            state.fast = true;
            state.slow = true;
            fire_burn_alert(store, notifier, monitor, slo_bp, burn_1h, "1h").await;
        }
        return;
    }
    if burn_1h < fast_threshold {
        state.fast = false;
    }

    let burn_6h = burn_window(store, &monitor.id, now - 6 * 3600, slo_bp).await;
    let slow_threshold = slo::slow_burn_threshold_x10(window_days);
    let slow_now = burn_6h >= slow_threshold
        && burn_window(store, &monitor.id, now - 1800, slo_bp).await >= slow_threshold;
    if slow_now && !state.slow {
        state.slow = true;
        fire_burn_alert(store, notifier, monitor, slo_bp, burn_6h, "6h").await;
    } else if burn_6h < slow_threshold {
        state.slow = false;
    }
}

/// The burn rate over one lookback window, in tenths. A read error counts as
/// zero: never alert (or re-arm) off unreadable data.
async fn burn_window(store: &Store, id: &str, since: i64, slo_bp: u32) -> i64 {
    match db::availability(store, id, since).await {
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
    store: &Store,
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
    let exhausted_in_secs = match db::availability(store, &monitor.id, since).await {
        Ok((available, total)) => {
            let remaining = slo::remaining_minutes(window_days, slo_bp, available, total);
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

/// Compute topology annotation for a down alert: the nearest down upstream
/// (`cause`, as config id + display name) if any, or the list of impacted
/// dependents (`impacted`) if this monitor is a root cause. Returns
/// `(None, vec![])` when the monitor has no topology configured.
///
/// An upstream is down by its *own* threshold, as its alert and its card on
/// the page have it: judged by this monitor's, a database confirmed on its
/// first failure would not yet be the cause of an API that waits for three,
/// and an upstream that waits for three would be blamed by a symptom that
/// confirms at one.
async fn down_context(
    config: &Config,
    store: &Store,
    monitor: &Monitor,
) -> (Option<(String, String)>, Vec<String>) {
    let upstreams = topology::transitive_upstreams(&config.monitors, &monitor.id);
    for up_id in &upstreams {
        let threshold_i64 = i64::from(config.fail_threshold(up_id));
        let Ok(recent) = db::recent_checks(store, up_id, threshold_i64).await else {
            continue;
        };
        if db::derive_status(&recent, threshold_i64) == MonitorState::Down
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::CheckStatus;

    use std::time::Duration;

    /// A push monitor's interval is its tolerance, not how often it is
    /// looked at: a daily job is judged every minute, a probe at its pace.
    #[test]
    fn push_monitors_are_evaluated_at_least_every_minute() {
        let config = crate::config::parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "nightly"
            name = "Nightly"
            kind = "push"
            interval_secs = 90000
            push_token = "0123456789abcdef"
            [[monitors]]
            id = "fast"
            name = "Fast"
            kind = "push"
            interval_secs = 20
            push_token = "0123456789abcdef"
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 600
            "#,
        )
        .expect("config");
        let every =
            |id: &str| evaluation_interval(config.monitors.iter().find(|m| m.id == id).unwrap());
        assert_eq!(every("nightly"), std::time::Duration::from_mins(1));
        assert_eq!(every("fast"), std::time::Duration::from_secs(20));
        assert_eq!(every("web"), std::time::Duration::from_mins(10));
    }

    #[test]
    fn an_unannounced_down_is_looked_at_again_until_the_outage_ends() {
        let mut state = AlertState::default();
        assert!(state.observe_down(1));
        state.level = AlertLevel::Down;
        state.announce_checked = Some(1_000);
        assert!(!state.reannounce_due(1_000 + REANNOUNCE_SECS - 1));
        assert!(state.reannounce_due(1_000 + REANNOUNCE_SECS));
        // Seen delivered: nothing left to watch.
        state.announce_checked = None;
        assert!(!state.reannounce_due(i64::MAX));
        // The outage over, whatever was owed is dropped with it.
        state.announce_checked = Some(1_000);
        state.outage_ended();
        assert_eq!(state.announce_checked, None);
        state.announce_checked = Some(1_000);
        state.observe_up();
        assert_eq!(state.announce_checked, None);
        // A resumed outage was announced by the previous run.
        assert_eq!(AlertState::resumed_down(2).announce_checked, None);
    }

    #[test]
    fn stagger_offset_is_stable_and_bounded() {
        let minute = std::time::Duration::from_mins(1);
        // Same id, same phase - across restarts and config reloads.
        assert_eq!(stagger_offset("api", minute), stagger_offset("api", minute));
        // Different ids should generally land on different phases.
        assert_ne!(stagger_offset("api", minute), stagger_offset("db", minute));
        // Never past the interval, and capped at a minute for slow cadences.
        let five_secs = std::time::Duration::from_secs(5);
        assert!(stagger_offset("api", five_secs) < five_secs);
        let daily = std::time::Duration::from_hours(24);
        assert!(stagger_offset("api", daily) < minute);
    }

    #[test]
    fn a_local_only_down_is_asked_about_again_until_it_recovers() {
        let mut state = AlertState::default();
        assert!(state.observe_down(1));
        state.level = AlertLevel::Down;
        // A down the peers confirmed (or could not judge) is never re-asked.
        assert!(!state.reask_due(10_000));
        // A local-only one is, every LOCAL_ONLY_REASK_SECS.
        state.local_only_asked = Some(1_000);
        assert!(!state.reask_due(1_000 + LOCAL_ONLY_REASK_SECS - 1));
        assert!(state.reask_due(1_000 + LOCAL_ONLY_REASK_SECS));
        // Recovery ends it.
        assert!(state.observe_up());
        assert_eq!(state.local_only_asked, None);
    }

    /// A local-only down, then a daemon restart: the fresh monitor task seeds
    /// the fresh coalescer from the incident, so the recovery reaches the
    /// quiet channel only - not the usual one, which never got the down.
    #[tokio::test]
    async fn a_restart_keeps_a_local_only_recovery_quiet() {
        let (main_url, main) = crate::testing::webhook_sink().await;
        let (quiet_url, quiet) = crate::testing::webhook_sink().await;
        let config = Arc::new(
            crate::config::parse(&format!(
                r#"
                [page]
                [server]
                [alerts]
                notify_unconfirmed = ["quiet"]
                [[channels]]
                name = "main"
                type = "webhook"
                url = "{main_url}"
                [[channels]]
                name = "quiet"
                type = "webhook"
                url = "{quiet_url}"
                [[monitors]]
                id = "api"
                name = "API"
                target = "https://example.com"
                interval_secs = 60
                notify = ["main"]
                "#
            ))
            .unwrap(),
        );
        let store = Store::in_memory().await;
        // Before the restart: the down was local-only, recorded as such.
        let incident = db::insert_incident_start(&store, "api", None, None, None, &[], None, None)
            .await
            .unwrap();
        record_routing(&store, &config.monitors[0], Some(incident), true).await;
        assert_eq!(
            db::incident_local_only(&store, incident).await.unwrap(),
            Some(true)
        );

        // After it: a fresh coalescer, a fresh task resuming the incident.
        let client = crate::http::client(None).unwrap();
        let notifier = crate::notifications::shared(&config, &client);
        let (_config_tx, config_rx) = watch::channel(Arc::clone(&config));
        let (tx, rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let coalescer = crate::coalesce::spawn(config_rx, notifier, store.clone(), rx, shutdown_rx);
        // An ordinary incident seeds nothing (and does not ask the peers).
        let ordinary = db::insert_incident_start(&store, "web", None, None, None, &[], None, None)
            .await
            .unwrap();
        let mut state = AlertState::resumed_down(3);
        resume_local_only(&store, &tx, "web", ordinary, &mut state).await;
        assert_eq!(state.local_only_asked, None);

        let mut state = AlertState::resumed_down(3);
        resume_local_only(&store, &tx, "api", incident, &mut state).await;
        // Still local-only: the peers keep being asked.
        assert!(state.local_only_asked.is_some());

        assert!(state.observe_up());
        send_alert(
            &tx,
            "api",
            AlertMsg::Recovered {
                id: "api".to_owned(),
                name: "API".to_owned(),
                notify: Some(vec!["main".to_owned()]),
            },
        );
        // Closing the inbox lets the coalescer finish what it received.
        drop(tx);
        coalescer.await.unwrap();
        drop(shutdown_tx);
        assert_eq!(crate::testing::events(&main), Vec::<String>::new());
        assert_eq!(crate::testing::events(&quiet), ["recovered"]);
        assert_eq!(quiet.lock().unwrap()[0]["local_only"], true);
    }

    #[test]
    fn stagger_hash_is_fixed_across_toolchains() {
        // The published FNV-1a 64 test vectors: phases never move on upgrade.
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn event_phrase_formats_the_age() {
        assert_eq!(
            event_phrase("deploy api v2.3", 45),
            "deploy api v2.3, 45 s before"
        );
        assert_eq!(
            event_phrase("deploy api v2.3", 180),
            "deploy api v2.3, 3 min before"
        );
        assert_eq!(
            event_phrase("deploy api v2.3", 4380),
            "deploy api v2.3, 1 h 13 min before"
        );
        // A clock skew can't produce a negative age.
        assert_eq!(event_phrase("x", -5), "x, 0 s before");
    }

    #[test]
    fn alert_state_confirms_once_and_recovers_once() {
        let mut state = AlertState::default();
        assert!(!state.observe_down(2));
        assert!(state.observe_down(2), "threshold reached");
        state.level = AlertLevel::Down;
        assert!(!state.observe_down(2), "already announced");

        // A restarted task resumes the same state: no second down...
        let mut resumed = state;
        assert!(!resumed.observe_down(2));
        // ...and the recovery is still owed.
        assert!(resumed.observe_up());
        resumed.level = AlertLevel::Healthy;
        assert!(!resumed.observe_up());

        // Found mid-outage at startup: the same, from the open incident.
        let mut seeded = AlertState::resumed_down(2);
        assert!(!seeded.observe_down(2));
        assert!(seeded.observe_up());
    }

    #[test]
    fn a_down_after_a_muted_recovery_is_a_new_outage() {
        // Announced down, then silenced: an up tick closes the incident but
        // the muted loop skips `observe_up`, so the level still reads Down.
        let mut state = AlertState::resumed_down(2);
        state.outage_ended();
        // The service fails again once the silence is over: the streak
        // restarts from zero and confirms a new outage at the threshold.
        assert!(!state.observe_down(2), "anti-flap threshold still applies");
        assert!(state.observe_down(2), "second outage is announced");
        state.level = AlertLevel::Down;
        assert!(!state.observe_down(2), "and only once");
        assert!(state.observe_up(), "its recovery is owed");

        // Without a second failure, the muted recovery is still announced
        // by the first unmuted healthy tick, and nothing stays armed.
        let mut state = AlertState::resumed_down(2);
        state.outage_ended();
        assert!(state.observe_up());
        state.level = AlertLevel::Healthy;
        assert!(!state.observe_down(2));
    }

    /// A push monitor ticking every two seconds with a threshold of 1, wired to a
    /// test coalescer inbox.
    fn push_task(
        store: &Store,
        config: &watch::Receiver<Arc<Config>>,
        alerts: &mpsc::UnboundedSender<AlertMsg>,
        cell: &AlertCell,
        shutdown: &watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        monitor_task(0, store, config, alerts, cell, shutdown)
    }

    /// The task of the config's `index`-th monitor, wired to a test
    /// coalescer inbox.
    fn monitor_task(
        index: usize,
        store: &Store,
        config: &watch::Receiver<Arc<Config>>,
        alerts: &mpsc::UnboundedSender<AlertMsg>,
        cell: &AlertCell,
        shutdown: &watch::Receiver<bool>,
    ) -> JoinHandle<()> {
        let client = crate::http::client(None).expect("client");
        let monitor = config.borrow().monitors[index].clone();
        spawn_monitor(
            monitor,
            config.clone(),
            MonitorDeps {
                store: store.clone(),
                client: client.clone(),
                confirm_client: client.clone(),
                notifier: crate::notifications::shared(&config.borrow(), &client),
                alerts: alerts.clone(),
                last_tick: Arc::new(AtomicU64::new(0)),
                alert_state: cell.clone(),
            },
            shutdown.clone(),
        )
    }

    /// The next alert within `wait`, as `("down" | "recovered", id)`.
    async fn next_alert(
        rx: &mut mpsc::UnboundedReceiver<AlertMsg>,
        wait: Duration,
    ) -> Option<(&'static str, String)> {
        match tokio::time::timeout(wait, rx.recv()).await {
            Ok(Some(AlertMsg::Down(alert))) => Some(("down", alert.id)),
            Ok(Some(AlertMsg::Recovered { id, .. })) => Some(("recovered", id)),
            _ => None,
        }
    }

    #[tokio::test]
    async fn restarted_monitor_task_keeps_its_alert_state() {
        let store = Store::in_memory().await;
        let config = crate::config::parse(
            r#"
            [page]
            [server]
            [alerts]
            fail_threshold = 1
            [[monitors]]
            id = "job"
            name = "Job"
            kind = "push"
            allow_unauthenticated = true
            interval_secs = 2
            "#,
        )
        .expect("config");
        let (_config_tx, config_rx) = watch::channel(Arc::new(config));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (alerts_tx, mut alerts_rx) = mpsc::unbounded_channel();
        let cell = AlertCell::default();

        // The job reports a failure: confirmed down, announced once.
        db::insert_push(
            &store,
            "job",
            CheckStatus::Down,
            None,
            Some("backup failed"),
        )
        .await
        .unwrap();
        let task = push_task(&store, &config_rx, &alerts_tx, &cell, &shutdown_rx);
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_secs(5)).await,
            Some(("down", "job".to_owned()))
        );

        // The daemon stops before that alert reached a channel (nothing here
        // delivers it): the next start finds an open incident that was never
        // announced, and announces it instead of only its recovery.
        task.abort();
        let cell = AlertCell::default();
        let task = push_task(&store, &config_rx, &alerts_tx, &cell, &shutdown_rx);
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_secs(5)).await,
            Some(("down", "job".to_owned()))
        );
        // This time a channel takes it.
        let incident = db::find_open_incident(&store, "job")
            .await
            .unwrap()
            .expect("still the same open incident");
        db::mark_incident_notified(&store, incident).await.unwrap();

        // A config edit restarts the task while still down: no second alert.
        task.abort();
        let task = push_task(&store, &config_rx, &alerts_tx, &cell, &shutdown_rx);
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_secs(3)).await,
            None
        );

        // A daemon restart (no state carried over) seeds it from the open
        // incident instead: still no second alert.
        task.abort();
        let fresh = AlertCell::default();
        let task = push_task(&store, &config_rx, &alerts_tx, &fresh, &shutdown_rx);
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_secs(3)).await,
            None
        );

        // The job recovers: the recovery is announced, exactly once.
        db::insert_push(&store, "job", CheckStatus::Up, None, None)
            .await
            .unwrap();
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_secs(5)).await,
            Some(("recovered", "job".to_owned()))
        );
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_millis(1500)).await,
            None
        );
        task.abort();
    }

    /// A local port nothing listens on: a target that fails at once.
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    }

    /// Two monitors on the same dead target, one with `fail_threshold = 1`:
    /// it is confirmed on its first failed check, while its neighbour waits
    /// for the global three.
    #[tokio::test]
    async fn a_monitor_with_its_own_threshold_does_not_wait_for_the_global_one() {
        let port = closed_port().await;
        let config = crate::config::parse(&format!(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "strict"
            name = "Strict"
            target = "http://127.0.0.1:{port}"
            interval_secs = 2
            fail_threshold = 1
            [[monitors]]
            id = "lenient"
            name = "Lenient"
            target = "http://127.0.0.1:{port}"
            interval_secs = 2
            "#
        ))
        .expect("config");
        let (_config_tx, config_rx) = watch::channel(Arc::new(config));
        assert_eq!(alert_settings(&config_rx, "strict").0, 1);
        assert_eq!(alert_settings(&config_rx, "lenient").0, 3);

        let store = Store::in_memory().await;
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (alerts_tx, mut alerts_rx) = mpsc::unbounded_channel();
        let tasks = [0, 1].map(|index| {
            monitor_task(
                index,
                &store,
                &config_rx,
                &alerts_tx,
                &AlertCell::default(),
                &shutdown_rx,
            )
        });

        // Each down alert, with the failed checks its monitor had by then.
        let mut failures = std::collections::HashMap::new();
        while failures.len() < 2 {
            let (event, id) = next_alert(&mut alerts_rx, Duration::from_secs(20))
                .await
                .expect("both monitors are down");
            assert_eq!(event, "down");
            let checks = db::recent_checks(&store, &id, 10).await.unwrap().len();
            failures.insert(id, checks);
        }
        for task in tasks {
            task.abort();
        }
        assert!(
            failures["strict"] < 3,
            "waited for the global: {failures:?}"
        );
        assert!(failures["lenient"] >= 3, "did not wait: {failures:?}");
    }

    /// One tcp monitor on `port`, checked every second and confirmed down
    /// on its first failed check.
    fn tcp_config(port: u16) -> Config {
        crate::config::parse(&format!(
            r#"
            [page]
            [server]
            [alerts]
            fail_threshold = 1
            [[monitors]]
            id = "svc"
            name = "Svc"
            kind = "tcp"
            target = "127.0.0.1:{port}"
            interval_secs = 1
            timeout_secs = 1
            probe_retries = 0
            "#
        ))
        .expect("config")
    }

    /// Plan a window for `svc` through the API's own door, from `offset`
    /// seconds from now, for `secs` seconds.
    async fn plan_window(store: &Store, config: &Config, offset: i64, secs: u32) {
        let now = chrono::Utc::now().timestamp();
        crate::planned::plan(
            store,
            config,
            crate::planned::Plan {
                monitors: Some("svc"),
                starts_at: Some(&(now + offset).to_string()),
                duration: &format!("{secs}s"),
                reason: "kernel reboot",
                ..crate::planned::Plan::default()
            },
            now,
        )
        .await
        .expect("planned");
    }

    /// The host is not back when its window ends: the down that was muted
    /// all along is announced from that moment, once.
    #[tokio::test]
    async fn a_down_that_outlives_its_planned_window_is_announced_when_it_ends() {
        let config = tcp_config(closed_port().await);
        let store = Store::in_memory().await;
        // Under way, with a few seconds left.
        plan_window(&store, &config, -1, 5).await;
        let (_config_tx, config_rx) = watch::channel(Arc::new(config));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (alerts_tx, mut alerts_rx) = mpsc::unbounded_channel();
        let task = monitor_task(
            0,
            &store,
            &config_rx,
            &alerts_tx,
            &AlertCell::default(),
            &shutdown_rx,
        );

        // Muted while the window lasts: checked, recorded down, not alerted,
        // and no incident opened.
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_millis(2500)).await,
            None
        );
        let checks = db::recent_checks(&store, "svc", 10).await.unwrap();
        assert!(
            checks.iter().any(|check| check.status == CheckStatus::Down),
            "checks are still recorded: {checks:?}"
        );
        assert_eq!(db::find_open_incident(&store, "svc").await.unwrap(), None);

        // The window ends and the service is still down.
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_secs(8)).await,
            Some(("down", "svc".to_owned()))
        );
        assert!(
            db::find_open_incident(&store, "svc")
                .await
                .unwrap()
                .is_some()
        );
        // Once.
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_millis(2500)).await,
            None
        );
        task.abort();
    }

    /// The host came back before its window ended: nothing was announced
    /// going down, so nothing is when the window ends - no down, and no
    /// recovery of an outage nobody heard about.
    #[tokio::test]
    async fn a_monitor_back_before_its_planned_window_ends_is_never_announced() {
        let port = closed_port().await;
        let config = tcp_config(port);
        let store = Store::in_memory().await;
        plan_window(&store, &config, -1, 5).await;
        let (_config_tx, config_rx) = watch::channel(Arc::new(config));
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (alerts_tx, mut alerts_rx) = mpsc::unbounded_channel();
        let task = monitor_task(
            0,
            &store,
            &config_rx,
            &alerts_tx,
            &AlertCell::default(),
            &shutdown_rx,
        );

        // Down inside the window...
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_millis(2200)).await,
            None
        );
        let checks = db::recent_checks(&store, "svc", 10).await.unwrap();
        assert!(checks.iter().any(|check| check.status == CheckStatus::Down));
        // ...and back before it ends.
        let _listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("the port is free again");

        // Through the end of the window and well past it: silence.
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_secs(7)).await,
            None
        );
        let checks = db::recent_checks(&store, "svc", 3).await.unwrap();
        assert!(
            checks.iter().all(|check| check.status == CheckStatus::Up),
            "{checks:?}"
        );
        assert_eq!(db::find_open_incident(&store, "svc").await.unwrap(), None);
        task.abort();
    }

    /// A window planned for later mutes nothing until it starts.
    #[tokio::test]
    async fn a_planned_window_that_has_not_started_mutes_nothing() {
        let config = tcp_config(closed_port().await);
        let store = Store::in_memory().await;
        plan_window(&store, &config, 3600, 600).await;
        let (_config_tx, config_rx) = watch::channel(Arc::new(config));
        assert!(!muted(&store, &config_rx, "svc").await);
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let (alerts_tx, mut alerts_rx) = mpsc::unbounded_channel();
        let task = monitor_task(
            0,
            &store,
            &config_rx,
            &alerts_tx,
            &AlertCell::default(),
            &shutdown_rx,
        );
        assert_eq!(
            next_alert(&mut alerts_rx, Duration::from_secs(5)).await,
            Some(("down", "svc".to_owned()))
        );
        task.abort();
    }

    /// The cause of a down is an upstream that is down by its *own*
    /// threshold, whatever the symptom's is.
    #[tokio::test]
    async fn an_upstream_is_down_by_its_own_threshold() {
        let config = crate::config::parse(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "db"
            name = "Database"
            kind = "tcp"
            target = "db.example.com:5432"
            interval_secs = 60
            fail_threshold = 1
            [[monitors]]
            id = "cache"
            name = "Cache"
            kind = "tcp"
            target = "cache.example.com:6379"
            interval_secs = 60
            fail_threshold = 5
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://api.example.com"
            interval_secs = 60
            depends_on = ["db"]
            [[monitors]]
            id = "edge"
            name = "Edge"
            target = "https://edge.example.com"
            interval_secs = 60
            fail_threshold = 1
            depends_on = ["cache"]
            "#,
        )
        .expect("config");
        let store = Store::in_memory().await;
        let monitor = |id: &str| config.find_monitor(id).expect("configured").clone();
        // Failed checks, one per minute of the range (minutes ago).
        let fail = async |id: &'static str, minutes_ago: std::ops::Range<i64>| {
            let now = chrono::Utc::now().timestamp();
            for ago in minutes_ago {
                sqlx::query("INSERT INTO checks (time, monitor_id, status) VALUES (?, ?, 0)")
                    .bind(now - ago * 60)
                    .bind(id)
                    .execute(store.fixture_pool())
                    .await
                    .unwrap();
            }
        };

        // One failed check: the database (threshold 1) is down, and is the
        // cause of the API, which itself waits for three.
        fail("db", 0..1).await;
        let (cause, impacted) = down_context(&config, &store, &monitor("api")).await;
        assert_eq!(cause, Some(("db".to_owned(), "Database".to_owned())));
        assert_eq!(impacted, Vec::<String>::new());

        // Three failed checks: the cache (threshold 5) is not down yet, so
        // it is not blamed by a symptom that confirms on its first failure.
        fail("cache", 0..3).await;
        let (cause, _) = down_context(&config, &store, &monitor("edge")).await;
        assert_eq!(cause, None);
        // Two more, and it is.
        fail("cache", 3..5).await;
        let (cause, _) = down_context(&config, &store, &monitor("edge")).await;
        assert_eq!(cause, Some(("cache".to_owned(), "Cache".to_owned())));
    }
}
