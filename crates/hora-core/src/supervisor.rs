//! Supervises monitor tasks and hot-reloads the configuration.
//!
//! The live config is shared through a [`watch`] channel every component reads.
//! On SIGHUP or a change to the config file, the file is re-read and the running
//! monitor tasks are reconciled: new monitors start, removed ones stop, changed
//! ones restart - unchanged monitors keep running, so a reload never interrupts
//! existing checks. Each monitor's (and peer's) alert state lives here, outside
//! its task, so a restarted task neither re-announces an outage nor loses its
//! recovery; a task that died (a panic) is noticed, logged and restarted.
//!
//! `server.bind` is read once at startup; changing it still requires a restart.
//! Everything else - monitors, peers (the surveillance mesh), notification
//! channels, intervals, thresholds, retention, the certificate window - reloads
//! live: peer-watch tasks are reconciled like monitors, and the outbound
//! heartbeat reads `[health]` on every cycle.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use crate::db::Store;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use reqwest::Client;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::coalesce::{self, AlertMsg};
use crate::config::{self, Config, Monitor, Peer};
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

    /// Remove (and log) every task that finished on its own - only a panic
    /// does that before shutdown - so the next reconcile restarts it with its
    /// alert state. Returns whether any was found.
    async fn reap_dead(&mut self, what: &str) -> bool {
        let dead: Vec<String> = self
            .running
            .iter()
            .filter(|(_, run)| run.task.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            if let Some(run) = self.running.remove(id) {
                // A JoinError's Display carries the panic message.
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

/// How often the supervisor checks for tasks that died (a panic in a probe or
/// in the timer, which nothing else would notice: a monitor would silently
/// stop being checked).
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

    // The raw text last applied: a file event whose content is unchanged (a touch,
    // or a spurious event from some filesystems) is ignored, so a flapping watcher
    // can never spin the supervisor.
    let mut last_raw = std::fs::read_to_string(&config_path).unwrap_or_default();

    let mut reloads = reload_signals(&config_path);
    let mut liveness = tokio::time::interval(LIVENESS_CHECK);
    liveness.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut coalescer_dead_logged = false;
    loop {
        let signal = tokio::select! {
            signal = reloads.recv() => signal,
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
                    error!("the alert coalescer stopped: down and recovered alerts can no longer be delivered until restart");
                }
                continue;
            }
            _ = shutdown.changed() => break,
        };
        if signal.is_none() {
            break;
        }
        // Debounce: let a burst settle, then drain everything that piled up, so one
        // edit (which fires several events) becomes a single reload.
        tokio::time::sleep(RELOAD_DEBOUNCE).await;
        while reloads.try_recv().is_ok() {}

        let raw = match std::fs::read_to_string(&config_path) {
            Ok(raw) => raw,
            Err(err) => {
                warn!("config reload skipped, cannot read file: {err:#}");
                continue;
            }
        };
        if raw == last_raw {
            continue; // Content unchanged: nothing to do.
        }

        match config::parse(&raw) {
            Ok(config) => {
                last_raw = raw;
                let config = Arc::new(config);
                // Rebuild the channels too, so credential/channel changes
                // apply live. The per-channel failure counters are carried over
                // so a channel that has been failing survives the reload.
                let health = deps.notifier.load().health();
                deps.notifier.store(Arc::new(notifications::build(
                    &config,
                    &deps.client,
                    Some(health),
                )));
                if tx.send(Arc::clone(&config)).is_err() {
                    break;
                }
                reconcile(&mut monitors, &rx, &deps, &shutdown);
                reconcile_peers(&mut peers, &rx, &deps, &shutdown);
                info!(
                    "configuration reloaded ({} monitors, {} peers, {} channels)",
                    config.monitors.len(),
                    config.peers.iter().filter(|peer| peer.is_watched()).count(),
                    deps.notifier.load().len(),
                );
            }
            Err(err) => warn!("config reload failed, keeping current config: {err:#}"),
        }
    }

    // On shutdown the monitor and peer-watch tasks observe the same signal and
    // break; await them, then the coalescer (which drains its queue).
    monitors.drain().await;
    peers.drain().await;
    let _ = coalescer.await;
}

/// How long to wait after the first file event before reloading, so a burst of
/// events (and any self-triggered ones) collapse into a single reload.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);

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

/// A channel that fires whenever the config should be reloaded: on SIGHUP, and
/// whenever the config file changes on disk.
fn reload_signals(config_path: &Path) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel(8);

    spawn_sighup(tx.clone());

    match file_watcher(config_path, tx) {
        Ok(watcher) => {
            // The watcher stops once dropped, so keep it alive for the process.
            tokio::spawn(async move {
                let _watcher = watcher;
                std::future::pending::<()>().await;
            });
        }
        Err(err) => warn!("config file watching disabled: {err}"),
    }

    rx
}

#[cfg(unix)]
fn spawn_sighup(tx: mpsc::Sender<()>) {
    use tokio::signal::unix::{SignalKind, signal};

    tokio::spawn(async move {
        let mut hangup = match signal(SignalKind::hangup()) {
            Ok(hangup) => hangup,
            Err(err) => {
                warn!("cannot listen for SIGHUP: {err}");
                return;
            }
        };
        while hangup.recv().await.is_some() {
            if tx.send(()).await.is_err() {
                break;
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_sighup(_tx: mpsc::Sender<()>) {}

fn file_watcher(config_path: &Path, tx: mpsc::Sender<()>) -> notify::Result<RecommendedWatcher> {
    // Resolve symlinks so we watch the real file's directory: a symlinked config
    // would otherwise point the watcher at the link's directory and miss edits to
    // the target. (Reads still go through the original path.)
    let resolved = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    let watched_name = resolved.file_name().map(OsStr::to_owned);
    let directory = resolved
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);

    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else { return };
        // Ignore access (read) events: reading the file - including our own reload
        // read - must never trigger a reload, or the watcher feeds itself forever.
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let touches_config = event
            .paths
            .iter()
            .any(|path| path.file_name().map(OsStr::to_owned) == watched_name);
        if touches_config {
            // Non-blocking: a dropped redundant event is harmless (the next one
            // triggers a reload), and this avoids any off-runtime blocking_send.
            let _ = tx.try_send(());
        }
    })?;
    watcher.watch(&directory, RecursiveMode::NonRecursive)?;
    Ok(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;

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
