//! Supervises monitor tasks and hot-reloads the configuration.
//!
//! The live config is shared through a [`watch`] channel every component reads.
//! On SIGHUP or a change to the config file (how one is noticed is
//! [`config_watch`]'s business), the file is re-read and the running
//! monitor tasks are reconciled: new monitors start, removed ones stop, changed
//! ones restart - unchanged monitors keep running, so a reload never interrupts
//! existing checks. Each monitor's (and peer's) alert state lives here, outside
//! its task, so a restarted task neither re-announces an outage nor loses its
//! recovery; a task that stopped on its own is noticed, logged and restarted.
//!
//! That last net does not catch a panic in a release build: the release
//! profile is `panic = "abort"` (binary size), so a panic anywhere ends the
//! whole process, and it is the container runtime or the service manager
//! (`restart: unless-stopped`, `Restart=on-failure`) that brings Hora back. The
//! in-memory alert state is lost with the process; what the next start
//! rebuilds it from is in the database: an open incident says the monitor was
//! down (so its down is not announced twice and its recovery is), and
//! `incidents.notified` says whether that down alert had gone out (if not, the
//! start confirms and announces it). Only a debug build, which unwinds, sees a
//! panicked task here.
//!
//! `server.bind` is read once at startup; changing it still requires a restart.
//! Everything else - monitors, peers (the surveillance mesh), notification
//! channels, intervals, thresholds, retention, the certificate window - reloads
//! live: peer-watch tasks are reconciled like monitors, and the outbound
//! heartbeat reads `[health]` on every cycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::db::Store;
use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::coalesce::{self, AlertMsg};
use crate::config::{self, Config, Monitor, Peer};
use crate::config_watch;
use crate::mesh::peer::spawn_watch;
use crate::notifications::{self, Notifiers};
use crate::scheduler::{self, AlertCell};

/// A running task and the spec (monitor or peer) it was spawned for.
struct Running<T> {
    spec: T,
    task: JoinHandle<()>,
}

/// The tasks of one kind (monitors or peer watches), with the alert state of
/// each id. The state outlives the tasks: it is dropped only when the id leaves
/// the configuration.
struct Fleet<T> {
    running: HashMap<String, Running<T>>,
    alert_states: HashMap<String, AlertCell>,
}

impl<T> Default for Fleet<T> {
    fn default() -> Self {
        Self {
            running: HashMap::new(),
            alert_states: HashMap::new(),
        }
    }
}

impl<T: PartialEq> Fleet<T> {
    /// Stop the tasks whose id left `desired` or whose spec changed, and drop
    /// the alert state of the ids that left.
    fn retire(&mut self, desired: &HashMap<&str, &T>) {
        self.running
            .retain(|id, run| match desired.get(id.as_str()) {
                Some(spec) if **spec == run.spec => true,
                _ => {
                    run.task.abort();
                    false
                }
            });
        self.alert_states
            .retain(|id, _| desired.contains_key(id.as_str()));
    }

    /// The alert state for `id`, created empty on first use.
    fn alert_state(&mut self, id: &str) -> AlertCell {
        self.alert_states.entry(id.to_owned()).or_default().clone()
    }

    /// Remove (and log) every task that finished on its own, so the next
    /// reconcile restarts it with its alert state. Returns whether any was
    /// found. Before shutdown a task has no reason to return, so this is a net
    /// for a loop that ended by mistake; a panic reaches it only in a build
    /// that unwinds (debug, tests) - in a release build it has already aborted
    /// the process (see the module docs).
    async fn reap_dead(&mut self, what: &str) -> bool {
        let dead: Vec<String> = self
            .running
            .iter()
            .filter(|(_, run)| run.task.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            if let Some(run) = self.running.remove(id) {
                // A JoinError's Display carries the panic message (builds
                // that unwind only).
                let cause = run
                    .task
                    .await
                    .err()
                    .map_or_else(|| "it returned".to_owned(), |err| err.to_string());
                error!(id = %id, "{what} task stopped unexpectedly ({cause}); restarting it");
            }
        }
        !dead.is_empty()
    }

    /// Await every task (they observe the shutdown signal themselves).
    async fn drain(&mut self) {
        for (_, run) in self.running.drain() {
            let _ = run.task.await;
        }
    }
}

/// How often the supervisor checks for tasks that stopped on their own, which
/// nothing else would notice: a monitor would silently stop being checked.
/// (A panic is not one of them in a release build: it aborts the process.)
const LIVENESS_CHECK: Duration = Duration::from_secs(30);

/// The shared handles threaded from [`start`] through the reload loop into each
/// spawned monitor: the database pool, the notifier client (used to rebuild
/// channels on reload), the hot-swappable notifier set, and the scheduler
/// liveness beacon.
struct Deps {
    store: Store,
    client: Client,
    /// The monitors' probe clients, one per proxy (see [`ProbeClients`]).
    probe_clients: Mutex<ProbeClients>,
    notifier: Notifiers,
    /// Inbox of the alert coalescer (root-cause grouping); every monitor loop
    /// gets a clone.
    alerts: mpsc::UnboundedSender<AlertMsg>,
    last_tick: Arc<AtomicU64>,
}

/// A handle to the running supervisor: the live config and the hot-swappable
/// notifier set, both shared with the rest of the application.
pub struct Handle {
    pub config: watch::Receiver<Arc<Config>>,
    pub notifier: Notifiers,
    /// The supervise task; await it on shutdown to drain the monitor tasks.
    pub task: JoinHandle<()>,
}

/// Start supervising: build the notifier set, spawn the initial monitors and the
/// reload loop, and return the handles every component reads.
#[must_use]
pub fn start(
    initial: Config,
    config_path: PathBuf,
    store: Store,
    client: Client,
    last_tick: Arc<AtomicU64>,
    shutdown: watch::Receiver<bool>,
) -> Handle {
    let notifier = notifications::shared(&initial, &client);
    let (tx, rx) = watch::channel(Arc::new(initial));
    let (alerts_tx, alerts_rx) = mpsc::unbounded_channel();
    let coalescer = coalesce::spawn(
        rx.clone(),
        Arc::clone(&notifier),
        store.clone(),
        alerts_rx,
        shutdown.clone(),
    );
    let deps = Deps {
        store,
        client,
        probe_clients: Mutex::default(),
        notifier: Arc::clone(&notifier),
        alerts: alerts_tx,
        last_tick,
    };
    let task = tokio::spawn(supervise(
        tx,
        rx.clone(),
        config_path,
        deps,
        coalescer,
        shutdown,
    ));
    Handle {
        config: rx,
        notifier,
        task,
    }
}

async fn supervise(
    tx: watch::Sender<Arc<Config>>,
    rx: watch::Receiver<Arc<Config>>,
    config_path: PathBuf,
    deps: Deps,
    coalescer: JoinHandle<()>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut monitors: Fleet<Monitor> = Fleet::default();
    let mut peers: Fleet<Peer> = Fleet::default();
    reconcile(&mut monitors, &rx, &deps, &shutdown);
    reconcile_peers(&mut peers, &rx, &deps, &shutdown);

    // Everything that can ask for a reload, debounced and reduced to "the file
    // now holds a text that was not seen before"; dropped (and so stopped) when
    // this function returns.
    let mut reloads = config_watch::start(&config_path);
    let mut liveness = tokio::time::interval(LIVENESS_CHECK);
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut coalescer_dead_logged = false;
    loop {
        let changed = tokio::select! {
            changed = reloads.next_change() => changed,
            _ = liveness.tick() => {
                // A task finishing because shutdown began is not a death.
                if *shutdown.borrow() {
                    break;
                }
                if monitors.reap_dead("monitor").await {
                    reconcile(&mut monitors, &rx, &deps, &shutdown);
                }
                if peers.reap_dead("peer watch").await {
                    reconcile_peers(&mut peers, &rx, &deps, &shutdown);
                }
                if coalescer.is_finished() && !coalescer_dead_logged {
                    coalescer_dead_logged = true;
                    error!("the alert coalescer stopped: down and recovered alerts can no longer be delivered until Hora is restarted");
                }
                continue;
            }
            _ = shutdown.changed() => break,
        };
        let Some(raw) = changed else {
            break;
        };

        let Some(config) = apply_reload(&raw, &config_path, &tx, &deps.notifier, &deps.client)
        else {
            continue;
        };
        reconcile(&mut monitors, &rx, &deps, &shutdown);
        reconcile_peers(&mut peers, &rx, &deps, &shutdown);
        info!(
            "configuration reloaded ({} monitors, {} peers, {} channels)",
            config.monitors.len(),
            config.peers.iter().filter(|peer| peer.is_watched()).count(),
            deps.notifier.load().len(),
        );
    }

    // On shutdown the monitor and peer-watch tasks observe the same signal and
    // break; await them, then close the coalescer's inbox (the last sender is
    // ours) so it handles every alert they sent on their way out, releases its
    // grouping window and empties the delivery lanes.
    monitors.drain().await;
    peers.drain().await;
    drop(deps);
    let _ = coalescer.await;
}

/// Parse the re-read config text and, if it loads, put it in force: the
/// channels are rebuilt (so credential and channel changes apply live, with
/// the per-channel failure counters carried over so a channel that has been
/// failing survives the reload) and the new config is published. A text that
/// does not load - a syntax error, a value validation refuses - changes
/// nothing: the node keeps watching and alerting on the config it had, and
/// says so. What the loader only warns about (a `${VAR}` that is not set, a
/// channel it leaves disabled) applies, with its warning.
fn apply_reload(
    raw: &str,
    config_path: &Path,
    tx: &watch::Sender<Arc<Config>>,
    notifier: &Notifiers,
    client: &Client,
) -> Option<Arc<Config>> {
    match config::parse_at(raw, config_path) {
        Ok(config) => {
            let config = Arc::new(config);
            let health = notifier.load().health();
            notifier.store(Arc::new(notifications::build(
                &config,
                client,
                Some(health),
            )));
            // The supervisor holds a receiver of its own, so nobody is ever
            // missing to hear this.
            tx.send_replace(Arc::clone(&config));
            Some(config)
        }
        Err(err) => {
            warn!("config reload failed, keeping current config: {err:#}");
            None
        }
    }
}

/// Diff running tasks against the latest config: stop removed or changed
/// monitors, start new or changed ones, leave unchanged ones untouched. A
/// restarted monitor resumes its alert state (see [`AlertCell`]).
fn reconcile(
    fleet: &mut Fleet<Monitor>,
    rx: &watch::Receiver<Arc<Config>>,
    deps: &Deps,
    shutdown: &watch::Receiver<bool>,
) {
    let config = rx.borrow().clone();

    let desired: HashMap<&str, &Monitor> =
        config.monitors.iter().map(|m| (m.id.as_str(), m)).collect();
    fleet.retire(&desired);

    let mut probe_clients = deps
        .probe_clients
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    probe_clients.retain_used(&config);
    for monitor in &config.monitors {
        if !fleet.running.contains_key(&monitor.id) {
            // The proxy URL is validated at config load, so this rarely fails.
            let client = match probe_clients.get(monitor.proxy()) {
                Ok(client) => client,
                Err(err) => {
                    warn!(monitor = %monitor.id, "monitor not started, bad proxy: {err:#}");
                    continue;
                }
            };
            let task = scheduler::spawn_monitor(
                monitor.clone(),
                rx.clone(),
                scheduler::MonitorDeps {
                    store: deps.store.clone(),
                    client,
                    confirm_client: deps.client.clone(),
                    notifier: Arc::clone(&deps.notifier),
                    alerts: deps.alerts.clone(),
                    last_tick: Arc::clone(&deps.last_tick),
                    alert_state: fleet.alert_state(&monitor.id),
                    reannounce_secs: scheduler::REANNOUNCE_SECS,
                },
                shutdown.clone(),
            );
            fleet.running.insert(
                monitor.id.clone(),
                Running {
                    spec: monitor.clone(),
                    task,
                },
            );
        }
    }
}

/// The probe clients monitors share: one for every monitor without a proxy,
/// one per proxy URL for the others. A client carries its own TLS setup -
/// about 45 KB - so one per monitor cost 450 MB at 10,000 monitors. Nothing
/// about a probe client is per monitor (credentials and timeouts are set per
/// request), and it keeps no connection between probes
/// (see [`probe_client`](crate::http::probe_client)).
#[derive(Default)]
struct ProbeClients {
    clients: HashMap<Option<String>, Client>,
}

impl ProbeClients {
    /// The client for `proxy`, built on first use.
    fn get(&mut self, proxy: Option<&str>) -> reqwest::Result<Client> {
        let key = proxy.map(str::to_owned);
        if let Some(client) = self.clients.get(&key) {
            return Ok(client.clone());
        }
        let client = crate::http::probe_client(proxy)?;
        self.clients.insert(key, client.clone());
        Ok(client)
    }

    /// Forget the clients of proxies no monitor uses any more (running
    /// monitors keep their own handle on theirs until they restart).
    fn retain_used(&mut self, config: &Config) {
        self.clients.retain(|proxy, _| {
            proxy.is_none()
                || config
                    .monitors
                    .iter()
                    .any(|monitor| monitor.proxy() == proxy.as_deref())
        });
    }
}

/// Diff running peer-watch tasks against the latest config: stop removed or
/// changed peers, start new or changed ones, leave unchanged ones running. Only
/// watched peers (those with `expect_every_secs`) get a task.
fn reconcile_peers(
    fleet: &mut Fleet<Peer>,
    rx: &watch::Receiver<Arc<Config>>,
    deps: &Deps,
    shutdown: &watch::Receiver<bool>,
) {
    let config = rx.borrow().clone();

    let desired: HashMap<&str, &Peer> = config
        .peers
        .iter()
        .filter(|peer| peer.is_watched())
        .map(|peer| (peer.id.as_str(), peer))
        .collect();
    fleet.retire(&desired);

    for peer in config.peers.iter().filter(|peer| peer.is_watched()) {
        if !fleet.running.contains_key(&peer.id) {
            let task = spawn_watch(
                peer.clone(),
                rx.clone(),
                deps.store.clone(),
                deps.client.clone(),
                Arc::clone(&deps.notifier),
                fleet.alert_state(&peer.id),
                shutdown.clone(),
            );
            fleet.running.insert(
                peer.id.clone(),
                Running {
                    spec: peer.clone(),
                    task,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `reap_dead` is still for: a task that returned on its own.
    #[tokio::test]
    async fn a_task_that_returned_is_reaped_once() {
        let mut fleet: Fleet<u8> = Fleet::default();
        let task = tokio::spawn(async {});
        while !task.is_finished() {
            tokio::task::yield_now().await;
        }
        fleet
            .running
            .insert("api".to_owned(), Running { spec: 1, task });
        assert!(fleet.reap_dead("monitor").await);
        assert!(fleet.running.is_empty());
        assert!(!fleet.reap_dead("monitor").await);
    }

    /// The docs of this module, of `AlertCell` and of the status refresher say
    /// a panic aborts a release build. They are wrong the day this changes.
    #[test]
    fn the_release_profile_aborts_on_panic() {
        let manifest = include_str!("../../../Cargo.toml");
        let release = manifest
            .split("[profile.release]")
            .nth(1)
            .expect("a release profile");
        let release = release.split("\n[").next().unwrap_or(release);
        assert!(release.contains("panic = \"abort\""), "{release}");
    }

    const RELOAD_BEFORE: &str = "[page]\n[server]\n[[channels]]\nname = \"ops\"\n\
         type = \"discord\"\nwebhook_url = \"https://discord.example/hook\"\n";

    /// A reload that does not load (here: a duplicate channel name, then text
    /// that is not TOML) leaves the config and the channels in force exactly
    /// as they were; the next good one applies.
    #[test]
    fn a_failed_reload_keeps_the_config_and_the_channels() {
        let path = Path::new("config.toml");
        let client = Client::new();
        let initial = Arc::new(config::parse(RELOAD_BEFORE).unwrap());
        let notifier = notifications::shared(&initial, &client);
        let (tx, rx) = watch::channel(Arc::clone(&initial));

        let broken = format!(
            "{RELOAD_BEFORE}[[channels]]\nname = \"ops\"\ntype = \"slack\"\n\
             webhook_url = \"https://slack.example/hook\"\n"
        );
        assert!(apply_reload(&broken, path, &tx, &notifier, &client).is_none());
        assert!(Arc::ptr_eq(&rx.borrow(), &initial));
        assert_eq!(notifier.load().names().collect::<Vec<_>>(), ["ops"]);
        // Not TOML at all: the same.
        assert!(apply_reload("[page", path, &tx, &notifier, &client).is_none());
        assert!(Arc::ptr_eq(&rx.borrow(), &initial));

        let renamed = RELOAD_BEFORE.replace("\"ops\"", "\"oncall\"");
        let applied = apply_reload(&renamed, path, &tx, &notifier, &client).expect("loads");
        assert!(Arc::ptr_eq(&rx.borrow(), &applied));
        assert_eq!(notifier.load().names().collect::<Vec<_>>(), ["oncall"]);
    }

    /// A variable that is not set is no reason to refuse a reload: the config
    /// applies, the channel left without its secret is disabled while the
    /// other keeps delivering, and both facts are warned about by name.
    #[test]
    fn a_reload_with_an_unset_variable_applies_with_its_warnings() {
        let path = Path::new("config.toml");
        let client = Client::new();
        let initial = Arc::new(config::parse(RELOAD_BEFORE).unwrap());
        let notifier = notifications::shared(&initial, &client);
        let (tx, rx) = watch::channel(Arc::clone(&initial));

        let edited = format!(
            "{RELOAD_BEFORE}[[channels]]\nname = \"phone\"\ntype = \"gotify\"\n\
             url = \"https://gotify.example\"\n\
             token = \"${{HORA_TEST_A_VARIABLE_NOBODY_SETS}}\"\n"
        );
        let (applied, warnings) =
            config::captured_warnings(|| apply_reload(&edited, path, &tx, &notifier, &client));
        let applied = applied.expect("the reload applies");
        assert!(Arc::ptr_eq(&rx.borrow(), &applied));
        assert_eq!(applied.channels.len(), 2);
        assert_eq!(notifier.load().names().collect::<Vec<_>>(), ["ops"]);
        assert_eq!(
            warnings,
            [
                "channels[1].token: environment variable HORA_TEST_A_VARIABLE_NOBODY_SETS is \
                 not set (write ${HORA_TEST_A_VARIABLE_NOBODY_SETS:-} if it is optional, or \
                 ${HORA_TEST_A_VARIABLE_NOBODY_SETS:-fallback} for a default)",
                "channel phone: token is empty, the channel is disabled until it is set",
            ]
        );
    }

    #[test]
    fn monitors_share_a_probe_client_per_proxy() {
        let mut clients = ProbeClients::default();
        clients.get(None).unwrap();
        clients.get(None).unwrap();
        clients.get(Some("http://proxy.example:3128")).unwrap();
        clients.get(Some("http://proxy.example:3128")).unwrap();
        assert_eq!(
            clients.clients.len(),
            2,
            "one client per proxy, not per monitor"
        );

        // A reload that drops the proxied monitor forgets its client.
        let config = config::parse(
            "[page]\n[server]\n[[monitors]]\nid = \"a\"\nname = \"A\"\n\
             target = \"https://example.com\"\ninterval_secs = 60\n",
        )
        .unwrap();
        clients.retain_used(&config);
        assert_eq!(clients.clients.len(), 1);
        assert!(clients.clients.contains_key(&None));
    }
}
