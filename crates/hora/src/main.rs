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
            "backup" => {
                let Some(dest) = args.get(2) else {
                    eprintln!("Usage: hora backup <destination.db>");
                    std::process::exit(1);
                };
                backup(dest).await?;
            }
            "incidents" => {
                let limit = args
                    .get(2)
                    .map_or(Ok(20), |raw| raw.parse::<i64>())
                    .unwrap_or_else(|_| {
                        eprintln!("Usage: hora incidents [limit]");
                        std::process::exit(1);
                    });
                list_incidents(limit.max(1)).await?;
            }
            "annotate" => {
                if args.len() < 4 {
                    eprintln!("Usage: hora annotate <incident-id|last> <note>");
                    eprintln!("An empty note (\"\") clears the annotation.");
                    std::process::exit(1);
                }
                annotate(&args[2], &args[3..].join(" ")).await?;
            }
            "silence" => {
                silence(&args[2..]).await?;
            }
            "--version" | "-V" => println!("hora {}", env!("CARGO_PKG_VERSION")),
            "--help" | "-h" => print_help(),
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

fn print_help() {
    println!("Hora - a tiny self-hosted uptime monitor");
    println!();
    println!("Usage: hora [COMMAND]");
    println!();
    println!("Commands:");
    println!("  import kuma <file>  Convert an Uptime Kuma backup JSON to Hora TOML (stdout)");
    println!("  check               Validate the configuration and exit");
    println!("  test-alert [id]     Send a test down + recovered through the configured");
    println!("                      channels (all of them, or the routed ones of monitor [id])");
    println!("  silence <ids> <for> [reason]  Mute alerts for monitors (comma-separated ids");
    println!("                      or 'all') for a duration like 10m or 1h30m (max 7d)");
    println!("  silence list        Show the active silences");
    println!("  silence clear       Remove every silence");
    println!("  incidents [limit]   List recent incidents with their ids");
    println!("  annotate <id> <note>  Attach a note to an incident ('last' targets the");
    println!("                      most recent one; an empty note clears it)");
    println!("  backup <dest.db>    Snapshot the database with VACUUM INTO");
    println!("  --version, -V       Show the version");
    println!("  --help, -h          Show this help message");
}

/// Open the daemon's database for a CLI subcommand. Refuses to *create* one: a
/// missing file means the config points somewhere the daemon never wrote (a
/// different working directory, usually), and silently creating an empty
/// database there would only hide the mistake.
async fn open_database() -> anyhow::Result<(hora_core::config::Config, hora_core::db::SqlitePool)> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    let path = &config.server.database_path;
    if path != ":memory:" && !path.starts_with("file:") && !std::path::Path::new(path).exists() {
        anyhow::bail!(
            "database {path} not found - run from the daemon's working directory, \
             or point HORA_CONFIG at its config"
        );
    }
    let pool = hora_core::db::connect(path)
        .await
        .context("opening database")?;
    Ok((config, pool))
}

/// Snapshot the database to `dest` via `VACUUM INTO`: consistent and compacted,
/// safe while the daemon runs. Meant for cron ("a one-statement answer to 'what
/// if I lose a year of history?'").
async fn backup(dest: &str) -> anyhow::Result<()> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    let source = &config.server.database_path;
    hora_core::db::backup_into(source, dest).await?;
    let size = std::fs::metadata(dest).map_or(0, |meta| meta.len());
    println!("Backed up {source} to {dest} ({} KiB).", size / 1024);
    Ok(())
}

/// List recent incidents with their ids - the lookup companion of `annotate`.
async fn list_incidents(limit: i64) -> anyhow::Result<()> {
    let (config, pool) = open_database().await?;
    let incidents = hora_core::db::recent_incidents(&pool, limit).await?;
    if incidents.is_empty() {
        println!("No incidents recorded.");
        return Ok(());
    }
    for incident in incidents {
        let name = config
            .monitors
            .iter()
            .find(|monitor| monitor.id == incident.monitor_id)
            .map_or(incident.monitor_id.as_str(), |monitor| {
                monitor.name.as_str()
            });
        let span = match incident.ended_at {
            Some(ended) => format!(
                "{} -> {} ({})",
                format_epoch(incident.started_at),
                format_epoch(ended),
                format_secs(incident.duration_s.unwrap_or(0))
            ),
            None => format!("{} -> ongoing", format_epoch(incident.started_at)),
        };
        println!("#{}  {name}  {span}", incident.id);
        if let Some(error) = &incident.error {
            println!("      error: {error}");
        }
        // The full snapshot lives on /history; the status line is enough here.
        if let Some(first_line) = incident
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.lines().next())
        {
            println!("      answered: {first_line}");
        }
        if let Some(note) = &incident.note {
            println!("      note:  {note}");
        }
    }
    Ok(())
}

/// Attach (or clear, with an empty note) an annotation on an incident, shown
/// on /history and in the Atom feed. `last` targets the most recent incident.
async fn annotate(id_arg: &str, note: &str) -> anyhow::Result<()> {
    let (_, pool) = open_database().await?;
    let id = if id_arg == "last" {
        let Some(id) = hora_core::db::latest_incident_id(&pool).await? else {
            eprintln!("No incidents recorded yet.");
            std::process::exit(1);
        };
        id
    } else {
        id_arg.parse().unwrap_or_else(|_| {
            eprintln!("Invalid incident id {id_arg:?} (a number, or 'last').");
            std::process::exit(1);
        })
    };
    if !hora_core::db::set_incident_note(&pool, id, note).await? {
        eprintln!("No incident #{id}. 'hora incidents' lists the recent ones.");
        std::process::exit(1);
    }
    if note.is_empty() {
        println!("Cleared the note on incident #{id}.");
    } else {
        println!("Annotated incident #{id}: {note}");
    }
    Ok(())
}

/// `hora silence <ids|all> <duration> [reason]` / `list` / `clear`: ad-hoc
/// alert muting (a deploy window) written straight into the daemon's database,
/// picked up on its next tick. The HTTP counterpart is `POST /api/silence`.
async fn silence(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let (_, pool) = open_database().await?;
            let now = chrono::Utc::now().timestamp();
            let silences = hora_core::db::active_silences(&pool, now).await?;
            if silences.is_empty() {
                println!("No active silences.");
            }
            for silence in silences {
                let target = if silence.monitor_id == "*" {
                    "all monitors"
                } else {
                    &silence.monitor_id
                };
                let reason = silence
                    .reason
                    .map(|reason| format!(" - {reason}"))
                    .unwrap_or_default();
                println!(
                    "{target}: until {} ({} left){reason}",
                    format_epoch(silence.until),
                    format_secs(silence.until - now)
                );
            }
        }
        Some("clear") => {
            let (_, pool) = open_database().await?;
            let cleared =
                hora_core::db::clear_silences(&pool, chrono::Utc::now().timestamp()).await?;
            println!("Cleared {cleared} active silence(s).");
        }
        Some(ids) if args.len() >= 2 => {
            let Some(duration_secs) = hora_core::parse_duration(&args[1])
                .filter(|secs| *secs <= hora_core::MAX_SILENCE_SECS)
            else {
                eprintln!(
                    "Invalid duration {:?} (use e.g. 10m, 1h30m; max 7d).",
                    args[1]
                );
                std::process::exit(1);
            };
            let (config, pool) = open_database().await?;
            let monitors: Vec<&str> = if ids == "all" || ids == "*" {
                vec!["*"]
            } else {
                let ids: Vec<&str> = ids.split(',').map(str::trim).collect();
                // Fail on a typo'd id rather than silencing nothing.
                for id in &ids {
                    if !config.monitors.iter().any(|monitor| monitor.id == *id) {
                        eprintln!("Unknown monitor {id:?}. Configured ids:");
                        for monitor in &config.monitors {
                            eprintln!("  {}", monitor.id);
                        }
                        std::process::exit(1);
                    }
                }
                ids
            };
            let reason = (args.len() > 2).then(|| args[2..].join(" "));
            let until =
                chrono::Utc::now().timestamp() + i64::try_from(duration_secs).unwrap_or(i64::MAX);
            for id in &monitors {
                hora_core::db::insert_silence(&pool, id, until, reason.as_deref()).await?;
            }
            let target = if monitors == ["*"] {
                "all monitors".to_owned()
            } else {
                monitors.join(", ")
            };
            println!("Silenced {target} until {}.", format_epoch(until));
        }
        _ => {
            eprintln!("Usage: hora silence <ids|all> <duration> [reason]");
            eprintln!("       hora silence list");
            eprintln!("       hora silence clear");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn format_epoch(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn format_secs(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
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
