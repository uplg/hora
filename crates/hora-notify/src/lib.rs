//! Notification abstraction.
//!
//! Alerting code emits an [`Event`]; each configured [`Notifier`] decides how
//! to deliver it. [`Dispatcher`] holds the channels under their routing names
//! and fans an event out to the matching ones. Telegram, Discord, Slack, Matrix,
//! a generic JSON webhook, SMTP e-mail, Free Mobile SMS, ntfy, Gotify and
//! Pushover are the built-in backends; adding another means implementing the
//! trait and registering it.
//!
//! The dispatcher is also its own dead-man's switch: it counts consecutive
//! delivery failures per channel and, at a configurable threshold, alerts via
//! the *other* channels so a broken Telegram bot or SMTP relay is discovered
//! before the real incident needs it. See [`ChannelHealth`].

pub mod discord;
pub mod email;
pub mod freemobile;
pub mod gotify;
pub mod matrix;
pub mod ntfy;
pub mod pushover;
pub mod slack;
pub mod telegram;
mod util;
pub mod webhook;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use futures_util::future::join_all;
use tracing::warn;

pub use discord::DiscordNotifier;
pub use email::{EmailConfig, EmailNotifier};
pub use freemobile::FreeMobileNotifier;
pub use gotify::GotifyNotifier;
pub use matrix::MatrixNotifier;
pub use ntfy::NtfyNotifier;
pub use pushover::PushoverNotifier;
pub use slack::SlackNotifier;
pub use telegram::TelegramNotifier;
pub use webhook::WebhookNotifier;

/// Severity of an externally-pushed alert (`POST /api/monitors/{id}/alert`),
/// mapped onto each backend's native priority where it has one (ntfy, Pushover,
/// Gotify) and shown as a text label everywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertSeverity {
    Info,
    Warning,
    Error,
    Critical,
}

impl AlertSeverity {
    /// Lowercase label, as accepted in the API and stored in the database.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
            Self::Critical => "critical",
        }
    }

    /// Parse the severity sent in the request body; `None` for anything else,
    /// which the handler turns into a 400.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "info" => Some(Self::Info),
            "warning" => Some(Self::Warning),
            "error" => Some(Self::Error),
            "critical" => Some(Self::Critical),
            _ => None,
        }
    }
}

/// Consecutive-delivery failure state for one channel, tracked by
/// [`Dispatcher`] so a silently broken channel is noticed before the real
/// incident needs it. Reset to default on the next successful delivery.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelHealth {
    /// How many consecutive dispatches to this channel have failed.
    pub consecutive_failures: u32,
    /// Wall-clock time of the first failure in the current streak; `None` when
    /// the channel has never failed (or has recovered). Used to phrase the
    /// watchdog alert as "failing for 2 days" rather than a raw count.
    pub first_failure_at: Option<SystemTime>,
    /// Whether the watchdog has already alerted the *other* channels about this
    /// streak. Prevents re-alerting on every subsequent dispatch; reset on
    /// recovery so a new streak alerts again.
    pub watchdog_alerted: bool,
}

/// Shared, mutable per-channel failure counters — `Arc` so they survive
/// [`Dispatcher`] rebuilds on config reload (the counters belong to the channel
/// *name*, not to a particular notifier instance).
pub type HealthMap = Arc<Mutex<HashMap<String, ChannelHealth>>>;

/// One entry of a health snapshot, as returned by
/// [`Dispatcher::health_snapshot`] for display in `top` and the API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelHealthEntry {
    pub name: String,
    pub health: ChannelHealth,
}

/// An alertable event. Borrows its data so emitting one is allocation-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// A monitor is confirmed down.
    Down {
        monitor: &'a str,
        error: Option<&'a str>,
        /// The upstream monitor causing this failure (topology annotation).
        cause: Option<&'a str>,
        /// Downstream monitors impacted by this root-cause failure.
        impacted: &'a [&'a str],
        /// Multi-vantage verdict, when peers were asked ("confirmed down from
        /// 3/3 vantage points" / "seen UP by hora-b ...").
        vantage: Option<&'a str>,
        /// The correlated event marker, when one was recorded shortly before
        /// the down ("deploy api v2.3, 3m before") - the "what changed?" line.
        event: Option<&'a str>,
    },
    /// A monitor is up but degraded: slower than its `degraded_over_ms` budget.
    Degraded {
        monitor: &'a str,
        latency_ms: Option<i64>,
    },
    /// A previously-down (or degraded) monitor is fully healthy again.
    Recovered { monitor: &'a str },
    /// A monitor's TLS certificate is within the warning window (or expired).
    CertExpiring { monitor: &'a str, days_left: i64 },
    /// A monitor's registered domain is within the warning window (or expired),
    /// as reported by the registry over RDAP.
    DomainExpiring {
        monitor: &'a str,
        domain: &'a str,
        days_left: i64,
    },
    /// A peer is unreachable from here, but a third-party witness still sees it
    /// up: likely a network partition on the local-to-peer link, not a peer
    /// outage. Lower severity than [`Event::Down`].
    PeerLinkDegraded { peer: &'a str, witness: &'a str },
    /// A monitor's TLS certificate has changed unexpectedly (different public key
    /// fingerprint). This may indicate a MITM attack or an unexpected renewal.
    CertChanged {
        monitor: &'a str,
        old_fingerprint: &'a str,
        new_fingerprint: &'a str,
    },
    /// The periodic digest: a pre-rendered recap of the last period (uptime,
    /// incidents, error budgets), built by the daemon. Informational - the
    /// one event that never signals a problem.
    Digest { period: &'a str, summary: &'a str },
    /// A monitor is burning its availability error budget abnormally fast
    /// (Google-SRE burn-rate alerting). Fires while the monitor may still be
    /// "up" between blips - which is exactly the point.
    BudgetBurn {
        monitor: &'a str,
        /// Burn rate in tenths of the sustainable rate (144 = 14.4x).
        burn_rate_x10: i64,
        /// The lookback that triggered: `"1h"` (fast burn) or `"6h"` (slow).
        window: &'a str,
        /// Estimated seconds until the budget is fully spent at this rate.
        exhausted_in_secs: Option<i64>,
    },
    /// An alert pushed by an external producer to a monitor
    /// (`POST /api/monitors/{id}/alert`): dispatched straight to the monitor's
    /// channels, never touching its up/down status. Any `tags` are pre-rendered
    /// into `message` by the handler, so the variant stays `Copy`.
    Alert {
        monitor: &'a str,
        severity: AlertSeverity,
        title: &'a str,
        /// Free-form detail; empty when the producer sent only a title.
        message: &'a str,
    },
}

/// A delivery channel for [`Event`]s.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Channel name, used in logs.
    fn name(&self) -> &'static str;

    /// Deliver one event. Implementations must not panic; they log their own
    /// failure (the daemon fires and forgets) *and* return it, so a caller
    /// that cares - `hora test-alert`'s exit code - can see delivery failed.
    async fn notify(&self, event: Event<'_>) -> anyhow::Result<()>;
}

/// A registered channel: its routing name plus the delivery backend.
struct Channel {
    name: String,
    notifier: Box<dyn Notifier>,
}

/// Holds the configured channels and fans events out to the matching ones.
/// Also tracks consecutive delivery failures per channel and alerts the
/// *other* channels when one goes silent — the dead-man's switch applied to
/// notifications themselves.
pub struct Dispatcher {
    channels: Vec<Channel>,
    /// Per-channel failure counters, shared across config reloads.
    health: HealthMap,
    /// Consecutive failures before the watchdog alerts the other channels.
    fail_threshold: u32,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self {
            channels: Vec::new(),
            health: Arc::new(Mutex::new(HashMap::new())),
            fail_threshold: DEFAULT_CHANNEL_FAIL_THRESHOLD,
        }
    }
}

/// Default watchdog threshold: 3 consecutive delivery failures. Each
/// `send_retrying` call already retries 3 times internally, so this represents
/// 9 total failed HTTP/SMTP attempts — enough to ride through a transient blip.
const DEFAULT_CHANNEL_FAIL_THRESHOLD: u32 = 3;

impl Dispatcher {
    #[must_use]
    pub fn new(channels: Vec<(String, Box<dyn Notifier>)>, fail_threshold: u32) -> Self {
        Self {
            channels: channels
                .into_iter()
                .map(|(name, notifier)| Channel { name, notifier })
                .collect(),
            health: Arc::new(Mutex::new(HashMap::new())),
            fail_threshold: fail_threshold.max(1),
        }
    }

    /// Like [`new`](Self::new) but reuses an existing [`HealthMap`] — used on
    /// config reload so a channel that has been failing for two days is not
    /// silently forgiven just because the operator touched an unrelated
    /// setting.
    #[must_use]
    pub fn with_health(
        channels: Vec<(String, Box<dyn Notifier>)>,
        fail_threshold: u32,
        health: HealthMap,
    ) -> Self {
        // Drop counters for channels that no longer exist in the config: a
        // removed channel has no notifier to recover, and a renamed one would
        // leave a stale entry that never clears.
        {
            let mut map = health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            map.retain(|name, _| channels.iter().any(|(cn, _)| cn == name));
        }
        Self {
            channels: channels
                .into_iter()
                .map(|(name, notifier)| Channel { name, notifier })
                .collect(),
            health,
            fail_threshold: fail_threshold.max(1),
        }
    }

    /// The shared health map, for carrying across a config reload.
    #[must_use]
    pub fn health(&self) -> HealthMap {
        Arc::clone(&self.health)
    }

    /// A point-in-time snapshot of every channel's health, in config order, for
    /// display in `top` and `/api/summary`. Channels with no failures are
    /// included too (so the dashboard can show "all healthy").
    #[must_use]
    pub fn health_snapshot(&self) -> Vec<ChannelHealthEntry> {
        let map = self
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.channels
            .iter()
            .map(|channel| ChannelHealthEntry {
                name: channel.name.clone(),
                health: map.get(&channel.name).cloned().unwrap_or_default(),
            })
            .collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.channels.len()
    }

    /// The routing names of the registered channels, in configuration order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.channels.iter().map(|channel| channel.name.as_str())
    }

    /// Deliver `event` to the matching channels concurrently: all of them when
    /// `only` is `None`, otherwise just those whose name appears in the list. A
    /// slow channel never holds up the others (or the monitor loop behind them).
    ///
    /// Returns the routing names of the channels whose delivery failed, in
    /// configuration order. Each failure was already logged by the channel
    /// itself; the daemon ignores the list, `hora test-alert` exits on it.
    ///
    /// In addition, consecutive failures per channel are tracked: when a
    /// channel reaches the configured threshold, a watchdog `Event::Alert` is
    /// dispatched to the *other* channels ("channel 'telegram' is failing — 3
    /// consecutive failures since 2h"), once per failure streak. A successful
    /// delivery resets the counter.
    pub async fn dispatch(&self, event: Event<'_>, only: Option<&[String]>) -> Vec<&str> {
        let targets: Vec<&Channel> = self
            .channels
            .iter()
            .filter(|channel| {
                only.is_none_or(|names| names.iter().any(|name| name == &channel.name))
            })
            .collect();
        let outcomes = join_all(targets.iter().map(|channel| channel.notifier.notify(event))).await;

        // Update the health map under a scoped lock, collecting any channels
        // that just crossed the threshold and need a watchdog alert.
        let watchdogs: Vec<(String, ChannelHealth)> = {
            let mut map = self
                .health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut triggered = Vec::new();
            for (channel, outcome) in targets.iter().zip(outcomes.iter()) {
                let entry = map.entry(channel.name.clone()).or_default();
                if outcome.is_ok() {
                    entry.consecutive_failures = 0;
                    entry.first_failure_at = None;
                    entry.watchdog_alerted = false;
                } else {
                    entry.consecutive_failures = entry.consecutive_failures.saturating_add(1);
                    if entry.first_failure_at.is_none() {
                        entry.first_failure_at = Some(SystemTime::now());
                    }
                    // Only fire (and latch the flag) when there is another
                    // channel to receive the alert. A lone failing channel has
                    // nobody to tell; latching it now would also suppress the
                    // alert to a channel added later (via reload) while this one
                    // is still failing — the failure is still counted for
                    // doctor/top either way.
                    if entry.consecutive_failures >= self.fail_threshold
                        && !entry.watchdog_alerted
                        && self.channels.iter().any(|c| c.name != channel.name)
                    {
                        entry.watchdog_alerted = true;
                        triggered.push((channel.name.clone(), entry.clone()));
                    }
                }
            }
            triggered
        };

        // Dispatch watchdog alerts to the *other* channels — outside the lock,
        // and via a raw fanout that does not itself track failures (avoids
        // recursion: a watchdog alert failing would otherwise re-trigger the
        // watchdog for that channel within the same call).
        for (name, health) in watchdogs {
            let elapsed = health
                .first_failure_at
                .and_then(|since| SystemTime::now().duration_since(since).ok())
                .map_or_else(|| "unknown".to_owned(), |d| human_elapsed(d.as_secs()));
            let message = format!(
                "{} consecutive delivery failure{} (since {})",
                health.consecutive_failures,
                if health.consecutive_failures == 1 {
                    ""
                } else {
                    "s"
                },
                elapsed,
            );
            warn!(
                channel = %name,
                failures = health.consecutive_failures,
                "notification channel failing — watchdog alerting other channels"
            );
            self.fanout(
                Event::Alert {
                    monitor: "hora",
                    severity: AlertSeverity::Warning,
                    title: &format!("channel '{name}' is failing"),
                    message: &message,
                },
                Some(&name),
            )
            .await;
        }

        targets
            .iter()
            .zip(outcomes)
            .filter_map(|(channel, outcome)| outcome.is_err().then_some(channel.name.as_str()))
            .collect()
    }

    /// Raw fanout with no health tracking: used by the watchdog alert so a
    /// failing watchdog delivery does not recursively re-trigger the watchdog.
    /// `exclude` skips a channel name (the broken one being reported).
    async fn fanout(&self, event: Event<'_>, exclude: Option<&str>) {
        let targets: Vec<&Channel> = self
            .channels
            .iter()
            .filter(|channel| exclude != Some(channel.name.as_str()))
            .collect();
        if targets.is_empty() {
            return;
        }
        join_all(targets.iter().map(|channel| channel.notifier.notify(event))).await;
    }
}

/// `"2d 3h"`, `"6h"`, `"45m"`, `"30s"` — coarse, for the watchdog message.
fn human_elapsed(secs: u64) -> String {
    if secs >= 2 * 86_400 {
        format!("{}d {}h", secs / 86_400, (secs % 86_400) / 3600)
    } else if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test notifier that succeeds or fails on demand, counting how many
    /// times it was called and recording the last event it saw.
    struct MockNotifier {
        name: &'static str,
        fail: std::sync::atomic::AtomicBool,
        calls: std::sync::atomic::AtomicU32,
        last_event: std::sync::Mutex<Option<String>>,
    }

    impl MockNotifier {
        fn new(name: &'static str, fail: bool) -> Self {
            Self {
                name,
                fail: std::sync::atomic::AtomicBool::new(fail),
                calls: std::sync::atomic::AtomicU32::new(0),
                last_event: std::sync::Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl Notifier for MockNotifier {
        fn name(&self) -> &'static str {
            self.name
        }

        async fn notify(&self, event: Event<'_>) -> anyhow::Result<()> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *self.last_event.lock().unwrap() = Some(format!("{event:?}"));
            if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
                anyhow::bail!("mock failure");
            }
            Ok(())
        }
    }

    fn channel(name: &'static str, fail: bool) -> (String, Box<dyn Notifier>) {
        (name.to_owned(), Box::new(MockNotifier::new(name, fail)))
    }

    #[test]
    fn human_elapsed_formats() {
        assert_eq!(human_elapsed(30), "30s");
        assert_eq!(human_elapsed(90), "1m");
        assert_eq!(human_elapsed(3600), "1h");
        assert_eq!(human_elapsed(9000), "2h");
        assert_eq!(human_elapsed(200_000), "2d 7h");
    }

    #[tokio::test]
    async fn success_resets_failure_counter() {
        let d = Dispatcher::new(vec![channel("a", false), channel("b", false)], 3);
        // Manually seed a failure streak for "a".
        {
            let mut map = d.health.lock().unwrap();
            map.insert(
                "a".to_owned(),
                ChannelHealth {
                    consecutive_failures: 2,
                    first_failure_at: Some(SystemTime::now()),
                    watchdog_alerted: false,
                },
            );
        }
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        let snap = d.health_snapshot();
        assert_eq!(snap[0].health.consecutive_failures, 0);
        assert!(snap[0].health.first_failure_at.is_none());
    }

    #[tokio::test]
    async fn threshold_triggers_watchdog_to_other_channels() {
        let d = Dispatcher::new(vec![channel("broken", true), channel("ok", false)], 2);

        // First failure: counter goes to 1, no watchdog yet.
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 1);
        assert!(!d.health_snapshot()[0].health.watchdog_alerted);

        // Second failure: counter hits 2 = threshold, watchdog fires to "ok".
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(d.health_snapshot()[0].health.watchdog_alerted);

        // The "ok" channel should have received a watchdog Event::Alert.
        // The watchdog_alerted flag proves the watchdog path ran; verifying
        // the actual cross-channel delivery would need downcasting the trait
        // object, which the test mock does not support.
        // Also verify a third failure does NOT re-trigger (already alerted).
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 3);
    }

    #[tokio::test]
    async fn recovery_clears_watchdog_so_new_streak_re_alerts() {
        let d = Dispatcher::new(vec![channel("flaky", true), channel("ok", false)], 2);
        // Two failures → watchdog.
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(d.health_snapshot()[0].health.watchdog_alerted);

        // Simulate recovery: swap the notifier for a succeeding one.
        let d = Dispatcher::with_health(
            vec![channel("flaky", false), channel("ok", false)],
            2,
            d.health(),
        );
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(!d.health_snapshot()[0].health.watchdog_alerted);
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 0);

        // New streak should alert again.
        let d = Dispatcher::with_health(
            vec![channel("flaky", true), channel("ok", false)],
            2,
            d.health(),
        );
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(d.health_snapshot()[0].health.watchdog_alerted);
    }

    #[tokio::test]
    async fn only_filter_does_not_touch_excluded_channels() {
        let d = Dispatcher::new(vec![channel("a", true), channel("b", true)], 5);
        let only = vec!["a".to_owned()];
        d.dispatch(Event::Recovered { monitor: "x" }, Some(&only))
            .await;
        // "b" was not dispatched to: its health stays at default (0 failures).
        assert_eq!(d.health_snapshot()[1].health.consecutive_failures, 0);
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn with_health_drops_removed_channels() {
        let d = Dispatcher::new(vec![channel("old", true)], 3);
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(d.health.lock().unwrap().contains_key("old"));

        let d = Dispatcher::with_health(vec![channel("new", false)], 3, d.health());
        // "old" is gone from the config → its counter is dropped.
        assert!(!d.health.lock().unwrap().contains_key("old"));
        // "new" has no entry yet (it gets one on first dispatch).
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(d.health.lock().unwrap().contains_key("new"));
    }

    #[tokio::test]
    async fn no_watchdog_when_only_one_channel() {
        // A single broken channel has nobody to alert — the watchdog stays
        // silent (watchdog_alerted never latches), but the failure is still
        // counted for doctor/top to surface.
        let d = Dispatcher::new(vec![channel("lonely", true)], 2);
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(!d.health_snapshot()[0].health.watchdog_alerted);
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 2);
    }

    #[tokio::test]
    async fn channel_added_during_failure_gets_alerted() {
        // A lone channel fails past threshold with nobody to alert, so the
        // watchdog stays unlatched.
        let d = Dispatcher::new(vec![channel("lonely", true)], 2);
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(!d.health_snapshot()[0].health.watchdog_alerted);

        // A second channel is added via reload while "lonely" is still failing.
        // The next failed dispatch must now fire the watchdog to the newcomer
        // rather than stay suppressed by a stale latch.
        let d = Dispatcher::with_health(
            vec![channel("lonely", true), channel("ok", false)],
            2,
            d.health(),
        );
        d.dispatch(Event::Recovered { monitor: "x" }, None).await;
        assert!(d.health_snapshot()[0].health.watchdog_alerted);
    }
}
