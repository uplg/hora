//! Hora watching itself: the built-in monitor [`SELF_MONITOR_ID`].
//!
//! The dead-man heartbeat catches a dead process, a wedged scheduler, a
//! broken database, checks skipped for lack of resources and a node whose
//! every channel fails. What it cannot see is subtler: one monitor that
//! silently stopped checking, a task the supervisor keeps restarting. Every
//! minute this task looks for those and reports them through the self-check
//! monitor (down while any is present), so they alert like an outage.
//!
//! One channel of several that keeps failing is not among them: the channel
//! watchdog already tells the other channels, and a channel's failures only
//! end with its next delivery - a finding here would stay long after the
//! channel was fixed, until something else had to be sent through it.
//!
//! It reuses what the rest of Hora already knows instead of measuring again:
//! the supervisor's reaped tasks and its view of the alert coalescer, the
//! probes' resource-exhaustion note behind `/healthz`'s `probes_ok`, and the
//! newest stored check of each monitor - one primary-key seek per monitor,
//! all in one statement.
//!
//! When the coalescer is gone no down or recovered alert is delivered any
//! more: [`SelfHealth::cannot_deliver`] then stops the heartbeat too, like a
//! node whose every channel fails, and the dead-man service raises the alarm.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::warn;

use crate::config::{Config, MonitorKind, SELF_MONITOR_ID};
use crate::db::{self, Store};
use crate::status::CheckStatus;

/// How often the self-check looks.
const CHECK_EVERY: Duration = Duration::from_mins(1);
/// Re-record an unchanged state this often (the monitor's deadline is far longer).
const REFRESH_SECS: i64 = 6 * 3600;
/// A task death stays reported this long after it happened.
const DEATH_WINDOW_SECS: i64 = 3600;
/// How many deaths are remembered at most (a crash loop would grow it).
const MAX_DEATHS: usize = 100;
/// A monitor is stale after this many of its periods without a check.
const STALE_PERIODS: u64 = 3;
/// Slack on top, for probe timeouts and scheduling jitter.
const STALE_SLACK_SECS: u64 = 60;
/// A look this long after the previous one came late itself.
const LATE_SECS: i64 = 180;

/// Whether the self-check missed its own looks between `previous` and `now`:
/// the host was suspended, the process stopped, or the clock was set (back
/// as well). The monitors' tasks waited with it, so what it would call stale
/// is only as late as itself.
fn slept(previous: Option<i64>, now: i64) -> bool {
    previous.is_some_and(|at| !(0..=LATE_SECS).contains(&(now - at)))
}

/// What the supervisor tells the self-check (and the heartbeat reads).
#[derive(Debug, Default)]
pub struct SelfHealth {
    deaths: Mutex<VecDeque<Death>>,
    coalescer_dead: AtomicBool,
}

#[derive(Debug, Clone)]
struct Death {
    at: i64,
    task: String,
}

impl SelfHealth {
    /// A monitor or peer-watch task (`what`, `id`) stopped unexpectedly at
    /// `at` and is being restarted.
    pub fn task_died(&self, what: &str, id: &str, at: i64) {
        let mut deaths = self.deaths.lock().unwrap_or_else(PoisonError::into_inner);
        deaths.push_back(Death {
            at,
            task: format!("{what} {id}"),
        });
        while deaths.len() > MAX_DEATHS {
            deaths.pop_front();
        }
    }

    /// The alert coalescer stopped: no down or recovered alert is delivered
    /// any more.
    pub fn coalescer_died(&self) {
        self.coalescer_dead.store(true, Ordering::Release);
    }

    /// No down or recovered alert can reach anybody: the coalescer is gone.
    /// (Every channel failing is the heartbeat's own check.)
    #[must_use]
    pub fn cannot_deliver(&self) -> bool {
        self.coalescer_dead.load(Ordering::Acquire)
    }

    /// The tasks that died within the window before `now`, each once.
    fn recent_deaths(&self, now: i64) -> Vec<String> {
        let mut deaths = self.deaths.lock().unwrap_or_else(PoisonError::into_inner);
        deaths.retain(|death| now - death.at <= DEATH_WINDOW_SECS);
        let mut tasks: Vec<String> = deaths.iter().map(|death| death.task.clone()).collect();
        tasks.sort();
        tasks.dedup();
        tasks
    }
}

/// What the self-check sees at one look; [`findings`] judges it.
#[derive(Debug, Default)]
pub(crate) struct Observed {
    /// The newest stored check of each monitor that has one.
    pub(crate) last_checks: HashMap<String, i64>,
    /// When each monitor was first seen in the config (the grace of a new one).
    pub(crate) first_seen: HashMap<String, i64>,
    /// Tasks that died within the window.
    pub(crate) deaths: Vec<String>,
    pub(crate) coalescer_dead: bool,
    /// Checks were skipped lately for lack of descriptors or memory.
    pub(crate) probes_starved: bool,
}

/// Whether the self-check watches `monitor`'s checks: push monitors (the
/// built-ins among them) watch their own deadline.
fn watched(monitor: &crate::config::Monitor) -> bool {
    !matches!(monitor.spec, MonitorKind::Push(_))
}

/// Everything wrong right now, in a fixed order.
pub(crate) fn findings(config: &Config, seen: &Observed, now: i64) -> Vec<String> {
    let mut found = Vec::new();
    if seen.coalescer_dead {
        found.push(
            "the alert coalescer stopped: down and recovered alerts are not delivered until \
             restart"
                .to_owned(),
        );
    }
    if seen.probes_starved {
        found.push(
            "checks are being skipped: this host is out of file descriptors or memory".to_owned(),
        );
    }
    for task in &seen.deaths {
        found.push(format!(
            "{task} task stopped within the last hour (restarted)"
        ));
    }
    for monitor in config.monitors.iter().filter(|monitor| watched(monitor)) {
        let period = monitor.interval_secs.max(monitor.timeout_secs);
        let allowed = i64::try_from(period.saturating_mul(STALE_PERIODS) + STALE_SLACK_SECS)
            .unwrap_or(i64::MAX);
        let since = seen
            .last_checks
            .get(&monitor.id)
            .copied()
            .max(seen.first_seen.get(&monitor.id).copied())
            .unwrap_or(now);
        if now - since > allowed {
            found.push(format!(
                "monitor {} has not checked for {} min (interval {} s)",
                monitor.id,
                (now - since) / 60,
                monitor.interval_secs
            ));
        }
    }
    found
}

/// The self-check monitor's status and message for `found`.
#[must_use]
pub fn state(found: &[String]) -> (CheckStatus, String) {
    if found.is_empty() {
        return (
            CheckStatus::Up,
            "all monitors checking, no task restarted".to_owned(),
        );
    }
    let message = format!(
        "{} self-check finding(s): {}",
        found.len(),
        found.join("; ")
    );
    (
        CheckStatus::Down,
        crate::bounded(&message, crate::config_status::MAX_MESSAGE_CHARS),
    )
}

/// Start the self-check until `shutdown`.
#[must_use]
pub fn spawn(
    store: Store,
    config: watch::Receiver<Arc<Config>>,
    health: Arc<SelfHealth>,
    mut shutdown: watch::Receiver<bool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut first_seen: HashMap<String, i64> = HashMap::new();
        let mut recorded: Option<(CheckStatus, String, i64)> = None;
        let mut looked: Option<i64> = None;
        loop {
            let snapshot = config.borrow().clone();
            let now = jiff::Timestamp::now().as_second();
            // Late itself: every monitor gets the grace of a new one again,
            // instead of an alert for each on waking up.
            if slept(looked.replace(now), now) {
                for seen in first_seen.values_mut() {
                    *seen = now;
                }
            }
            let seen = observe(&store, &snapshot, &health, &mut first_seen, now).await;
            let (status, message) = state(&findings(&snapshot, &seen, now));
            let due = recorded.as_ref().is_none_or(|(was, said, at)| {
                *was != status || *said != message || now - at >= REFRESH_SECS
            });
            if due {
                match db::insert_push(&store, SELF_MONITOR_ID, status, None, Some(&message)).await {
                    Ok(()) => recorded = Some((status, message, now)),
                    Err(err) => warn!("cannot record the self-check: {err:#}"),
                }
            }
            tokio::select! {
                () = tokio::time::sleep(CHECK_EVERY) => {}
                _ = shutdown.changed() => break,
            }
        }
    })
}

/// Gather one look's [`Observed`]. A monitor's last check that cannot be
/// read counts as unknown, never as stale.
async fn observe(
    store: &Store,
    config: &Config,
    health: &SelfHealth,
    first_seen: &mut HashMap<String, i64>,
    now: i64,
) -> Observed {
    first_seen.retain(|id, _| config.find_monitor(id).is_some());
    for monitor in &config.monitors {
        first_seen.entry(monitor.id.clone()).or_insert(now);
    }
    // One statement for all of them: a query per monitor costs a pool round
    // trip each, about twenty times the CPU for a hundred monitors.
    let ids: Vec<&str> = config
        .monitors
        .iter()
        .filter(|monitor| watched(monitor))
        .map(|monitor| monitor.id.as_str())
        .collect();
    let last_checks = match db::last_check_times(store, &ids).await {
        Ok(times) => times,
        Err(err) => {
            warn!("self-check cannot read the last checks: {err:#}");
            ids.iter().map(|id| ((*id).to_owned(), now)).collect()
        }
    };
    Observed {
        last_checks,
        first_seen: first_seen.clone(),
        deaths: health.recent_deaths(now),
        coalescer_dead: health.cannot_deliver(),
        probes_starved: crate::probe::local::recent(now),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn config() -> Config {
        crate::config::parse_lenient(
            r#"
            [page]
            [server]
            [alerts]
            channel_fail_threshold = 3
            [[monitors]]
            id = "web"
            name = "Web"
            target = "https://example.com"
            interval_secs = 60
            [[monitors]]
            id = "job"
            name = "Job"
            kind = "push"
            interval_secs = 60
            push_token = "0123456789abcdef"
            "#,
        )
        .unwrap()
    }

    fn seen(last: &[(&str, i64)], first_seen: i64) -> Observed {
        Observed {
            last_checks: last.iter().map(|(id, t)| ((*id).to_owned(), *t)).collect(),
            first_seen: config()
                .monitors
                .iter()
                .map(|m| (m.id.clone(), first_seen))
                .collect(),
            ..Observed::default()
        }
    }

    #[test]
    fn a_healthy_node_has_no_findings() {
        let observed = seen(&[("web", NOW - 30)], NOW - 3600);
        let found = findings(&config(), &observed, NOW);
        assert_eq!(found, Vec::<String>::new());
        assert_eq!(state(&found).0, CheckStatus::Up);
    }

    #[test]
    fn a_monitor_that_stopped_checking_is_reported() {
        // 60 s interval: allowed 3 x 60 + 60 = 240 s.
        let found = findings(&config(), &seen(&[("web", NOW - 600)], NOW - 3600), NOW);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(
            found[0].starts_with("monitor web has not checked for 10 min"),
            "{found:?}"
        );
        assert_eq!(state(&found).0, CheckStatus::Down);
        let found = findings(&config(), &seen(&[("web", NOW - 240)], NOW - 3600), NOW);
        assert_eq!(found, Vec::<String>::new(), "on the bound is in time");
    }

    #[test]
    fn a_new_monitor_gets_its_grace_and_push_and_builtins_are_skipped() {
        // No check yet, but only seen 100 s ago: not stale. `job` (push) and
        // the built-ins never count, however long they are silent.
        let found = findings(&config(), &seen(&[], NOW - 100), NOW);
        assert_eq!(found, Vec::<String>::new());
        let found = findings(&config(), &seen(&[], NOW - 3600), NOW);
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("monitor web "), "{found:?}");
    }

    #[test]
    fn stopped_tasks_starved_probes_and_a_dead_coalescer_are_reported() {
        let mut observed = seen(&[("web", NOW - 30)], NOW - 3600);
        observed.deaths = vec!["monitor web".to_owned()];
        observed.coalescer_dead = true;
        observed.probes_starved = true;
        let found = findings(&config(), &observed, NOW);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found[0].contains("coalescer"));
        assert!(found[1].contains("out of file descriptors or memory"));
        assert_eq!(
            found[2],
            "monitor web task stopped within the last hour (restarted)"
        );
        let (status, message) = state(&found);
        assert_eq!(status, CheckStatus::Down);
        assert!(
            message.starts_with("3 self-check finding(s): "),
            "{message}"
        );
    }

    /// A look that comes late itself - the host was suspended, the clock
    /// was set - blames nobody: every monitor waited with it.
    #[test]
    fn a_self_check_that_slept_gives_every_monitor_its_grace_again() {
        assert!(!slept(None, NOW), "the first look");
        assert!(!slept(Some(NOW - 60), NOW), "on time");
        assert!(!slept(Some(NOW - LATE_SECS), NOW));
        assert!(slept(Some(NOW - LATE_SECS - 1), NOW), "hours of suspend");
        assert!(slept(Some(NOW + 5), NOW), "the clock went back");
        // What the look after a night's suspend sees: checks hours old, and
        // a grace that starts now.
        let asleep = seen(&[("web", NOW - 8 * 3600)], NOW);
        assert_eq!(findings(&config(), &asleep, NOW), Vec::<String>::new());
    }

    #[test]
    fn deaths_age_out_and_a_dead_coalescer_blocks_delivery() {
        let health = SelfHealth::default();
        health.task_died("monitor", "web", NOW - DEATH_WINDOW_SECS - 1);
        health.task_died("monitor", "api", NOW - 10);
        health.task_died("monitor", "api", NOW - 5);
        assert_eq!(health.recent_deaths(NOW), ["monitor api"]);
        assert!(!health.cannot_deliver());
        health.coalescer_died();
        assert!(health.cannot_deliver());
    }

    /// The real look: the newest check comes from the store, a monitor that
    /// never checked counts from when it was first seen.
    #[tokio::test]
    async fn observe_reads_the_newest_check_of_each_watched_monitor() {
        let store = Store::in_memory().await;
        let config = config();
        let outcome = crate::probe::Outcome::up(false, Some(5), None);
        db::insert_check(&store, "web", &outcome).await.unwrap();
        db::insert_push(&store, "job", CheckStatus::Up, None, None)
            .await
            .unwrap();
        let health = SelfHealth::default();
        let mut first_seen = HashMap::new();
        let now = jiff::Timestamp::now().as_second();
        let seen = observe(&store, &config, &health, &mut first_seen, now).await;
        assert_eq!(seen.last_checks.len(), 1, "push monitors are not looked up");
        assert!(seen.last_checks["web"] >= now - 5);
        assert_eq!(seen.first_seen.len(), config.monitors.len());
        assert_eq!(findings(&config, &seen, now), Vec::<String>::new());
    }
}

#[cfg(test)]
mod spawn_tests {
    use super::*;

    /// The task records its verdict as a push the built-in monitor reads.
    #[tokio::test]
    async fn the_self_check_records_its_state() {
        let store = Store::in_memory().await;
        let config = Arc::new(crate::config::parse_lenient("[page]\n[server]\n").unwrap());
        let health = Arc::new(SelfHealth::default());
        health.coalescer_died();
        let (_tx, rx) = watch::channel(config);
        let (stop, stopped) = watch::channel(false);
        let task = spawn(store.clone(), rx, health, stopped);
        let beat = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(beat) = db::last_heartbeat(&store, SELF_MONITOR_ID).await.unwrap() {
                    break beat;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("recorded");
        assert_eq!(beat.status, CheckStatus::Down);
        assert!(beat.error.unwrap().contains("coalescer"));
        stop.send(true).unwrap();
        task.await.unwrap();
    }
}
