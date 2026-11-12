//! The dashboard state and how keys and fetched data change it.

use crossterm::event::KeyCode;
use hora_core::announce::{self, Announcement};
use ratatui::widgets::TableState;

use super::fetch::{Action, Fetched, Fetcher, FollowUp, Monitor, RATE_LIMIT_COOLDOWN, Summary};

/// What the input bar is collecting, when open.
pub(super) enum Input {
    /// `a`: an announcement line - `title :: body`, plus optional
    /// `--severity` / `--until` flags, exactly like the CLI.
    Announce { buffer: String },
    /// `s`: a silence duration for the selected monitor (pre-filled `10m`).
    Silence { monitor_id: String, buffer: String },
}

impl Input {
    fn buffer_mut(&mut self) -> &mut String {
        match self {
            Self::Announce { buffer } | Self::Silence { buffer, .. } => buffer,
        }
    }
}

/// Everything the draw pass needs, updated as fetch results arrive.
pub(super) struct App {
    pub(super) url: String,
    pub(super) summary: Option<Summary>,
    /// 24h latency series of the selected monitor, oldest first.
    pub(super) spark: Vec<u64>,
    pub(super) table: TableState,
    /// The last fetch error, shown without wiping the previous data.
    pub(super) error: Option<String>,
    /// Why the selected monitor's sparkline is empty, when its fetch failed:
    /// an empty chart alone reads as "no checks", which is another fact.
    pub(super) spark_error: Option<String>,
    pub(super) updated: String,
    /// The input bar, replacing the footer while open.
    pub(super) input: Option<Input>,
    /// Outcome of the last action: `Ok("pinned ...")` or the reason it failed.
    pub(super) notice: Option<Result<String, String>>,
    /// Back-off deadline after the server rate-limited us (HTTP 429).
    cooldown_until: Option<tokio::time::Instant>,
}

impl App {
    pub(super) fn new(url: String) -> Self {
        let mut table = TableState::default();
        table.select(Some(0));
        Self {
            url,
            summary: None,
            spark: Vec::new(),
            table,
            error: None,
            spark_error: None,
            updated: "-".to_owned(),
            input: None,
            notice: None,
            cooldown_until: None,
        }
    }

    pub(super) fn selected(&self) -> Option<&Monitor> {
        let summary = self.summary.as_ref()?;
        summary.monitors.get(self.table.selected()?)
    }

    pub(super) fn selected_id(&self) -> Option<String> {
        self.selected().map(|monitor| monitor.id.clone())
    }

    pub(super) fn select_delta(&mut self, delta: i64) {
        let count = self.summary.as_ref().map_or(0, |s| s.monitors.len());
        if count == 0 {
            return;
        }
        let current = i64::try_from(self.table.selected().unwrap_or(0)).unwrap_or(0);
        let next = (current + delta).rem_euclid(i64::try_from(count).unwrap_or(1));
        self.table.select(Some(usize::try_from(next).unwrap_or(0)));
    }

    /// The selection moved on: its chart, and why it was empty, go with it.
    pub(super) fn clear_spark(&mut self) {
        self.spark.clear();
        self.spark_error = None;
    }

    /// What the header shows in red: the summary's fetch error first (the
    /// whole screen is stale then), else the sparkline's.
    pub(super) fn status_error(&self) -> Option<&str> {
        self.error.as_deref().or(self.spark_error.as_deref())
    }

    /// Back off after the server rate-limited us, and say so.
    fn rate_limited(&mut self) {
        self.cooldown_until = Some(tokio::time::Instant::now() + RATE_LIMIT_COOLDOWN);
        self.error = Some("rate limited by the server (HTTP 429) - backing off 15 s".to_owned());
    }

    /// Whether a 429 back-off is still running (clearing it once expired).
    pub(super) fn cooling_down(&mut self) -> bool {
        match self.cooldown_until {
            Some(until) if tokio::time::Instant::now() < until => true,
            _ => {
                self.cooldown_until = None;
                false
            }
        }
    }
}

/// What a key press asks of the event loop.
pub(super) enum KeyOutcome {
    Quit,
    /// The selection moved: schedule the new monitor's sparkline.
    Moved,
    Handled,
}

/// Handle one key event. Never awaits: anything that talks to the server is
/// handed to the fetcher.
pub(super) fn on_key(
    app: &mut App,
    fetcher: &mut Fetcher,
    key: crossterm::event::KeyEvent,
) -> KeyOutcome {
    if key.kind != crossterm::event::KeyEventKind::Press {
        return KeyOutcome::Handled;
    }
    if key
        .modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && key.code == KeyCode::Char('c')
    {
        return KeyOutcome::Quit;
    }
    // While the input bar is open it owns the keyboard.
    if app.input.is_some() {
        if let Some(action) = input_key(app, key.code) {
            start_action(app, fetcher, action);
        }
        return KeyOutcome::Handled;
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return KeyOutcome::Quit,
        KeyCode::Down | KeyCode::Char('j') => {
            app.select_delta(1);
            return KeyOutcome::Moved;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            app.select_delta(-1);
            return KeyOutcome::Moved;
        }
        KeyCode::Char('r') => {
            if !app.cooling_down() {
                fetcher.summary();
            }
        }
        KeyCode::Char('a') => {
            app.notice = None;
            app.input = Some(Input::Announce {
                buffer: String::new(),
            });
        }
        KeyCode::Char('s') => {
            if let Some(id) = app.selected_id() {
                app.notice = None;
                app.input = Some(Input::Silence {
                    monitor_id: id,
                    buffer: "10m".to_owned(),
                });
            }
        }
        KeyCode::Char('C') => start_action(
            app,
            fetcher,
            Ok(Action {
                method: reqwest::Method::DELETE,
                path: "/api/announce",
                params: Vec::new(),
                done: "announcements cleared".to_owned(),
            }),
        ),
        _ => {}
    }
    KeyOutcome::Handled
}

/// Fold a finished fetch into the app. A failure keeps the previous data on
/// screen and surfaces the reason in the header; a 429 backs off instead of
/// hammering the server's rate limit further.
pub(super) fn apply(app: &mut App, fetched: Fetched) -> FollowUp {
    match fetched {
        Fetched::Summary(Ok(summary)) => {
            // Keep the selection on the same monitor across refreshes.
            if let Some(id) = app.selected_id() {
                let index = summary.monitors.iter().position(|m| m.id == id);
                app.table.select(index.or(Some(0)));
            }
            app.summary = Some(summary);
            app.error = None;
            app.updated = chrono::Utc::now().format("%H:%M:%S UTC").to_string();
            app.selected_id().map_or(FollowUp::Nothing, FollowUp::Spark)
        }
        Fetched::Summary(Err(err)) => {
            if err.is_rate_limited() {
                app.rate_limited();
            } else {
                app.error = Some(err.to_string());
            }
            FollowUp::Nothing
        }
        Fetched::Spark { monitor_id, result } => {
            // A late answer for a monitor no longer selected is stale.
            if app.selected_id().as_deref() == Some(monitor_id.as_str()) {
                match result {
                    Ok(points) => {
                        app.spark = points;
                        app.spark_error = None;
                    }
                    // An empty chart with no word read as a monitor without
                    // checks; say the fetch failed, and back off on a 429
                    // like the summary does.
                    Err(err) => {
                        app.spark.clear();
                        if err.is_rate_limited() {
                            app.rate_limited();
                        }
                        app.spark_error = Some(format!("latency history: {err}"));
                    }
                }
            }
            FollowUp::Nothing
        }
        Fetched::Action(outcome) => {
            app.notice = Some(outcome);
            FollowUp::Refresh
        }
    }
}

/// Start what a finished fetch asked for - unless the server rate-limited us:
/// the back-off covers every request, not only the ticker's. The refresh
/// after an action and the sparkline after a summary used to go out during
/// it, each one more request against a limit already hit. Skipped, they come
/// with the first tick after the back-off.
pub(super) fn follow_up(app: &mut App, fetcher: &mut Fetcher, next: FollowUp) {
    if next == FollowUp::Nothing || app.cooling_down() {
        return;
    }
    match next {
        FollowUp::Nothing => {}
        FollowUp::Spark(id) => fetcher.spark(id),
        FollowUp::Refresh => {
            fetcher.summary();
        }
    }
}

/// Start `action` (or show why it could not be built or sent).
fn start_action(app: &mut App, fetcher: &mut Fetcher, action: Result<Action, String>) {
    app.notice = match action.and_then(|action| fetcher.action(action)) {
        Ok(()) => None,
        Err(reason) => Some(Err(reason)),
    };
}

/// Keys while the input bar is open: type, Backspace, Enter submits (closing
/// the bar and yielding the action, or why it is invalid), Esc cancels.
pub(super) fn input_key(app: &mut App, code: KeyCode) -> Option<Result<Action, String>> {
    match code {
        KeyCode::Enter => match app.input.take()? {
            Input::Announce { buffer } => Some(announce_action(&buffer)),
            Input::Silence { monitor_id, buffer } => Some(silence_action(&monitor_id, &buffer)),
        },
        KeyCode::Esc => {
            app.input = None;
            None
        }
        KeyCode::Backspace => {
            app.input.as_mut()?.buffer_mut().pop();
            None
        }
        KeyCode::Char(c) => {
            app.input.as_mut()?.buffer_mut().push(c);
            None
        }
        _ => None,
    }
}

/// Pin an announcement from the input line: `title :: body`, with optional
/// `--severity <s>` and `--until <4h|18:00>` flags anywhere - the same flag
/// grammar and validation as `hora announce` ([`announce::split_flags`]); the
/// server re-validates and resolves `--until` against its own clock.
pub(super) fn announce_action(line: &str) -> Result<Action, String> {
    let flags = announce::split_flags(line.split_whitespace()).map_err(|err| err.to_string())?;
    // Announcing from `top` usually means trouble.
    let severity = flags.severity.unwrap_or(announce::Severity::Warning);
    let text = flags.words.join(" ");
    let (title, body) = text
        .split_once("::")
        .map_or((text.as_str(), ""), |(t, b)| (t, b));
    let announcement =
        Announcement::new(title, body, severity, None).map_err(|err| err.to_string())?;
    let done = format!("announced [{severity}] {:?}", announcement.title);
    let mut params = vec![
        ("title", announcement.title),
        ("severity", severity.to_string()),
    ];
    if !announcement.body.is_empty() {
        params.push(("body", announcement.body));
    }
    if let Some(until) = flags.until {
        params.push(("until", until.to_owned()));
    }
    Ok(Action {
        method: reqwest::Method::POST,
        path: "/api/announce",
        params,
        done,
    })
}

/// Silence the selected monitor for the typed duration.
pub(super) fn silence_action(monitor_id: &str, duration: &str) -> Result<Action, String> {
    let duration = duration.trim();
    if duration.is_empty() {
        return Err("silence needs a duration (e.g. 10m)".to_owned());
    }
    Ok(Action {
        method: reqwest::Method::POST,
        path: "/api/silence",
        params: vec![
            ("monitors", monitor_id.to_owned()),
            ("duration", duration.to_owned()),
            ("reason", "silenced from hora top".to_owned()),
        ],
        done: format!(
            "silenced {monitor_id} for {}",
            hora_core::parse_duration(duration)
                .and_then(|secs| i64::try_from(secs).ok())
                .map_or_else(|| duration.to_owned(), hora_core::fmt::duration)
        ),
    })
}
