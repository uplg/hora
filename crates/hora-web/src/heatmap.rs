//! The smokeping-style latency heatmap: hours x days, colour = how slow that
//! hour was *relative to the monitor's own median*. Patterns pop out at a
//! glance ("slow every Monday at 9am") without any threshold to tune - the
//! info-only companion of an adaptive baseline, with zero false-positive risk.

use std::collections::HashMap;
use std::fmt::Write as _;

use chrono::DateTime;

use crate::render::xml_escape;

/// The window rendered: four full weeks, so each weekday appears four times
/// and a weekly pattern is visible as a horizontal stripe rhythm.
pub(crate) const HEATMAP_DAYS: i64 = 28;

const CELL_W: f64 = 16.0;
const CELL_H: f64 = 9.0;
const GAP: f64 = 1.5;
/// Left gutter for the hour labels, top gutter for the day labels.
const LEFT: f64 = 34.0;
const TOP: f64 = 18.0;

/// Colour for a cell at `ratio` = cell latency / monitor median. The tiers
/// are deliberately coarse: the heatmap shows *patterns*, not numbers (the
/// numbers are in each cell's tooltip).
fn tier_color(ratio: f64) -> &'static str {
    if ratio <= 1.25 {
        "#10b981" // around the median: normal
    } else if ratio <= 2.0 {
        "#84cc16"
    } else if ratio <= 3.0 {
        "#f59e0b"
    } else if ratio <= 5.0 {
        "#f97316"
    } else {
        "#ef4444" // 5x the median or worse
    }
}

/// Render the heatmap SVG from `(hour_ts, avg_latency_ms)` cells (as returned
/// by [`hora_core::db::latency_hourly`]). `now` anchors the window: the last
/// column is today (UTC), hours run top to bottom.
pub(crate) fn render(cells: &[(i64, i64)], now: i64, monitor_name: &str) -> String {
    let day_start = (now / 86_400 - (HEATMAP_DAYS - 1)) * 86_400;
    let by_hour: HashMap<i64, i64> = cells.iter().copied().collect();

    // The reference for "how slow is unusual": the median of the rendered
    // cells. A monitor with a stable baseline shows green everywhere; the
    // outliers carry the colour.
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

    let width = LEFT + coordf(HEATMAP_DAYS) * CELL_W + GAP;
    let height = TOP + 24.0 * CELL_H + GAP;
    let name = xml_escape(monitor_name);
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {width:.0} {height:.0}\" \
         width=\"{width:.0}\" height=\"{height:.0}\" role=\"img\" \
         aria-label=\"Latency heatmap of {name}, last {HEATMAP_DAYS} days\" \
         font-family=\"ui-sans-serif,system-ui,sans-serif\" font-size=\"8\">"
    );
    let _ = write!(
        svg,
        "<rect width=\"{width:.0}\" height=\"{height:.0}\" rx=\"6\" fill=\"#11161f\"/>"
    );

    // Hour gutter: a label every six rows is enough to orient.
    for hour in [0_i64, 6, 12, 18] {
        let y = TOP + coordf(hour) * CELL_H + 7.0;
        let _ = write!(
            svg,
            "<text x=\"{x:.0}\" y=\"{y:.1}\" fill=\"#8b93a1\" text-anchor=\"end\">{hour:02}h</text>",
            x = LEFT - 5.0,
        );
    }

    for day in 0..HEATMAP_DAYS {
        let day_ts = day_start + day * 86_400;
        let x = LEFT + coordf(day) * CELL_W;

        // Day labels weekly, anchored on the column's weekday ("Mon 08"):
        // the weekday is the whole point of a 4-week window.
        if day % 7 == 0
            && let Some(date) = DateTime::from_timestamp(day_ts, 0)
        {
            let _ = write!(
                svg,
                "<text x=\"{x:.1}\" y=\"{y:.0}\" fill=\"#8b93a1\">{label}</text>",
                y = TOP - 6.0,
                label = date.format("%a %d"),
            );
        }

        for hour in 0..24_i64 {
            let Some(latency) = by_hour.get(&(day_ts + hour * 3600)) else {
                continue; // no data: the background shows through
            };
            let ratio = coordf(*latency) / coordf(median);
            let color = tier_color(ratio);
            let y = TOP + coordf(hour) * CELL_H;
            let title = DateTime::from_timestamp(day_ts, 0).map_or_else(String::new, |date| {
                format!(
                    "{} {:02}:00 UTC - {latency} ms",
                    date.format("%a %Y-%m-%d"),
                    hour
                )
            });
            let _ = write!(
                svg,
                "<rect x=\"{x:.1}\" y=\"{y:.1}\" width=\"{w:.1}\" height=\"{h:.1}\" rx=\"1.5\" \
                 fill=\"{color}\"><title>{title}</title></rect>",
                w = CELL_W - GAP,
                h = CELL_H - GAP,
            );
        }
    }

    svg.push_str("</svg>");
    svg
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
        assert_eq!(tier_color(1.0), "#10b981");
        assert_eq!(tier_color(1.8), "#84cc16");
        assert_eq!(tier_color(2.5), "#f59e0b");
        assert_eq!(tier_color(4.0), "#f97316");
        assert_eq!(tier_color(10.0), "#ef4444");
    }

    #[test]
    fn renders_cells_relative_to_the_median() {
        let now = 30 * 86_400; // some UTC midnight
        // Three cells in-window: two at the median (100ms), one 10x slower.
        let day = (now / 86_400 - 1) * 86_400;
        let cells = vec![(day, 100), (day + 3600, 100), (day + 7200, 1000)];

        let svg = render(&cells, now, "API <prod>");
        assert!(svg.starts_with("<svg"));
        // The name is escaped into the aria-label.
        assert!(svg.contains("API &lt;prod&gt;"));
        // Median cells are green, the outlier red, tooltips carry the numbers.
        assert!(svg.contains("#10b981"));
        assert!(svg.contains("#ef4444"));
        assert!(svg.contains("1000 ms"));
        // Exactly three data cells are drawn (plus the background rect).
        assert_eq!(svg.matches("<rect").count(), 4);
    }

    #[test]
    fn empty_series_renders_background_only() {
        let svg = render(&[], 30 * 86_400, "API");
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert_eq!(svg.matches("<rect").count(), 1);
    }
}
