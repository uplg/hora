//! Supervises monitor tasks and hot-reloads the configuration.
//!
//! The live config is shared through a [`watch`] channel every component reads.
//! On SIGHUP or a change to the config file, the file is re-read and the running
//! monitor tasks are reconciled: new monitors start, removed ones stop, changed
//! ones restart - unchanged monitors keep running, so a reload never interrupts
//! existing checks.
//!
//! `server.bind` and notification credentials are read once at startup; changing
//! them still requires a restart. Everything else (monitors, intervals,
//! thresholds, retention, the certificate window) reloads live.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use reqwest::Client;
use sqlx::SqlitePool;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::{self, Config, Monitor};
use crate::notifications::{self, Notifiers};
use crate::scheduler;

struct Running {
    monitor: Monitor,
    task: JoinHandle<()>,
}

/// A handle to the running supervisor: the live config and the hot-swappable
/// notifier set, both shared with the rest of the application.
pub struct Handle {
    pub config: watch::Receiver<Arc<Config>>,
    pub notifier: Notifiers,
}

/// Start supervising: build the notifier set, spawn the initial monitors and the
/// reload loop, and return the handles every component reads.
#[must_use]
pub fn start(initial: Config, config_path: PathBuf, pool: SqlitePool, client: Client) -> Handle {
    let notifier = notifications::shared(&initial, &client);
    let (tx, rx) = watch::channel(Arc::new(initial));
    tokio::spawn(supervise(
        tx,
        rx.clone(),
        config_path,
        pool,
        client,
        Arc::clone(&notifier),
    ));
    Handle {
        config: rx,
        notifier,
    }
}

async fn supervise(
    tx: watch::Sender<Arc<Config>>,
    rx: watch::Receiver<Arc<Config>>,
    config_path: PathBuf,
    pool: SqlitePool,
    client: Client,
    notifier: Notifiers,
) {
    let mut running: HashMap<String, Running> = HashMap::new();
    reconcile(&mut running, &rx, &pool, &notifier);

    // The raw text last applied: a file event whose content is unchanged (a touch,
    // or a spurious event from some filesystems) is ignored, so a flapping watcher
    // can never spin the supervisor.
    let mut last_raw = std::fs::read_to_string(&config_path).unwrap_or_default();

    let mut reloads = reload_signals(&config_path);
    while reloads.recv().await.is_some() {
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
                notifier.store(Arc::new(notifications::build(&config, &client)));
                if tx.send(Arc::clone(&config)).is_err() {
                    break;
                }
                reconcile(&mut running, &rx, &pool, &notifier);
                info!(
                    "configuration reloaded ({} monitors, {} channels)",
                    config.monitors.len(),
                    notifier.load().len(),
                );
            }
            Err(err) => warn!("config reload failed, keeping current config: {err:#}"),
        }
    }
}

/// How long to wait after the first file event before reloading, so a burst of
/// events (and any self-triggered ones) collapse into a single reload.
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(500);

/// Diff running tasks against the latest config: stop removed or changed
/// monitors, start new or changed ones, leave unchanged ones untouched.
fn reconcile(
    running: &mut HashMap<String, Running>,
    rx: &watch::Receiver<Arc<Config>>,
    pool: &SqlitePool,
    notifier: &Notifiers,
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
            let client = match crate::http::client(monitor.proxy.as_deref()) {
                Ok(client) => client,
                Err(err) => {
                    warn!(monitor = %monitor.id, "monitor not started, bad proxy: {err:#}");
                    continue;
                }
            };
            let task = scheduler::spawn_monitor(
                monitor.clone(),
                rx.clone(),
                pool.clone(),
                client,
                Arc::clone(notifier),
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
    let watched_name = config_path.file_name().map(OsStr::to_owned);
    let directory = config_path
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
