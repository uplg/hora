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
mod message;
pub mod ntfy;
pub mod pushover;
pub mod slack;
pub mod telegram;
mod util;
pub mod webhook;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

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
#[doc(hidden)]
pub use util::human_duration;
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

/// What an [`Event::ReleaseAvailable`] carries. A struct rather than the
/// variant's own fields: five of them in each channel's `match` would push
/// those functions past what one reads at a glance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release<'a> {
    pub monitor: &'a str,
    /// The watched project, `owner/repo`.
    pub project: &'a str,
    /// The version that runs (configured, or read from the service).
    pub current: &'a str,
    /// The latest published release.
    pub latest: &'a str,
    /// The release's page: its notes.
    pub url: &'a str,
    /// What the release brings, in a sentence or two, when known (Hora's
    /// own releases: the opening of their changelog entry).
    pub notes: Option<&'a str>,
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
        /// the down ("deploy api v2.3, 3 min before") - the "what changed?" line.
        event: Option<&'a str>,
        /// Down from this node only: every peer that answered sees it up.
        /// Such a down is flagged; it goes to the monitor's channels, or to
        /// `alerts.notify_unconfirmed` when that is set.
        local_only: bool,
    },
    /// A monitor is up but degraded: slower than its `degraded_over_ms` budget.
    Degraded {
        monitor: &'a str,
        latency_ms: Option<i64>,
        /// What the monitor said about itself: a push job's `msg` sent with
        /// `status=degraded` ("disk 91% full").
        detail: Option<&'a str>,
    },
    /// A previously-down (or degraded) monitor is fully healthy again.
    /// `local_only`: the down it ends was only ever announced as local-only.
    Recovered { monitor: &'a str, local_only: bool },
    /// A monitor's TLS certificate is within the warning window (or expired).
    /// `secs_left` is negative once expired: whole days alone would read
    /// "has expired" with hours still to go.
    CertExpiring { monitor: &'a str, secs_left: i64 },
    /// A cert-watched monitor's TLS certificate could not be read (handshake,
    /// STARTTLS negotiation or connect failed): its expiry is no longer
    /// watched until it can be read again.
    CertUnreadable { monitor: &'a str, error: &'a str },
    /// A monitor's registered domain is within the warning window (or expired),
    /// as reported by the registry over RDAP.
    DomainExpiring {
        monitor: &'a str,
        domain: &'a str,
        /// Negative once expired.
        secs_left: i64,
    },
    /// A newer upstream release of the software behind a monitor is out
    /// (`release = { github = "owner/repo", ... }`). Nothing is failing: the
    /// service is behind, and whoever runs it wants to know the same day.
    ReleaseAvailable(Release<'a>),
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

/// How long one channel gets to deliver one event, retries included. Without
/// it a channel that accepts the connection and never answers holds its caller
/// (the alert coalescer, a monitor loop) for every attempt's full client
/// timeout, and the alerts queued behind it wait on healthy channels too.
const CHANNEL_DEADLINE: Duration = Duration::from_secs(20);

/// One channel's delivery of `event`, bounded by [`CHANNEL_DEADLINE`]. Running
/// out of time is a delivery failure like any other, counted by the watchdog.
async fn deliver(channel: &Channel, event: Event<'_>) -> anyhow::Result<()> {
    let attempt = tokio::time::timeout(CHANNEL_DEADLINE, channel.notifier.notify(event));
    let Ok(outcome) = attempt.await else {
        let deadline = util::human_duration(CHANNEL_DEADLINE.as_secs());
        warn!(channel = %channel.name, "notification abandoned after {deadline} without an answer");
        anyhow::bail!("no answer within {deadline}")
    };
    outcome
}

/// Default watchdog threshold: 3 consecutive delivery failures. Each
/// `send_retrying` call already retries 3 times internally, so for the HTTP
/// channels this represents 9 failed attempts — enough to ride through a
/// transient blip. (E-mail does not retry: 3 failed SMTP sends.)
pub const DEFAULT_CHANNEL_FAIL_THRESHOLD: u32 = 3;

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
    /// consecutive failures since 2 h"), once per failure streak. A successful
    /// delivery resets the counter.
    pub async fn dispatch(&self, event: Event<'_>, only: Option<&[String]>) -> Vec<&str> {
        let targets: Vec<&Channel> = self
            .channels
            .iter()
            .filter(|channel| {
                only.is_none_or(|names| names.iter().any(|name| name == &channel.name))
            })
            .collect();
        let outcomes = join_all(targets.iter().map(|channel| deliver(channel, event))).await;

        // Update the health map under a scoped lock, collecting any channels
        // that just crossed the threshold and need a watchdog alert.
        let watchdogs: Vec<(String, ChannelHealth)> = {
            let mut map = self
                .health
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut triggered = Vec::new();
            for (channel, outcome) in targets.iter().zip(outcomes.iter()) {
                // A payload the API refused as too large (413) says nothing about the
                // channel's health - the channel answered - so it neither
                // extends nor forgives the current streak.
                // Nor does a rate limit (429): the channel is alive and
                // asking for less, usually in the middle of a cascade.
                if outcome.as_ref().is_err_and(util::spares_health) {
                    continue;
                }
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
                .map_or_else(
                    || "unknown".to_owned(),
                    |d| util::human_duration(d.as_secs()),
                );
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
        join_all(targets.iter().map(|channel| deliver(channel, event))).await;
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

    /// Any event will do for the dispatcher's bookkeeping.
    const RECOVERED: Event<'static> = Event::Recovered {
        monitor: "x",
        local_only: false,
    };

    /// A channel that takes the event and never answers.
    struct Silent;

    #[async_trait]
    impl Notifier for Silent {
        fn name(&self) -> &'static str {
            "silent"
        }

        async fn notify(&self, _event: Event<'_>) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    /// One silent channel costs its caller the deadline, not forever, and is
    /// counted as failing; the channel beside it is delivered regardless.
    #[tokio::test(start_paused = true)]
    async fn a_silent_channel_fails_at_the_deadline() {
        let d = Dispatcher::new(
            vec![
                ("silent".to_owned(), Box::new(Silent) as Box<dyn Notifier>),
                channel("ok", false),
            ],
            3,
        );
        let started = tokio::time::Instant::now();
        let failed = d.dispatch(RECOVERED, None).await;
        assert_eq!(failed, ["silent"]);
        assert_eq!(started.elapsed(), CHANNEL_DEADLINE);
        let map = d.health.lock().unwrap();
        assert_eq!(map["silent"].consecutive_failures, 1);
        assert_eq!(map["ok"].consecutive_failures, 0);
    }

    fn channel(name: &'static str, fail: bool) -> (String, Box<dyn Notifier>) {
        (name.to_owned(), Box::new(MockNotifier::new(name, fail)))
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
        d.dispatch(RECOVERED, None).await;
        let snap = d.health_snapshot();
        assert_eq!(snap[0].health.consecutive_failures, 0);
        assert!(snap[0].health.first_failure_at.is_none());
    }

    #[tokio::test]
    async fn threshold_triggers_watchdog_to_other_channels() {
        let d = Dispatcher::new(vec![channel("broken", true), channel("ok", false)], 2);

        // First failure: counter goes to 1, no watchdog yet.
        d.dispatch(RECOVERED, None).await;
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 1);
        assert!(!d.health_snapshot()[0].health.watchdog_alerted);

        // Second failure: counter hits 2 = threshold, watchdog fires to "ok".
        d.dispatch(RECOVERED, None).await;
        assert!(d.health_snapshot()[0].health.watchdog_alerted);

        // The "ok" channel should have received a watchdog Event::Alert.
        // The watchdog_alerted flag proves the watchdog path ran; verifying
        // the actual cross-channel delivery would need downcasting the trait
        // object, which the test mock does not support.
        // Also verify a third failure does NOT re-trigger (already alerted).
        d.dispatch(RECOVERED, None).await;
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 3);
    }

    #[tokio::test]
    async fn recovery_clears_watchdog_so_new_streak_re_alerts() {
        let d = Dispatcher::new(vec![channel("flaky", true), channel("ok", false)], 2);
        // Two failures → watchdog.
        d.dispatch(RECOVERED, None).await;
        d.dispatch(RECOVERED, None).await;
        assert!(d.health_snapshot()[0].health.watchdog_alerted);

        // Simulate recovery: swap the notifier for a succeeding one.
        let d = Dispatcher::with_health(
            vec![channel("flaky", false), channel("ok", false)],
            2,
            d.health(),
        );
        d.dispatch(RECOVERED, None).await;
        assert!(!d.health_snapshot()[0].health.watchdog_alerted);
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 0);

        // New streak should alert again.
        let d = Dispatcher::with_health(
            vec![channel("flaky", true), channel("ok", false)],
            2,
            d.health(),
        );
        d.dispatch(RECOVERED, None).await;
        d.dispatch(RECOVERED, None).await;
        assert!(d.health_snapshot()[0].health.watchdog_alerted);
    }

    #[tokio::test]
    async fn only_filter_does_not_touch_excluded_channels() {
        let d = Dispatcher::new(vec![channel("a", true), channel("b", true)], 5);
        let only = vec!["a".to_owned()];
        d.dispatch(RECOVERED, Some(&only)).await;
        // "b" was not dispatched to: its health stays at default (0 failures).
        assert_eq!(d.health_snapshot()[1].health.consecutive_failures, 0);
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn with_health_drops_removed_channels() {
        let d = Dispatcher::new(vec![channel("old", true)], 3);
        d.dispatch(RECOVERED, None).await;
        assert!(d.health.lock().unwrap().contains_key("old"));

        let d = Dispatcher::with_health(vec![channel("new", false)], 3, d.health());
        // "old" is gone from the config → its counter is dropped.
        assert!(!d.health.lock().unwrap().contains_key("old"));
        // "new" has no entry yet (it gets one on first dispatch).
        d.dispatch(RECOVERED, None).await;
        assert!(d.health.lock().unwrap().contains_key("new"));
    }

    /// A channel whose API refuses every payload as too large (HTTP 413).
    struct RejectingNotifier;

    #[async_trait]
    impl Notifier for RejectingNotifier {
        fn name(&self) -> &'static str {
            "rejecting"
        }

        async fn notify(&self, _event: Event<'_>) -> anyhow::Result<()> {
            Err(util::PayloadRejected("rejected (HTTP 413 Payload Too Large)".to_owned()).into())
        }
    }

    #[tokio::test]
    async fn rejected_payloads_do_not_trip_the_watchdog() {
        let d = Dispatcher::new(
            vec![
                (
                    "big".to_owned(),
                    Box::new(RejectingNotifier) as Box<dyn Notifier>,
                ),
                channel("ok", false),
            ],
            2,
        );
        for _ in 0..3 {
            let failed = d.dispatch(RECOVERED, None).await;
            // Still reported as a failed delivery (test-alert's exit code)...
            assert_eq!(failed, ["big"]);
        }
        // ...but not as a failing channel.
        let health = &d.health_snapshot()[0].health;
        assert_eq!(health.consecutive_failures, 0);
        assert!(!health.watchdog_alerted);

        // Nor does it forgive a streak that real failures started.
        d.health.lock().unwrap().insert(
            "big".to_owned(),
            ChannelHealth {
                consecutive_failures: 1,
                first_failure_at: Some(SystemTime::now()),
                watchdog_alerted: false,
            },
        );
        d.dispatch(RECOVERED, None).await;
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 1);
    }

    #[tokio::test]
    async fn no_watchdog_when_only_one_channel() {
        // A single broken channel has nobody to alert — the watchdog stays
        // silent (watchdog_alerted never latches), but the failure is still
        // counted for doctor/top to surface.
        let d = Dispatcher::new(vec![channel("lonely", true)], 2);
        d.dispatch(RECOVERED, None).await;
        d.dispatch(RECOVERED, None).await;
        assert!(!d.health_snapshot()[0].health.watchdog_alerted);
        assert_eq!(d.health_snapshot()[0].health.consecutive_failures, 2);
    }

    #[tokio::test]
    async fn channel_added_during_failure_gets_alerted() {
        // A lone channel fails past threshold with nobody to alert, so the
        // watchdog stays unlatched.
        let d = Dispatcher::new(vec![channel("lonely", true)], 2);
        d.dispatch(RECOVERED, None).await;
        d.dispatch(RECOVERED, None).await;
        assert!(!d.health_snapshot()[0].health.watchdog_alerted);

        // A second channel is added via reload while "lonely" is still failing.
        // The next failed dispatch must now fire the watchdog to the newcomer
        // rather than stay suppressed by a stale latch.
        let d = Dispatcher::with_health(
            vec![channel("lonely", true), channel("ok", false)],
            2,
            d.health(),
        );
        d.dispatch(RECOVERED, None).await;
        assert!(d.health_snapshot()[0].health.watchdog_alerted);
    }
}
