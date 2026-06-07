//! Server-rendered SVG: the latency sparkline and the status/uptime badges.

use std::fmt::Write as _;

use axum::http::header;
use axum::response::IntoResponse;

use hora_core::db::Point;

// --- Server-rendered latency chart --------------------------------------
// Colours come from CSS (the `status` class on the <svg>), not inline here.

pub(crate) const CHART_W: f64 = 680.0;
pub(crate) const CHART_H: f64 = 120.0;
pub(crate) const CHART_PAD: f64 = 8.0;

/// Saturating conversion of any integer to an SVG coordinate (`f64`).
pub(crate) fn coord<T: TryInto<i32>>(value: T) -> f64 {
    f64::from(value.try_into().unwrap_or(i32::MAX))
}

/// Render the last-24h latency series as a self-contained inline SVG sparkline.
pub(crate) fn sparkline(points: &[Point], status: &str) -> String {
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
    format!(
        "<svg viewBox=\"0 0 {CHART_W} {CHART_H}\" class=\"spark {status}\" preserveAspectRatio=\"none\">\
         <path class=\"spark-area\" d=\"{line}L{last_x:.1} {baseline:.1} L{CHART_PAD:.1} {baseline:.1} Z\"/>\
         <path class=\"spark-line\" d=\"{line}\"/>\
         </svg>"
    )
}

// --- SVG status / uptime badges (flat shields style) --------------------

pub(crate) const BADGE_CHAR_W: f64 = 7.0;
pub(crate) const BADGE_PAD: f64 = 6.0;

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
    // Width uses the visible length; the text is XML-escaped before embedding so
    // the inputs (today server-controlled) can never break out of the SVG.
    let label_w = coord(label.chars().count()) * BADGE_CHAR_W + 2.0 * BADGE_PAD;
    let message_w = coord(message.chars().count()) * BADGE_CHAR_W + 2.0 * BADGE_PAD;
    let total_w = label_w + message_w;
    let label_x = label_w / 2.0;
    let message_x = label_w + message_w / 2.0;
    let label = xml_escape(label);
    let message = xml_escape(message);
    format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{total_w:.0}\" height=\"20\" role=\"img\" aria-label=\"{label}: {message}\">\
         <title>{label}: {message}</title>\
         <linearGradient id=\"g\" x2=\"0\" y2=\"100%\"><stop offset=\"0\" stop-color=\"#bbb\" stop-opacity=\".1\"/><stop offset=\"1\" stop-opacity=\".1\"/></linearGradient>\
         <clipPath id=\"r\"><rect width=\"{total_w:.0}\" height=\"20\" rx=\"3\" fill=\"#fff\"/></clipPath>\
         <g clip-path=\"url(#r)\">\
         <rect width=\"{label_w:.0}\" height=\"20\" fill=\"#555\"/>\
         <rect x=\"{label_w:.0}\" width=\"{message_w:.0}\" height=\"20\" fill=\"{color}\"/>\
         <rect width=\"{total_w:.0}\" height=\"20\" fill=\"url(#g)\"/>\
         </g>\
         <g fill=\"#fff\" text-anchor=\"middle\" font-family=\"Verdana,Geneva,DejaVu Sans,sans-serif\" font-size=\"11\">\
         <text x=\"{label_x:.0}\" y=\"15\" fill=\"#010101\" fill-opacity=\".3\">{label}</text>\
         <text x=\"{label_x:.0}\" y=\"14\">{label}</text>\
         <text x=\"{message_x:.0}\" y=\"15\" fill=\"#010101\" fill-opacity=\".3\">{message}</text>\
         <text x=\"{message_x:.0}\" y=\"14\">{message}</text>\
         </g></svg>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparkline_renders_svg_with_status_class() {
        assert!(sparkline(&[], "up").contains("no data"));
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
        let svg = sparkline(&points, "degraded");
        assert!(svg.contains("class=\"spark degraded\""));
        assert!(svg.contains("spark-line"));
    }

    #[test]
    fn badge_has_label_message_and_color() {
        let svg = badge("status", "up", status_color("up"));
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains(">status<") && svg.contains(">up<"));
        assert!(svg.contains(status_color("up")));
    }
    #[test]
    fn uptime_color_tiers() {
        assert_eq!(uptime_color(1000), "#4c1");
        assert_eq!(uptime_color(995), "#97ca00");
        assert_eq!(uptime_color(800), "#e05d44");
    }
}
