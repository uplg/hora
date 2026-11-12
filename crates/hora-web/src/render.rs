//! Server-rendered SVG: the owl, the daily bar, the latency sparkline and the
//! status/uptime badges.

use std::fmt::Write as _;

use axum::http::header;
use axum::response::IntoResponse;
use badgelib::{Badge, Color, Style};
use chrono::DateTime;

use hora_core::db::{EventMarker, Point};
use hora_core::fmt::xml_escape;

// --- The owl ------------------------------------------------------------
// HoraFace: outlined (surface fill, ink line) so it follows the theme; its
// pose is the page's state, set by `data-state` and drawn by the stylesheet.
// It is never the only carrier of the state, so it is `aria-hidden`.

pub(crate) const OWL_PARTS: &str = concat!(
    r#"<path class="ln tl" d="M17.6 16.8C15.4 15.9 13.9 14 13.4 11.3 15.9 11.7 18.4 13 20.2 15.2Z"/>"#,
    r#"<path class="ln tr" d="M30.4 16.8C32.6 15.9 34.1 14 34.6 11.3 32.1 11.7 29.6 13 27.8 15.2Z"/>"#,
    r#"<ellipse class="ft" cx="20.6" cy="41.1" rx="1.9" ry="1.2"/><ellipse class="ft" cx="27.4" cy="41.1" rx="1.9" ry="1.2"/>"#,
    r#"<path class="ln" d="M24 14C30.9 14 35.6 19.2 35.6 26.6 35.6 34.6 30.6 40.4 24 40.4S12.4 34.6 12.4 26.6C12.4 19.2 17.1 14 24 14Z"/>"#,
    r#"<g class="tx"><circle cx="18.9" cy="24.6" r="4.9"/><circle cx="29.1" cy="24.6" r="4.9"/><path d="M14.4 30.8C15.8 33.7 18.1 36.3 21 37.8M33.6 30.8C32.2 33.7 29.9 36.3 27 37.8"/></g>"#,
    r#"<g class="ey"><g class="shut-l"><path d="M16.7 24.4a2.2 2.2 0 0 0 4.4 0"/></g><g class="shut-r"><path d="M26.9 24.4a2.2 2.2 0 0 0 4.4 0"/></g>"#,
    r#"<g class="open-l"><ellipse cx="18.9" cy="24.6" rx="2.2" ry="2.6"/><circle class="glint" cx="19.7" cy="23.6" r=".75"/></g>"#,
    r#"<g class="open-r"><ellipse cx="29.1" cy="24.6" rx="2.2" ry="2.6"/><circle class="glint" cx="29.9" cy="23.6" r=".75"/></g>"#,
    r#"<g class="dash"><path d="M16.9 24.8h4M27.1 24.8h4"/></g></g>"#,
    r#"<path class="bk" d="M22.9 28.3h2.2L24 30.1Z"/>"#,
    r#"<ellipse class="ck" cx="16.4" cy="30.6" rx="2" ry="1.2"/><ellipse class="ck" cx="31.6" cy="30.6" rx="2" ry="1.2"/>"#,
    r#"<g class="badge"><circle class="bd" cx="39.5" cy="38.5" r="7.2"/>"#,
    r##"<g class="b-degraded"><svg x="33.5" y="32.5" width="12" height="12"><use href="#g-degraded"/></svg></g>"##,
    r##"<g class="b-down"><svg x="33.5" y="32.5" width="12" height="12"><use href="#g-down"/></svg></g>"##,
    r##"<g class="b-maint"><svg x="33.5" y="32.5" width="12" height="12"><use href="#g-maint"/></svg></g>"##,
    r##"<g class="b-none"><svg x="33.5" y="32.5" width="12" height="12"><use href="#g-none"/></svg></g></g>"##,
);

/// The owl in the pose of `state` (`up`, `degraded`, `down`, `maint`,
/// `none`), `size` CSS pixels square. One per page, beside its sentence.
pub(crate) fn owl(state: &str, size: u32) -> String {
    format!(
        "<svg class=\"owl\" data-state=\"{state}\" viewBox=\"6 6 40 40\" width=\"{size}\" \
         height=\"{size}\" aria-hidden=\"true\" focusable=\"false\">{OWL_PARTS}</svg>"
    )
}

/// A service's answer times over the last 24 hours as a bare line, to sit
/// beside its state on the status page: the shape only (a flat line, a
/// step, a spike), each point at its own time so a hole stays a hole in
/// width. Empty with fewer than two points. Decorative: the figures are on
/// the monitor's page.
pub(crate) fn mini_spark(points: &[Point]) -> String {
    const W: f64 = 88.0;
    const H: f64 = 18.0;
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        return String::new();
    };
    if points.len() < 2 {
        return String::new();
    }
    let low = points
        .iter()
        .map(|point| point.latency_ms)
        .min()
        .unwrap_or(0);
    let high = points
        .iter()
        .map(|point| point.latency_ms)
        .max()
        .unwrap_or(0);
    let span = coord(last.t - first.t).max(1.0);
    // A steady service draws a flat line in the middle, not noise blown up
    // to the full height: the scale never spans less than 20 ms.
    let range = coord(high - low).max(20.0);
    let mid = coord(low + high) / 2.0;
    let mut path = String::with_capacity(points.len() * 12);
    for (index, point) in points.iter().enumerate() {
        let x = coord(point.t - first.t) / span * W;
        let y = H / 2.0 - (coord(point.latency_ms) - mid) / range * (H - 2.0);
        let _ = write!(path, "{}{x:.1} {y:.1}", if index == 0 { 'M' } else { 'L' });
    }
    format!(
        "<svg class=\"mini-spark\" viewBox=\"0 0 {W:.0} {H:.0}\" preserveAspectRatio=\"none\" \
         aria-hidden=\"true\" focusable=\"false\"><path d=\"{path}\"/></svg>"
    )
}

/// Several owls on one branch, one per place, each in its own pose: the mesh
/// drawn (the watchers page and the vantage panels only). `label` is the
/// sentence the picture says, for assistive tech.
pub(crate) fn owls(places: &[(&str, &str)], label: &str) -> String {
    const STEP: usize = 46;
    // Drawn at one and a half CSS pixels a unit, so the 8-unit names read at
    // 12 px; the stylesheet only ever shrinks it to fit a narrow screen.
    let width = STEP * places.len() + 8;
    let mut out = format!(
        "<svg class=\"owls\" viewBox=\"0 0 {width} 62\" width=\"{}\" height=\"93\" role=\"img\" \
         aria-label=\"{}\">\
         <path class=\"branch\" d=\"M0 45.5C{:.0} 43.5 {:.0} 47.5 {width} 44.5\"/>",
        width * 3 / 2,
        xml_escape(label),
        coord(width) * 0.3,
        coord(width) * 0.7,
    );
    for (index, (state, _)) in places.iter().enumerate() {
        let x = 4 + index * STEP;
        let _ = write!(
            out,
            "<svg class=\"owl\" data-state=\"{state}\" x=\"{x}\" y=\"0\" width=\"46\" height=\"46\" \
             viewBox=\"6 6 40 40\">{OWL_PARTS}</svg>"
        );
    }
    for (index, (_, name)) in places.iter().enumerate() {
        // A perch is 46 units wide: a long name is cut here (it is whole in
        // the list beside the picture, and in the picture's label).
        let short: String = if name.chars().count() > OWL_NAME {
            name.chars()
                .take(OWL_NAME - 1)
                .chain(['\u{2026}'])
                .collect()
        } else {
            (*name).to_owned()
        };
        let _ = write!(
            out,
            "<text class=\"olab\" x=\"{}\" y=\"58\" text-anchor=\"middle\">{}</text>",
            4 + index * STEP + 23,
            xml_escape(&short),
        );
    }
    out.push_str("</svg>");
    out
}

/// The longest name written whole under an owl, in characters.
const OWL_NAME: usize = 10;

// --- The daily bar ------------------------------------------------------
// ONE svg per monitor with a few run-length rects; the gaps between days are
// one mask shared by the whole page (`#gaps`, in the sprite). A day's height
// is part of its state, so the bar survives greyscale: up full height, slow
// shorter, down full and below the line, maintenance half, no data a stub.

/// A day's cell class in the bar: `u` up, `d` slow, `x` down, `m`
/// maintenance, `n` no data.
pub(crate) fn day_class(state: &str) -> u8 {
    match state {
        "up" => b'u',
        "degraded" => b'd',
        "down" => b'x',
        "maint" => b'm',
        _ => b'n',
    }
}

/// Render the daily bar, oldest day first, from day cell classes.
pub(crate) fn day_bar(days: &[u8]) -> String {
    let mut rects = String::new();
    let mut start = 0;
    while let Some(&class) = days.get(start) {
        let run = days[start..]
            .iter()
            .take_while(|day| **day == class)
            .count();
        let (y, height) = match class {
            b'u' => (0, 22),
            b'd' => (7, 15),
            b'x' => (0, 29),
            b'm' => (11, 11),
            _ => (18, 4),
        };
        let _ = write!(
            rects,
            "<rect class=\"{}\" x=\"{start}\" y=\"{y}\" width=\"{run}\" height=\"{height}\"/>",
            char::from(class)
        );
        start += run;
    }
    format!(
        "<svg viewBox=\"0 0 {} 29\" preserveAspectRatio=\"none\" aria-hidden=\"true\" \
         focusable=\"false\"><g mask=\"url(#gaps)\">{rects}</g></svg>",
        days.len().max(1)
    )
}

/// A month's day strip for the report, first day first, from day cell
/// classes: run-length rects like [`day_bar`], on a 16-unit height (down
/// fills it, up stops short). The gaps between days are the page's shared
/// mask (`#gaps`, as for the bar: a drawn mask prints as it shows, where a
/// stylesheet one lost its gaps on paper). The svg's class says the month's
/// length (`d31`), which sets its width: a running month's strip keeps the
/// whole month's width, its days to come left empty.
pub(crate) fn day_strip(days: &[u8], month_days: usize) -> String {
    let mut rects = String::new();
    let mut start = 0;
    while let Some(&class) = days.get(start) {
        let run = days[start..]
            .iter()
            .take_while(|day| **day == class)
            .count();
        let (y, height) = match class {
            b'u' => (4, 12),
            b'd' => (8, 8),
            b'x' => (0, 16),
            b'm' => (10, 6),
            _ => (14, 2),
        };
        let _ = write!(
            rects,
            "<rect class=\"{}\" x=\"{start}\" y=\"{y}\" width=\"{run}\" height=\"{height}\"/>",
            char::from(class)
        );
        start += run;
    }
    let width = month_days.max(days.len()).max(1);
    format!(
        "<svg class=\"d{width}\" viewBox=\"0 0 {width} 16\" preserveAspectRatio=\"none\" \
         aria-hidden=\"true\" focusable=\"false\"><g mask=\"url(#gaps)\">{rects}</g></svg>"
    )
}

/// A thin horizontal meter (`value` of `max`), drawn as an svg so its width is
/// an attribute, not an inline style the CSP would refuse.
pub(crate) fn meter(value: i64, max: i64, class: &str) -> String {
    let filled = if max > 0 {
        (value.clamp(0, max) * 100) / max
    } else {
        0
    };
    format!(
        "<svg class=\"{class}\" viewBox=\"0 0 100 8\" preserveAspectRatio=\"none\" aria-hidden=\"true\" \
         focusable=\"false\"><rect class=\"track\" width=\"100\" height=\"8\"/>\
         <rect class=\"fill\" width=\"{filled}\" height=\"8\"/></svg>"
    )
}

// --- Server-rendered latency chart --------------------------------------
// Colours come from CSS (the `status` class on the <svg>), not inline here.
// The svg is stretched to its box (`preserveAspectRatio="none"`), so it holds
// shapes only: every label is HTML around it, never distorted text inside.

pub(crate) const CHART_W: f64 = 680.0;
pub(crate) const CHART_H: f64 = 120.0;

/// Saturating conversion of any integer to an SVG coordinate (`f64`).
pub(crate) fn coord<T: TryInto<i32>>(value: T) -> f64 {
    f64::from(value.try_into().unwrap_or(i32::MAX))
}

/// The response-time chart, and what it shows said in words: nothing on it
/// is carried by the drawing (or by a hover tooltip) alone.
pub(crate) struct Chart {
    /// The scale's labels, the svg and the time labels.
    pub(crate) html: String,
    /// "between 58 ms and 212 ms, typically 64 ms"; empty without data.
    pub(crate) summary: String,
    /// The markers drawn, oldest first, for the list under the chart.
    pub(crate) events: Vec<ChartEvent>,
    /// The monitor's "slow above" threshold, when it has one.
    pub(crate) limit: Option<ChartLimit>,
}

/// One marker of the chart ("deploy api v2.3"), with when it was recorded.
pub(crate) struct ChartEvent {
    /// "Thu 14:02 UTC".
    pub(crate) at: String,
    /// The same instant for `<time datetime>`.
    pub(crate) iso: String,
    pub(crate) title: String,
}

/// The threshold above which an answer counts as slow.
pub(crate) struct ChartLimit {
    /// "800 ms".
    pub(crate) text: String,
    /// Whether its line is on the chart (it is left out when it sits far
    /// above every answer: drawing it would flatten the curve).
    pub(crate) drawn: bool,
}

/// The top of the scale for a series peaking at `max` ms: the next "nice"
/// number (1, 2, 4, 5 or 10 times a power of ten) whose half is a whole
/// number too, so the three gridlines read 0, half and top.
pub(crate) fn nice_top(max: i64) -> i64 {
    let max = max.max(1);
    for half in [1, 2, 5] {
        if half * 2 >= max {
            return half * 2;
        }
    }
    let mut decade: i64 = 10;
    while decade < i64::MAX / 100 {
        for half in [decade, decade * 2, decade / 2 * 5, decade * 5] {
            if half * 2 >= max {
                return half * 2;
            }
        }
        decade *= 10;
    }
    max
}

/// A latency as an axis label: `0`, `150 ms`, `2.5 s`.
fn axis_label(latency: i64) -> String {
    if latency == 0 {
        "0".to_owned()
    } else {
        crate::layout::ms(latency)
    }
}

/// The chart's scale: time from the first sample over `span` seconds, answer
/// time from zero to `top` ms.
struct Scale {
    first: i64,
    span: i64,
    top: i64,
}

impl Scale {
    fn x(&self, t: i64) -> f64 {
        CHART_W * coord(t - self.first) / coord(self.span)
    }

    fn y(&self, latency: i64) -> f64 {
        CHART_H * (1.0 - coord(latency.clamp(0, self.top)) / coord(self.top))
    }
}

/// The line and its area, as path data. A hole (a step well over the series'
/// usual one) stops both: the line is not drawn across checks that never ran.
fn series_paths(points: &[Point], scale: &Scale) -> (String, String) {
    // The series' own step is the median one: a lone short step (the
    // newest bucket, still filling) must not make every other one a hole.
    let mut steps: Vec<i64> = points
        .windows(2)
        .map(|pair| pair[1].t - pair[0].t)
        .filter(|delta| *delta > 0)
        .collect();
    steps.sort_unstable();
    let step = steps
        .get(steps.len().saturating_sub(1) / 2)
        .copied()
        .unwrap_or(scale.span);
    let mut line = String::new();
    let mut area = String::new();
    let mut start = 0;
    while start < points.len() {
        let mut end = start + 1;
        while end < points.len() && (points[end].t - points[end - 1].t) * 2 <= step * 5 {
            end += 1;
        }
        let run = &points[start..end];
        let mut path = String::new();
        for (index, point) in run.iter().enumerate() {
            let _ = write!(
                path,
                "{}{:.1} {:.1}",
                if index == 0 { 'M' } else { 'L' },
                scale.x(point.t),
                scale.y(point.latency_ms)
            );
        }
        if let ([from, _, ..], Some(to)) = (run, run.last()) {
            let _ = write!(
                area,
                "{path}L{:.1} {CHART_H:.0}L{:.1} {CHART_H:.0}Z",
                scale.x(to.t),
                scale.x(from.t)
            );
            line.push_str(&path);
        } else {
            // One sample alone: a dot (a zero-length stroke, round caps).
            let _ = write!(line, "{path}h0");
        }
        start = end;
    }
    (line, area)
}

/// The chart's svg, opened: the three gridlines, the four time ticks, then
/// the area and the line. The caller adds what lies over them and closes it.
fn chart_frame(status: &str, line: &str, area: &str) -> String {
    let mut svg = format!(
        "<svg viewBox=\"0 0 {CHART_W} {CHART_H}\" class=\"spark {status}\" \
         preserveAspectRatio=\"none\" aria-hidden=\"true\" focusable=\"false\">"
    );
    for y in [0.0, CHART_H / 2.0, CHART_H] {
        let _ = write!(
            svg,
            "<line class=\"spark-grid\" x1=\"0\" x2=\"{CHART_W}\" y1=\"{y:.0}\" y2=\"{y:.0}\"/>"
        );
    }
    for third in 0..4_i32 {
        let x = CHART_W * f64::from(third) / 3.0;
        let _ = write!(
            svg,
            "<line class=\"spark-tick\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"{CHART_H:.0}\" y2=\"{:.0}\"/>",
            CHART_H + 4.0
        );
    }
    if !area.is_empty() {
        let _ = write!(svg, "<path class=\"spark-area\" d=\"{area}\"/>");
    }
    let _ = write!(svg, "<path class=\"spark-line\" d=\"{line}\"/>");
    svg
}

/// The labels, as HTML around the svg: the scale on its left (top, half,
/// zero), four times under it (a lone sample has one).
fn chart_labels(svg: &str, top: i64, first: i64, last: i64) -> String {
    let mut html = format!(
        "<div class=\"chart\"><div class=\"chart-y\"><span>{}</span><span>{}</span>\
         <span>{}</span></div>{svg}<div class=\"chart-x\">",
        axis_label(top),
        axis_label(top / 2),
        axis_label(0),
    );
    let ticks = if last > first { 4 } else { 1 };
    for third in 0..ticks {
        let at = first + (last - first) * third / 3;
        if let Some(at) = DateTime::from_timestamp(at, 0) {
            let _ = write!(
                html,
                "<span class=\"x{third}\">{}</span>",
                at.format("%H:%M")
            );
        }
    }
    html.push_str("</div></div>");
    html
}

/// Render the last-24h latency series: a line and its area on a scale from
/// zero to a round maximum, three labelled gridlines, four time labels, and
/// the monitor's "slow above" line (`limit_ms`) when it has one. Each point
/// sits at its own time, so a hole in the series is a hole in the line and
/// the event markers (placed by time too) stay where they happened.
/// `events` overlays operator-recorded markers ("deploy api v2.3") as
/// vertical lines - empty for the public view, where deploy titles must not
/// leak.
pub(crate) fn sparkline(
    points: &[Point],
    status: &str,
    events: &[EventMarker],
    limit_ms: Option<i64>,
) -> Chart {
    let limit = limit_ms.filter(|limit| *limit > 0);
    let (Some(first), Some(last)) = (points.first(), points.last()) else {
        // No viewBox: the stretched (`preserveAspectRatio="none"`) chart
        // coordinates would squash the text too. Without one, user units are
        // CSS pixels and the label is centred undistorted at any card width.
        return Chart {
            html: format!(
                "<svg class=\"spark {status}\">\
                 <text x=\"50%\" y=\"50%\" class=\"spark-empty\" text-anchor=\"middle\" \
                 dominant-baseline=\"middle\">no data yet</text></svg>"
            ),
            summary: String::new(),
            events: Vec::new(),
            limit: limit.map(|limit| ChartLimit {
                text: crate::layout::ms(limit),
                drawn: false,
            }),
        };
    };

    let mut sorted: Vec<i64> = points.iter().map(|point| point.latency_ms).collect();
    sorted.sort_unstable();
    let low = sorted.first().copied().unwrap_or(0);
    let high = sorted.last().copied().unwrap_or(0);
    let median = sorted.get(sorted.len() / 2).copied().unwrap_or(0);

    // The threshold joins the scale unless it is far above every answer.
    let limit_drawn = limit.filter(|limit| *limit <= high.max(1).saturating_mul(2));
    let scale = Scale {
        first: first.t,
        span: (last.t - first.t).max(1),
        top: nice_top(high.max(limit_drawn.unwrap_or(0))),
    };
    let top = scale.top;
    let (line, area) = series_paths(points, &scale);

    let mut svg = chart_frame(status, &line, &area);
    if let Some(limit) = limit_drawn {
        let y = scale.y(limit);
        let _ = write!(
            svg,
            "<line class=\"spark-limit\" x1=\"0\" x2=\"{CHART_W}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>"
        );
    }
    let mut listed = Vec::new();
    for event in events
        .iter()
        .filter(|event| event.created_at >= first.t && event.created_at <= last.t)
    {
        let x = scale.x(event.created_at);
        let _ = write!(
            svg,
            "<line class=\"spark-event\" x1=\"{x:.1}\" x2=\"{x:.1}\" y1=\"0\" \
             y2=\"{CHART_H:.0}\"><title>{}</title></line>",
            xml_escape(&event.title),
        );
        if let Some(at) = DateTime::from_timestamp(event.created_at, 0) {
            listed.push(ChartEvent {
                at: at.format("%a %H:%M UTC").to_string(),
                iso: at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                title: event.title.clone(),
            });
        }
    }
    svg.push_str("</svg>");
    listed.sort_by(|a, b| a.iso.cmp(&b.iso));

    let html = chart_labels(&svg, top, first.t, last.t);

    Chart {
        html,
        summary: if low == high {
            format!("steady at {}", crate::layout::ms(median))
        } else {
            format!(
                "between {} and {}, typically {}",
                crate::layout::ms(low),
                crate::layout::ms(high),
                crate::layout::ms(median)
            )
        },
        events: listed,
        limit: limit.map(|limit| ChartLimit {
            text: crate::layout::ms(limit),
            drawn: limit_drawn.is_some(),
        }),
    }
}

// --- SVG status / uptime badges -----------------------------------------

// The badges use the brand palette: up is petrol, not green. Text-grade
// shades, so the white message keeps its contrast on every colour.
const BADGE_UP: &str = "#0e5e68";
const BADGE_UP_SOFT: &str = "#3e7a84";
const BADGE_SLOW: &str = "#8f4b22";
const BADGE_DOWN: &str = "#a5372a";
/// A badge with no data yet (also the uptime badge's colour then).
pub(crate) const BADGE_NONE: &str = "#6b6760";

pub(crate) fn status_color(status: &str) -> &'static str {
    match status {
        "up" => BADGE_UP,
        "down" => BADGE_DOWN,
        "degraded" => BADGE_SLOW,
        _ => BADGE_NONE,
    }
}

pub(crate) fn uptime_color(permille: i64) -> &'static str {
    if permille >= 999 {
        BADGE_UP
    } else if permille >= 990 {
        BADGE_UP_SOFT
    } else if permille >= 900 {
        BADGE_SLOW
    } else {
        BADGE_DOWN
    }
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

/// Render a shields-style badge: a grey label and a coloured message.
pub(crate) fn badge(label: &str, message: &str, color: &str, style: Style) -> String {
    Badge::new()
        .label(label)
        .label_color(Color::Hex("1b222c".into()))
        .value(message)
        .value_color(Color::Hex(color.trim_start_matches('#').into()))
        .style(style)
        .to_svg()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn points(series: &[(i64, i64)]) -> Vec<Point> {
        series
            .iter()
            .map(|&(t, latency_ms)| Point { t, latency_ms })
            .collect()
    }

    #[test]
    fn sparkline_renders_svg_with_status_class() {
        let empty = sparkline(&[], "up", &[], None);
        assert!(empty.html.contains("no data"));
        assert_eq!(empty.summary, "");
        // The placeholder text must not be stretched with the chart.
        assert!(!empty.html.contains("preserveAspectRatio") && !empty.html.contains("viewBox"));
        let chart = sparkline(&points(&[(1, 10), (2, 20)]), "degraded", &[], None);
        assert!(chart.html.contains("class=\"spark degraded\""));
        assert!(chart.html.contains("spark-line"));
        // No text inside the stretched svg: the labels are HTML around it.
        assert!(!chart.html.contains("<text"), "{}", chart.html);
        assert_eq!(chart.summary, "between 10 ms and 20 ms, typically 20 ms");
    }

    #[test]
    fn scale_starts_at_zero_and_ends_on_a_round_number() {
        for (max, top) in [
            (0, 2),
            (1, 2),
            (3, 4),
            (7, 10),
            (64, 100),
            (101, 200),
            (212, 400),
            (450, 500),
            (900, 1000),
            (1849, 2000),
            (4200, 5000),
        ] {
            assert_eq!(nice_top(max), top, "{max}");
            assert_eq!(nice_top(max) % 2, 0);
        }
        // A 60 to 64 ms jitter no longer fills the height: on a 0 to 100 ms
        // scale it sits flat, a little over the half line.
        let chart = sparkline(&points(&[(0, 60), (60, 64), (120, 62)]), "up", &[], None);
        assert!(
            chart.html.contains(
                "<div class=\"chart-y\"><span>100 ms</span><span>50 ms</span><span>0</span></div>"
            ),
            "{}",
            chart.html
        );
        assert!(
            chart.html.contains("M0.0 48.0L340.0 43.2L680.0 45.6"),
            "{}",
            chart.html
        );
        assert_eq!(chart.html.matches("spark-grid").count(), 3);
        assert_eq!(chart.html.matches("<span class=\"x").count(), 4);
    }

    #[test]
    fn points_sit_at_their_time_and_a_hole_breaks_the_line() {
        // Samples every 10 s, then nothing for 60 s: placed by index the
        // last two would sit at 50 % and 100 %; by time they are at the end.
        let series = points(&[(0, 5), (10, 5), (20, 5), (80, 5), (100, 5)]);
        let chart = sparkline(&series, "up", &[], None);
        assert!(
            chart.html.contains("L136.0 60.0M544.0 60.0L680.0 60.0"),
            "{}",
            chart.html
        );
        assert_eq!(chart.summary, "steady at 5 ms");
        // A short last step (the newest bucket) makes no hole of the others.
        let fresh = sparkline(
            &points(&[(0, 5), (10, 5), (20, 5), (30, 5), (31, 5)]),
            "up",
            &[],
            None,
        );
        assert_eq!(fresh.html.matches('M').count(), 2, "{}", fresh.html);
        // A sample alone between two holes is a dot, not nothing.
        let lone = sparkline(
            &points(&[(0, 5), (10, 5), (100, 5), (200, 5), (210, 5)]),
            "up",
            &[],
            None,
        );
        assert!(lone.html.contains("M323.8 60.0h0"), "{}", lone.html);
    }

    #[test]
    fn threshold_is_drawn_when_it_is_near_and_said_either_way() {
        let series = points(&[(0, 300), (60, 420)]);
        let near = sparkline(&series, "up", &[], Some(800));
        assert!(near.html.contains("spark-limit"), "{}", near.html);
        // The scale makes room for it: 0 to 1 s, the line at 800 ms.
        assert!(
            near.html.contains("<span>1.0 s</span><span>500 ms</span>"),
            "{}",
            near.html
        );
        assert!(near.html.contains("y1=\"24.0\""), "{}", near.html);
        let limit = near.limit.unwrap();
        assert!(limit.drawn && limit.text == "800 ms");
        let far = sparkline(&series, "up", &[], Some(5000));
        assert!(!far.html.contains("spark-limit"));
        assert!(!far.limit.unwrap().drawn);
        assert!(sparkline(&series, "up", &[], None).limit.is_none());
    }

    #[test]
    fn sparkline_overlays_events_inside_the_span_only() {
        let series = points(&[(100, 10), (200, 20)]);
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
        let chart = sparkline(&series, "up", &events, None);
        let svg = &chart.html;
        // In-span marker drawn at the interpolated midpoint, title escaped.
        assert!(svg.contains("spark-event"), "{svg}");
        assert!(svg.contains("<title>deploy &lt;v2&gt;</title>"), "{svg}");
        let mid = CHART_W / 2.0;
        assert!(svg.contains(&format!("x1=\"{mid:.1}\"")), "{svg}");
        // Out-of-span events never render.
        assert!(!svg.contains("too old"), "{svg}");
        // The marker is listed too (raw: the template escapes it), with its
        // time: a hover tooltip alone reaches neither touch nor keyboard.
        assert_eq!(chart.events.len(), 1);
        assert_eq!(chart.events[0].title, "deploy <v2>");
        assert_eq!(chart.events[0].at, "Thu 00:02 UTC");
        assert_eq!(chart.events[0].iso, "1970-01-01T00:02:30Z");
    }

    #[test]
    fn badge_has_label_message_and_color() {
        let svg = badge("status", "up", status_color("up"), Style::Flat);
        assert!(svg.starts_with("<svg"));
        assert!(svg.contains(">status<") && svg.contains(">up<"));
        assert!(svg.contains(&Color::Hex(status_color("up").into()).to_css()));
    }

    #[test]
    fn badge_supports_styles() {
        let flat = badge("status", "up", status_color("up"), Style::Flat);
        let flat_square = badge("status", "up", status_color("up"), Style::FlatSquare);
        let for_the_badge = badge("status", "up", status_color("up"), Style::ForTheBadge);

        assert!(flat.contains(r#"id="s""#));
        assert!(!flat_square.contains(r#"id="s""#));
        assert!(for_the_badge.contains(r#"height="28""#));
    }
    #[test]
    fn uptime_color_tiers() {
        assert_eq!(uptime_color(1000), BADGE_UP);
        assert_eq!(uptime_color(995), BADGE_UP_SOFT);
        assert_eq!(uptime_color(950), BADGE_SLOW);
        assert_eq!(uptime_color(800), BADGE_DOWN);
        // Up is petrol, never green.
        assert_eq!(status_color("up"), "#0e5e68");
    }

    #[test]
    fn day_bar_is_one_svg_of_run_length_rects() {
        let mut days = vec![b'u'; 90];
        days[40] = b'x';
        days[88] = b'd';
        days[89] = b'd';
        let svg = day_bar(&days);
        assert!(svg.starts_with("<svg viewBox=\"0 0 90 29\""), "{svg}");
        // up, down, up, slow: four runs, not ninety nodes.
        assert_eq!(svg.matches("<rect").count(), 4, "{svg}");
        assert!(
            svg.contains("class=\"x\" x=\"40\" y=\"0\" width=\"1\" height=\"29\""),
            "{svg}"
        );
        assert!(
            svg.contains("class=\"d\" x=\"88\" y=\"7\" width=\"2\""),
            "{svg}"
        );
        assert!(svg.contains("mask=\"url(#gaps)\""));
        // An empty bar still renders a valid (blank) svg.
        assert!(day_bar(&[]).contains("viewBox=\"0 0 1 29\""));
    }

    #[test]
    fn owl_is_decorative_and_posed_by_state() {
        let svg = owl("degraded", 96);
        assert!(svg.contains("data-state=\"degraded\"") && svg.contains("aria-hidden=\"true\""));
        let many = owls(
            &[("down", "Paris <1>"), ("up", "Frankfurt")],
            "Paris sees it down",
        );
        assert_eq!(many.matches("class=\"owl\"").count(), 2);
        assert!(many.contains("Paris &lt;1&gt;") && many.contains("role=\"img\""));
        // Sized so the names read at 12 px; a long one is cut, not overlapping.
        assert!(many.contains("viewBox=\"0 0 100 62\" width=\"150\" height=\"93\""));
        let long = owls(&[("up", "Johannesburg")], "one place");
        assert!(long.contains(">Johannesb\u{2026}</text>"), "{long}");
    }

    #[test]
    fn meter_width_is_an_attribute() {
        let svg = meter(31, 43, "meter");
        assert!(svg.contains("class=\"fill\" width=\"72\""), "{svg}");
        assert!(!svg.contains("style"));
        assert!(meter(5, 0, "meter").contains("width=\"0\""));
    }
}
