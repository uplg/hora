//! Rendering: the frame layout, the monitor table, the trouble pane and the
//! footer.

use hora_core::display::DisplayState;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Row, Sparkline, Table};

use super::app::{App, Input};

pub(super) fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let [header, table_area, spark_area, trouble, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(5),
        Constraint::Length(4),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    // Header: title - overall - updated - source.
    let (title, (label, badge)) = app.summary.as_ref().map_or_else(
        || {
            (
                "hora".to_owned(),
                ("connecting...".to_owned(), overall_style("")),
            )
        },
        |summary| (summary.title.clone(), header_badge(summary)),
    );
    let mut spans = vec![
        Span::styled(format!(" {title} "), Style::new().bold()),
        Span::styled(format!(" {label} "), badge),
        Span::raw(format!("  updated {}  {}", app.updated, app.url)),
    ];
    if let Some(storage) = app.summary.as_ref().and_then(|s| s.storage.as_ref()) {
        spans.push(Span::raw(format!(
            "  db {}",
            hora_core::fmt::bytes(storage.db_bytes)
        )));
    }
    if let Some(error) = app.status_error() {
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
                "latency 24 h - {} (p95 {})",
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
        let (text, style) = match notice {
            Ok(done) => (done, Style::new().fg(Color::Green)),
            Err(reason) => (reason, Style::new().fg(Color::Red).bold()),
        };
        spans.push(Span::styled(format!("   {text}"), style));
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
                    format!(" {}", monitor.shown().symbol()),
                    monitor.name.clone(),
                    monitor.group.clone().unwrap_or_default(),
                    monitor
                        .uptime_permille
                        .map_or_else(|| "-".to_owned(), hora_core::fmt::permille),
                    ms(monitor.p50),
                    ms(monitor.p95),
                    ms(monitor.p99),
                    monitor.last_error.clone().unwrap_or_default(),
                ])
                .style(status_style(monitor.shown()))
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
/// or one green line. Failing notification channels are shown here too: a
/// broken Telegram bot discovered at incident time is the failure this dashboard
/// exists to prevent.
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
            .filter(|monitor| !matches!(monitor.shown(), DisplayState::Up | DisplayState::Maint))
            .take(3)
            .map(|monitor| {
                Line::from(vec![
                    Span::styled(
                        format!(" {} {} ", monitor.shown().symbol(), monitor.name),
                        status_style(monitor.shown()).bold(),
                    ),
                    Span::raw(monitor.last_error.clone().unwrap_or_default()),
                ])
            })
            .collect()
    });
    let broken_channels: Vec<Line> = app.summary.as_ref().map_or_else(Vec::new, |summary| {
        summary
            .channels
            .iter()
            .filter(|ch| ch.failing)
            .map(|ch| {
                Line::from(Span::styled(
                    format!(
                        " ⚠ channel '{}' failing ({} failures, {})",
                        ch.name,
                        ch.consecutive_failures,
                        ch.failing_for_secs
                            .map_or_else(|| "?".to_owned(), hora_core::fmt::elapsed),
                    ),
                    Style::new().fg(Color::Yellow),
                ))
            })
            .collect()
    });
    let compact = app
        .summary
        .as_ref()
        .and_then(|summary| summary.storage.as_ref())
        .and_then(compact_hint)
        .map(|hint| Line::from(Span::styled(hint, Style::new().fg(Color::DarkGray))));
    if troubled.is_empty() && banners.is_empty() && broken_channels.is_empty() {
        let mut lines = vec![Line::from(Span::styled(
            " all monitors up",
            Style::new().fg(Color::Green),
        ))];
        lines.extend(compact);
        return lines;
    }
    banners.extend(troubled);
    banners.extend(broken_channels);
    banners.extend(compact);
    banners
}

/// Over this share of the file in free pages, `hora top` suggests a
/// compaction: below it the file reuses its free pages soon enough.
const COMPACT_HINT_PERCENT: u64 = 20;

/// The one-line suggestion to run `hora compact`, when it would give back
/// more than [`COMPACT_HINT_PERCENT`] of the file.
pub(super) fn compact_hint(storage: &super::fetch::Storage) -> Option<String> {
    (storage.reclaimable_bytes.saturating_mul(100)
        > storage.db_bytes.saturating_mul(COMPACT_HINT_PERCENT))
    .then(|| {
        format!(
            " hora compact would give back at least {} of {} (stop the daemon first)",
            hora_core::fmt::bytes(storage.reclaimable_bytes),
            hora_core::fmt::bytes(storage.db_bytes)
        )
    })
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
    value.map_or_else(|| "-".to_owned(), hora_core::fmt::latency)
}

fn status_style(state: DisplayState) -> Style {
    match state {
        DisplayState::Up => Style::new().fg(Color::Green),
        DisplayState::Degraded => Style::new().fg(Color::Yellow),
        DisplayState::Down => Style::new().fg(Color::Red),
        DisplayState::Maint => Style::new().fg(Color::Cyan),
        DisplayState::None => Style::new().fg(Color::DarkGray),
    }
}

/// The header's badge. A daemon older than 0.12 folds the raw statuses
/// into `overall`: when every monitor that is not up is inside a
/// maintenance window, the page says maintenance and so does this.
pub(super) fn header_badge(summary: &super::fetch::Summary) -> (String, Style) {
    let troubled = summary
        .monitors
        .iter()
        .any(|monitor| matches!(monitor.shown(), DisplayState::Down | DisplayState::Degraded));
    let maintained = summary
        .monitors
        .iter()
        .any(|monitor| monitor.shown() == DisplayState::Maint);
    if summary.overall != "up" && !troubled && maintained {
        (
            DisplayState::Maint.word().to_owned(),
            Style::new().fg(Color::Black).bg(Color::Cyan),
        )
    } else {
        (
            summary.overall_label.clone(),
            overall_style(&summary.overall),
        )
    }
}

/// The header badge colour, from the summary's machine-readable `overall`.
pub(super) fn overall_style(overall: &str) -> Style {
    match overall {
        "up" => Style::new().fg(Color::Black).bg(Color::Green),
        "degraded" => Style::new().fg(Color::Black).bg(Color::Yellow),
        "down" => Style::new().fg(Color::White).bg(Color::Red),
        "maintenance" => Style::new().fg(Color::Black).bg(Color::Cyan),
        _ => Style::new().fg(Color::Black).bg(Color::DarkGray),
    }
}
