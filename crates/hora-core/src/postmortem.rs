//! Auto-generated post-mortems: assemble everything an incident already
//! knows - when it started and ended, the first failure's reason, what the
//! service actually answered, the multi-vantage verdict, the topology
//! context, the correlated change and the operator's note - into markdown
//! ready to paste into a ticket. The chore nobody writes, written by the
//! tool that saw everything.

use chrono::DateTime;

use crate::db::Incident;

/// Render one incident as a markdown post-mortem. `monitor_name` is the
/// display name resolved by the caller (falling back to the stored id for
/// monitors no longer in the config). Pure: no I/O, fully testable.
#[must_use]
pub fn render(incident: &Incident, monitor_name: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::with_capacity(1024);
    let day = format_date(incident.started_at);
    let _ = writeln!(out, "# Post-mortem: {monitor_name} - {day}");
    let _ = writeln!(out);

    let status = if incident.ended_at.is_some() {
        "resolved"
    } else {
        "ongoing"
    };
    let _ = writeln!(out, "- **Incident:** #{} ({status})", incident.id);
    let _ = writeln!(out, "- **Monitor:** {monitor_name}");
    let _ = writeln!(out, "- **Started:** {}", format_utc(incident.started_at));
    if let Some(ended) = incident.ended_at {
        let _ = writeln!(out, "- **Ended:** {}", format_utc(ended));
    }
    if let Some(duration) = incident.duration_s {
        let _ = writeln!(out, "- **Duration:** {}", format_duration(duration));
    }
    if let Some(error) = &incident.error {
        let _ = writeln!(out, "- **First failure:** {error}");
    }
    if let Some(vantage) = &incident.vantage {
        let _ = writeln!(out, "- **Multi-vantage:** {vantage}");
    }
    if let Some(cause) = &incident.cause {
        let _ = writeln!(out, "- **Probable cause (topology):** {cause}");
    }
    if let Some(impacted) = impacted_names(incident.impacted.as_deref()) {
        let _ = writeln!(out, "- **Impacted:** {impacted}");
    }
    if let Some(event) = &incident.event {
        let _ = writeln!(out, "- **Recent change:** {event}");
    }

    if let Some(note) = &incident.note {
        let _ = writeln!(out);
        let _ = writeln!(out, "## Operator note");
        let _ = writeln!(out);
        let _ = writeln!(out, "{note}");
    }

    if let Some(snapshot) = &incident.snapshot {
        let _ = writeln!(out);
        let _ = writeln!(out, "## What the service answered");
        let _ = writeln!(out);
        // A fence longer than any backtick run inside keeps the block intact
        // even if the captured body itself contains ``` sequences.
        let fence = "`".repeat(longest_backtick_run(snapshot).max(3) + 1);
        let _ = writeln!(out, "{fence}");
        let _ = writeln!(out, "{snapshot}");
        let _ = writeln!(out, "{fence}");
    }

    let _ = writeln!(out);
    let _ = writeln!(out, "## Timeline (UTC)");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "- {} - down confirmed{}",
        format_utc(incident.started_at),
        incident
            .error
            .as_deref()
            .map(|error| format!(" ({error})"))
            .unwrap_or_default()
    );
    if let Some(ended) = incident.ended_at {
        let _ = writeln!(out, "- {} - recovered", format_utc(ended));
    } else {
        let _ = writeln!(out, "- ongoing at generation time");
    }
    out
}

/// The stored `impacted` JSON list joined for display; `None` when absent,
/// unparseable or empty.
fn impacted_names(impacted: Option<&str>) -> Option<String> {
    let names: Vec<String> = serde_json::from_str(impacted?).ok()?;
    (!names.is_empty()).then(|| names.join(", "))
}

/// The longest run of consecutive backticks in `text` (to size the fence).
fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in text.chars() {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

fn format_utc(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn format_date(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |dt| dt.format("%Y-%m-%d").to_string(),
    )
}

fn format_duration(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn incident() -> Incident {
        Incident {
            id: 7,
            monitor_id: "api".to_owned(),
            started_at: 1_700_000_000,
            ended_at: Some(1_700_000_540),
            duration_s: Some(540),
            cause: Some("Database".to_owned()),
            impacted: Some(r#"["Web","Worker"]"#.to_owned()),
            error: Some("HTTP 503: upstream connect error".to_owned()),
            note: Some("fiber cut at the DC".to_owned()),
            snapshot: Some("HTTP/2 503\n\n<html>maintenance</html>".to_owned()),
            event: Some("deploy api v2.3, 3m before".to_owned()),
            vantage: Some("confirmed down from 2/2 vantage points".to_owned()),
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn renders_every_section_the_incident_knows() {
        let md = render(&incident(), "API");
        assert!(md.starts_with("# Post-mortem: API - 2023-11-14"), "{md}");
        assert!(md.contains("- **Incident:** #7 (resolved)"));
        assert!(md.contains("- **Duration:** 9m 0s"));
        assert!(md.contains("- **First failure:** HTTP 503: upstream connect error"));
        assert!(md.contains("- **Multi-vantage:** confirmed down from 2/2 vantage points"));
        assert!(md.contains("- **Probable cause (topology):** Database"));
        assert!(md.contains("- **Impacted:** Web, Worker"));
        assert!(md.contains("- **Recent change:** deploy api v2.3, 3m before"));
        assert!(md.contains("## Operator note\n\nfiber cut at the DC"));
        assert!(md.contains("## What the service answered"));
        assert!(md.contains("<html>maintenance</html>"));
        assert!(md.contains("## Timeline (UTC)"));
        assert!(md.contains("- 2023-11-14 22:22:20 UTC - recovered"));
    }

    #[test]
    fn omits_what_the_incident_does_not_know() {
        let bare = Incident {
            ended_at: None,
            duration_s: None,
            cause: None,
            impacted: Some("[]".to_owned()),
            error: None,
            note: None,
            snapshot: None,
            event: None,
            vantage: None,
            ..incident()
        };
        let md = render(&bare, "API");
        assert!(md.contains("(ongoing)"));
        assert!(md.contains("- ongoing at generation time"));
        for absent in [
            "**Ended:**",
            "**Duration:**",
            "**First failure:**",
            "**Multi-vantage:**",
            "**Probable cause",
            "**Impacted:**",
            "**Recent change:**",
            "## Operator note",
            "## What the service answered",
        ] {
            assert!(!md.contains(absent), "{absent} should be omitted:\n{md}");
        }
    }

    #[test]
    fn snapshot_fence_survives_embedded_backticks() {
        let hostile = Incident {
            snapshot: Some("body with ```` four backticks".to_owned()),
            ..incident()
        };
        let md = render(&hostile, "API");
        // The fence must be strictly longer than any run inside the snapshot.
        assert!(md.contains("`````\n"), "{md}");
    }
}
