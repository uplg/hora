//! Supervises monitor tasks and hot-reloads the configuration.
//!
//! The live config is shared through a [`watch`] channel every component reads.
//! On SIGHUP or a change to the config file, the file is re-read and the running
//! monitor tasks are reconciled: new monitors start, removed ones stop, changed
//! ones restart - unchanged monitors keep running, so a reload never interrupts
//! existing checks.
//!
//! `server.bind` is read once at startup; changing it still requires a restart.
//! Everything else - monitors, peers (the surveillance mesh), notification
//! channels, intervals, thresholds, retention, the certificate window - reloads
//! live: peer-watch tasks are reconciled like monitors, and the outbound
//! heartbeat reads `[health]` on every cycle.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::coalesce::{self, AlertMsg};
use crate::config::{self, Config, Monitor, Peer};
use crate::notifications::{self, Notifiers};
use crate::peer::spawn_watch;
use crate::scheduler;

struct Running {
    monitor: Monitor,
    task: JoinHandle<()>,
}

struct RunningPeer {
    peer: Peer,
    task: JoinHandle<()>,
}

/// The shared handles threaded from [`start`] through the reload loop into each
/// spawned monitor: the database pool, the notifier client (used to rebuild
/// channels on reload), the hot-swappable notifier set, and the scheduler
/// liveness beacon.
struct Deps {
    pool: SqlitePool,
    client: Client,
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
    pool: SqlitePool,
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
        pool,
        client,
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
    let mut running: HashMap<String, Running> = HashMap::new();
    let mut running_peers: HashMap<String, RunningPeer> = HashMap::new();
    reconcile(&mut running, &rx, &deps, &shutdown);
    reconcile_peers(&mut running_peers, &rx, &deps, &shutdown);

    // The raw text last applied: a file event whose content is unchanged (a touch,
    // or a spurious event from some filesystems) is ignored, so a flapping watcher
    // can never spin the supervisor.
    let mut last_raw = std::fs::read_to_string(&config_path).unwrap_or_default();

    let mut reloads = reload_signals(&config_path);
    loop {
        let signal = tokio::select! {
            signal = reloads.recv() => signal,
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
                // Rebuild the channels too, so credential/channel changes apply live.
                deps.notifier
                    .store(Arc::new(notifications::build(&config, &deps.client)));
                if tx.send(Arc::clone(&config)).is_err() {
                    break;
                }
                reconcile(&mut running, &rx, &deps, &shutdown);
                reconcile_peers(&mut running_peers, &rx, &deps, &shutdown);
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
    for (_, run) in running.drain() {
        let _ = run.task.await;
    }
    for (_, run) in running_peers.drain() {
        let _ = run.task.await;
    }
    let _ = coalescer.await;
}

/// How long to wait after the first file event before reloading, so a burst of
/// events (and any self-triggered ones) collapse into a single reload.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);

/// Diff running tasks against the latest config: stop removed or changed
/// monitors, start new or changed ones, leave unchanged ones untouched.
fn reconcile(
    running: &mut HashMap<String, Running>,
    rx: &watch::Receiver<Arc<Config>>,
    deps: &Deps,
    shutdown: &watch::Receiver<bool>,
) {
    let config = rx.borrow().clone();

    let desired: HashMap<&str, &Monitor> =
        config.monitors.iter().map(|m| (m.id.as_str(), m)).collect();

    running.retain(|id, run| match desired.get(id.as_str()) {
        Some(monitor) if **monitor == run.monitor => true,
        _ => {
            run.task.abort();
            false
        }
    });

    for monitor in &config.monitors {
        if !running.contains_key(&monitor.id) {
            // Each monitor gets its own client so it can carry its own proxy.
            // The proxy URL is validated at config load, so this rarely fails.
            let client = match crate::http::probe_client(monitor.proxy.as_deref()) {
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
                    pool: deps.pool.clone(),
                    client,
                    notifier: Arc::clone(&deps.notifier),
                    alerts: deps.alerts.clone(),
                    last_tick: Arc::clone(&deps.last_tick),
                },
                shutdown.clone(),
            );
            running.insert(
                monitor.id.clone(),
                Running {
                    monitor: monitor.clone(),
                    task,
                },
            );
        }
    }
}

/// Diff running peer-watch tasks against the latest config: stop removed or
/// changed peers, start new or changed ones, leave unchanged ones running. Only
/// watched peers (those with `expect_every_secs`) get a task.
fn reconcile_peers(
    running: &mut HashMap<String, RunningPeer>,
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

    running.retain(|id, run| match desired.get(id.as_str()) {
        Some(peer) if **peer == run.peer => true,
        _ => {
            run.task.abort();
            false
        }
    });

    for peer in config.peers.iter().filter(|peer| peer.is_watched()) {
        if !running.contains_key(&peer.id) {
            let task = spawn_watch(
                peer.clone(),
                rx.clone(),
                deps.pool.clone(),
                deps.client.clone(),
                Arc::clone(&deps.notifier),
                shutdown.clone(),
            );
            running.insert(
                peer.id.clone(),
                RunningPeer {
                    peer: peer.clone(),
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
