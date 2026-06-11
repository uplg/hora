//! `hora top`: a live terminal dashboard over the JSON API - statuses,
//! uptime, latency percentiles, a sparkline for the selected monitor, and
//! the current trouble, refreshed in place. Self-hosters live in SSH; this
//! is the status page for them. Read-only: it consumes `/api/summary` and
//! `/api/monitors/{id}/latency` exactly like any other API client, local or
//! remote (`--url https://status.example --token ...`).

use std::io::IsTerminal as _;
use std::time::Duration;

use anyhow::Context as _;
use crossterm::event::KeyCode;
use futures_util::StreamExt as _;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Sparkline, Table, TableState};
use serde::Deserialize;

/// What `hora top` reads from `/api/summary` (unknown fields ignored, so the
/// dashboard tolerates both older and newer servers).
#[derive(Deserialize)]
struct Summary {
    title: String,
    overall_label: String,
    monitors: Vec<Monitor>,
    /// The pinned status-page banners (config + `hora announce`).
    #[serde(default)]
    incidents: Vec<Banner>,
}

#[derive(Deserialize)]
struct Banner {
    title: String,
    severity: String,
}

#[derive(Deserialize)]
struct Monitor {
    id: String,
    name: String,
    status: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default, rename = "uptime_24h_permille")]
    uptime_permille: Option<i64>,
    #[serde(default, rename = "latency_p50_ms")]
    p50: Option<i64>,
    #[serde(default, rename = "latency_p95_ms")]
    p95: Option<i64>,
    #[serde(default, rename = "latency_p99_ms")]
    p99: Option<i64>,
}

#[derive(Deserialize)]
struct Point {
    latency_ms: i64,
}

/// What the input bar is collecting, when open.
enum Input {
    /// `a`: an announcement line - `title :: body`, plus optional
    /// `--severity` / `--until` flags, exactly like the CLI.
    Announce { buffer: String },
    /// `s`: a silence duration for the selected monitor (pre-filled `10m`).
    Silence { monitor_id: String, buffer: String },
}

/// Everything the draw pass needs, refreshed by the fetch loop.
struct App {
    url: String,
    token: Option<String>,
    summary: Option<Summary>,
    /// 24h latency series of the selected monitor, oldest first.
    spark: Vec<u64>,
    table: TableState,
    /// The last fetch error, shown without wiping the previous data.
    error: Option<String>,
    updated: String,
    /// The input bar, replacing the footer while open.
    input: Option<Input>,
    /// Outcome of the last action ("pinned", or the API's refusal).
    notice: Option<String>,
    /// Back-off deadline after the server rate-limited us (HTTP 429).
    cooldown_until: Option<tokio::time::Instant>,
}

impl App {
    fn selected(&self) -> Option<&Monitor> {
        let summary = self.summary.as_ref()?;
        summary.monitors.get(self.table.selected()?)
    }

    fn select_delta(&mut self, delta: i64) {
        let count = self.summary.as_ref().map_or(0, |s| s.monitors.len());
        if count == 0 {
            return;
        }
        let current = i64::try_from(self.table.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).rem_euclid(i64::try_from(count).unwrap_or(1));
        self.table.select(Some(usize::try_from(next).unwrap_or(0)));
    }
}

/// Parse `hora top` arguments and run the dashboard until `q`/`Esc`.
pub async fn run(args: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        std::io::stdout().is_terminal(),
        "hora top needs an interactive terminal"
    );
    let (url, token, interval) = parse_args(args)?;
    let client = hora_core::http::client(None).context("building HTTP client")?;

    // `ratatui::init` enters the alternate screen, enables raw mode, and
    // installs a panic hook that restores the terminal - a crash never
    // leaves the shell in raw mode.
    let mut terminal = ratatui::init();
    let result = event_loop(&mut terminal, &client, url, token, interval).await;
    ratatui::restore();
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    client: &reqwest::Client,
    url: String,
    token: Option<String>,
    interval: Duration,
) -> anyhow::Result<()> {
    let mut app = App {
        url,
        token,
        summary: None,
        spark: Vec::new(),
        table: TableState::default(),
        error: None,
        updated: "-".to_owned(),
        input: None,
        notice: None,
        cooldown_until: None,
    };
    app.table.select(Some(0));
    refresh(client, &mut app).await;

    let mut events = crossterm::event::EventStream::new();
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
            _ = ticker.tick() => refresh(client, &mut app).await,
            () = spark_timer => {
                spark_due = None;
                refresh_spark(client, &mut app).await;
            }
            event = events.next() => {
                let Some(Ok(crossterm::event::Event::Key(key))) = event else { continue };
                if key.kind != crossterm::event::KeyEventKind::Press {
                    continue;
                }
                if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                    && key.code == KeyCode::Char('c')
                {
                    return Ok(());
                }
                // While the input bar is open it owns the keyboard.
                if app.input.is_some() {
                    handle_input_key(client, &mut app, key.code).await;
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        app.select_delta(1);
                        app.spark.clear();
                        spark_due =
                            Some(tokio::time::Instant::now() + Duration::from_millis(300));
                    }
                    KeyCode::Up | KeyCode::Char('k') => {
                        app.select_delta(-1);
                        app.spark.clear();
                        spark_due =
                            Some(tokio::time::Instant::now() + Duration::from_millis(300));
                    }
                    KeyCode::Char('r') => refresh(client, &mut app).await,
                    KeyCode::Char('a') => {
                        app.notice = None;
                        app.input = Some(Input::Announce {
                            buffer: String::new(),
                        });
                    }
                    KeyCode::Char('s') => {
                        if let Some(id) = app.selected().map(|monitor| monitor.id.clone()) {
                            app.notice = None;
                            app.input = Some(Input::Silence {
                                monitor_id: id,
                                buffer: "10m".to_owned(),
                            });
                        }
                    }
                    KeyCode::Char('C') => {
                        let outcome = act(client, &app, reqwest::Method::DELETE, "/api/announce", &[])
                            .await;
                        app.notice = Some(match outcome {
                            Ok(()) => "announcements cleared".to_owned(),
                            Err(err) => err,
                        });
                        refresh(client, &mut app).await;
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Fetch the summary (and the selected monitor's sparkline). A failure keeps
/// the previous data on screen and surfaces the reason in the header; a 429
/// backs off instead of hammering the server's rate limit further.
async fn refresh(client: &reqwest::Client, app: &mut App) {
    if let Some(until) = app.cooldown_until {
        if tokio::time::Instant::now() < until {
            return; // Rate-limited: skip this round, keep what we have.
        }
        app.cooldown_until = None;
    }
    match fetch_json::<Summary>(client, app, "/api/summary").await {
        Ok(summary) => {
            // Keep the selection on the same monitor across refreshes.
            let selected_id = app.selected().map(|monitor| monitor.id.clone());
            if let Some(id) = selected_id {
                let index = summary.monitors.iter().position(|m| m.id == id);
                app.table.select(index.or(Some(0)));
            }
            app.summary = Some(summary);
            app.error = None;
            app.updated = chrono::Utc::now().format("%H:%M:%S UTC").to_string();
        }
        Err(err) => {
            if err.contains("429") {
                app.cooldown_until = Some(tokio::time::Instant::now() + Duration::from_secs(15));
                app.error =
                    Some("rate limited by the server (HTTP 429) - backing off 15s".to_owned());
                return;
            }
            app.error = Some(err);
        }
    }
    refresh_spark(client, app).await;
}

async fn refresh_spark(client: &reqwest::Client, app: &mut App) {
    let Some(id) = app.selected().map(|monitor| monitor.id.clone()) else {
        app.spark.clear();
        return;
    };
    let path = format!("/api/monitors/{id}/latency?hours=24");
    match fetch_json::<Vec<Point>>(client, app, &path).await {
        Ok(points) => {
            app.spark = points
                .iter()
                .map(|point| u64::try_from(point.latency_ms.max(0)).unwrap_or(0))
                .collect();
        }
        Err(_) => app.spark.clear(),
    }
}

/// Keys while the input bar is open: type, Backspace, Enter submits,
/// Esc cancels.
async fn handle_input_key(client: &reqwest::Client, app: &mut App, code: KeyCode) {
    let Some(input) = &mut app.input else { return };
    let buffer = match input {
        Input::Announce { buffer } | Input::Silence { buffer, .. } => buffer,
    };
    match code {
        KeyCode::Esc => app.input = None,
        KeyCode::Backspace => {
            buffer.pop();
        }
        KeyCode::Char(c) => buffer.push(c),
        KeyCode::Enter => {
            let input = app.input.take().expect("input open");
            let outcome = match &input {
                Input::Announce { buffer } => submit_announce(client, app, buffer).await,
                Input::Silence { monitor_id, buffer } => {
                    submit_silence(client, app, monitor_id, buffer).await
                }
            };
            app.notice = Some(match outcome {
                Ok(done) => done,
                Err(err) => err,
            });
            refresh(client, app).await;
        }
        _ => {}
    }
}

/// Pin an announcement from the input line: `title :: body`, with optional
/// `--severity <s>` and `--until <4h|18:00-style duration>` flags anywhere -
/// the same grammar as `hora announce`.
async fn submit_announce(
    client: &reqwest::Client,
    app: &App,
    line: &str,
) -> Result<String, String> {
    let mut severity = "warning"; // announcing from `top` usually means trouble
    let mut until = None;
    let mut words: Vec<&str> = Vec::new();
    let mut iter = line.split_whitespace();
    while let Some(word) = iter.next() {
        match word {
            "--severity" => {
                let value = iter.next().unwrap_or("warning");
                if !matches!(value, "info" | "warning" | "critical" | "resolved") {
                    return Err("severity must be info, warning, critical or resolved".to_owned());
                }
                severity = value;
            }
            "--until" => until = iter.next(),
            other => words.push(other),
        }
    }
    let text = words.join(" ");
    let (title, body) = text
        .split_once("::")
        .map_or((text.as_str(), ""), |(t, b)| (t, b));
    let (title, body) = (title.trim().to_owned(), body.trim().to_owned());
    if title.is_empty() {
        return Err("announce needs a title".to_owned());
    }
    let mut params = vec![("title", title.clone()), ("severity", severity.to_owned())];
    if !body.is_empty() {
        params.push(("body", body));
    }
    if let Some(until) = until {
        params.push(("until", until.to_owned()));
    }
    act(client, app, reqwest::Method::POST, "/api/announce", &params).await?;
    Ok(format!("announced [{severity}] {title:?}"))
}

/// Silence the selected monitor for the typed duration.
async fn submit_silence(
    client: &reqwest::Client,
    app: &App,
    monitor_id: &str,
    duration: &str,
) -> Result<String, String> {
    let duration = duration.trim();
    if duration.is_empty() {
        return Err("silence needs a duration (e.g. 10m)".to_owned());
    }
    act(
        client,
        app,
        reqwest::Method::POST,
        "/api/silence",
        &[
            ("monitors", monitor_id.to_owned()),
            ("duration", duration.to_owned()),
            ("reason", "silenced from hora top".to_owned()),
        ],
    )
    .await?;
    Ok(format!("silenced {monitor_id} for {duration}"))
}

/// One authenticated write to the API. Maps the typical refusals to messages
/// that say what to do ("pass `--token`", "configure `server.auth_token`").
async fn act(
    client: &reqwest::Client,
    app: &App,
    method: reqwest::Method,
    path: &str,
    params: &[(&str, String)],
) -> Result<(), String> {
    let Some(token) = &app.token else {
        return Err(
            "this action needs a token: run hora top --token ... (or HORA_TOKEN)".to_owned(),
        );
    };
    let response = client
        .request(method, format!("{}{path}", app.url.trim_end_matches('/')))
        .query(params)
        .bearer_auth(token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|err| format!("request failed: {}", err.without_url()))?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let detail = response.text().await.unwrap_or_default();
    let detail: String = detail.chars().take(120).collect();
    Err(format!("server refused ({status}): {detail}"))
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    app: &App,
    path: &str,
) -> Result<T, String> {
    let mut request = client
        .get(format!("{}{path}", app.url.trim_end_matches('/')))
        .timeout(Duration::from_secs(10));
    if let Some(token) = &app.token {
        request = request.bearer_auth(token);
    }
    let response = request.send().await.map_err(|err| {
        // The reqwest error embeds the URL (which may carry nothing secret
        // here, but stay consistent with the daemon's logging policy).
        format!("request failed: {}", err.without_url())
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("server answered HTTP {status}"));
    }
    response
        .json::<T>()
        .await
        .map_err(|_| "unexpected response shape".to_owned())
}

fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let [header, table_area, spark_area, trouble, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(5),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Header: title - overall - updated - source.
    let (title, overall) = app.summary.as_ref().map_or_else(
        || ("hora".to_owned(), "connecting...".to_owned()),
        |summary| (summary.title.clone(), summary.overall_label.clone()),
    );
    let mut spans = vec![
        Span::styled(format!(" {title} "), Style::new().bold()),
        Span::styled(format!(" {overall} "), overall_style(&overall)),
        Span::raw(format!("  updated {}  {}", app.updated, app.url)),
    ];
    if let Some(error) = &app.error {
        spans.push(Span::styled(
            format!("  {error}"),
            Style::new().fg(Color::Red).bold(),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), header);

    frame.render_stateful_widget(monitor_table(app), table_area, &mut app.table);

    // Sparkline of the selected monitor's last 24h.
    let spark_title = app.selected().map_or_else(
        || "latency".to_owned(),
        |monitor| {
            format!(
                "latency 24h - {} (p95 {})",
                monitor.name,
                ms(monitor.p95).trim().to_owned()
            )
        },
    );
    let spark = Sparkline::default()
        .block(
            Block::new()
                .borders(Borders::TOP)
                .title(spark_title)
                .border_style(Style::new().fg(Color::DarkGray)),
        )
        .data(&app.spark)
        .style(Style::new().fg(Color::Cyan));
    frame.render_widget(spark, spark_area);

    frame.render_widget(
        Paragraph::new(trouble_lines(app)).block(
            Block::new()
                .borders(Borders::TOP)
                .title("trouble")
                .border_style(Style::new().fg(Color::DarkGray)),
        ),
        trouble,
    );

    frame.render_widget(Paragraph::new(footer_line(app)), footer);
}

/// The footer: the input bar while it is open, otherwise the key hints plus
/// the outcome of the last action.
fn footer_line(app: &App) -> Line<'static> {
    if let Some(input) = &app.input {
        let (label, buffer) = match input {
            Input::Announce { buffer } => (
                " announce: title :: body [--severity info|warning|critical|resolved] [--until 4h|18:00] "
                    .to_owned(),
                buffer,
            ),
            Input::Silence { monitor_id, buffer } => {
                (format!(" silence {monitor_id} for: "), buffer)
            }
        };
        return Line::from(vec![
            Span::styled(label, Style::new().fg(Color::Black).bg(Color::Cyan)),
            Span::raw(format!(" {buffer}")),
            Span::styled("█", Style::new().fg(Color::Cyan)),
            Span::styled(
                "  (Enter sends · Esc cancels)",
                Style::new().fg(Color::DarkGray),
            ),
        ]);
    }
    let mut spans = vec![Span::styled(
        " q quit · ↑/↓ select · r refresh · a announce · s silence · C clear banners",
        Style::new().fg(Color::DarkGray),
    )];
    if let Some(notice) = &app.notice {
        let style = if notice.starts_with("server refused")
            || notice.contains("needs a")
            || notice.contains("failed")
        {
            Style::new().fg(Color::Red).bold()
        } else {
            Style::new().fg(Color::Green)
        };
        spans.push(Span::styled(format!("   {notice}"), style));
    }
    Line::from(spans)
}

/// The monitor table: one row per monitor, coloured by status.
fn monitor_table(app: &App) -> Table<'static> {
    let rows: Vec<Row> = app.summary.as_ref().map_or_else(Vec::new, |summary| {
        summary
            .monitors
            .iter()
            .map(|monitor| {
                Row::new(vec![
                    format!(" {}", status_dot(&monitor.status)),
                    monitor.name.clone(),
                    monitor.group.clone().unwrap_or_default(),
                    monitor
                        .uptime_permille
                        .map_or_else(|| "-".to_owned(), |p| format!("{}.{}%", p / 10, p % 10)),
                    ms(monitor.p50),
                    ms(monitor.p95),
                    ms(monitor.p99),
                    monitor.last_error.clone().unwrap_or_default(),
                ])
                .style(status_style(&monitor.status))
            })
            .collect()
    });
    Table::new(
        rows,
        [
            Constraint::Length(2),
            Constraint::Min(16),
            Constraint::Length(10),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Length(7),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(vec![
            "",
            "MONITOR",
            "GROUP",
            "UP 24H",
            "P50",
            "P95",
            "P99",
            "LAST ERROR",
        ])
        .style(Style::new().fg(Color::DarkGray)),
    )
    .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
    .block(
        Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().fg(Color::DarkGray)),
    )
}

/// Pinned banners first, then every monitor that is not up with its reason -
/// or one green line.
fn trouble_lines(app: &App) -> Vec<Line<'static>> {
    let mut banners: Vec<Line> = app.summary.as_ref().map_or_else(Vec::new, |summary| {
        summary
            .incidents
            .iter()
            .take(2)
            .map(|banner| {
                Line::from(Span::styled(
                    format!(" 📌 [{}] {}", banner.severity, banner.title),
                    severity_style(&banner.severity),
                ))
            })
            .collect()
    });
    let troubled: Vec<Line> = app.summary.as_ref().map_or_else(Vec::new, |summary| {
        summary
            .monitors
            .iter()
            .filter(|monitor| monitor.status != "up")
            .take(3)
            .map(|monitor| {
                Line::from(vec![
                    Span::styled(
                        format!(" {} {} ", status_dot(&monitor.status), monitor.name),
                        status_style(&monitor.status).bold(),
                    ),
                    Span::raw(monitor.last_error.clone().unwrap_or_default()),
                ])
            })
            .collect()
    });
    if troubled.is_empty() && banners.is_empty() {
        return vec![Line::from(Span::styled(
            " all monitors up",
            Style::new().fg(Color::Green),
        ))];
    }
    banners.extend(troubled);
    banners
}

fn severity_style(severity: &str) -> Style {
    match severity {
        "critical" => Style::new().fg(Color::Red),
        "warning" => Style::new().fg(Color::Yellow),
        "resolved" => Style::new().fg(Color::Green),
        _ => Style::new().fg(Color::Cyan),
    }
}

fn ms(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_owned(), |ms| format!("{ms}ms"))
}

fn status_dot(status: &str) -> &'static str {
    match status {
        "up" => "●",
        "degraded" => "◐",
        "down" => "○",
        _ => "·",
    }
}

fn status_style(status: &str) -> Style {
    match status {
        "up" => Style::new().fg(Color::Green),
        "degraded" => Style::new().fg(Color::Yellow),
        "down" => Style::new().fg(Color::Red),
        _ => Style::new().fg(Color::DarkGray),
    }
}

fn overall_style(label: &str) -> Style {
    if label.contains("operational") {
        Style::new().fg(Color::Black).bg(Color::Green)
    } else if label.contains("Degraded") {
        Style::new().fg(Color::Black).bg(Color::Yellow)
    } else if label.contains("outage") {
        Style::new().fg(Color::White).bg(Color::Red)
    } else {
        Style::new().fg(Color::Black).bg(Color::DarkGray)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_parse_flags_and_fall_back() {
        let (url, token, interval) = parse_args(&[
            "--url".to_owned(),
            "https://s.example".to_owned(),
            "--token".to_owned(),
            "tok".to_owned(),
            "--interval".to_owned(),
            "2".to_owned(),
        ])
        .expect("parse");
        assert_eq!(url, "https://s.example");
        assert_eq!(token.as_deref(), Some("tok"));
        assert_eq!(interval, Duration::from_secs(2));

        // Unknown flags fail loudly; a zero interval is clamped.
        assert!(parse_args(&["--nope".to_owned()]).is_err());
        let (_, _, interval) = parse_args(&[
            "--url".to_owned(),
            "https://s".to_owned(),
            "--interval".to_owned(),
            "0".to_owned(),
        ])
        .expect("parse");
        assert_eq!(interval, Duration::from_secs(1));
    }

    #[test]
    fn summary_json_from_the_real_api_shape_deserializes() {
        let summary: Summary = serde_json::from_str(
            r#"{
                "title": "uplg.status",
                "overall": "degraded",
                "overall_label": "Degraded performance",
                "incidents": [],
                "maintenances": [],
                "groups": [{"name": "Frank", "ids": ["web"]}],
                "peers": [],
                "monitors": [{
                    "id": "web",
                    "name": "Web",
                    "status": "degraded",
                    "last_latency_ms": 812,
                    "last_error": "slow",
                    "last_checked": "12s ago",
                    "uptime_24h_permille": 998,
                    "latency_p50_ms": 120,
                    "latency_p95_ms": 800,
                    "latency_p99_ms": 950,
                    "history": [],
                    "group": "Frank",
                    "maintenance": null
                }]
            }"#,
        )
        .expect("summary shape");
        assert_eq!(summary.monitors[0].uptime_permille, Some(998));
        assert_eq!(summary.monitors[0].p95, Some(800));
        assert_eq!(summary.overall_label, "Degraded performance");
    }
}
