//! Hora — a tiny self-hosted uptime monitor.
//!
//! Wires the pieces together: load config, open the database, start the
//! supervisor (which owns the live config and notification channels), spawn the
//! certificate watcher and pruner, and serve the status page and JSON API.

use anyhow::Context as _;
use hora_core::config;
use reqwest::Client;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config_path = config::path();
    let initial = config::load_from(&config_path).context("loading configuration")?;
    let pool = hora_core::db::connect(&initial.server.database_path)
        .await
        .context("opening database")?;
    let client = build_http_client().context("building HTTP client")?;

    // The supervisor owns the live config + notification channels and reconciles
    // monitor tasks on reload; other components read through its handles.
    let handle = hora_core::supervisor::start(initial, config_path, pool.clone(), client);
    hora_core::cert::spawn_watcher(pool.clone(), handle.config.clone(), handle.notifier.clone());
    hora_core::db::spawn_pruner(&pool, handle.config.clone());

    let bind = handle.config.borrow().server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(
        "hora {} listening on http://{bind}",
        env!("CARGO_PKG_VERSION")
    );

    let state = hora_web::AppState::new(pool, handle.config);
    // Connect-info gives the rate limiter a peer IP to fall back on when there
    // is no `X-Forwarded-For` (i.e. direct access, not behind a proxy).
    let app = hora_web::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("running HTTP server")?;
    Ok(())
}

fn init_tracing() {
    let filter = EnvFilter::try_from_env("HORA_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn build_http_client() -> reqwest::Result<Client> {
    Client::builder()
        .user_agent(concat!("hora/", env!("CARGO_PKG_VERSION")))
        .build()
}

async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        tracing::error!("failed to listen for shutdown signal: {err}");
    }
    tracing::info!("shutting down");
}
