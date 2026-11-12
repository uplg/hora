//! Shared human-facing formatting: durations, timestamps, percentages and the
//! two escapings the pages need. One implementation each, so the history page,
//! the post-mortem, the monthly report and the CLI word the same incident the
//! same way.
//!
//! Durations are written one way everywhere a person reads them, the SI way:
//! the unit symbols `d`, `h`, `min`, `s`, a space between each number and its
//! unit, no padding, the two largest units and no zero second component
//! (`"42 s"`, `"3 min 10 s"`, `"2 h"`, `"1 d 2 h"`). [`elapsed`] is the
//! deliberately coarser variant for estimates and "failing for ..." lines,
//! where the smaller unit would be noise. Percentages take the same space
//! before their sign (`"99.97 %"`).
//!
//! What a program reads does not come through here: JSON fields, metrics and
//! the webhook payload carry numbers, and the CLI's *input* syntax
//! (`--for 2h30m`) is [`crate::parse_duration`]'s.

use std::fmt::Write as _;

use chrono::DateTime;

/// The units a duration is written in, largest first, with their SI symbols.
const UNITS: [(u64, &str); 4] = [(86_400, "d"), (3600, "h"), (60, "min"), (1, "s")];

/// `"42 s"`, `"3 min 10 s"`, `"2 h 5 min"`, `"1 d 2 h"`: an exact span
/// (incident duration, downtime, MTTR, silence length), as its two largest
/// units. A zero second unit is left out (`"2 h"`, not `"2 h 0 min"`).
/// Negative spans clamp to zero (`"0 s"`).
#[must_use]
pub fn duration(seconds: i64) -> String {
    let seconds = seconds.max(0).unsigned_abs();
    let Some(lead) = UNITS.iter().position(|(size, _)| seconds >= *size) else {
        return "0 s".to_owned();
    };
    let (size, symbol) = UNITS[lead];
    let mut out = format!("{} {symbol}", seconds / size);
    if let Some((next_size, next_symbol)) = UNITS.get(lead + 1) {
        let rest = (seconds % size) / next_size;
        if rest > 0 {
            let _ = write!(out, " {rest} {next_symbol}");
        }
    }
    out
}

/// [`duration`] for a configured period, which the config holds unsigned
/// (an interval, a timeout, a grace): `30 s`, `5 min`, `1 min 30 s`, `1 d`.
#[must_use]
pub fn period(seconds: u64) -> String {
    duration(i64::try_from(seconds).unwrap_or(i64::MAX))
}

/// `"2 d 3 h"`, `"6 h"`, `"45 min"`, `"30 s"`: a coarse span, for estimates
/// and "failing for ..." phrasing where the exact figure is noise. One unit
/// under two days, days and hours from there (`"2 d"` on the hour).
///
/// It is the notifications' own wording (hora-notify sits below this
/// crate), under the name the rest of the code reads.
pub use hora_notify::human_duration as elapsed;

/// `"2026-05-14 08:03:12 UTC"`. An out-of-range timestamp prints as the raw
/// number rather than vanishing.
#[must_use]
pub fn utc(timestamp: i64) -> String {
    format_timestamp(timestamp, "%Y-%m-%d %H:%M:%S UTC")
}

/// `"2026-05-14"` (UTC).
#[must_use]
pub fn date(timestamp: i64) -> String {
    format_timestamp(timestamp, "%Y-%m-%d")
}

/// `"May 14"` (UTC): the short form for a period that never spans years.
#[must_use]
pub fn short_date(timestamp: i64) -> String {
    format_timestamp(timestamp, "%b %d")
}

fn format_timestamp(timestamp: i64, pattern: &str) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |at| at.format(pattern).to_string(),
    )
}

/// The share `part / total` in basis points (9 997 = 99.97 %), rounded to the
/// nearest one, in integer math so the result is exact. `None` without data.
#[must_use]
pub fn basis_points(part: i64, total: i64) -> Option<i64> {
    (total > 0).then(|| (part * 10_000 + total / 2) / total)
}

/// `"99.97 %"` from basis points: the space before the sign, as SI writes it.
#[must_use]
pub fn pct_bp(basis_points: i64) -> String {
    let sign = if basis_points < 0 { "-" } else { "" };
    let magnitude = basis_points.unsigned_abs();
    format!("{sign}{}.{:02} %", magnitude / 100, magnitude % 100)
}

/// `"99.9 %"` from permille (clamped to 0..=1000), and `"100 %"` for all
/// of it: the one-decimal uptime of the status page, the badges and
/// `hora top`.
#[must_use]
pub fn permille(permille: i64) -> String {
    let permille = permille.clamp(0, 1000);
    if permille == 1000 {
        "100 %".to_owned()
    } else {
        format!("{}.{} %", permille / 10, permille % 10)
    }
}

/// The share `part / total` in permille, rounded down: an uptime never
/// reads better than it was. `None` without data.
#[must_use]
pub fn permille_of(part: i64, total: i64) -> Option<i64> {
    (total > 0).then(|| part.saturating_mul(1000) / total)
}

/// A latency in the unit a person reads: `64 ms`, `1.8 s`.
#[must_use]
pub fn latency(ms: i64) -> String {
    if ms >= 1000 {
        // Saturating: not every latency was measured here (a push monitor's
        // `ping`, a peer's `p50_ms`), so it may be any `i64`.
        let tenths = ms.saturating_add(50) / 100;
        format!("{}.{} s", tenths / 10, tenths % 10)
    } else {
        format!("{ms} ms")
    }
}

/// `"99.97 %"` for `part / total`, or `"no checks"` when there is no data.
#[must_use]
pub fn pct(part: i64, total: i64) -> String {
    basis_points(part, total).map_or_else(|| "no checks".to_owned(), pct_bp)
}

/// `"62.1 GiB"`: a size in binary units, one decimal from KiB up, for
/// `hora compact` and `hora top`.
#[must_use]
pub fn bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut scaled = bytes;
    // Tenths of the unit, computed in integers (no float cast).
    let mut tenths = 0;
    while scaled >= 1024 && unit + 1 < UNITS.len() {
        tenths = (scaled % 1024) * 10 / 1024;
        scaled /= 1024;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{scaled}.{tenths} {}", UNITS[unit])
    }
}

/// Escape the five XML metacharacters, for text embedded in SVG, Atom or HTML.
#[must_use]
pub fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// `text` with every control character other than a newline or a tab
/// replaced by U+FFFD, so remote-controlled text (a probed server's body, a
/// push `msg`, a plugin's output) cannot carry terminal escape sequences
/// (`ESC ]0;...`, `ESC [2J`, OSC 52 clipboard writes) into a terminal that
/// prints it - `hora incidents`, `hora postmortem`, `hora probe`.
#[must_use]
pub fn printable(text: &str) -> std::borrow::Cow<'_, str> {
    if text.chars().any(is_unprintable) {
        std::borrow::Cow::Owned(
            text.chars()
                .map(|c| if is_unprintable(c) { '\u{FFFD}' } else { c })
                .collect(),
        )
    } else {
        std::borrow::Cow::Borrowed(text)
    }
}

fn is_unprintable(c: char) -> bool {
    c.is_control() && c != '\n' && c != '\t'
}

/// Percent-encode `value` for a URL component (RFC 3986): every byte outside
/// the unreserved set becomes `%XX`, uppercase hex.
#[must_use]
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(char::from(byte));
            }
            _ => {
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_neutralises_terminal_escapes() {
        assert_eq!(printable("HTTP 500: ok"), "HTTP 500: ok");
        assert_eq!(
            printable("\u{1b}]0;title\u{7}\u{1b}[2Jx\nline\ttab"),
            "\u{FFFD}]0;title\u{FFFD}\u{FFFD}[2Jx\nline\ttab"
        );
        // C1 controls (a single-byte CSI) too.
        assert_eq!(printable("a\u{9b}31mb"), "a\u{FFFD}31mb");
    }

    /// A latency is not always measured here: a peer's `p50_ms` and a push
    /// monitor's `ping` are whatever `i64` was sent. Found by the `numbers`
    /// and `wire` fuzz targets: `i64::MAX` overflowed the rounding.
    #[test]
    fn a_latency_at_the_edge_of_i64_is_written() {
        assert_eq!(latency(999), "999 ms");
        assert_eq!(latency(1849), "1.8 s");
        assert_eq!(latency(1850), "1.9 s");
        assert_eq!(latency(i64::MAX), "9223372036854775.8 s");
        assert_eq!(latency(i64::MIN), "-9223372036854775808 ms");
    }

    #[test]
    fn sizes_read_in_binary_units() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1536), "1.5 KiB");
        assert_eq!(bytes(66_643 * 1024 * 1024), "65.0 GiB");
        assert_eq!(bytes(u64::MAX), "16777215.9 TiB");
    }

    #[test]
    fn durations_keep_the_two_largest_units() {
        assert_eq!(duration(-5), "0 s");
        assert_eq!(duration(0), "0 s");
        assert_eq!(duration(42), "42 s");
        assert_eq!(duration(60), "1 min");
        assert_eq!(duration(180), "3 min");
        assert_eq!(duration(190), "3 min 10 s");
        assert_eq!(duration(3600 + 420), "1 h 7 min");
        assert_eq!(duration(7200), "2 h");
        // The seconds under an hour's minutes are below the two units kept.
        assert_eq!(duration(7200 + 59), "2 h");
        assert_eq!(duration(7500), "2 h 5 min");
        assert_eq!(duration(3 * 3600 + 7 * 60), "3 h 7 min");
        assert_eq!(duration(86_400), "1 d");
        assert_eq!(duration(93_600), "1 d 2 h");
        assert_eq!(duration(50 * 3600), "2 d 2 h");
        assert_eq!(duration(i64::MAX), "106751991167300 d 15 h");
    }

    /// The coarse spans, as both this crate and hora-notify must write them.
    const COARSE: [(u64, &str); 12] = [
        (0, "0 s"),
        (30, "30 s"),
        (59, "59 s"),
        (60, "1 min"),
        (90, "1 min"),
        (45 * 60 + 59, "45 min"),
        (3600, "1 h"),
        (6 * 3600 + 1800, "6 h"),
        (86_400 + 3600, "25 h"),
        (2 * 86_400, "2 d"),
        (2 * 86_400 + 3 * 3600, "2 d 3 h"),
        (200_000, "2 d 7 h"),
    ];

    #[test]
    fn elapsed_is_coarse() {
        for (seconds, written) in COARSE {
            assert_eq!(elapsed(seconds), written, "{seconds} s");
        }
    }

    #[test]
    fn a_share_in_permille_rounds_down_and_needs_data() {
        assert_eq!(permille_of(999, 1000), Some(999));
        assert_eq!(permille_of(1000, 1000), Some(1000));
        // 2879 of 2880 checks: 999.65 permille, never rounded up to "all".
        assert_eq!(permille_of(2879, 2880), Some(999));
        assert_eq!(permille_of(1, 3), Some(333));
        assert_eq!(permille_of(0, 7), Some(0));
        assert_eq!(permille_of(0, 0), None);
        assert_eq!(permille_of(5, -1), None);
        assert_eq!(basis_points(1, 3), Some(3333));
        assert_eq!(basis_points(2, 3), Some(6667));
    }

    #[test]
    fn a_latency_reads_in_seconds_from_one_second_up() {
        assert_eq!(latency(0), "0 ms");
        assert_eq!(latency(64), "64 ms");
        assert_eq!(latency(999), "999 ms");
        assert_eq!(latency(1000), "1.0 s");
        assert_eq!(latency(1849), "1.8 s");
        assert_eq!(latency(1850), "1.9 s");
        assert_eq!(latency(12_340), "12.3 s");
        assert_eq!(period(90), "1 min 30 s");
    }

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(utc(1_700_000_540), "2023-11-14 22:22:20 UTC");
        assert_eq!(date(1_700_000_540), "2023-11-14");
        assert_eq!(short_date(1_700_000_540), "Nov 14");
        // Out of chrono's range: the raw number, not an empty string.
        assert_eq!(utc(i64::MAX), i64::MAX.to_string());
    }

    #[test]
    fn percentages_use_integer_math() {
        assert_eq!(basis_points(0, 0), None);
        assert_eq!(basis_points(1, 3), Some(3333));
        assert_eq!(basis_points(2, 3), Some(6667));
        assert_eq!(pct_bp(10_000), "100.00 %");
        assert_eq!(pct_bp(9_997), "99.97 %");
        assert_eq!(pct_bp(5), "0.05 %");
        assert_eq!(pct_bp(-150), "-1.50 %");
        assert_eq!(pct_bp(0), "0.00 %");
        assert_eq!(pct(0, 0), "no checks");
        assert_eq!(pct(9997, 10_000), "99.97 %");
        assert_eq!(permille(1000), "100 %");
        assert_eq!(permille(999), "99.9 %");
        assert_eq!(permille(0), "0.0 %");
        assert_eq!(permille(1200), "100 %");
    }

    #[test]
    fn escaping_and_encoding() {
        assert_eq!(
            xml_escape(r#"<a href="x">Tom & 'Jerry'</a>"#),
            "&lt;a href=&quot;x&quot;&gt;Tom &amp; &apos;Jerry&apos;&lt;/a&gt;"
        );
        assert_eq!(percent_encode("a b/c~d-é"), "a%20b%2Fc~d-%C3%A9");
    }
}
