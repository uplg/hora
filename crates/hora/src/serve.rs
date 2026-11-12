//! The monitor itself (plain `hora`): the supervisor, the background tasks and
//! the HTTP server, until a shutdown signal.

use std::io::IsTerminal as _;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::Context as _;
use axum::serve::ListenerExt as _;
use hora_core::config;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

/// Run the monitor: load config, open the database, start the supervisor and
/// background tasks, and serve the status page until a shutdown signal.
pub(crate) async fn serve() -> anyhow::Result<()> {
    let config_path = config::path();
    let initial = config::load_from(&config_path).context("loading configuration")?;
    // Held until the process ends: `hora compact` rewrites the file and must
    // never do so under a running daemon (nor two daemons share one file).
    let _lock = hora_core::db::lock_exclusive(&initial.server.database_path)
        .context("locking the database")?;
    let store = hora_core::db::connect(&initial.server.database_path)
        .await
        .context("opening database")?;
    // The notifier client (no proxy); per-monitor probe clients are built by the
    // supervisor so each can carry its own proxy.
    let client = hora_core::http::client(None).context("building HTTP client")?;

    // A shutdown signal lets the background tasks stop cleanly (finishing their
    // current iteration) instead of being aborted when the runtime drops. The
    // HTTP server stops accepting on the same signal.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // The scheduler's liveness beacon: each monitor tick bumps it, and the
    // dead-man heartbeat and /healthz read it to tell a live scheduler from a
    // wedged one. Shared with the supervisor (writers) and the web layer (reader).
    let last_tick = Arc::new(AtomicU64::new(0));

    // The supervisor owns the live config + notification channels and reconciles
    // monitor tasks on reload; other components read through its handles.
    let handle = hora_core::supervisor::start(
        initial,
        config_path,
        store.clone(),
        client.clone(),
        Arc::clone(&last_tick),
        shutdown_rx.clone(),
    );
    let cert_task = hora_core::cert::spawn_watcher(
        store.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        client.clone(),
        shutdown_rx.clone(),
    );

    // Mutual surveillance: the outbound dead-man heartbeat. It self-gates on the
    // [health] section and reads it live, so it is always spawned (and activates if
    // [health] is added on reload). The inbound peer-watch tasks are owned and
    // hot-reloaded by the supervisor alongside the monitors.
    let heartbeat_task = hora_core::mesh::peer::spawn_heartbeat(
        handle.config.clone(),
        store.clone(),
        client.clone(),
        Arc::clone(&handle.notifier),
        Arc::clone(&last_tick),
        shutdown_rx.clone(),
    );

    // Per-vantage latency: poll the peers' /api/peer/monitors in the
    // background and share the snapshot with the web layer. Self-gating like
    // the heartbeat (no peers = a no-op per round).
    let vantage_map = hora_core::mesh::vantage::new_map();
    let vantage_task = hora_core::mesh::vantage::spawn_poller(
        handle.config.clone(),
        client.clone(),
        Arc::clone(&vantage_map),
        shutdown_rx.clone(),
    );

    let digest_task = hora_core::digest::spawn(
        store.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        shutdown_rx.clone(),
    );

    // Hora's own new releases, for the operator's page and channels.
    // Self-gating on `[updates] check`, read live.
    let updates = hora_core::updates::new_slot();
    let updates_task = hora_core::updates::spawn(
        store.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        client,
        Arc::clone(&updates),
        shutdown_rx.clone(),
    );

    let prune_task =
        hora_core::maintenance::spawn_pruner(&store, handle.config.clone(), shutdown_rx);

    let (listener, connections) = listen(&handle.config.borrow().server).await?;

    let state = hora_web::AppState::new(
        store,
        handle.config.clone(),
        Arc::clone(&last_tick),
        handle.notifier.clone(),
    )
    .with_vantage(vantage_map)
    .with_updates(updates);
    // Build the status summary while the server comes up, so the first page
    // view finds it ready instead of waiting for it.
    state.start_refresher();
    // Connect-info gives the rate limiter the peer socket IP: the client key
    // unless `server.client_ip_header` names a trusted proxy header (forwarded
    // headers are never read by default - a direct client could forge them).
    let app = hora_web::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();

    let background = background_stopped(
        handle.task,
        [
            cert_task,
            prune_task,
            heartbeat_task,
            digest_task,
            vantage_task,
            updates_task,
        ],
    );
    serve_until_stopped(
        listener,
        connections,
        app,
        &shutdown_tx,
        shutdown_signal(),
        background,
        HTTP_DRAIN,
    )
    .await
}

/// What stopping waits for besides the requests in flight, once the tasks
/// were told. The periodic tasks only finish an iteration. The supervisor
/// gets its own, longer wait: it awaits the monitor loops' last tick, then
/// the alert coalescer, which releases its grouping window and lets the
/// delivery lanes send what they hold - an alert confirmed on the way out
/// must still leave.
async fn background_stopped<const N: usize>(
    supervisor: tokio::task::JoinHandle<()>,
    periodic: [tokio::task::JoinHandle<()>; N],
) {
    let periodic = tokio::time::timeout(TASKS_STOP, async {
        // All were told together; awaiting them in turn waits for the slowest.
        for task in periodic {
            let _ = task.await;
        }
    });
    let supervisor = tokio::time::timeout(SUPERVISOR_STOP, supervisor);
    let (_, supervisor) = tokio::join!(periodic, supervisor);
    if supervisor.is_err() {
        tracing::warn!(
            "monitors or alert delivery still busy after {SUPERVISOR_STOP:?}: exiting anyway"
        );
    }
}

/// How long the requests in flight get to finish once shutdown began. Every
/// request is bounded by [`hora_web::REQUEST_TIMEOUT`] already; this is
/// shorter because a stop should not wait for the slowest request allowed:
/// a page or an API answer comes from memory in milliseconds, so ten seconds
/// only ever cut a request that was already stuck.
const HTTP_DRAIN: Duration = Duration::from_secs(10);

/// How long the periodic tasks (certificates, pruning, heartbeat, digest,
/// vantage, updates) get to finish their current iteration.
const TASKS_STOP: Duration = Duration::from_secs(5);

/// How long the supervisor gets: the monitor loops' last tick, then the alert
/// coalescer, which may hold its inbox open 15 s and gives its delivery lanes
/// 30 s to empty (`hora_core::coalesce`). Nothing waits that long when no
/// alert is on its way out; cutting it shorter (it was 5 s, with everything
/// else) lost the alerts of a stop during an outage.
const SUPERVISOR_STOP: Duration = Duration::from_mins(1);

/// Bind the listening socket, and settle how many connections it serves at
/// once (see [`crate::listener`]).
async fn listen(
    server: &hora_core::config::Server,
) -> anyhow::Result<(tokio::net::TcpListener, u32)> {
    let bind = &server.bind;
    let listener = crate::listener::bind(bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    let connections = crate::listener::connection_limit(
        server.max_connections,
        crate::listener::open_files_limit(),
        crate::listener::memory_limit(),
    );
    tracing::info!(
        "hora {} listening on http://{bind} (up to {connections} connections at once)",
        env!("CARGO_PKG_VERSION")
    );
    Ok((listener, connections))
}

/// Serve `app` until `signal`, then stop within bounds: everything is told to
/// stop at once (`shutdown`), the requests in flight get `drain` to finish,
/// and `background` (the tasks' own bounded wait) runs beside that drain, not
/// after it.
///
/// The background tasks used to be told only once the server had drained,
/// and the drain had no bound: one request that never ended kept the monitors
/// probing and the process alive until it was killed, with no clean stop of
/// anything.
async fn serve_until_stopped(
    listener: tokio::net::TcpListener,
    connections: u32,
    app: axum::extract::connect_info::IntoMakeServiceWithConnectInfo<
        axum::Router,
        std::net::SocketAddr,
    >,
    shutdown: &watch::Sender<bool>,
    signal: impl Future<Output = ()>,
    background: impl Future<Output = ()>,
    drain: Duration,
) -> anyhow::Result<()> {
    let mut stop = shutdown.subscribe();
    // `tap_io` changes nothing on the connection: it is the wrapper axum
    // knows how to read the peer address through.
    let listener = crate::listener::Guarded::new(listener, connections).tap_io(|_| {});
    let server = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            // A dropped sender is a stop too.
            let _ = stop.wait_for(|stop| *stop).await;
        })
        .into_future();
    tokio::pin!(server);

    // The server only ends by itself on an error; that stops the rest too.
    let ended = tokio::select! {
        result = &mut server => Some(result),
        () = signal => None,
    };
    let _ = shutdown.send(true);

    let http = async {
        match ended {
            Some(result) => result.context("running HTTP server"),
            None => match tokio::time::timeout(drain, &mut server).await {
                Ok(result) => result.context("running HTTP server"),
                Err(_elapsed) => {
                    // Leaving drops the runtime, and their connections with it.
                    tracing::warn!("requests still in flight after {drain:?}: closing them");
                    Ok(())
                }
            },
        }
    };
    let (result, ()) = tokio::join!(http, background);
    result
}

pub(crate) fn init_tracing() {
    // Distinguish "unset" (silent default) from "set but invalid" (warn, so a
    // typo'd filter isn't silently ignored). Tracing isn't up yet, so use stderr.
    let filter = match std::env::var("HORA_LOG") {
        Ok(value) => EnvFilter::try_new(&value).unwrap_or_else(|err| {
            eprintln!("warning: invalid HORA_LOG {value:?} ({err}); using info");
            EnvFilter::new("info")
        }),
        Err(_) => EnvFilter::new("info"),
    };
    let ansi = colours_logs(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
    );
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(ansi)
        .init();
}

/// Whether the log lines carry ANSI colours: only when standard output (where
/// they go) is a terminal, since `docker logs`, journald and a redirected file
/// keep the escape codes as noise in every line. A non-empty `NO_COLOR`
/// (<https://no-color.org>) turns them off on a terminal too.
fn colours_logs(stdout_is_terminal: bool, no_color: Option<&std::ffi::OsStr>) -> bool {
    stdout_is_terminal && no_color.is_none_or(std::ffi::OsStr::is_empty)
}

/// Resolve when the process receives a shutdown signal. Listens for Ctrl-C on
/// every platform and, on Unix, also `SIGTERM` - the signal `docker stop` and
/// most init systems send - so the server drains in-flight requests cleanly
/// instead of being killed after the grace period.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to listen for Ctrl-C: {err}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::error!("failed to listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::sync::{Notify, oneshot, watch};

    use super::{colours_logs, serve_until_stopped};

    /// A pipe (`docker logs`, journald) gets plain lines; so does a terminal
    /// whose user set `NO_COLOR`.
    #[test]
    fn logs_are_coloured_only_on_a_terminal() {
        use std::ffi::OsStr;
        assert!(colours_logs(true, None));
        assert!(!colours_logs(false, None));
        assert!(!colours_logs(true, Some(OsStr::new("1"))));
        // The convention: an empty NO_COLOR is as good as unset.
        assert!(colours_logs(true, Some(OsStr::new(""))));
        assert!(!colours_logs(false, Some(OsStr::new(""))));
    }

    /// A request that never ends must not hold the stop: the background tasks
    /// hear of it at once, and the server gives up on the request after the
    /// drain. (Waiting for the server first, without a bound, this test never
    /// finishes.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_request_that_never_ends_does_not_hold_the_stop() {
        const DRAIN: Duration = Duration::from_millis(300);

        let entered = Arc::new(Notify::new());
        let app = axum::Router::new()
            .route(
                "/hang",
                axum::routing::get({
                    let entered = Arc::clone(&entered);
                    move || {
                        let entered = Arc::clone(&entered);
                        async move {
                            entered.notify_one();
                            std::future::pending::<()>().await;
                        }
                    }
                }),
            )
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");

        let (shutdown, mut stopping) = watch::channel(false);
        let (signal, signalled) = oneshot::channel::<()>();
        let (told, told_at) = oneshot::channel::<Instant>();
        let background = async move {
            let _ = stopping.wait_for(|stop| *stop).await;
            let _ = told.send(Instant::now());
        };
        let server = tokio::spawn(async move {
            serve_until_stopped(
                listener,
                64,
                app,
                &shutdown,
                async {
                    let _ = signalled.await;
                },
                background,
                DRAIN,
            )
            .await
        });

        // One request in flight, in its handler for good.
        let mut client = std::net::TcpStream::connect(address).expect("connect");
        client
            .write_all(b"GET /hang HTTP/1.1\r\nhost: hora\r\n\r\n")
            .expect("request");
        entered.notified().await;

        let began = Instant::now();
        signal.send(()).expect("the server is running");
        let result = tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .expect("the stop is bounded")
            .expect("no panic");
        assert!(result.is_ok(), "{result:?}");
        let took = began.elapsed();
        assert!(took >= DRAIN, "the request got its drain ({took:?})");

        // The background tasks were told before the drain ran out, not after.
        let told = told_at.await.expect("the background was told");
        assert!(told.duration_since(began) < DRAIN);
        drop(client);
    }
}
