//! Core of Hora: configuration, probing, storage, TLS-expiry and scheduling.

mod alert_once;
pub mod announce;
pub mod cert;
pub mod coalesce;
pub mod config;
pub mod config_watch;
mod connect;
pub mod db;
pub mod digest;
pub mod doctor;
mod domain_expiry;
mod exec;
pub mod fmt;
mod heartbeat;
pub mod histogram;
pub mod http;
pub mod import;
pub mod maintenance;
pub mod mesh;
pub mod notifications;
pub mod planned;
pub mod postmortem;
pub mod probe;
mod rdap;
mod release;
pub mod report;
pub mod scheduler;
pub mod silence;
pub mod slo;
pub mod status;
pub mod supervisor;
#[cfg(test)]
mod testing;
pub mod timeline;
pub mod topology;
pub mod tune;
mod tunnel;
pub mod updates;

/// Seconds in a day (UTC), shared across the time-bucketing logic.
pub const SECONDS_PER_DAY: i64 = 86_400;

/// The longest accepted ad-hoc silence (7 days): a "mute while deploying"
/// window, not a way to accidentally turn alerting off forever. Longer muting
/// belongs in a configured `[[maintenance]]` window, which is visible on the
/// status page.
pub const MAX_SILENCE_SECS: u64 = 7 * 24 * 3600;

// Caps on free text written through the HTTP API, the CLI or `hora top`, so a
// record looks the same whichever door it came in through and a buggy hook or
// producer cannot bloat the database. Counted in characters.

/// An event marker's title (`hora event`, `POST /api/event`).
pub const MAX_EVENT_TITLE_CHARS: usize = 200;
/// An announcement's banner title.
pub const MAX_ANNOUNCE_TITLE_CHARS: usize = 200;
/// An announcement's banner body.
pub const MAX_ANNOUNCE_BODY_CHARS: usize = 500;
/// The free-text reason recorded with an ad-hoc silence.
pub const MAX_SILENCE_REASON_CHARS: usize = 500;
/// A pushed alert's title (`POST /api/monitors/{id}/alert`).
pub const MAX_ALERT_TITLE_CHARS: usize = 200;
/// A push heartbeat's `msg`, and a pushed alert's message.
pub const MAX_PUSH_MSG_CHARS: usize = 500;

/// `text` trimmed, cut to at most `max_chars` characters, its control
/// characters (bar newlines and tabs) replaced (see [`fmt::printable`]).
#[must_use]
pub fn bounded(text: &str, max_chars: usize) -> String {
    fmt::printable(text.trim())
        .chars()
        .take(max_chars)
        .collect()
}

/// Parse a human duration like `90s`, `10m`, `2h`, `1d` or a concatenation
/// (`1h30m`) into seconds. Returns `None` for anything unparseable or zero,
/// so a typo'd duration is rejected instead of silently becoming "no time".
#[must_use]
pub fn parse_duration(input: &str) -> Option<u64> {
    let mut total: u64 = 0;
    let mut digits = String::new();
    for c in input.trim().chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let unit: u64 = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        let value: u64 = digits.parse().ok()?;
        digits.clear();
        total = total.checked_add(value.checked_mul(unit)?)?;
    }
    // Trailing digits ("90") have no unit: ambiguous, rejected.
    (digits.is_empty() && total > 0).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::parse_duration;

    #[test]
    fn durations_parse_units_and_concatenations() {
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("10m"), Some(600));
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("1d"), Some(86_400));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration(" 5m "), Some(300));
    }

    #[test]
    fn durations_reject_ambiguity_zero_and_garbage() {
        assert_eq!(parse_duration("90"), None); // no unit
        assert_eq!(parse_duration("0m"), None); // zero mutes nothing
        assert_eq!(parse_duration("m"), None); // unit without a value
        assert_eq!(parse_duration("ten minutes"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("99999999999999999999d"), None); // overflow
    }
}
