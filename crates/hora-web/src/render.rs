//! Server-rendered SVG: the latency sparkline and the status/uptime badges.

use std::fmt::Write as _;

use axum::http::header;
use axum::response::IntoResponse;
use badgelib::{Badge, Color};

use hora_core::db::{EventMarker, Point};

// --- Server-rendered latency chart --------------------------------------
// Colours come from CSS (the `status` class on the <svg>), not inline here.

pub(crate) const CHART_W: f64 = 680.0;
pub(crate) const CHART_H: f64 = 120.0;
pub(crate) const CHART_PAD: f64 = 8.0;

/// Saturating conversion of any integer to an SVG coordinate (`f64`).
pub(crate) fn coord<T: TryInto<i32>>(value: T) -> f64 {
    f64::from(value.try_into().unwrap_or(i32::MAX))
}

/// Render the last-24h latency series as a self-contained inline SVG
/// sparkline. `events` overlays operator-recorded markers ("deploy api
/// v2.3") as vertical lines, positioned by time within the series' span -
/// empty for the public view, where deploy titles must not leak.
pub(crate) fn sparkline(points: &[Point], status: &str, events: &[EventMarker]) -> String {
    if points.is_empty() {
        return format!(
            "<svg viewBox=\"0 0 {CHART_W} {CHART_H}\" class=\"spark {status}\" preserveAspectRatio=\"none\">\
             <text x=\"{x:.0}\" y=\"{y:.0}\" class=\"spark-empty\" text-anchor=\"middle\">no data yet</text>\
             </svg>",
            x = CHART_W / 2.0,
            y = CHART_H / 2.0,
        );
    }

    let count = points.len();
    let max = points
        .iter()
        .map(|p| p.latency_ms)
        .max()
        .unwrap_or(1)
        .max(1);
    let min = points.iter().map(|p| p.latency_ms).min().unwrap_or(0);
    let span = coord((max - min).max(1));
    let plot_h = CHART_H - 2.0 * CHART_PAD;
    let step = if count > 1 {
        (CHART_W - 2.0 * CHART_PAD) / (coord(count) - 1.0)
    } else {
        0.0
    };

    let mut line = String::new();
    for (index, point) in points.iter().enumerate() {
        let x = CHART_PAD + step * coord(index);
        let y = CHART_PAD + plot_h * (1.0 - coord(point.latency_ms - min) / span);
        let _ = write!(line, "{}{x:.1} {y:.1} ", if index == 0 { 'M' } else { 'L' });
    }

    let last_x = CHART_PAD + step * (coord(count) - 1.0);
    let baseline = CHART_H - CHART_PAD;
    let markers = event_markers(points, events);
    format!(
        "<svg viewBox=\"0 0 {CHART_W} {CHART_H}\" class=\"spark {status}\" preserveAspectRatio=\"none\">\
         <path class=\"spark-area\" d=\"{line}L{last_x:.1} {baseline:.1} L{CHART_PAD:.1} {baseline:.1} Z\"/>\
         <path class=\"spark-line\" d=\"{line}\"/>\
         {markers}</svg>"
    )
}

/// Vertical marker lines for the events falling inside the series' time span,
/// each carrying its title as a hover tooltip. The x position interpolates the
/// event's time between the first and last sample.
fn event_markers(points: &[Point], events: &[EventMarker]) -> String {
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return String::new();
    };
    let span = coord((last.t - first.t).max(1));
    let plot_w = CHART_W - 2.0 * CHART_PAD;
    let mut out = String::new();
    for event in events
        .iter()
        .filter(|event| event.created_at >= first.t && event.created_at <= last.t)
    {
        let x = CHART_PAD + plot_w * (coord(event.created_at - first.t) / span);
        let _ = write!(
            out,
            "<line class=\"spark-event\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"{CHART_PAD:.1}\" \
             y2=\"{:.1}\"><title>{}</title></line>",
            CHART_H - CHART_PAD,
            xml_escape(&event.title),
        );
    }
    out
}

// --- SVG status / uptime badges (flat shields style) --------------------

pub(crate) fn status_color(status: &str) -> &'static str {
    match status {
        "up" => "#4c1",
        "down" => "#e05d44",
        "degraded" => "#fe7d37",
        _ => "#9f9f9f",
    }
}

pub(crate) fn uptime_color(permille: i64) -> &'static str {
    if permille >= 999 {
        "#4c1"
    } else if permille >= 990 {
        "#97ca00"
    } else if permille >= 950 {
        "#dfb317"
    } else if permille >= 900 {
        "#fe7d37"
    } else {
        "#e05d44"
    }
}

/// Escape XML metacharacters for safe embedding in an SVG document.
pub(crate) fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub(crate) fn svg_response(svg: String) -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=60"),
        ],
        svg,
    )
}

/// Render a flat shields-style badge: a grey label and a coloured message.
pub(crate) fn badge(label: &str, message: &str, color: &str) -> String {
    Badge::new()
        .label(label)
        .label_color(Color::Hex("555".into()))
        .value(message)
        .value_color(Color::Hex(color.trim_start_matches('#').into()))
        .to_svg()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparkline_renders_svg_with_status_class() {
        assert!(sparkline(&[], "up", &[]).contains("no data"));
        let points = vec![
            Point {
                t: 1,
                latency_ms: 10,
            },
            Point {
                t: 2,
                latency_ms: 20,
            },
        ];
        let svg = sparkline(&points, "degraded", &[]);
        assert!(svg.contains("class=\"spark degraded\""));
        assert!(svg.contains("spark-line"));
    }

    #[test]
    fn sparkline_overlays_events_inside_the_span_only() {
        let points = vec![
            Point {
                t: 100,
                latency_ms: 10,
            },
            Point {
                t: 200,
                latency_ms: 20,
            },
        ];
        let events = vec![
            EventMarker {
                id: 1,
                title: "deploy <v2>".to_owned(),
                created_at: 150,
            },
            EventMarker {
                id: 2,
                title: "too old".to_owned(),
                created_at: 50,
            },
        ];
        let svg = sparkline(&points, "up", &events);
        // In-span marker drawn at the interpolated midpoint, title escaped.
        assert!(svg.contains("spark-event"), "{svg}");
        assert!(svg.contains("<title>deploy &lt;v2&gt;</title>"), "{svg}");
        let mid = CHART_PAD + (CHART_W - 2.0 * CHART_PAD) / 2.0;
        assert!(svg.contains(&format!("x1=\"{mid:.1}\"")), "{svg}");
        // Out-of-span events never render.
        assert!(!svg.contains("too old"), "{svg}");
    }

    #[test]
    fn badge_has_label_message_and_color() {
        let svg = badge("status", "up", status_color("up"));
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains(">status<") && svg.contains(">up<"));
        assert!(svg.contains(&Color::Hex(status_color("up").into()).to_css()));
    }
    #[test]
    fn uptime_color_tiers() {
        assert_eq!(uptime_color(1000), "#4c1");
        assert_eq!(uptime_color(995), "#97ca00");
        assert_eq!(uptime_color(800), "#e05d44");
    }
}
