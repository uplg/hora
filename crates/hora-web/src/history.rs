//! Incident history: the Askama-rendered `/history` page and the
//! `/history.atom` feed. The Atom XML is written by hand (a feed is a dozen
//! lines; a template engine would not pull its weight there).

use std::collections::HashMap;
use std::fmt::Write as _;

use askama::Template;
use chrono::DateTime;
use hora_core::db::Incident;

/// The `/history` page; rows are pre-formatted [`IncidentRow`]s, Askama does
/// the escaping.
#[derive(Template)]
#[template(path = "history.html")]
pub(crate) struct HistoryTemplate {
    /// The status page title, linked back to from the footer.
    pub(crate) title: String,
    pub(crate) incidents: Vec<IncidentRow>,
}

/// One incident, formatted for display.
pub(crate) struct IncidentRow {
    monitor: String,
    resolved: bool,
    started: String,
    ended: Option<String>,
    duration: Option<String>,
    error: Option<String>,
    cause: Option<String>,
    impacted: Option<String>,
    note: Option<String>,
    snapshot: Option<String>,
}

/// Build the view rows: ids resolved to display names (falling back to the id
/// for monitors no longer in the config), timestamps and durations formatted.
pub(crate) fn incident_rows(
    incidents: &[Incident],
    monitor_names: &HashMap<String, String>,
) -> Vec<IncidentRow> {
    incidents
        .iter()
        .map(|incident| IncidentRow {
            monitor: monitor_names
                .get(&incident.monitor_id)
                .unwrap_or(&incident.monitor_id)
                .clone(),
            resolved: incident.ended_at.is_some(),
            started: format_utc(incident.started_at),
            ended: incident.ended_at.map(format_utc),
            duration: incident.duration_s.map(format_duration),
            error: incident.error.clone(),
            cause: incident.cause.clone(),
            impacted: incident
                .impacted
                .as_deref()
                .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
                .filter(|impacted| !impacted.is_empty())
                .map(|impacted| impacted.join(", ")),
            note: incident.note.clone(),
            snapshot: incident.snapshot.clone(),
        })
        .collect()
}

pub(crate) fn render_atom(
    incidents: &[Incident],
    monitor_names: &HashMap<String, String>,
    base_url: &str,
) -> String {
    // `base_url` is built from the client-supplied Host header, so escape it
    // before it lands in XML attributes/elements - an unescaped `"` or `<` would
    // otherwise break out of an `href` or inject feed elements.
    let base_url = xml_escape(base_url);
    let mut out = String::with_capacity(4096);
    let _ = writeln!(out, "<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    let _ = writeln!(out, "<feed xmlns=\"http://www.w3.org/2005/Atom\">");
    let _ = writeln!(out, "  <title>Incident History</title>");
    let _ = writeln!(
        out,
        "  <link href=\"{base_url}/history.atom\" rel=\"self\"/>"
    );
    let _ = writeln!(out, "  <link href=\"{base_url}/history\"/>");
    let _ = writeln!(out, "  <id>{base_url}/history.atom</id>");
    let _ = writeln!(out, "  <author><name>Hora</name></author>");

    // The feed's `updated` is the most recent activity: an incident's end when
    // resolved, its start otherwise. Required by Atom, so fall back to the
    // epoch for an empty feed rather than emitting an invalid document.
    let updated = incidents
        .iter()
        .map(|incident| incident.ended_at.unwrap_or(incident.started_at))
        .max()
        .unwrap_or(0);
    if let Some(dt) = DateTime::from_timestamp(updated, 0) {
        let _ = writeln!(out, "  <updated>{}</updated>", dt.to_rfc3339());
    }

    for incident in incidents {
        let monitor_name = monitor_names
            .get(&incident.monitor_id)
            .map_or(incident.monitor_id.as_str(), String::as_str);

        let status = if incident.ended_at.is_some() {
            "Resolved"
        } else {
            "Ongoing"
        };
        let title = format!("{monitor_name} - {status}");

        let _ = writeln!(out, "  <entry>");
        let _ = writeln!(out, "    <title>{}</title>", xml_escape(&title));
        let _ = writeln!(out, "    <id>{base_url}/incidents/{}</id>", incident.id);

        if let Some(dt) = DateTime::from_timestamp(incident.started_at, 0) {
            let _ = writeln!(out, "    <published>{}</published>", dt.to_rfc3339());
        }
        // `updated` moves when the incident resolves, so feed readers refresh
        // the entry instead of keeping the stale "Ongoing" version.
        let entry_updated = incident.ended_at.unwrap_or(incident.started_at);
        if let Some(dt) = DateTime::from_timestamp(entry_updated, 0) {
            let _ = writeln!(out, "    <updated>{}</updated>", dt.to_rfc3339());
        }

        let mut content = String::new();
        if let Some(error) = &incident.error {
            let _ = write!(
                content,
                "<p><strong>Error:</strong> {}</p>",
                xml_escape(error)
            );
        }
        if let Some(cause) = &incident.cause {
            let _ = write!(
                content,
                "<p><strong>Caused by:</strong> {}</p>",
                xml_escape(cause)
            );
        }
        if let Some(duration_s) = incident.duration_s {
            let _ = write!(
                content,
                "<p><strong>Duration:</strong> {}</p>",
                format_duration(duration_s)
            );
        }
        if let Some(note) = &incident.note {
            let _ = write!(
                content,
                "<p><strong>Note:</strong> {}</p>",
                xml_escape(note)
            );
        }

        let _ = writeln!(out, "    <content type=\"html\">{content}</content>");
        let _ = writeln!(out, "  </entry>");
    }

    let _ = writeln!(out, "</feed>");
    out
}

fn format_utc(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(String::new, |dt| {
        dt.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    })
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

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_format_humanely() {
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(90), "1m 30s");
        assert_eq!(format_duration(3720), "1h 2m");
    }

    #[test]
    fn atom_escapes_host_derived_base_url() {
        // base_url comes from the client's Host header; a crafted one must not
        // break out of the href attribute or inject feed elements.
        let xml = render_atom(&[], &HashMap::new(), "http://x\"><inject");
        assert!(!xml.contains("\"><inject"), "{xml}");
        assert!(xml.contains("&quot;&gt;&lt;inject"), "{xml}");
    }

    #[test]
    fn escapes_xml() {
        assert_eq!(
            xml_escape("<b>&\"x'\"</b>"),
            "&lt;b&gt;&amp;&quot;x&apos;&quot;&lt;/b&gt;"
        );
    }

    #[test]
    fn rows_resolve_names_and_parse_impacted() {
        let incident = Incident {
            id: 1,
            monitor_id: "db".to_owned(),
            started_at: 1000,
            ended_at: Some(1090),
            duration_s: Some(90),
            cause: None,
            impacted: Some(r#"["API","Web"]"#.to_owned()),
            error: Some("boom".to_owned()),
            note: Some("fiber cut".to_owned()),
            snapshot: Some("HTTP/2 503\n\nmaintenance".to_owned()),
            created_at: 1000,
        };
        let names = HashMap::from([("db".to_owned(), "Database".to_owned())]);

        let rows = incident_rows(&[incident], &names);
        assert_eq!(rows[0].monitor, "Database");
        assert!(rows[0].resolved);
        assert_eq!(rows[0].duration.as_deref(), Some("1m 30s"));
        assert_eq!(rows[0].impacted.as_deref(), Some("API, Web"));
        assert_eq!(rows[0].note.as_deref(), Some("fiber cut"));

        // Unknown id (monitor removed from config): fall back to the id.
        let orphan = Incident {
            id: 2,
            monitor_id: "gone".to_owned(),
            started_at: 1000,
            ended_at: None,
            duration_s: None,
            cause: None,
            impacted: None,
            error: None,
            note: None,
            snapshot: None,
            created_at: 1000,
        };
        let rows = incident_rows(&[orphan], &HashMap::new());
        assert_eq!(rows[0].monitor, "gone");
        assert!(!rows[0].resolved);
    }
}
