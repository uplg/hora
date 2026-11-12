//! The shared edge trigger of the "alert once" watchers: certificate expiry,
//! unreadable certificates and domain expiry (the release watch keeps its own
//! `release_watch.notified` column and shares [`delivered`]).
//!
//! Each watcher used to remember what it had warned about in a map of its own,
//! in memory: every restart sent every expiring-certificate and domain alert
//! again, and the map was marked whether or not a channel had accepted the
//! alert, so one lost to a failing channel was not retried before the next
//! restart. The mark now lives in the `meta` table
//! (`alerted:<watch>:<monitor id>` = the stage alerted for) and is written
//! only once the alert was delivered:
//!
//! - [`should_alert`] is true while no alert was delivered for this stage;
//! - [`mark_delivered`] records the stage, after a delivery (see [`delivered`]);
//! - [`rearm`] forgets it once the condition is gone (the certificate was
//!   renewed, the read works again), so the next occurrence alerts again.
//!
//! The stage is what a watcher escalates on: alerting for a stage other than
//! the recorded one fires again. Today every watcher has a single stage (one
//! warning threshold each).
//!
//! The marks are per-monitor state, swept with the monitor's rows when it
//! leaves the configuration (see `db::retention`).

use hora_notify::{Dispatcher, Event};
use tracing::warn;

use crate::db::{self, Store};
use crate::notifications::Notifiers;

/// One alert-once watcher: the namespace of its marks in `meta`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Watch {
    /// The certificate is inside the `alerts.cert_expiry_days` window.
    CertExpiry,
    /// The certificate could not be read.
    CertUnreadable,
    /// The domain is inside the `alerts.domain_expiry_days` window.
    DomainExpiry,
}

impl Watch {
    /// The `meta` key prefix of this watcher's marks; the monitor id follows.
    const fn prefix(self) -> &'static str {
        match self {
            Self::CertExpiry => "alerted:cert_expiry:",
            Self::CertUnreadable => "alerted:cert_unreadable:",
            Self::DomainExpiry => "alerted:domain_expiry:",
        }
    }

    fn key(self, monitor_id: &str) -> String {
        format!("{}{monitor_id}", self.prefix())
    }
}

/// Every prefix [`Watch`] writes under, for the orphan sweep.
pub(crate) const META_PREFIXES: [&str; 3] = [
    Watch::CertExpiry.prefix(),
    Watch::CertUnreadable.prefix(),
    Watch::DomainExpiry.prefix(),
];

/// The only stage of the watchers that have a single threshold.
pub(crate) const STAGE_WARNING: &str = "warning";

/// Whether `watch` still owes `monitor_id` an alert for `stage`: none was
/// delivered for it since the last [`rearm`]. A mark that cannot be read
/// answers no: an unreadable database must not turn every sweep into a repeat
/// of every alert, and the next sweep asks again.
pub(crate) async fn should_alert(
    store: &Store,
    watch: Watch,
    monitor_id: &str,
    stage: &str,
) -> bool {
    match db::meta_get(store, &watch.key(monitor_id)).await {
        Ok(recorded) => recorded.as_deref() != Some(stage),
        Err(err) => {
            warn!(monitor = %monitor_id, "failed to read the {watch:?} alert mark: {err:#}");
            false
        }
    }
}

/// Record that the alert for `stage` was delivered, so neither the next sweep
/// nor the next start sends it again.
pub(crate) async fn mark_delivered(store: &Store, watch: Watch, monitor_id: &str, stage: &str) {
    if let Err(err) = db::meta_set(store, &watch.key(monitor_id), stage).await {
        warn!(monitor = %monitor_id, "failed to record the {watch:?} alert: {err:#}");
    }
}

/// Forget the mark: the condition is gone, its next occurrence alerts again.
pub(crate) async fn rearm(store: &Store, watch: Watch, monitor_id: &str) {
    if let Err(err) = db::meta_delete(store, &watch.key(monitor_id)).await {
        warn!(monitor = %monitor_id, "failed to clear the {watch:?} alert mark: {err:#}");
    }
}

/// Whether a dispatch that reported `failed` reached someone: at least one of
/// the channels it targeted accepted the event. With no channel to target
/// (none configured, or `only` naming none that is) there is nobody to retry
/// for, and the alert counts as delivered - like an incident settled without
/// a notification.
pub(crate) fn delivered(dispatcher: &Dispatcher, only: Option<&[String]>, failed: &[&str]) -> bool {
    let targeted = dispatcher
        .names()
        .filter(|name| only.is_none_or(|only| only.iter().any(|wanted| wanted == name)))
        .count();
    targeted == 0 || failed.len() < targeted
}

/// Dispatch `event` to the monitor's channels and say whether it was
/// [`delivered`]. A total failure is logged here, once for all the watchers.
pub(crate) async fn dispatch(
    notifier: &Notifiers,
    event: Event<'_>,
    only: Option<&[String]>,
    monitor_id: &str,
) -> bool {
    let dispatcher = notifier.load_full();
    let failed = dispatcher.dispatch(event, only).await;
    let delivered = delivered(&dispatcher, only, &failed);
    if !delivered {
        warn!(
            monitor = %monitor_id,
            "alert failed on every channel ({}); retrying at the next check",
            failed.join(", ")
        );
    }
    delivered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stage_alerts_until_delivered_then_once_per_occurrence() {
        let store = Store::in_memory().await;
        let watch = Watch::CertExpiry;
        assert!(should_alert(&store, watch, "api", STAGE_WARNING).await);
        // Not delivered: nothing recorded, the next sweep asks again.
        assert!(should_alert(&store, watch, "api", STAGE_WARNING).await);

        mark_delivered(&store, watch, "api", STAGE_WARNING).await;
        assert!(!should_alert(&store, watch, "api", STAGE_WARNING).await);
        // Another stage, another monitor and another watcher are not covered.
        assert!(should_alert(&store, watch, "api", "expired").await);
        assert!(should_alert(&store, watch, "web", STAGE_WARNING).await);
        assert!(should_alert(&store, Watch::DomainExpiry, "api", STAGE_WARNING).await);

        // Renewed, then expiring again.
        rearm(&store, watch, "api").await;
        assert!(should_alert(&store, watch, "api", STAGE_WARNING).await);
    }

    #[test]
    fn delivered_means_one_targeted_channel_accepted() {
        // Never sent to: only the routing names matter here.
        let channel = |name: &str| -> (String, Box<dyn hora_notify::Notifier>) {
            let hook = hora_notify::WebhookNotifier::new(
                reqwest::Client::new(),
                "http://127.0.0.1:1/".to_owned(),
            );
            (name.to_owned(), Box::new(hook))
        };
        let dispatcher = Dispatcher::new(vec![channel("ops"), channel("oncall")], 3);
        let only = |names: &[&str]| names.iter().map(|&n| n.to_owned()).collect::<Vec<_>>();
        assert!(delivered(&dispatcher, None, &[]));
        assert!(delivered(&dispatcher, None, &["ops"]));
        assert!(!delivered(&dispatcher, None, &["ops", "oncall"]));
        assert!(!delivered(&dispatcher, Some(&only(&["ops"])), &["ops"]));
        assert!(delivered(&dispatcher, Some(&only(&["ops"])), &[]));
        // Nobody to tell: nothing to retry for.
        assert!(delivered(&dispatcher, Some(&only(&["gone"])), &[]));
        assert!(delivered(&Dispatcher::default(), None, &[]));
    }
}
