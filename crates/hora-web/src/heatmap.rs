//! The smokeping-style latency heatmap: hours x days, colour = how slow that
//! hour was *relative to the monitor's own median*. Patterns pop out at a
//! glance ("slow every Monday at 9am") without any threshold to tune - the
//! info-only companion of an adaptive baseline, with zero false-positive risk.

use std::collections::HashMap;
use std::fmt::Write as _;

use chrono::DateTime;

use maud::{Markup, PreEscaped, html};

/// The window rendered: four full weeks, so each weekday appears four times
/// and a weekly pattern is visible as a horizontal stripe rhythm.
pub(crate) const HEATMAP_DAYS: i64 = 28;

const CELL_W: f64 = 14.0;
const CELL_H: f64 = 9.0;
const GAP: f64 = 1.5;
/// Left gutter for the hour labels, top gutter for the day labels. The
/// labels are 12 units tall and the image is never shown under one CSS pixel
/// a unit (the stylesheet's `min-width`; a narrow screen scrolls it), so
/// they stay 12 px or more.
const LEFT: f64 = 36.0;
const TOP: f64 = 22.0;
/// How many of the slowest hours the page lists under the picture.
const SLOWEST: usize = 5;

/// The tier of a cell at `ratio` = cell latency / monitor median, `t0` to
/// `t4`. The tiers are deliberately coarse: the heatmap shows *patterns*,
/// not numbers (the numbers are in each cell's tooltip, and the slowest
/// hours are written out under the picture).
fn tier(ratio: f64) -> usize {
    if ratio <= 1.25 {
        0 // around the median: usual
    } else if ratio <= 2.0 {
        1
    } else if ratio <= 3.0 {
        2
    } else if ratio <= 5.0 {
        3
    } else {
        4 // 5x the median or worse
    }
}

// tokens:begin - generated from brand/tokens.json by brand/src/tokens.py, do not edit
/// Each tier's colour, from the brand tokens (`--heat-0` to `--heat-4`, light
/// theme): the fill attribute every viewer can read, even one that drops the
/// stylesheet. On the monitor page the stylesheet repaints the tiers from the
/// live tokens (and so follows the theme); the standalone image restyles
/// itself for dark mode with its own `<style>`.
const TIER_LIGHT: [&str; 5] = ["#d5e3e2", "#9fc0c2", "#5a959c", "#0e5e68", "#1b222c"];
const TIER_DARK: [&str; 5] = ["#27383f", "#2f5f69", "#4d8f99", "#8cc4cb", "#e6f4f5"];
/// The standalone image's card (`--surface`) and its labels (`--ink-muted`).
const SURFACE_LIGHT: &str = "#faf9f5";
const SURFACE_DARK: &str = "#1a2029";
const INK_MUTED_LIGHT: &str = "#525a66";
const INK_MUTED_DARK: &str = "#a8aeb8";
// tokens:end

/// The standalone image's own theme: a dark variant for dark-mode viewers.
fn standalone_style() -> Markup {
    let mut tiers = String::new();
    for (index, color) in TIER_DARK.iter().enumerate() {
        let _ = write!(tiers, ".t{index}{{fill:{color}}}");
    }
    // Our own constants only: nothing here to escape, and a stylesheet's
    // braces and colons are not text.
    let css = format!(
        "@media (prefers-color-scheme: dark){{.bg{{fill:{SURFACE_DARK}}}text{{fill:{INK_MUTED_DARK}}}{tiers}}}"
    );
    html! { style { (PreEscaped(css)) } }
}

/// Render the heatmap SVG from `(hour_ts, avg_latency_ms)` cells (as returned
/// by [`hora_core::db::latency_hourly`]). `now` anchors the window: the last
/// column is today (UTC), hours run top to bottom. `standalone` is the image
/// served on its own (it carries its dark-mode style). Without it this is
/// the monitor page's panel: the same image, themed by the stylesheet, in a
/// scroller a keyboard can reach, then its key and the slowest hours in
/// words (a cell's tooltip reaches neither touch nor keyboard).
pub(crate) fn render(
    cells: &[(i64, i64)],
    now: i64,
    monitor_name: &str,
    standalone: bool,
) -> String {
    let svg = image(cells, now, monitor_name, standalone);
    if standalone {
        return svg.into_string();
    }
    html! {
        div.heat tabindex="0" role="group"
            aria-label="Hour by hour, scrolls sideways on a small screen" { (svg) }
        div.heatkey {
            span { i.t0 {} "Usual" }
            span { i.t1 {} "Up to 2× slower" }
            span { i.t2 {} "Up to 3×" }
            span { i.t3 {} "Up to 5×" }
            span { i.t4 {} "Slower still" }
        }
        p.muted.small {
            "Each square is one hour, compared with this service's usual answer time."
        }
        (slowest(cells, now))
    }
    .into_string()
}

/// The first second the heatmap drawn at `now` covers: what a caller must
/// fetch from, so that the cells and the grid agree on their first day.
pub(crate) fn since(now: i64) -> i64 {
    (now / 86_400 - (HEATMAP_DAYS - 1)) * 86_400
}

/// The first hour of the window, and the median of its cells: the reference
/// for "how slow is unusual". A monitor with a stable baseline shows the
/// usual tier everywhere; the outliers carry the colour.
fn window(cells: &[(i64, i64)], now: i64) -> (i64, i64) {
    let day_start = since(now);
    let mut latencies: Vec<i64> = cells
        .iter()
        .filter(|(hour, _)| *hour >= day_start)
        .map(|(_, latency)| *latency)
        .collect();
    latencies.sort_unstable();
    let median = latencies
        .get(latencies.len() / 2)
        .copied()
        .unwrap_or(1)
        .max(1);
    (day_start, median)
}

/// The slowest hours of the window in words, slowest first: the figures the
/// picture only gives on hover. Only hours that stand out (over the usual
/// tier) are listed; a steady monitor gets one sentence instead.
fn slowest(cells: &[(i64, i64)], now: i64) -> Markup {
    let (day_start, median) = window(cells, now);
    let mut inside: Vec<(i64, i64)> = cells
        .iter()
        .copied()
        .filter(|(hour, _)| *hour >= day_start)
        .collect();
    if inside.is_empty() {
        return html! {};
    }
    let usual = crate::layout::ms(median);
    inside.retain(|(_, latency)| tier(coordf(*latency) / coordf(median)) > 0);
    // Slowest first; of two alike, the latest.
    inside.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    inside.truncate(SLOWEST);
    if inside.is_empty() {
        return html! {
            p.slowest-none.small {
                "No hour stood out: each one stayed close to the usual " (usual) "."
            }
        };
    }
    let hours = inside.into_iter().filter_map(|(hour, latency)| {
        // One decimal, in integers: 3.3 times the usual.
        let tenths = (latency * 10 + median / 2) / median;
        DateTime::from_timestamp(hour, 0).map(|at| (at, latency, tenths))
    });
    html! {
        h3.slowest-h { "Slowest hours" }
        p.muted.small { "The usual answer time is " (usual) ". Times in UTC." }
        ol.slowest {
            @for (at, latency, tenths) in hours {
                li {
                    time datetime=(at.format("%Y-%m-%dT%H:%MZ")) { (at.format("%a %-d %b, %H:%M")) }
                    b { (crate::layout::ms(latency)) }
                    span { (tenths / 10) "." (tenths % 10) "× the usual" }
                }
            }
        }
    }
}

/// The image itself.
fn image(cells: &[(i64, i64)], now: i64, monitor_name: &str, standalone: bool) -> Markup {
    let (day_start, median) = window(cells, now);
    let by_hour: HashMap<i64, i64> = cells.iter().copied().collect();

    let width = LEFT + coordf(HEATMAP_DAYS) * CELL_W + GAP;
    let height = TOP + 24.0 * CELL_H + GAP;
    // Coordinates as the drawing writes them: whole units, or one decimal.
    let whole = |value: f64| format!("{value:.0}");
    let tenth = |value: f64| format!("{value:.1}");
    let days = (0..HEATMAP_DAYS).map(|day| (day, day_start + day * 86_400));
    html! {
        svg xmlns="http://www.w3.org/2000/svg"
            viewBox={ "0 0 " (whole(width)) " " (whole(height)) }
            width=(whole(width)) height=(whole(height)) role="img"
            aria-label={ "Latency heatmap of " (monitor_name) ", last " (HEATMAP_DAYS) " days" }
            font-family="ui-sans-serif,system-ui,sans-serif" font-size="12" {
            @if standalone { (standalone_style()) }
            rect.bg width=(whole(width)) height=(whole(height)) rx="6" fill=(SURFACE_LIGHT) {}
            // Hour gutter: a label every six rows is enough to orient.
            @for hour in [0_i64, 6, 12, 18] {
                text x=(whole(LEFT - 6.0)) y=(tenth(TOP + coordf(hour) * CELL_H + 8.5))
                    fill=(INK_MUTED_LIGHT) text-anchor="end" { (format!("{hour:02}h")) }
            }
            @for (day, day_ts) in days {
                @let x = tenth(LEFT + coordf(day) * CELL_W);
                @let date = DateTime::from_timestamp(day_ts, 0);
                // Day labels weekly, anchored on the column's weekday
                // ("Mon 08"): the weekday is the whole point of a 4-week window.
                @if let (0, Some(date)) = (day % 7, date) {
                    text x=(x) y=(whole(TOP - 7.0)) fill=(INK_MUTED_LIGHT) { (date.format("%a %d")) }
                }
                @for hour in 0..24_i64 {
                    // No data: the background shows through.
                    @if let Some(latency) = by_hour.get(&(day_ts + hour * 3600)) {
                        @let tier = tier(coordf(*latency) / coordf(median));
                        rect class={ "t" (tier) } x=(x) y=(tenth(TOP + coordf(hour) * CELL_H))
                            width=(tenth(CELL_W - GAP)) height=(tenth(CELL_H - GAP)) rx="1.5"
                            fill=(TIER_LIGHT[tier]) {
                            title {
                                @if let Some(date) = date {
                                    (date.format("%a %Y-%m-%d")) " " (format!("{hour:02}")) ":00 UTC - " (latency) " ms"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// `i64` to SVG coordinate, mirroring `render::coord` for the i64-heavy math here.
fn coordf(value: i64) -> f64 {
    crate::render::coord(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiers_scale_with_the_ratio() {
        assert_eq!(tier(1.0), 0);
        assert_eq!(tier(1.8), 1);
        assert_eq!(tier(2.5), 2);
        assert_eq!(tier(4.0), 3);
        assert_eq!(tier(10.0), 4);
    }

    #[test]
    fn renders_cells_relative_to_the_median() {
        let now = 30 * 86_400; // some UTC midnight
        // Three cells in-window: two at the median (100ms), one 10x slower.
        let day = (now / 86_400 - 1) * 86_400;
        let cells = vec![(day, 100), (day + 3600, 100), (day + 7200, 1000)];

        let svg = render(&cells, now, "API <prod>", true);
        assert!(svg.starts_with("<svg"));
        // The name is escaped into the aria-label.
        assert!(svg.contains("API &lt;prod&gt;"));
        // Median cells are the usual tier, the outlier the worst; the fills
        // are the brand's (up is petrol, never green); tooltips carry numbers.
        assert!(svg.contains("class=\"t0\"") && svg.contains(TIER_LIGHT[0]));
        assert!(svg.contains("class=\"t4\"") && svg.contains(TIER_LIGHT[4]));
        assert!(!svg.contains("#10b981"));
        assert!(svg.contains("1000 ms"));
        // Exactly three data cells are drawn (plus the background rect).
        assert_eq!(svg.matches("<rect").count(), 4);
        // The standalone image follows dark mode by itself; the inline one
        // carries no <style> (the page's CSP has no 'unsafe-inline').
        assert!(svg.contains("prefers-color-scheme: dark"));
        let inline = render(&cells, now, "API", false);
        assert!(!inline.contains("<style"));
        // Labels of 12 units on a 430-unit image the stylesheet never shows
        // smaller than that: 12 px or more, where 8 units gave under 5.
        assert!(svg.contains("viewBox=\"0 0 430 240\"") && svg.contains("font-size=\"12\""));
        // On the page the scroller takes focus, and the figures a tooltip
        // held are written out: the one hour that stood out, slowest first.
        assert!(
            inline.starts_with("<div class=\"heat\" tabindex=\"0\""),
            "{inline}"
        );
        assert!(
            inline.contains(
                "<li><time datetime=\"1970-01-30T02:00Z\">Fri 30 Jan, 02:00</time><b>1.0 s</b>\
                 <span>10.0× the usual</span></li>"
            ),
            "{inline}"
        );
        assert_eq!(inline.matches("<li>").count(), 1);
        assert!(inline.contains("The usual answer time is 100 ms."));
        // A steady monitor gets a sentence, an empty one nothing.
        let steady = render(&[(day, 100), (day + 3600, 110)], now, "API", false);
        assert!(steady.contains("No hour stood out"), "{steady}");
        assert!(!render(&[], now, "API", false).contains("slowest"));
    }

    #[test]
    fn empty_series_renders_background_only() {
        let svg = render(&[], 30 * 86_400, "API", true);
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert_eq!(svg.matches("<rect").count(), 1);
    }

    /// What a config can put in a monitor's name never ends the attribute it
    /// is written in: the escaping is the template's, not a call to remember.
    #[test]
    fn a_hostile_name_stays_inside_its_attribute() {
        let svg = render(&[], 1_800_000_000, r#"A&B "<svg onload=x>" 'c'"#, true);
        assert!(
            svg.contains(r#"aria-label="Latency heatmap of A&amp;B &quot;&lt;svg onload=x&gt;&quot; 'c', last 28 days""#),
            "{svg}"
        );
        assert_eq!(svg.matches("<svg").count(), 1);
    }
}
