//! Hora - a tiny self-hosted uptime monitor.
//!
//! Wires the pieces together: load config, open the database, start the
//! supervisor (which owns the live config and notification channels), spawn the
//! certificate watcher and pruner, and serve the status page and JSON API.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::Context as _;
use hora_core::config;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if run_subcommand().await? {
        return Ok(());
    }

    init_tracing();
    serve().await
}

/// Handle a CLI subcommand. `Ok(true)` means one ran and the process should
/// exit; plain `hora` (no arguments) returns `Ok(false)` and starts the
/// monitor.
async fn run_subcommand() -> anyhow::Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "import" => {
                if args.len() < 4 || args[2] != "kuma" {
                    eprintln!("Usage: hora import kuma <backup.json>");
                    std::process::exit(1);
                }
                let json_path = &args[3];
                let json_str = std::fs::read_to_string(json_path)
                    .with_context(|| format!("reading {json_path}"))?;
                let toml_out = hora_core::import::convert_kuma_to_hora(&json_str)?;
                println!("{toml_out}");
            }
            "check" => {
                // Validate the config and exit non-zero on error: meant for CI
                // and pre-deploy hooks.
                let config_path = config::path();
                match config::load_from(&config_path) {
                    Ok(_) => println!("{} is valid.", config_path.display()),
                    Err(err) => {
                        eprintln!("Configuration error: {err:#}");
                        std::process::exit(1);
                    }
                }
            }
            "test-alert" => {
                // Tracing first: delivery failures surface as per-channel
                // warnings from the notifiers, and that is the whole point.
                init_tracing();
                test_alert(args.get(2).map(String::as_str)).await?;
            }
            "--version" | "-V" => println!("hora {}", env!("CARGO_PKG_VERSION")),
            "--help" | "-h" => {
                println!("Hora - a tiny self-hosted uptime monitor");
                println!();
                println!("Usage: hora [COMMAND]");
                println!();
                println!("Commands:");
                println!(
                    "  import kuma <file>  Convert an Uptime Kuma backup JSON to Hora TOML (stdout)"
                );
                println!("  check               Validate the configuration and exit");
                println!(
                    "  test-alert [id]     Send a test down + recovered through the configured"
                );
                println!(
                    "                      channels (all of them, or the routed ones of monitor [id])"
                );
                println!("  --version, -V       Show the version");
                println!("  --help, -h          Show this help message");
            }
            _ => {
                eprintln!("Unknown command: {}", args[1]);
                eprintln!("Run 'hora --help' for usage information.");
                std::process::exit(1);
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// Send a test `Down` then `Recovered` through the real notification chain, so
/// an operator verifies delivery *before* the first real incident instead of
/// during it. Without an id every configured channel is exercised; with one,
/// the monitor's `notify` routing applies - testing exactly what would fire.
/// Failures surface as the notifiers' own per-channel warnings.
async fn test_alert(monitor_id: Option<&str>) -> anyhow::Result<()> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;

    let (name, notify) = match monitor_id {
        None => ("Hora test".to_owned(), None),
        Some(id) => {
            let Some(monitor) = config.monitors.iter().find(|monitor| monitor.id == id) else {
                eprintln!("Unknown monitor {id:?}. Configured ids:");
                for monitor in &config.monitors {
                    eprintln!("  {}", monitor.id);
                }
                std::process::exit(1);
            };
            (monitor.name.clone(), monitor.notify.clone())
        }
    };

    let client = hora_core::http::client(None).context("building HTTP client")?;
    let dispatcher = hora_core::notifications::build(&config, &client);
    let targeted: Vec<&str> = dispatcher
        .names()
        .filter(|channel| {
            notify
                .as_ref()
                .is_none_or(|only| only.iter().any(|name| name == channel))
        })
        .collect();
    if targeted.is_empty() {
        eprintln!("No notification channel to test (none configured, or none routed).");
        std::process::exit(1);
    }

    println!(
        "Sending a test alert (down + recovered) as {name:?} to: {}",
        targeted.join(", ")
    );
    let event = hora_core::notifications::Event::Down {
        monitor: &name,
        error: Some("test alert sent by `hora test-alert` - not a real incident"),
        cause: None,
        impacted: &[],
    };
    dispatcher.dispatch(event, notify.as_deref()).await;
    dispatcher
        .dispatch(
            hora_core::notifications::Event::Recovered { monitor: &name },
            notify.as_deref(),
        )
        .await;
    println!("Done. A channel that stayed silent has a warning above.");
    Ok(())
}

/// Run the monitor: load config, open the database, start the supervisor and
/// background tasks, and serve the status page until a shutdown signal.
async fn serve() -> anyhow::Result<()> {
    let config_path = config::path();
    let initial = config::load_from(&config_path).context("loading configuration")?;
    let pool = hora_core::db::connect(&initial.server.database_path)
        .await
        .context("opening database")?;
    // The notifier client (no proxy); per-monitor probe clients are built by the
    // supervisor so each can carry its own proxy.
    let client = hora_core::http::client(None).context("building HTTP client")?;

    // A shutdown signal lets the background tasks stop cleanly (finishing their
    // current iteration) instead of being aborted when the runtime drops.
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
        pool.clone(),
        client.clone(),
        Arc::clone(&last_tick),
        shutdown_rx.clone(),
    );
    let cert_task = hora_core::cert::spawn_watcher(
        pool.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        shutdown_rx.clone(),
    );

    // Mutual surveillance: the outbound dead-man heartbeat. It self-gates on the
    // [health] section and reads it live, so it is always spawned (and activates if
    // [health] is added on reload). The inbound peer-watch tasks are owned and
    // hot-reloaded by the supervisor alongside the monitors.
    let heartbeat_task = hora_core::peer::spawn_heartbeat(
        handle.config.clone(),
        pool.clone(),
        client,
        Arc::clone(&last_tick),
        shutdown_rx.clone(),
    );

    let prune_task = hora_core::db::spawn_pruner(&pool, handle.config.clone(), shutdown_rx);

    let bind = handle.config.borrow().server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(
        "hora {} listening on http://{bind}",
        env!("CARGO_PKG_VERSION")
    );

    let state = hora_web::AppState::new(pool, handle.config.clone(), Arc::clone(&last_tick));
    // Connect-info gives the rate limiter a peer IP to fall back on when there
    // is no `X-Forwarded-For` (i.e. direct access, not behind a proxy).
    let app = hora_web::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("running HTTP server")?;

    // The HTTP server has drained; now stop the background tasks and wait briefly
    // for them to finish their current iteration before the runtime drops.
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = tokio::join!(handle.task, cert_task, prune_task, heartbeat_task);
    })
    .await;
    Ok(())
}

fn init_tracing() {
    // Distinguish "unset" (silent default) from "set but invalid" (warn, so a
    // typo'd filter isn't silently ignored). Tracing isn't up yet, so use stderr.
    let filter = match std::env::var("HORA_LOG") {
        Ok(value) => EnvFilter::try_new(&value).unwrap_or_else(|err| {
            eprintln!("warning: invalid HORA_LOG {value:?} ({err}); using info");
            EnvFilter::new("info")
        }),
        Err(_) => EnvFilter::new("info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
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
