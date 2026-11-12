//! Root-cause alert grouping: one notification per incident, not one per
//! affected monitor.
//!
//! Every down/recovered alert flows through here. Alerts from monitors with
//! no configured upstreams (`depends_on`) are roots: they go out immediately,
//! and their `impacted` list already names the blast radius. Monitors *with*
//! upstreams wait out a short grouping window: if any transitive upstream
//! alerted - before, or while they wait - the alert folds into that single
//! notification (suppressed), and so does its eventual recovery. Grouping is
//! keyed on the *configured* upstreams, not on the cause derived at
//! confirmation time: in a cascade the dependent often confirms a tick before
//! its upstream has enough recorded failures to be derivably down, and the
//! grouping must not lose that race. A monitor that recovers while still
//! queued was a flap inside the window: nothing is sent at all. Incident
//! *records* are written by the scheduler before alerts reach this point, so
//! the history stays complete whatever is folded here.
//!
//! A *local-only* down (every peer that answered sees the target up) is not an
//! outage: it skips the grouping, covers nothing, and is flagged as such. It
//! goes to the monitor's own channels unless `alerts.notify_unconfirmed` names
//! quiet ones to take it instead (an empty list: nowhere, it is only
//! recorded) - a peer's answer alone never silences a down by default. If the
//! peers later confirm it, the real down follows as usual. A recovery goes to
//! whoever received a down: the usual channels, the quiet ones, or both.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use hora_notify::Event;
use tokio::sync::{mpsc, watch};
use tokio::task::{JoinHandle, JoinSet};
use tracing::{error, info};

use crate::config::Config;
use crate::db::{self, Store};
use crate::notifications::Notifiers;

/// One alert routed through the coalescer.
#[derive(Debug)]
pub enum AlertMsg {
    Down(DownAlert),
    Recovered {
        id: String,
        name: String,
        notify: Option<Vec<String>>,
    },
    /// A monitor task resumed an open incident whose down was local-only
    /// (from its `local_only` column): the routing a restart lost, so the
    /// recovery still goes only to whoever received the down. `confirmed`:
    /// the peers later confirmed it and the usual down went out too.
    ResumedLocalOnly {
        id: String,
        confirmed: bool,
    },
}

/// A confirmed-down alert, with its topology annotations resolved.
#[derive(Debug)]
pub struct DownAlert {
    pub id: String,
    pub name: String,
    pub error: Option<String>,
    /// Every transitive upstream id from the config: the grouping key.
    pub upstreams: Vec<String>,
    /// The nearest *derived-down* upstream's display name, for the message.
    pub cause_name: Option<String>,
    pub impacted: Vec<String>,
    pub notify: Option<Vec<String>>,
    /// Multi-vantage verdict, when peers were asked before this alert.
    pub vantage: Option<String>,
    /// Correlated event marker, when one was recorded shortly before the down
    /// ("deploy api v2.3, 3 min before").
    pub event: Option<String>,
    /// Down from this node only (see [`crate::mesh::confirm::DownVerdict`]).
    pub local_only: bool,
    /// The incident this down belongs to, marked announced once a channel
    /// takes the alert (or once it is settled without one, by design).
    pub incident: Option<i64>,
}

/// Who receives a recovery: whoever received the down it ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recipients {
    /// Nobody: the down was folded or dropped as a flap.
    Nobody,
    /// The monitor's own channels.
    Usual,
    /// Only where the local-only down went (see [`LocalOnlyRoute`]).
    Quiet,
    /// Both: a local-only down later confirmed by the peers.
    Both,
}

/// A local-only down announced (wherever [`LocalOnlyRoute`] sent it).
#[derive(Debug, Clone, Copy, Default)]
struct Quiet {
    /// The peers later confirmed it: the usual down went out too.
    confirmed: bool,
}

/// How long a sent (or folded) down alert keeps absorbing late symptom
/// confirmations: monitors confirm at their own interval × threshold pace, so
/// a cascade can take minutes to fully declare itself.
const CAUSE_MEMORY_SECS: i64 = 600;

/// Sweep cadence with an empty queue; the driver wakes earlier for deadlines.
const IDLE_SWEEP: Duration = Duration::from_hours(1);

/// The grouping state machine, pure (time comes in as unix seconds) so the
/// decision table is unit-testable without a runtime.
#[derive(Default)]
struct Coalescer {
    /// Symptom alerts waiting out the grouping window.
    pending: Vec<Pending>,
    /// Down alerts recently sent or folded (id → when): anything caused by
    /// them folds too while the memory is fresh.
    covered: HashMap<String, i64>,
    /// Monitors whose down alert was folded away: their recovery is too.
    suppressed: HashSet<String>,
    /// Monitors whose current down was announced as local-only.
    quiet: HashMap<String, Quiet>,
    /// Incidents of downs folded since the driver last looked: settled
    /// without a notification, for it to record (see `settle`).
    settled: Vec<i64>,
}

struct Pending {
    deadline: i64,
    /// A symptom may wait once more for a cause that is itself still queued.
    requeued: bool,
    alert: DownAlert,
}

impl Coalescer {
    /// Route a confirmed-down alert. `Some` = send it now.
    fn on_down(&mut self, alert: DownAlert, now: i64, window_secs: u64) -> Option<DownAlert> {
        self.prune(now);
        if alert.local_only {
            // Not an outage: no grouping wait, and it covers nothing - a
            // dependent's real down must not fold into it.
            self.quiet.insert(alert.id.clone(), Quiet::default());
            return Some(alert);
        }
        if let Some(quiet) = self.quiet.get_mut(&alert.id) {
            quiet.confirmed = true;
        }
        if alert.upstreams.is_empty() || window_secs == 0 {
            // A root (nothing above it in the topology), or grouping disabled:
            // send immediately. The impacted list already names the dependents
            // being folded.
            self.mark_sent(&alert.id, now);
            return Some(alert);
        }
        if self.covered_upstream(&alert) {
            self.fold(alert, now);
            return None;
        }
        self.pending.push(Pending {
            deadline: now.saturating_add(i64::try_from(window_secs).unwrap_or(i64::MAX)),
            requeued: false,
            alert,
        });
        None
    }

    /// Whether any of the alert's transitive upstreams recently alerted (or
    /// folded), i.e. this alert is already covered by an incident.
    fn covered_upstream(&self, alert: &DownAlert) -> bool {
        alert
            .upstreams
            .iter()
            .any(|upstream| self.covered.contains_key(upstream.as_str()))
    }

    /// Route a recovery to whoever received the down: folded downs recover
    /// silently, a down still queued is dropped outright (a flap inside the
    /// window), and a local-only down recovers on the quiet channels only.
    fn on_recovered(&mut self, id: &str, now: i64) -> Recipients {
        self.prune(now);
        self.covered.remove(id);
        let quiet = self.quiet.remove(id);
        let usual = if let Some(position) = self.pending.iter().position(|p| p.alert.id == id) {
            self.pending.swap_remove(position);
            false
        } else {
            !self.suppressed.remove(id) && quiet.is_none_or(|quiet| quiet.confirmed)
        };
        match (usual, quiet.is_some()) {
            (true, true) => Recipients::Both,
            (true, false) => Recipients::Usual,
            (false, true) => Recipients::Quiet,
            (false, false) => Recipients::Nobody,
        }
    }

    /// Release every expired alert: folded if an upstream alerted meanwhile,
    /// sent otherwise. An expired alert with an upstream *itself still queued*
    /// waits (once) for that upstream's own verdict.
    fn flush(&mut self, now: i64) -> Vec<DownAlert> {
        self.prune(now);
        let mut out = Vec::new();
        loop {
            // Deadline order, so a queued upstream resolves before dependents.
            let expired = self
                .pending
                .iter()
                .enumerate()
                .filter(|(_, p)| p.deadline <= now)
                .min_by_key(|(_, p)| p.deadline)
                .map(|(index, _)| index);
            let Some(index) = expired else { break };
            let mut pending = self.pending.swap_remove(index);

            let upstream_queued = self
                .pending
                .iter()
                .filter(|p| pending.alert.upstreams.contains(&p.alert.id))
                .map(|p| p.deadline)
                .max();
            if self.covered_upstream(&pending.alert) {
                self.fold(pending.alert, now);
            } else if let Some(upstream_deadline) = upstream_queued
                && !pending.requeued
            {
                pending.requeued = true;
                pending.deadline = upstream_deadline.max(now).saturating_add(1);
                self.pending.push(pending);
            } else {
                self.mark_sent(&pending.alert.id, now);
                out.push(pending.alert);
            }
        }
        out
    }

    /// Empty the queue on shutdown: fold what an alerted upstream covers, send
    /// the rest - dropping confirmed downs on the floor is never right.
    fn drain(&mut self, now: i64) -> Vec<DownAlert> {
        self.prune(now);
        let pending = std::mem::take(&mut self.pending);
        pending
            .into_iter()
            .filter_map(|p| {
                if self.covered_upstream(&p.alert) {
                    self.fold(p.alert, now);
                    None
                } else {
                    self.suppressed.remove(&p.alert.id);
                    Some(p.alert)
                }
            })
            .collect()
    }

    /// A down alert goes out: it covers its dependents, and its recovery must
    /// be announced - even if an earlier, folded down of the same monitor left
    /// it in `suppressed` (a recovery that never reached us, a restarted task).
    fn mark_sent(&mut self, id: &str, now: i64) {
        self.covered.insert(id.to_owned(), now);
        self.suppressed.remove(id);
    }

    /// Forget folded downs of monitors no longer configured: their recovery
    /// will never come, and a monitor re-added under the same id later must
    /// not have its first real recovery swallowed.
    /// Seed the routing of a local-only down announced before a restart.
    fn resume_local_only(&mut self, id: String, confirmed: bool) {
        self.quiet.insert(id, Quiet { confirmed });
    }

    fn retain_monitors(&mut self, known: impl Fn(&str) -> bool) {
        self.suppressed.retain(|id| known(id));
        self.quiet.retain(|id, _| known(id));
    }

    /// Fold a symptom into its (already alerted) cause. The symptom becomes a
    /// cover itself, so a whole dependency chain collapses into one alert.
    fn fold(&mut self, alert: DownAlert, now: i64) {
        info!(monitor = %alert.id, "down alert folded into its root cause");
        self.settled.extend(alert.incident);
        self.covered.insert(alert.id.clone(), now);
        self.suppressed.insert(alert.id);
    }

    fn prune(&mut self, now: i64) {
        self.covered.retain(|_, at| now - *at < CAUSE_MEMORY_SECS);
    }

    fn next_deadline(&self) -> Option<i64> {
        self.pending.iter().map(|p| p.deadline).min()
    }
}

/// Spawn the coalescer: receives alerts from every monitor loop, groups them,
/// and hands the notifications to the per-channel delivery [`Lanes`]. The
/// grouping window is read live from the config (`alerts.group_window_secs`;
/// 0 disables grouping entirely).
///
/// It runs until its inbox closes. On shutdown the monitor loops are still
/// finishing their tick, and a down they confirm then must not be lost: the
/// inbox stays open for [`SHUTDOWN_GRACE`], then whatever waits in the
/// grouping window is released and the lanes get a bounded time to deliver.
pub fn spawn(
    config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    store: Store,
    mut alerts: mpsc::UnboundedReceiver<AlertMsg>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = Coalescer::default();
        let mut lanes = Lanes::new(notifier, store.clone());
        // Set once shutdown began: when the inbox stops being read.
        let mut closing: Option<tokio::time::Instant> = None;
        loop {
            let now = chrono::Utc::now().timestamp();
            let mut sleep_for = state.next_deadline().map_or(IDLE_SWEEP, |deadline| {
                Duration::from_secs(u64::try_from(deadline - now).unwrap_or(0).max(1))
            });
            if let Some(until) = closing {
                let left = until.saturating_duration_since(tokio::time::Instant::now());
                if left.is_zero() {
                    break;
                }
                sleep_for = sleep_for.min(left);
            }
            tokio::select! {
                message = alerts.recv() => {
                    let now = chrono::Utc::now().timestamp();
                    {
                        let config = config.borrow();
                        state.retain_monitors(|id| config.monitors.iter().any(|m| m.id == id));
                    }
                    match message {
                        Some(AlertMsg::Down(alert)) => {
                            let window = config.borrow().alerts.group_window_secs;
                            if let Some(alert) = state.on_down(alert, now, window) {
                                send_down(&mut lanes, &config, alert).await;
                            }
                        }
                        Some(AlertMsg::Recovered { id, name, notify }) => {
                            let recipients = state.on_recovered(&id, now);
                            send_recovered(&mut lanes, &config, &name, notify, recipients);
                        }
                        Some(AlertMsg::ResumedLocalOnly { id, confirmed }) => {
                            state.resume_local_only(id, confirmed);
                        }
                        None => break,
                    }
                }
                () = tokio::time::sleep(sleep_for) => {}
                // A dropped sender reads as "changed" forever: once is enough.
                _ = shutdown.changed(), if closing.is_none() => {
                    closing = Some(tokio::time::Instant::now() + SHUTDOWN_GRACE);
                }
            }
            let now = chrono::Utc::now().timestamp();
            for alert in state.flush(now) {
                send_down(&mut lanes, &config, alert).await;
            }
            settle(&store, &mut state).await;
        }
        for alert in state.drain(chrono::Utc::now().timestamp()) {
            send_down(&mut lanes, &config, alert).await;
        }
        settle(&store, &mut state).await;
        lanes.close().await;
    })
}

/// How long the inbox stays open once shutdown began, for the monitor loops
/// finishing their last tick. The supervisor closes it sooner (it drops the
/// last sender once those loops are done); this bounds the wait when a sender
/// outlives them.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(15);

/// Record the folded downs as settled: they are not announced by design, so a
/// restart must not take their open incident for an alert that never left.
async fn settle(store: &Store, state: &mut Coalescer) {
    for incident in std::mem::take(&mut state.settled) {
        mark_notified(store, incident).await;
    }
}

async fn mark_notified(store: &Store, incident: i64) {
    if let Err(err) = db::mark_incident_notified(store, incident).await {
        error!("failed to record incident {incident} as announced: {err:#}");
    }
}

/// How many notifications one channel may have waiting. A host that falls
/// takes every monitor on it down within seconds, and each is one
/// notification on a channel that may take a second to deliver one: the
/// depth has to hold a whole fleet's downs and recoveries. Beyond it the
/// channel has been stuck for a long while, and the memory spent queueing
/// for it has to stop somewhere (a queued notification is a few hundred
/// bytes).
const LANE_DEPTH: usize = 4096;

/// How long the lanes get to empty on shutdown.
const LANES_CLOSE: Duration = Duration::from_secs(30);

/// One notification queued for a channel.
#[derive(Clone)]
enum Delivery {
    Down(Arc<DownAlert>),
    Recovered { name: Arc<str>, local_only: bool },
}

/// One delivery queue per channel, each drained by its own task. A channel
/// that is slow or silent (every attempt running into its timeout) delays only
/// its own notifications: the coalescer never waits on a delivery, and the
/// alerts behind it reach the healthy channels at once. Within a channel the
/// order is kept, so a recovery never overtakes its down.
struct Lanes {
    notifier: Notifiers,
    store: Store,
    senders: HashMap<String, mpsc::Sender<Delivery>>,
    workers: JoinSet<()>,
    /// The downs a full lane did not take, as (channel, monitor name): the
    /// recovery of an outage a channel was never told about is not sent to
    /// it either. The down itself is not lost for good: its incident stays
    /// unannounced, and the scheduler sends it again (see
    /// `scheduler::REANNOUNCE_SECS`).
    dropped: HashSet<(String, String)>,
    /// [`LANE_DEPTH`], but for the tests.
    depth: usize,
}

impl Lanes {
    fn new(notifier: Notifiers, store: Store) -> Self {
        Self {
            notifier,
            store,
            senders: HashMap::new(),
            workers: JoinSet::new(),
            dropped: HashSet::new(),
            depth: LANE_DEPTH,
        }
    }

    /// Queue `delivery` on every configured channel `routes` selects (all of
    /// them for `None`). Returns whether there was any: with none, nothing
    /// will ever be delivered.
    fn send(&mut self, delivery: &Delivery, routes: Option<&[String]>) -> bool {
        let dispatcher = self.notifier.load_full();
        // A channel removed by a reload: closing its lane ends its task once
        // what it still holds is flushed.
        self.senders
            .retain(|name, _| dispatcher.names().any(|known| known == name));
        let mut targets: Vec<&str> = dispatcher
            .names()
            .filter(|name| routes.is_none_or(|routes| routes.iter().any(|route| route == name)))
            .collect();
        targets.sort_unstable();
        targets.dedup();
        for &name in &targets {
            let sender = if let Some(sender) = self.senders.get(name) {
                sender.clone()
            } else {
                let (sender, inbox) = mpsc::channel(self.depth);
                self.workers.spawn(lane(
                    name.to_owned(),
                    Arc::clone(&self.notifier),
                    self.store.clone(),
                    inbox,
                ));
                self.senders.insert(name.to_owned(), sender.clone());
                sender
            };
            let key = |monitor: &str| (name.to_owned(), monitor.to_owned());
            if let Delivery::Recovered { name: monitor, .. } = delivery
                && self.dropped.remove(&key(monitor))
            {
                info!(channel = %name, monitor = %monitor, "recovery not sent: this channel never got the down");
                continue;
            }
            let sent = sender.try_send(delivery.clone()).is_ok();
            if !sent {
                error!(channel = %name, depth = self.depth, "notification dropped: the queue of this channel is full");
            }
            if let Delivery::Down(alert) = delivery {
                if sent {
                    self.dropped.remove(&key(&alert.name));
                } else {
                    self.dropped.insert(key(&alert.name));
                }
            }
        }
        !targets.is_empty()
    }

    /// Stop accepting and let every lane deliver what it holds, for at most
    /// [`LANES_CLOSE`].
    async fn close(mut self) {
        self.senders.clear();
        let emptied = tokio::time::timeout(LANES_CLOSE, async {
            while self.workers.join_next().await.is_some() {}
        });
        if emptied.await.is_err() {
            error!("shutdown: some notifications were still waiting for a slow channel");
        }
    }
}

/// One channel's delivery task: its queue, in order, one at a time.
async fn lane(
    channel: String,
    notifier: Notifiers,
    store: Store,
    mut inbox: mpsc::Receiver<Delivery>,
) {
    let only = [channel];
    while let Some(delivery) = inbox.recv().await {
        let dispatcher = notifier.load_full();
        let configured = dispatcher.names().any(|name| name == only[0]);
        match delivery {
            Delivery::Down(alert) => {
                let impacted: Vec<&str> = alert.impacted.iter().map(String::as_str).collect();
                let event = Event::Down {
                    monitor: &alert.name,
                    error: alert.error.as_deref(),
                    cause: alert.cause_name.as_deref(),
                    impacted: &impacted,
                    vantage: alert.vantage.as_deref(),
                    event: alert.event.as_deref(),
                    local_only: alert.local_only,
                };
                let failed = dispatcher.dispatch(event, Some(&only)).await;
                // Announced as soon as one channel took it: from here on a
                // restart resumes the outage instead of alerting again.
                if let Some(incident) = alert.incident
                    && configured
                    && failed.is_empty()
                {
                    mark_notified(&store, incident).await;
                }
            }
            Delivery::Recovered { name, local_only } => {
                let event = Event::Recovered {
                    monitor: &name,
                    local_only,
                };
                dispatcher.dispatch(event, Some(&only)).await;
            }
        }
    }
}

/// Where local-only alerts go, as `alerts.notify_unconfirmed` says.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalOnlyRoute {
    /// The key is not set: the monitor's own channels. The default errs on
    /// the side of telling someone - "a peer answered up" must not be enough
    /// to silence a down.
    Monitor,
    /// The quiet channels the operator named.
    Quiet(Vec<String>),
    /// An explicitly empty list: recorded, never sent.
    RecordOnly,
}

/// The local-only routing, read live.
fn local_only_route(config: &watch::Receiver<Arc<Config>>) -> LocalOnlyRoute {
    match &config.borrow().alerts.notify_unconfirmed {
        None => LocalOnlyRoute::Monitor,
        Some(quiet) if quiet.is_empty() => LocalOnlyRoute::RecordOnly,
        Some(quiet) => LocalOnlyRoute::Quiet(quiet.clone()),
    }
}

/// Queue a down for its channels. One that has nowhere to go by design
/// (local-only and record-only, or no channel at all) is settled on its
/// incident right away.
async fn send_down(lanes: &mut Lanes, config: &watch::Receiver<Arc<Config>>, alert: DownAlert) {
    let incident = alert.incident;
    let routes = if alert.local_only {
        match local_only_route(config) {
            LocalOnlyRoute::Monitor => {
                info!(monitor = %alert.id, "down seen from this node only: sent to the monitor's channels, flagged");
                alert.notify.clone()
            }
            LocalOnlyRoute::Quiet(quiet) => {
                info!(monitor = %alert.id, "down seen from this node only: sent to notify_unconfirmed");
                Some(quiet)
            }
            LocalOnlyRoute::RecordOnly => {
                info!(monitor = %alert.id, "down seen from this node only: recorded, not sent");
                if let Some(incident) = incident {
                    mark_notified(&lanes.store, incident).await;
                }
                return;
            }
        }
    } else {
        alert.notify.clone()
    };
    let queued = lanes.send(&Delivery::Down(Arc::new(alert)), routes.as_deref());
    if let Some(incident) = incident
        && !queued
    {
        mark_notified(&lanes.store, incident).await;
    }
}

/// Announce a recovery to whoever received the down (see [`Recipients`]).
fn send_recovered(
    lanes: &mut Lanes,
    config: &watch::Receiver<Arc<Config>>,
    name: &str,
    notify: Option<Vec<String>>,
    recipients: Recipients,
) {
    let routes = match (recipients, local_only_route(config)) {
        (Recipients::Nobody, _) | (Recipients::Quiet, LocalOnlyRoute::RecordOnly) => return,
        // The local-only down went to the monitor's own channels, so that is
        // where "whoever received it" is, confirmed later or not.
        (Recipients::Usual, _)
        | (Recipients::Quiet | Recipients::Both, LocalOnlyRoute::Monitor)
        | (Recipients::Both, LocalOnlyRoute::RecordOnly) => notify,
        (Recipients::Quiet, LocalOnlyRoute::Quiet(quiet)) => Some(quiet),
        // `None` is every channel, which includes the quiet ones.
        (Recipients::Both, LocalOnlyRoute::Quiet(quiet)) => notify.map(|mut routes| {
            for route in quiet {
                if !routes.contains(&route) {
                    routes.push(route);
                }
            }
            routes
        }),
    };
    lanes.send(
        &Delivery::Recovered {
            name: Arc::from(name),
            local_only: recipients == Recipients::Quiet,
        },
        routes.as_deref(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down(id: &str, upstreams: &[&str]) -> DownAlert {
        DownAlert {
            id: id.to_owned(),
            name: id.to_uppercase(),
            error: None,
            upstreams: upstreams.iter().map(|&u| u.to_owned()).collect(),
            cause_name: upstreams.first().map(|u| u.to_uppercase()),
            impacted: Vec::new(),
            notify: None,
            vantage: None,
            event: None,
            local_only: false,
            incident: None,
        }
    }

    fn local(id: &str, upstreams: &[&str]) -> DownAlert {
        DownAlert {
            local_only: true,
            ..down(id, upstreams)
        }
    }

    #[test]
    fn a_local_only_down_skips_grouping_and_covers_nothing() {
        let mut c = Coalescer::default();
        // Out at once (to the quiet channels), even with an upstream.
        assert!(c.on_down(local("db", &[]), 100, 30).is_some());
        assert!(c.on_down(local("api", &["db"]), 100, 30).is_some());
        // A dependent's real down is not folded into a local-only one.
        assert!(c.on_down(down("web", &["db"]), 110, 30).is_none());
        assert_eq!(c.flush(141).len(), 1);
        // Recoveries go back to whoever received the down.
        assert_eq!(c.on_recovered("db", 200), Recipients::Quiet);
        assert_eq!(c.on_recovered("web", 200), Recipients::Usual);
    }

    #[test]
    fn a_local_only_down_confirmed_later_recovers_everywhere() {
        let mut c = Coalescer::default();
        assert!(c.on_down(local("db", &[]), 100, 30).is_some());
        // Re-asked, the peers now see it down too: the real alert goes out.
        assert!(c.on_down(down("db", &[]), 400, 30).is_some());
        assert_eq!(c.on_recovered("db", 500), Recipients::Both);
        // The next incident starts clean.
        assert!(c.on_down(down("db", &[]), 600, 30).is_some());
        assert_eq!(c.on_recovered("db", 700), Recipients::Usual);
    }

    #[test]
    fn roots_send_immediately_and_cover_their_dependents() {
        let mut c = Coalescer::default();
        // Root: out immediately.
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        // Dependent of a covered upstream: folded on arrival.
        assert!(c.on_down(down("api", &["db"]), 110, 30).is_none());
        assert!(c.flush(1000).is_empty());
        // The folded dependent's recovery is silent; the root's is not.
        assert_eq!(Recipients::Nobody, c.on_recovered("api", 120));
        assert_eq!(Recipients::Usual, c.on_recovered("db", 130));
    }

    #[test]
    fn dependent_waits_then_sends_when_no_upstream_alerts() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("api", &["db"]), 100, 30).is_none());
        assert!(c.flush(120).is_empty()); // window still open
        let sent = c.flush(131);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].id, "api");
        // Its recovery is announced normally.
        assert_eq!(Recipients::Usual, c.on_recovered("api", 200));
    }

    #[test]
    fn dependent_folds_when_upstream_alerts_within_window() {
        let mut c = Coalescer::default();
        // The dependent confirms FIRST - the realistic cascade race, where db
        // does not even have enough recorded failures yet to be derived down.
        assert!(c.on_down(down("api", &["db"]), 100, 30).is_none());
        assert!(c.on_down(down("db", &[]), 110, 30).is_some());
        assert!(c.flush(131).is_empty(), "api folded into db");
        assert_eq!(Recipients::Nobody, c.on_recovered("api", 200));
    }

    #[test]
    fn cascades_collapse_into_one_alert() {
        let mut c = Coalescer::default();
        // db -> api -> web chain: only db's alert goes out.
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        assert!(c.on_down(down("api", &["db"]), 105, 30).is_none());
        assert!(c.on_down(down("web", &["api", "db"]), 140, 30).is_none());
        assert!(c.flush(200).is_empty());
    }

    #[test]
    fn flap_within_window_sends_nothing() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("api", &["db"]), 100, 30).is_none());
        // Recovered before the window closed: drop the down AND the recovery.
        assert_eq!(Recipients::Nobody, c.on_recovered("api", 110));
        assert!(c.flush(131).is_empty());
    }

    #[test]
    fn dependent_waits_for_a_queued_upstream() {
        let mut c = Coalescer::default();
        // web (upstream api) expires before api (upstream db) does.
        assert!(c.on_down(down("web", &["api", "db"]), 100, 10).is_none());
        assert!(c.on_down(down("api", &["db"]), 105, 10).is_none());
        // At 111 web expires but api is still queued: web waits once more.
        assert!(c.flush(111).is_empty());
        // At 116 api expires (db never alerted): api sends, then web folds.
        let sent = c.flush(116);
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].id, "api");
        assert!(c.flush(200).is_empty());
        assert_eq!(Recipients::Nobody, c.on_recovered("web", 300));
    }

    #[test]
    fn a_sent_down_clears_a_stale_suppression() {
        let mut c = Coalescer::default();
        // api's down folds into db's: api is suppressed...
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        assert!(c.on_down(down("api", &["db"]), 105, 30).is_none());
        // ...and its recovery never arrives (the old task was replaced).
        // Much later api goes down on its own and the alert is sent: its
        // recovery must be announced, not swallowed by the stale entry.
        let late = 105 + CAUSE_MEMORY_SECS + 1;
        assert!(c.on_down(down("api", &["db"]), late, 30).is_none());
        assert_eq!(c.flush(late + 31).len(), 1);
        assert_eq!(Recipients::Usual, c.on_recovered("api", late + 100));
    }

    #[test]
    fn removed_monitors_are_forgotten() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        assert!(c.on_down(down("api", &["db"]), 105, 30).is_none());
        // api leaves the config: its suppression goes with it.
        c.retain_monitors(|id| id == "db");
        assert!(c.suppressed.is_empty());
    }

    #[test]
    fn window_zero_disables_grouping() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("api", &["db"]), 100, 0).is_some());
    }

    #[test]
    fn cause_memory_expires() {
        let mut c = Coalescer::default();
        assert!(c.on_down(down("db", &[]), 100, 30).is_some());
        // Much later (memory expired), a dependent alerts on its own.
        let late = 100 + CAUSE_MEMORY_SECS + 1;
        assert!(c.on_down(down("api", &["db"]), late, 30).is_none());
        let sent = c.flush(late + 31);
        assert_eq!(sent.len(), 1);
    }

    /// A coalescer over two real channels, `main` (the monitor's own) and
    /// `quiet`, for the local-only routing tests.
    struct Routed {
        main: crate::testing::Posts,
        quiet: crate::testing::Posts,
        tx: mpsc::UnboundedSender<AlertMsg>,
        config_tx: watch::Sender<Arc<Config>>,
        shutdown_tx: watch::Sender<bool>,
        task: JoinHandle<()>,
        toml: Box<dyn Fn(&str) -> String>,
    }

    impl Routed {
        /// `unconfirmed`: the `[alerts]` line setting `notify_unconfirmed`,
        /// or nothing.
        async fn new(unconfirmed: &str) -> Self {
            let (main_url, main) = crate::testing::webhook_sink().await;
            let (quiet_url, quiet) = crate::testing::webhook_sink().await;
            let toml = Box::new(move |unconfirmed: &str| {
                format!(
                    r#"
                    [page]
                    [server]
                    [alerts]
                    group_window_secs = 0
                    {unconfirmed}
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
                )
            });
            let config = Arc::new(crate::config::parse(&toml(unconfirmed)).unwrap());
            let client = crate::http::client(None).unwrap();
            let notifier = crate::notifications::shared(&config, &client);
            let (config_tx, config_rx) = watch::channel(Arc::clone(&config));
            let (tx, rx) = mpsc::unbounded_channel();
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let task = spawn(
                config_rx,
                notifier,
                Store::in_memory().await,
                rx,
                shutdown_rx,
            );
            Self {
                main,
                quiet,
                tx,
                config_tx,
                shutdown_tx,
                task,
                toml,
            }
        }

        fn reload(&self, unconfirmed: &str) {
            self.config_tx
                .send(Arc::new(
                    crate::config::parse(&(self.toml)(unconfirmed)).unwrap(),
                ))
                .unwrap();
        }

        fn down(&self, local_only: bool) {
            self.tx
                .send(AlertMsg::Down(DownAlert {
                    name: "API".to_owned(),
                    notify: Some(vec!["main".to_owned()]),
                    local_only,
                    ..down("api", &[])
                }))
                .unwrap();
        }

        fn recovered(&self) {
            self.tx
                .send(AlertMsg::Recovered {
                    id: "api".to_owned(),
                    name: "API".to_owned(),
                    notify: Some(vec!["main".to_owned()]),
                })
                .unwrap();
        }

        /// Wait until the coalescer has dispatched everything sent so far.
        async fn settle(&self) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        /// Shutdown, then the inbox closing, ends it at once.
        async fn close(self) {
            self.shutdown_tx.send(true).unwrap();
            drop(self.tx);
            self.task.await.unwrap();
        }
    }

    /// The whole route, through real channels: with quiet channels named, a
    /// local-only down reaches them only (flagged in the webhook), its
    /// confirmation by the peers reaches the usual one, and each recovery
    /// goes to whoever got a down.
    #[tokio::test]
    async fn local_only_downs_page_the_quiet_channel_only() {
        let routed = Routed::new(r#"notify_unconfirmed = ["quiet"]"#).await;
        let (main, quiet) = (&routed.main, &routed.quiet);

        // Seen up by the peers: the quiet channel only, flagged.
        routed.down(true);
        routed.settle().await;
        assert_eq!(crate::testing::events(main), Vec::<String>::new());
        assert_eq!(crate::testing::events(quiet), ["down"]);
        assert_eq!(quiet.lock().unwrap()[0]["local_only"], true);
        // Its recovery: the quiet channel only, flagged too.
        routed.recovered();
        routed.settle().await;
        assert_eq!(crate::testing::events(main), Vec::<String>::new());
        assert_eq!(crate::testing::events(quiet), ["down", "recovered"]);
        assert_eq!(quiet.lock().unwrap()[1]["local_only"], true);

        // Local-only, then confirmed when the peers are asked again: one
        // real down on the usual channel; the recovery reaches both.
        routed.down(true);
        routed.down(false);
        routed.recovered();
        routed.settle().await;
        assert_eq!(crate::testing::events(main), ["down", "recovered"]);
        assert_eq!(main.lock().unwrap()[0]["local_only"], false);
        assert_eq!(
            crate::testing::events(quiet),
            ["down", "recovered", "down", "recovered"]
        );

        // An explicitly empty notify_unconfirmed: only recorded.
        routed.reload("notify_unconfirmed = []");
        routed.down(true);
        routed.recovered();
        routed.settle().await;
        assert_eq!(crate::testing::events(main).len(), 2);
        assert_eq!(crate::testing::events(quiet).len(), 4);
        routed.close().await;
    }

    /// Without `notify_unconfirmed` a local-only down is never silent: a
    /// peer's "up" alone must not hide it. It reaches the monitor's own
    /// channels, flagged, and so does its recovery.
    #[tokio::test]
    async fn local_only_downs_default_to_the_monitors_own_channels() {
        let routed = Routed::new("").await;
        let (main, quiet) = (&routed.main, &routed.quiet);

        routed.down(true);
        routed.recovered();
        routed.settle().await;
        assert_eq!(crate::testing::events(main), ["down", "recovered"]);
        assert_eq!(main.lock().unwrap()[0]["local_only"], true);
        assert_eq!(main.lock().unwrap()[1]["local_only"], true);

        // Confirmed later: the real down follows on the same channels, and
        // the recovery is no longer a local-only one.
        routed.down(true);
        routed.down(false);
        routed.recovered();
        routed.settle().await;
        let sent = main.lock().unwrap().clone();
        assert_eq!(sent.len(), 5);
        assert_eq!(sent[2]["local_only"], true);
        assert_eq!(sent[3]["local_only"], false);
        assert_eq!(sent[4]["local_only"], false);

        // The quiet channel is only one more channel until it is named.
        assert_eq!(crate::testing::events(quiet), Vec::<String>::new());
        routed.close().await;
    }

    /// A channel that accepts the connection and never answers holds only its
    /// own lane: the other channel gets every alert at once, in order, and
    /// the incident is marked announced as soon as that one took the down.
    #[tokio::test]
    async fn a_silent_channel_delays_only_its_own_lane() {
        let (healthy_url, healthy) = crate::testing::webhook_sink().await;
        // Accepts, reads nothing, answers nothing.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent_url = format!("http://{}/hook", silent.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = silent.accept().await {
                held.push(socket);
            }
        });
        let config = Arc::new(
            crate::config::parse(&format!(
                r#"
                [page]
                [server]
                [[channels]]
                name = "healthy"
                type = "webhook"
                url = "{healthy_url}"
                [[channels]]
                name = "silent"
                type = "webhook"
                url = "{silent_url}"
                [[monitors]]
                id = "api"
                name = "API"
                target = "https://example.com"
                interval_secs = 60
                "#
            ))
            .unwrap(),
        );
        let store = Store::in_memory().await;
        let incident = db::insert_incident_start(&store, "api", None, None, None, &[], None, None)
            .await
            .unwrap();
        let client = crate::http::client(None).unwrap();
        let notifier = crate::notifications::shared(&config, &client);
        let (_config_tx, config_rx) = watch::channel(Arc::clone(&config));
        let (tx, rx) = mpsc::unbounded_channel();
        let (_shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = spawn(config_rx, notifier, store.clone(), rx, shutdown_rx);

        tx.send(AlertMsg::Down(DownAlert {
            incident: Some(incident),
            ..down("api", &[])
        }))
        .unwrap();
        tx.send(AlertMsg::Recovered {
            id: "api".to_owned(),
            name: "API".to_owned(),
            notify: None,
        })
        .unwrap();
        tx.send(AlertMsg::Down(down("api", &[]))).unwrap();

        // Well inside the silent channel's first timeout (15s per attempt).
        let started = std::time::Instant::now();
        while crate::testing::events(&healthy).len() < 3 {
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "the healthy channel waited on the silent one"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            crate::testing::events(&healthy),
            ["down", "recovered", "down"]
        );
        for _ in 0..100 {
            if db::incident_notified(&store, incident).await.unwrap() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(db::incident_notified(&store, incident).await.unwrap());
        // The silent lane is still on its first delivery: don't wait for it.
        task.abort();
    }

    /// A lane that is full refuses a down; the recovery of that outage is
    /// then not sent to that channel either, and a later down that does get
    /// in clears the slate.
    #[tokio::test]
    async fn a_recovery_is_not_sent_where_its_down_was_dropped() {
        // Accepts, reads nothing, answers nothing: the lane's worker stays
        // on its first delivery.
        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent_url = format!("http://{}/hook", silent.local_addr().unwrap());
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = silent.accept().await {
                held.push(socket);
            }
        });
        let config = Arc::new(
            crate::config::parse(&format!(
                r#"
                [page]
                [server]
                [[channels]]
                name = "silent"
                type = "webhook"
                url = "{silent_url}"
                "#
            ))
            .unwrap(),
        );
        let client = crate::http::client(None).unwrap();
        let notifier = crate::notifications::shared(&config, &client);
        let mut lanes = Lanes::new(notifier, Store::in_memory().await);
        lanes.depth = 1;
        let named = |name: &str| {
            Delivery::Down(Arc::new(DownAlert {
                name: name.to_owned(),
                ..down("api", &[])
            }))
        };
        let recovered = |name: &str| Delivery::Recovered {
            name: Arc::from(name),
            local_only: false,
        };
        let key = |name: &str| ("silent".to_owned(), name.to_owned());

        lanes.send(&named("First"), None);
        // Let the worker take it, so the queue's one place is free again.
        tokio::time::sleep(Duration::from_millis(200)).await;
        lanes.send(&named("Second"), None);
        assert!(lanes.dropped.is_empty());

        lanes.send(&named("Third"), None);
        assert!(lanes.dropped.contains(&key("Third")));
        // Its recovery is not queued behind a down that never was.
        lanes.send(&recovered("Third"), None);
        assert!(lanes.dropped.is_empty());
        // A monitor whose down got in keeps its recovery owed (here it
        // finds the queue full, which is the channel's trouble, not a rule).
        lanes.send(&recovered("Second"), None);
        assert!(lanes.dropped.is_empty());
        lanes.workers.abort_all();
    }
}
