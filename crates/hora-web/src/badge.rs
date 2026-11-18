//! The status and uptime badges: a shields-style SVG, a dark label beside a
//! coloured message, in the three shapes `?style=` names.
//!
//! The text is measured here, from Verdana's own advance widths, and each
//! `<text>` carries the result as `textLength`: a viewer without Verdana
//! fits its fallback font into the same box instead of overflowing it.

use maud::html;
use serde::Deserialize;

/// The label half's colour: the brand's ink.
const LABEL_COLOR: &str = "#1b222c";
const FONT_FAMILY: &str = "Verdana,Geneva,DejaVu Sans,sans-serif";

/// A badge's shape, as the `style` query parameter spells it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Style {
    /// Rounded corners and a soft vertical highlight.
    #[default]
    Flat,
    /// Square corners, solid colours.
    FlatSquare,
    /// Taller, square, in capitals, the message in bold.
    ForTheBadge,
}

/// Verdana's advance widths for the printable ASCII range (space to `~`),
/// in units of a 2048-unit em.
#[rustfmt::skip]
const REGULAR: [u16; 95] = [
    720, 806, 940, 1676, 1302, 2204, 1488, 550, 930, 930, 1302, 1676, 745, 930, 745, 930,
    1302, 1302, 1302, 1302, 1302, 1302, 1302, 1302, 1302, 1302, 930, 930, 1676, 1676, 1676, 1117,
    2048, 1400, 1404, 1430, 1578, 1295, 1177, 1588, 1539, 862, 931, 1419, 1140, 1726, 1532, 1612,
    1235, 1612, 1424, 1400, 1262, 1499, 1400, 2025, 1403, 1260, 1403, 930, 930, 930, 1676, 1302,
    1302, 1230, 1276, 1067, 1276, 1220, 720, 1276, 1296, 562, 705, 1212, 562, 1992, 1296, 1243,
    1276, 1276, 874, 1067, 807, 1296, 1212, 1676, 1212, 1212, 1076, 1300, 930, 1300, 1676,
];

/// The same for Verdana Bold.
#[rustfmt::skip]
const BOLD: [u16; 95] = [
    700, 824, 1203, 1776, 1456, 2605, 1766, 680, 1113, 1113, 1456, 1776, 740, 983, 740, 1412,
    1456, 1456, 1456, 1456, 1456, 1456, 1456, 1456, 1456, 1456, 824, 824, 1776, 1776, 1776, 1263,
    1974, 1590, 1560, 1482, 1700, 1399, 1332, 1661, 1715, 1118, 1137, 1579, 1305, 1941, 1734, 1741,
    1501, 1741, 1602, 1455, 1396, 1663, 1564, 2311, 1564, 1509, 1417, 1113, 1412, 1113, 1776, 1456,
    1456, 1368, 1432, 1205, 1432, 1360, 865, 1432, 1459, 700, 825, 1374, 700, 2167, 1459, 1406,
    1432, 1432, 1018, 1215, 933, 1459, 1331, 2006, 1370, 1333, 1222, 1456, 1113, 1456, 1776,
];

const UNITS_PER_EM: f64 = 2048.0;
/// The width given to a character outside the table: a digit's, which is
/// near the middle of the range.
const UNKNOWN: u16 = 1302;

/// The width of `text` in pixels at `font_size`, each character followed by
/// `spacing` pixels.
fn text_width(text: &str, widths: &[u16; 95], font_size: f64, spacing: f64) -> f64 {
    text.chars()
        .map(|character| {
            let units = (u32::from(character))
                .checked_sub(0x20)
                .and_then(|index| widths.get(usize::try_from(index).ok()?))
                .copied()
                .unwrap_or(UNKNOWN);
            f64::from(units) / UNITS_PER_EM * font_size + spacing
        })
        .sum()
}

/// One half of a badge: its text, measured and placed.
struct Half {
    text: String,
    /// The text's own width, for `textLength`.
    text_width: String,
    /// The half's left edge and width.
    x: f64,
    width: f64,
    /// The text's centre.
    center: String,
}

impl Half {
    fn new(text: &str, x: f64, text_width: f64, pad: f64) -> Self {
        // Whole pixels, so the two halves meet on a pixel boundary.
        let width = (text_width + 2.0 * pad).ceil();
        Self {
            text: text.to_owned(),
            text_width: format!("{text_width:.1}"),
            x,
            width,
            center: format!("{:.1}", x + width / 2.0),
        }
    }
}

/// Render a badge: `label` on ink, `message` on `color` (a CSS colour).
/// Both texts are escaped.
pub(crate) fn badge(label: &str, message: &str, color: &str, style: Style) -> String {
    match style {
        Style::Flat | Style::FlatSquare => standard(label, message, color, style == Style::Flat),
        Style::ForTheBadge => for_the_badge(label, message, color),
    }
}

/// `flat` and `flat-square`: 20 pixels high, 11-pixel text. `flat` rounds
/// the corners, lays a highlight over the colours and shadows the text.
fn standard(label: &str, message: &str, color: &str, rounded: bool) -> String {
    const HEIGHT: &str = "20";
    const FONT_SIZE: f64 = 11.0;
    const PAD: f64 = 6.0;
    let left = Half::new(label, 0.0, text_width(label, &REGULAR, FONT_SIZE, 0.0), PAD);
    let right = Half::new(
        message,
        left.width,
        text_width(message, &REGULAR, FONT_SIZE, 0.0),
        PAD,
    );
    let width = left.width + right.width;
    let title = format!("{label}: {message}");
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width=(width) height=(HEIGHT)
            role="img" aria-label=(title) {
            title { (title) }
            @if rounded {
                linearGradient id="s" x2="0" y2="100%" {
                    stop offset="0" stop-color="#bbb" stop-opacity=".1" {}
                    stop offset="1" stop-opacity=".1" {}
                }
                clipPath id="r" {
                    rect width=(width) height=(HEIGHT) rx="3" fill="#fff" {}
                }
            }
            g clip-path=[rounded.then_some("url(#r)")] {
                rect width=(left.width) height=(HEIGHT) fill=(LABEL_COLOR) {}
                rect x=(right.x) width=(right.width) height=(HEIGHT) fill=(color) {}
                @if rounded {
                    rect width=(width) height=(HEIGHT) fill="url(#s)" {}
                }
            }
            g fill="#fff" text-anchor="middle" font-family=(FONT_FAMILY) font-size="11"
                text-rendering="geometricPrecision" aria-hidden="true" {
                @for half in [&left, &right] {
                    @if rounded {
                        text x=(half.center) y="15" fill="#010101" fill-opacity=".3"
                            textLength=(half.text_width) { (half.text) }
                    }
                    text x=(half.center) y="14" textLength=(half.text_width) { (half.text) }
                }
            }
        }
    }
    .into_string()
}

/// `for-the-badge`: 28 pixels high, square, 10-pixel spaced capitals, the
/// message in bold.
fn for_the_badge(label: &str, message: &str, color: &str) -> String {
    const HEIGHT: &str = "28";
    const FONT_SIZE: f64 = 10.0;
    const SPACING: f64 = 1.25;
    const PAD: f64 = 9.0;
    // The accessible name keeps the text as written; only the drawing shouts.
    let title = format!("{label}: {message}");
    let (label, message) = (label.to_uppercase(), message.to_uppercase());
    let left = Half::new(
        &label,
        0.0,
        text_width(&label, &REGULAR, FONT_SIZE, SPACING),
        PAD,
    );
    let right = Half::new(
        &message,
        left.width,
        text_width(&message, &BOLD, FONT_SIZE, SPACING),
        PAD,
    );
    let width = left.width + right.width;
    html! {
        svg xmlns="http://www.w3.org/2000/svg" width=(width) height=(HEIGHT)
            role="img" aria-label=(title) {
            title { (title) }
            rect width=(left.width) height=(HEIGHT) fill=(LABEL_COLOR) {}
            rect x=(right.x) width=(right.width) height=(HEIGHT) fill=(color) {}
            g fill="#fff" text-anchor="middle" font-family=(FONT_FAMILY) font-size="10"
                letter-spacing="1.25" text-rendering="geometricPrecision" aria-hidden="true" {
                text x=(left.center) y="17.5" textLength=(left.text_width) { (left.text) }
                text x=(right.center) y="17.5" font-weight="bold"
                    textLength=(right.text_width) { (right.text) }
            }
        }
    }
    .into_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_badge_has_its_label_message_and_colour() {
        let svg = badge("status", "up", "#0e5e68", Style::Flat);
        assert!(svg.starts_with("<svg"), "{svg}");
        assert!(svg.contains(">status<") && svg.contains(">up<"), "{svg}");
        assert!(svg.contains(r##"fill="#0e5e68""##), "{svg}");
        assert!(svg.contains(r#"aria-label="status: up""#), "{svg}");
    }

    #[test]
    fn each_style_has_its_shape() {
        let flat = badge("status", "up", "#0e5e68", Style::Flat);
        let square = badge("status", "up", "#0e5e68", Style::FlatSquare);
        let tall = badge("status", "up", "#0e5e68", Style::ForTheBadge);
        // The highlight and the rounded corners are `flat`'s alone.
        assert!(flat.contains(r#"id="s""#) && flat.contains(r#"rx="3""#));
        assert!(!square.contains(r#"id="s""#) && !square.contains("clip-path"));
        assert!(flat.contains(r#"height="20""#) && square.contains(r#"height="20""#));
        assert!(tall.contains(r#"height="28""#) && tall.contains(">STATUS<"));
        assert!(tall.contains(r#"font-weight="bold""#) && tall.contains(">UP<"));
        assert!(tall.contains(r#"aria-label="status: up""#), "{tall}");
    }

    #[test]
    fn the_text_is_measured_from_the_font() {
        // "up" in Verdana 11: u 1296 + p 1276 units of 2048.
        let width = text_width("up", &REGULAR, 11.0, 0.0);
        assert!((width - 13.81).abs() < 0.01, "{width}");
        // The halves are whole pixels: 12 px of padding around the text.
        let svg = badge("status", "up", "#0e5e68", Style::Flat);
        assert!(svg.contains(r#"<svg xmlns="http://www.w3.org/2000/svg" width="72""#));
        assert!(svg.contains(r#"textLength="13.8""#), "{svg}");
        // A longer message makes a wider badge; an unknown character counts.
        let long = text_width("degraded", &REGULAR, 11.0, 0.0);
        assert!(long > width * 3.0);
        assert!(text_width("é", &REGULAR, 11.0, 0.0) > 0.0);
        // Capitals in bold, spaced, are wider than the same text in regular.
        assert!(text_width("UP", &BOLD, 10.0, 1.25) > text_width("UP", &REGULAR, 10.0, 0.0));
    }

    #[test]
    fn the_text_cannot_break_out_of_the_svg() {
        let svg = badge("a<b", "\"x\" & y", "#0e5e68", Style::Flat);
        assert!(!svg.contains("a<b") && svg.contains("a&lt;b"), "{svg}");
        assert!(svg.contains("&quot;x&quot; &amp; y"), "{svg}");
    }

    #[test]
    fn a_style_is_read_in_kebab_case() {
        let parse = |raw: &str| serde_json::from_str::<Style>(&format!("\"{raw}\""));
        assert_eq!(parse("flat").ok(), Some(Style::Flat));
        assert_eq!(parse("flat-square").ok(), Some(Style::FlatSquare));
        assert_eq!(parse("for-the-badge").ok(), Some(Style::ForTheBadge));
        assert!(parse("plastic").is_err());
    }
}
