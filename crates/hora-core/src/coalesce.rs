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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use hora_notify::Event;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::info;

use crate::config::Config;
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
        if alert.upstreams.is_empty() || window_secs == 0 {
            // A root (nothing above it in the topology), or grouping disabled:
            // send immediately. The impacted list already names the dependents
            // being folded.
            self.covered.insert(alert.id.clone(), now);
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

    /// Route a recovery. `true` = send it; folded downs recover silently, and
    /// a down still queued is dropped outright (a flap inside the window).
    fn on_recovered(&mut self, id: &str, now: i64) -> bool {
        self.prune(now);
        self.covered.remove(id);
        if let Some(position) = self.pending.iter().position(|p| p.alert.id == id) {
            self.pending.swap_remove(position);
            return false;
        }
        !self.suppressed.remove(id)
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
                self.covered.insert(pending.alert.id.clone(), now);
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
                    Some(p.alert)
                }
            })
            .collect()
    }

    /// Fold a symptom into its (already alerted) cause. The symptom becomes a
    /// cover itself, so a whole dependency chain collapses into one alert.
    fn fold(&mut self, alert: DownAlert, now: i64) {
        info!(monitor = %alert.id, "down alert folded into its root cause");
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

/// Spawn the coalescer: receives alerts from every monitor loop, dispatches
/// the grouped notifications. The grouping window is read live from the
/// config (`alerts.group_window_secs`; 0 disables grouping entirely).
pub fn spawn(
    config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    mut alerts: mpsc::UnboundedReceiver<AlertMsg>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = Coalescer::default();
        loop {
            let now = chrono::Utc::now().timestamp();
            let sleep_for = state.next_deadline().map_or(IDLE_SWEEP, |deadline| {
                Duration::from_secs(u64::try_from(deadline - now).unwrap_or(0).max(1))
            });
            tokio::select! {
                message = alerts.recv() => {
                    let now = chrono::Utc::now().timestamp();
                    match message {
                        Some(AlertMsg::Down(alert)) => {
                            let window = config.borrow().alerts.group_window_secs;
                            if let Some(alert) = state.on_down(alert, now, window) {
                                send_down(&notifier, &alert).await;
                            }
                        }
                        Some(AlertMsg::Recovered { id, name, notify }) => {
                            if state.on_recovered(&id, now) {
                                notifier
                                    .load_full()
                                    .dispatch(Event::Recovered { monitor: &name }, notify.as_deref())
                                    .await;
                            }
                        }
                        None => break,
                    }
                }
                () = tokio::time::sleep(sleep_for) => {}
                _ = shutdown.changed() => break,
            }
            let now = chrono::Utc::now().timestamp();
            for alert in state.flush(now) {
                send_down(&notifier, &alert).await;
            }
        }
        for alert in state.drain(chrono::Utc::now().timestamp()) {
            send_down(&notifier, &alert).await;
        }
    })
}

async fn send_down(notifier: &Notifiers, alert: &DownAlert) {
    let impacted: Vec<&str> = alert.impacted.iter().map(String::as_str).collect();
    notifier
        .load_full()
        .dispatch(
            Event::Down {
                monitor: &alert.name,
                error: alert.error.as_deref(),
                cause: alert.cause_name.as_deref(),
                impacted: &impacted,
                vantage: alert.vantage.as_deref(),
            },
            alert.notify.as_deref(),
        )
        .await;
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
        }
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
        assert!(!c.on_recovered("api", 120));
        assert!(c.on_recovered("db", 130));
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
        assert!(c.on_recovered("api", 200));
    }

    #[test]
    fn dependent_folds_when_upstream_alerts_within_window() {
        let mut c = Coalescer::default();
        // The dependent confirms FIRST - the realistic cascade race, where db
        // does not even have enough recorded failures yet to be derived down.
        assert!(c.on_down(down("api", &["db"]), 100, 30).is_none());
        assert!(c.on_down(down("db", &[]), 110, 30).is_some());
        assert!(c.flush(131).is_empty(), "api folded into db");
        assert!(!c.on_recovered("api", 200));
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
        assert!(!c.on_recovered("api", 110));
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
        assert!(!c.on_recovered("web", 300));
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
}
