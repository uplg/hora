//! `hora top`: a live terminal dashboard over the JSON API - statuses,
//! uptime, latency percentiles, a sparkline for the selected monitor, and
//! the current trouble, refreshed in place. Self-hosters live in SSH; this
//! is the status page for them. It reads `/api/summary` and
//! `/api/monitors/{id}/latency` exactly like any other API client, local or
//! remote (`--url https://status.example --token ...`), and with a token it
//! also writes: `a` pins an announcement, `C` clears them, `s` silences the
//! selected monitor (`/api/announce`, `/api/silence`).
//!
//! The UI never waits on the network: every request runs as a background
//! task that reports through a channel, so a slow or unreachable server
//! leaves the screen live and `q`/Ctrl-C always answer.

mod app;
mod fetch;
#[cfg(test)]
mod tests;
mod ui;

use std::io::IsTerminal as _;
use std::time::Duration;

use anyhow::Context as _;
use futures_util::StreamExt as _;
use tokio::sync::mpsc;

use app::{App, KeyOutcome, apply, follow_up, on_key};
use fetch::{Api, Fetcher, FollowUp};
use ui::draw;

/// Parse `hora top` arguments and run the dashboard until `q`/`Esc`.
pub async fn run(args: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        std::io::stdout().is_terminal(),
        "hora top needs an interactive terminal"
    );
    let (url, token, interval) = parse_args(args)?;
    let client = hora_core::http::client(None).context("building HTTP client")?;
    let api = Api { client, url, token };

    // `ratatui::init` enters the alternate screen, enables raw mode, and
    // installs a panic hook that restores the terminal - a crash never
    // leaves the shell in raw mode.
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, api, interval).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    api: Api,
    interval: Duration,
) -> anyhow::Result<()> {
    let mut app = App::new(api.url.clone());
    // At most one fetch of each kind is in flight, so a handful of slots is
    // all the channel ever holds.
    let (tx, mut results) = mpsc::channel(8);
    let mut fetcher = Fetcher::new(api, tx);

    let mut events = crossterm::event::EventStream::new();
    // The first tick fires at once: the initial fetch, without delaying the
    // first frame.
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Selection changes only schedule a sparkline fetch: held-down arrow keys
    // auto-repeat tens of times a second, and one request per repeat would
    // burn through the server's per-IP rate limit (the burst is 30 by
    // default). The fetch fires once the selection rests for a moment.
    let mut spark_due: Option<tokio::time::Instant> = None;

    loop {
        terminal.draw(|frame| draw(frame, &mut app))?;
        let spark_timer = async {
            match spark_due {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = ticker.tick() => {
                if !app.cooling_down() {
                    fetcher.summary();
                }
            }
            () = spark_timer => {
                spark_due = None;
                if let Some(id) = app.selected_id() {
                    follow_up(&mut app, &mut fetcher, FollowUp::Spark(id));
                }
            }
            // The fetcher holds a sender, so the channel never closes.
            Some(done) = results.recv() => {
                let next = apply(&mut app, done);
                follow_up(&mut app, &mut fetcher, next);
            }
            event = events.next() => {
                let Some(Ok(crossterm::event::Event::Key(key))) = event else { continue };
                match on_key(&mut app, &mut fetcher, key) {
                    KeyOutcome::Quit => return Ok(()),
                    KeyOutcome::Moved => {
                        app.clear_spark();
                        spark_due =
                            Some(tokio::time::Instant::now() + Duration::from_millis(300));
                    }
                    KeyOutcome::Handled => {}
                }
            }
        }
    }
}

/// `hora top [--url URL] [--token TOKEN] [--interval SECS]`. Without `--url`
/// the local config's `server.bind` is used; the token also falls back to
/// the `HORA_TOKEN` environment variable (kept out of `ps` output).
fn parse_args(args: &[String]) -> anyhow::Result<(String, Option<String>, Duration)> {
    let mut url = None;
    let mut token = std::env::var("HORA_TOKEN").ok();
    let mut interval = 5_u64;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--url" => url = Some(required(&mut iter, "--url")?),
            "--token" => token = Some(required(&mut iter, "--token")?),
            "--interval" => {
                interval = required(&mut iter, "--interval")?
                    .parse()
                    .context("--interval must be seconds")?;
            }
            other => anyhow::bail!("unknown option {other:?} (try --url, --token, --interval)"),
        }
    }
    let url = if let Some(url) = url {
        url
    } else {
        let config = hora_core::config::load_from(&hora_core::config::path())
            .context("no --url given and the local config did not load")?;
        // A wildcard bind (the Docker default, HORA_BIND=0.0.0.0:8787) is a
        // listen address, not a place to connect to: talk to loopback.
        let bind = config
            .server
            .bind
            .replacen("0.0.0.0", "127.0.0.1", 1)
            .replacen("[::]", "[::1]", 1);
        format!("http://{bind}")
    };
    Ok((url, token, Duration::from_secs(interval.max(1))))
}

fn required(iter: &mut std::slice::Iter<'_, String>, flag: &str) -> anyhow::Result<String> {
    iter.next()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value"))
}
