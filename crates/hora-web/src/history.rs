//! Incident history: the Askama-rendered `/history` page and the
//! `/history.atom` feed. The Atom XML is written by hand (a feed is a dozen
//! lines; a template engine would not pull its weight there).

use std::collections::HashMap;
use std::fmt::Write as _;

use askama::Template;
use chrono::DateTime;
use hora_core::db::{EventMarker, Incident, PushedAlert};
use hora_core::fmt::{self, xml_escape};

use crate::layout::Chrome;

/// The `/history` page; rows are pre-formatted [`IncidentRow`]s, Askama does
/// the escaping.
#[derive(Template)]
#[template(path = "history.html")]
pub(crate) struct HistoryTemplate {
    /// The page chrome (its links carry the viewer's `?token=`, so a
    /// viewer authenticated by query keeps their view from page to page).
    pub(crate) chrome: Chrome,
    pub(crate) incidents: Vec<IncidentRow>,
    /// Externally-pushed alerts (`POST /api/monitors/{id}/alert`), shown in
    /// their own section below the incidents.
    pub(crate) pushed_alerts: Vec<AlertRow>,
    /// Operator-recorded event markers (`hora event`), shown in their own
    /// section - authenticated viewers only (deploy titles are operator info).
    pub(crate) events: Vec<EventRow>,
}

/// One incident, formatted for display.
pub(crate) struct IncidentRow {
    id: i64,
    monitor: String,
    resolved: bool,
    error: Option<String>,
    cause: Option<String>,
    impacted: Option<String>,
    note: Option<String>,
    snapshot: Option<String>,
    /// Correlated event marker ("deploy api v2.3, 3 min before"), when one was
    /// recorded shortly before the down.
    event: Option<String>,
    /// Multi-vantage verdict recorded once the peers answered.
    vantage: Option<String>,
    /// The monitor's id, URL-encoded, for its page (`/monitor/{id}`).
    monitor_href: String,
    /// The sentence: "Web was down for 41 minutes" / "Web is down".
    headline: String,
    /// "Thursday 18 September 2026, 19:12 to 19:53 UTC".
    span: String,
    /// "18 Sep 2026".
    day: String,
    /// The start and the recovery as `<time datetime>` wants them.
    started_iso: String,
    ended_iso: Option<String>,
    /// This instant, for the "Now" of an incident still open.
    now_iso: String,
    /// "18 Sep, 19:12 UTC" and the recovery, for the facts.
    started_short: String,
    ended_short: Option<String>,
    /// "41 minutes".
    length: Option<String>,
    /// The verdict in the visitor's words, and its state.
    verdict: Option<(&'static str, String)>,
}

/// A timestamp as `<time datetime>` wants it: `2026-09-18T19:12:00Z`.
fn iso(timestamp: i64) -> String {
    DateTime::from_timestamp(timestamp, 0).map_or_else(String::new, |at| {
        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    })
}

/// The multi-vantage verdict (as `hora_core::mesh::confirm` writes it) in a
/// visitor's words, with the state it amounts to. `None` for a shape this
/// does not know (the raw verdict stays in the operator's details).
pub(crate) fn humanize_verdict(verdict: &str) -> Option<(&'static str, String)> {
    let fraction = |text: &str| -> Option<(usize, usize)> {
        let (_, rest) = text.split_once(" from ")?;
        let (counts, _) = rest.split_once(" vantage")?;
        let (part, total) = counts.split_once('/')?;
        Some((part.trim().parse().ok()?, total.trim().parse().ok()?))
    };
    if verdict.starts_with("confirmed down") {
        let (down, total) = fraction(verdict)?;
        Some((
            "down",
            if down == total {
                format!("Seen down from all {total} places: a real outage, not a network problem.")
            } else {
                format!("Seen down from {down} of {total} places.")
            },
        ))
    } else if verdict.starts_with("seen UP by") {
        let (down, total) = fraction(verdict)?;
        Some((
            "degraded",
            format!(
                "Seen down from {down} of {total} places only; the others could reach it. \
                 Likely a network problem near this checkpoint."
            ),
        ))
    } else if verdict.starts_with("no peer vantage") {
        Some((
            "none",
            "Not confirmed: no other place could be asked.".to_owned(),
        ))
    } else if verdict.starts_with("no peer watches") {
        Some((
            "none",
            "Not confirmed: no other place watches this service.".to_owned(),
        ))
    } else {
        None
    }
}

/// One operator-recorded event marker, formatted for display.
pub(crate) struct EventRow {
    title: String,
    at: String,
    iso: String,
}

/// Build the event rows: newest first, timestamps formatted.
pub(crate) fn event_rows(events: &[EventMarker]) -> Vec<EventRow> {
    events
        .iter()
        .map(|event| EventRow {
            title: event.title.clone(),
            at: fmt::utc(event.created_at),
            iso: iso(event.created_at),
        })
        .collect()
}

/// The auto-generated post-mortem page (`/incident/{id}`): the incident card
/// plus the raw markdown ready to paste into a ticket.
#[derive(Template)]
#[template(path = "incident.html")]
pub(crate) struct IncidentTemplate {
    pub(crate) chrome: Chrome,
    pub(crate) row: IncidentRow,
    /// The markdown post-mortem, shown in a copyable block.
    pub(crate) markdown: String,
    /// The owl: awake while it is down, asleep once it is over.
    pub(crate) owl: String,
    /// The raw verdict, for the operator (the visitor reads it humanized).
    pub(crate) operator: bool,
}

/// The `/timeline` page: the unified chronology, newest first.
#[derive(Template)]
#[template(path = "timeline.html")]
pub(crate) struct TimelineTemplate {
    pub(crate) chrome: Chrome,
    pub(crate) entries: Vec<TimelineRow>,
}

/// One merged timeline entry, formatted for display.
pub(crate) struct TimelineRow {
    at: String,
    iso: String,
    /// The entry kind's shape (`down`, `up`, `info`, ...) and word.
    state: &'static str,
    word: &'static str,
    title: String,
    detail: Option<String>,
    /// The incident behind a down/recovered entry, for the post-mortem link.
    incident: Option<i64>,
}

/// Build the timeline rows from the merged core entries.
pub(crate) fn timeline_rows(entries: &[hora_core::timeline::Entry]) -> Vec<TimelineRow> {
    entries
        .iter()
        .map(|entry| {
            let (state, word) = match entry.kind.as_str() {
                "down" => ("down", "Down"),
                "recovered" => ("up", "Up again"),
                "alert" => ("degraded", "Alert"),
                "silence" => ("maint", "Silenced"),
                "maintenance" => ("maint", "Maintenance"),
                "announce" => ("info", "Announcement"),
                _ => ("info", "Change"),
            };
            TimelineRow {
                at: DateTime::from_timestamp(entry.at, 0).map_or_else(
                    || fmt::utc(entry.at),
                    |at| at.format("%a %-d %b, %H:%M").to_string(),
                ),
                iso: iso(entry.at),
                state,
                word,
                title: entry.title.clone(),
                detail: entry.detail.clone(),
                incident: entry.incident_id,
            }
        })
        .collect()
}

/// One externally-pushed alert, formatted for display.
pub(crate) struct AlertRow {
    monitor: String,
    /// `info` | `warning` | `error` | `critical`.
    severity: String,
    /// The severity's shape: `info`, `degraded` or `down`.
    state: &'static str,
    title: String,
    message: Option<String>,
    at: String,
    iso: String,
}

/// Build the pushed-alert rows: ids resolved to display names, timestamp
/// formatted, an emptied message (sanitized for anonymous viewers) dropped.
pub(crate) fn alert_rows(
    alerts: &[PushedAlert],
    monitor_names: &HashMap<String, String>,
) -> Vec<AlertRow> {
    alerts
        .iter()
        .map(|alert| AlertRow {
            monitor: monitor_names
                .get(&alert.monitor_id)
                .unwrap_or(&alert.monitor_id)
                .clone(),
            severity: alert.severity.clone(),
            state: match alert.severity.as_str() {
                "error" | "critical" => "down",
                "warning" => "degraded",
                _ => "info",
            },
            title: alert.title.clone(),
            message: (!alert.message.is_empty()).then(|| alert.message.clone()),
            at: fmt::utc(alert.created_at),
            iso: iso(alert.created_at),
        })
        .collect()
}

/// Build the view rows: ids resolved to display names (falling back to the id
/// for monitors no longer in the config), timestamps and durations formatted.
pub(crate) fn incident_rows(
    incidents: &[Incident],
    monitor_names: &HashMap<String, String>,
) -> Vec<IncidentRow> {
    let at = |timestamp: i64, pattern: &str| {
        DateTime::from_timestamp(timestamp, 0).map_or_else(
            || timestamp.to_string(),
            |at| at.format(pattern).to_string(),
        )
    };
    incidents
        .iter()
        .map(|incident| {
            let monitor = monitor_names
                .get(&incident.monitor_id)
                .unwrap_or(&incident.monitor_id)
                .clone();
            let length = incident.duration_s.map(fmt::duration);
            let headline = match (&incident.ended_at, &length) {
                (Some(_), Some(length)) => format!("{monitor} was down for {length}"),
                (Some(_), None) => format!("{monitor} was down"),
                (None, _) => format!("{monitor} is down"),
            };
            let start_day = at(incident.started_at, "%Y-%m-%d");
            let span = match incident.ended_at {
                Some(ended) if at(ended, "%Y-%m-%d") == start_day => format!(
                    "{} to {}",
                    at(incident.started_at, "%A %-d %B %Y, %H:%M"),
                    at(ended, "%H:%M UTC")
                ),
                Some(ended) => format!(
                    "{} to {}",
                    at(incident.started_at, "%A %-d %B %Y, %H:%M UTC"),
                    at(ended, "%A %-d %B, %H:%M UTC")
                ),
                None => format!(
                    "Since {}",
                    at(incident.started_at, "%A %-d %B %Y, %H:%M UTC")
                ),
            };
            (monitor, length, headline, span)
        })
        .zip(incidents)
        .map(
            |((monitor, length, headline, span), incident)| IncidentRow {
                id: incident.id,
                monitor,
                monitor_href: hora_core::fmt::percent_encode(&incident.monitor_id),
                headline,
                span,
                day: at(incident.started_at, "%-d %b %Y"),
                started_iso: iso(incident.started_at),
                ended_iso: incident.ended_at.map(iso),
                now_iso: iso(chrono::Utc::now().timestamp()),
                started_short: at(incident.started_at, "%-d %b, %H:%M UTC"),
                ended_short: incident
                    .ended_at
                    .map(|ended| at(ended, "%-d %b, %H:%M UTC")),
                length,
                verdict: incident.vantage.as_deref().and_then(humanize_verdict),
                resolved: incident.ended_at.is_some(),
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
                event: incident.event.clone(),
                vantage: incident.vantage.clone(),
            },
        )
        .collect()
}

pub(crate) fn render_atom(
    incidents: &[Incident],
    monitor_names: &HashMap<String, String>,
    base_url: &str,
    page_title: &str,
) -> String {
    // `base_url` is built from the client-supplied Host header, so escape it
    // before it lands in XML attributes/elements - an unescaped `"` or `<` would
    // otherwise break out of an `href` or inject feed elements.
    let base_url = xml_escape(base_url);
    let mut out = String::with_capacity(4096);
    let _ = writeln!(out, "<?xml version=\"1.0\" encoding=\"utf-8\"?>");
    let _ = writeln!(out, "<feed xmlns=\"http://www.w3.org/2005/Atom\">");
    let _ = writeln!(
        out,
        "  <title>{} - incident history</title>",
        xml_escape(page_title)
    );
    let _ = writeln!(
        out,
        "  <link href=\"{base_url}/history.atom\" rel=\"self\"/>"
    );
    let _ = writeln!(
        out,
        "  <link href=\"{base_url}/history\" rel=\"alternate\"/>"
    );
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
        // The entry's permalink is its post-mortem page; it doubles as the id.
        let link = format!("{base_url}/incident/{}", incident.id);

        let _ = writeln!(out, "  <entry>");
        let _ = writeln!(out, "    <title>{}</title>", xml_escape(&title));
        let _ = writeln!(out, "    <id>{link}</id>");
        let _ = writeln!(out, "    <link href=\"{link}\" rel=\"alternate\"/>");

        if let Some(dt) = DateTime::from_timestamp(incident.started_at, 0) {
            let _ = writeln!(out, "    <published>{}</published>", dt.to_rfc3339());
        }
        // `updated` moves when the incident resolves, so feed readers refresh
        // the entry instead of keeping the stale "Ongoing" version.
        let entry_updated = incident.ended_at.unwrap_or(incident.started_at);
        if let Some(dt) = DateTime::from_timestamp(entry_updated, 0) {
            let _ = writeln!(out, "    <updated>{}</updated>", dt.to_rfc3339());
        }

        let _ = writeln!(
            out,
            "    <content type=\"html\">{}</content>",
            // `type="html"` carries *escaped* markup (RFC 4287 4.1.3.3): the
            // fragment's own text is escaped once as HTML, then the whole
            // fragment once more as XML. A reader decoding the element text
            // gets the fragment back with the reason still inert - which
            // matters, since a failure reason can quote a hostile target.
            xml_escape(&entry_html(incident))
        );
        let _ = writeln!(out, "  </entry>");
    }

    let _ = writeln!(out, "</feed>");
    out
}

/// The HTML body of one feed entry, its text escaped.
fn entry_html(incident: &Incident) -> String {
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
            fmt::duration(duration_s)
        );
    }
    if let Some(note) = &incident.note {
        let _ = write!(
            content,
            "<p><strong>Note:</strong> {}</p>",
            xml_escape(note)
        );
    }
    content
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_escapes_host_derived_base_url() {
        // base_url comes from the client's Host header; a crafted one must not
        // break out of the href attribute or inject feed elements.
        let xml = render_atom(&[], &HashMap::new(), "http://x\"><inject", "Status");
        assert!(!xml.contains("\"><inject"), "{xml}");
        assert!(xml.contains("&quot;&gt;&lt;inject"), "{xml}");
    }

    #[test]
    fn atom_content_is_escaped_html_with_permalinks() {
        let incident = Incident {
            id: 7,
            monitor_id: "web".to_owned(),
            started_at: 1000,
            ended_at: None,
            duration_s: None,
            cause: None,
            impacted: None,
            error: Some("HTTP 500: <script>alert(1)</script>".to_owned()),
            reason: None,
            note: None,
            snapshot: None,
            event: None,
            vantage: None,
            created_at: 1000,
        };
        let names = HashMap::from([("web".to_owned(), "Web".to_owned())]);
        let xml = render_atom(
            &[incident],
            &names,
            "https://status.example",
            "Acme <Status>",
        );
        // The feed title follows the page title (escaped).
        assert!(
            xml.contains("<title>Acme &lt;Status&gt; - incident history</title>"),
            "{xml}"
        );
        // No raw child elements inside `type="html"` content: the markup is
        // escaped once, the reason's own markup twice.
        assert!(xml.contains("&lt;p&gt;&lt;strong&gt;Error:"), "{xml}");
        assert!(xml.contains("&amp;lt;script&amp;gt;"), "{xml}");
        assert!(!xml.contains("<p>") && !xml.contains("<script>"), "{xml}");
        // Ids and links point at the real post-mortem route.
        assert!(
            xml.contains("<id>https://status.example/incident/7</id>"),
            "{xml}"
        );
        assert!(
            xml.contains("<link href=\"https://status.example/incident/7\" rel=\"alternate\"/>"),
            "{xml}"
        );
        assert!(!xml.contains("/incidents/"), "{xml}");
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
            reason: None,
            note: Some("fiber cut".to_owned()),
            snapshot: Some("HTTP/2 503\n\nmaintenance".to_owned()),
            event: Some("deploy api v2.3, 3 min before".to_owned()),
            vantage: Some("confirmed down from 2/2 vantage points".to_owned()),
            created_at: 1000,
        };
        let names = HashMap::from([("db".to_owned(), "Database".to_owned())]);

        let rows = incident_rows(&[incident], &names);
        assert_eq!(rows[0].monitor, "Database");
        assert!(rows[0].resolved);
        assert_eq!(rows[0].length.as_deref(), Some("1 min 30 s"));
        assert_eq!(rows[0].headline, "Database was down for 1 min 30 s");
        assert_eq!(rows[0].impacted.as_deref(), Some("API, Web"));
        assert_eq!(rows[0].note.as_deref(), Some("fiber cut"));
        assert_eq!(
            rows[0].event.as_deref(),
            Some("deploy api v2.3, 3 min before")
        );
        assert_eq!(
            rows[0].vantage.as_deref(),
            Some("confirmed down from 2/2 vantage points")
        );

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
            reason: None,
            note: None,
            snapshot: None,
            event: None,
            vantage: None,
            created_at: 1000,
        };
        let rows = incident_rows(&[orphan], &HashMap::new());
        assert_eq!(rows[0].monitor, "gone");
        assert!(!rows[0].resolved);
    }

    #[test]
    fn verdicts_read_as_calm_sentences() {
        let (state, text) = humanize_verdict("confirmed down from 3/3 vantage points").unwrap();
        assert_eq!(state, "down");
        assert!(text.contains("all 3 places"), "{text}");
        let (state, text) = humanize_verdict(
            "seen UP by Frankfurt, Montréal - down from 1/3 vantage points (network issue near this node?)",
        )
        .unwrap();
        assert_eq!(state, "degraded");
        assert!(
            text.contains("1 of 3 places only") && !text.contains("vantage"),
            "{text}"
        );
        assert_eq!(
            humanize_verdict("no peer vantage reachable, unconfirmed").map(|v| v.0),
            Some("none")
        );
        let (_, text) = humanize_verdict("no peer watches this target, unconfirmed").unwrap();
        assert!(text.contains("no other place watches"), "{text}");
        assert!(humanize_verdict("something new").is_none());
    }

    #[test]
    fn alert_rows_resolve_names_and_drop_empty_message() {
        let names = HashMap::from([("ekb".to_owned(), "EKB API".to_owned())]);
        let with_message = PushedAlert {
            id: 1,
            monitor_id: "ekb".to_owned(),
            severity: "error".to_owned(),
            title: "Patch materialization failed".to_owned(),
            message: "3/12 operations failed".to_owned(),
            dedup_key: Some("ekb:mat".to_owned()),
            created_at: 1000,
        };
        // An emptied message (sanitized for anonymous viewers) becomes None.
        let sanitized = PushedAlert {
            id: 2,
            monitor_id: "ekb".to_owned(),
            severity: "info".to_owned(),
            title: "deploy started".to_owned(),
            message: String::new(),
            dedup_key: None,
            created_at: 1001,
        };
        let rows = alert_rows(&[with_message, sanitized], &names);
        assert_eq!(rows[0].monitor, "EKB API");
        assert_eq!(rows[0].severity, "error");
        assert_eq!(rows[0].message.as_deref(), Some("3/12 operations failed"));
        assert!(rows[1].message.is_none());
    }
}
