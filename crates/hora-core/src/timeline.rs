//! The unified timeline: every kind of recorded moment - down/recovered
//! transitions (incidents), operator events, pushed alerts, announcements,
//! silences - merged into one chronology. "What happened this week?" in one
//! command (`hora timeline`) or one page (`/timeline`), built entirely from
//! data Hora already stores.

use std::collections::HashMap;

use sqlx::SqlitePool;

use crate::db::{self, Announcement, EventMarker, Incident, PushedAlert, Silence};

/// What kind of moment an entry records - one stable lowercase tag, used as a
/// CLI label and a CSS class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Down,
    Recovered,
    Event,
    Alert,
    Announce,
    Silence,
}

impl Kind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Recovered => "recovered",
            Self::Event => "event",
            Self::Alert => "alert",
            Self::Announce => "announce",
            Self::Silence => "silence",
        }
    }
}

/// One merged timeline entry.
#[derive(Debug)]
pub struct Entry {
    /// When it happened (unix epoch seconds, UTC).
    pub at: i64,
    pub kind: Kind,
    /// The main line ("API down", "deploy api v2.3").
    pub title: String,
    /// Secondary detail (the failure reason, an alert message, a reason).
    pub detail: Option<String>,
    /// The incident behind a down/recovered entry, for the post-mortem link.
    pub incident_id: Option<i64>,
}

/// Everything the merge draws from. The caller decides what goes in - the
/// CLI loads the full operator view, the web layer feeds the sanitized subset
/// an anonymous viewer may see - so the merge itself stays visibility-blind.
#[derive(Default)]
pub struct Sources {
    pub incidents: Vec<Incident>,
    pub events: Vec<EventMarker>,
    pub alerts: Vec<PushedAlert>,
    pub announcements: Vec<Announcement>,
    pub silences: Vec<Silence>,
}

/// Load the full (operator) sources since `since`. Each list is bounded by
/// `limit` on its own; the merge bounds the final count again.
///
/// # Errors
///
/// Returns an error if any query fails.
pub async fn fetch(pool: &SqlitePool, since: i64, limit: i64) -> sqlx::Result<Sources> {
    let mut incidents = db::recent_incidents(pool, limit).await?;
    incidents.retain(|incident| {
        incident.started_at >= since || incident.ended_at.is_some_and(|ended| ended >= since)
    });
    Ok(Sources {
        incidents,
        events: db::events_since(pool, since).await?,
        alerts: {
            let mut alerts = db::recent_pushed_alerts(pool, limit).await?;
            alerts.retain(|alert| alert.created_at >= since);
            alerts
        },
        announcements: db::announcements_since(pool, since).await?,
        silences: db::silences_since(pool, since).await?,
    })
}

/// Merge the sources into one chronology, newest first, at most `limit`
/// entries. `names` resolves monitor ids to display names (missing ids fall
/// back to the id, like everywhere else). Pure, so the decision table is
/// testable without a database.
#[must_use]
pub fn merge<S: std::hash::BuildHasher>(
    sources: &Sources,
    names: &HashMap<String, String, S>,
    since: i64,
    limit: usize,
) -> Vec<Entry> {
    let name = |id: &str| names.get(id).map_or(id, String::as_str).to_owned();
    let mut entries = Vec::new();

    for incident in &sources.incidents {
        if incident.started_at >= since {
            let mut detail = incident.error.clone().unwrap_or_default();
            if let Some(cause) = &incident.cause {
                push_part(&mut detail, &format!("caused by {cause}"));
            }
            if let Some(event) = &incident.event {
                push_part(&mut detail, &format!("recent change: {event}"));
            }
            entries.push(Entry {
                at: incident.started_at,
                kind: Kind::Down,
                title: format!("{} down", name(&incident.monitor_id)),
                detail: (!detail.is_empty()).then_some(detail),
                incident_id: Some(incident.id),
            });
        }
        if let Some(ended) = incident.ended_at.filter(|&ended| ended >= since) {
            entries.push(Entry {
                at: ended,
                kind: Kind::Recovered,
                title: format!("{} recovered", name(&incident.monitor_id)),
                detail: incident
                    .duration_s
                    .map(|secs| format!("after {}", human_secs(secs))),
                incident_id: Some(incident.id),
            });
        }
    }
    for event in &sources.events {
        entries.push(Entry {
            at: event.created_at,
            kind: Kind::Event,
            title: event.title.clone(),
            detail: None,
            incident_id: None,
        });
    }
    for alert in &sources.alerts {
        entries.push(Entry {
            at: alert.created_at,
            kind: Kind::Alert,
            title: format!(
                "[{}] {}: {}",
                alert.severity.to_ascii_uppercase(),
                name(&alert.monitor_id),
                alert.title
            ),
            detail: (!alert.message.is_empty()).then(|| alert.message.clone()),
            incident_id: None,
        });
    }
    for announcement in &sources.announcements {
        entries.push(Entry {
            at: announcement.created_at,
            kind: Kind::Announce,
            title: format!("announced: {}", announcement.title),
            detail: (!announcement.body.is_empty()).then(|| announcement.body.clone()),
            incident_id: None,
        });
    }
    for silence in &sources.silences {
        let target = if silence.monitor_id == "*" {
            "all monitors".to_owned()
        } else {
            name(&silence.monitor_id)
        };
        entries.push(Entry {
            at: silence.created_at,
            kind: Kind::Silence,
            title: format!(
                "silenced {target} for {}",
                human_secs(silence.until - silence.created_at)
            ),
            detail: silence.reason.clone(),
            incident_id: None,
        });
    }

    // Newest first; same-second moments keep a stable order by kind so a
    // down and its correlated event never swap between renders.
    entries.sort_by(|a, b| {
        b.at.cmp(&a.at)
            .then_with(|| a.kind.as_str().cmp(b.kind.as_str()))
    });
    entries.truncate(limit);
    entries
}

/// Append `part` to `text` with a separator when needed.
fn push_part(text: &mut String, part: &str) {
    if !text.is_empty() {
        text.push_str(" · ");
    }
    text.push_str(part);
}

/// `"42s"`, `"3m 10s"`, `"2h 5m"` - the timeline's duration phrasing.
fn human_secs(seconds: i64) -> String {
    let seconds = seconds.max(0);
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

    fn incident(id: i64, started: i64, ended: Option<i64>) -> Incident {
        Incident {
            id,
            monitor_id: "api".to_owned(),
            started_at: started,
            ended_at: ended,
            duration_s: ended.map(|end| end - started),
            cause: None,
            impacted: None,
            error: Some("connection refused".to_owned()),
            note: None,
            snapshot: None,
            event: Some("deploy api v2.3, 3m before".to_owned()),
            vantage: None,
            created_at: started,
        }
    }

    fn names() -> HashMap<String, String> {
        HashMap::from([("api".to_owned(), "API".to_owned())])
    }

    #[test]
    fn merges_every_source_newest_first() {
        let sources = Sources {
            incidents: vec![incident(7, 100, Some(160))],
            events: vec![EventMarker {
                id: 1,
                title: "deploy api v2.3".to_owned(),
                created_at: 90,
            }],
            alerts: vec![PushedAlert {
                id: 1,
                monitor_id: "api".to_owned(),
                severity: "error".to_owned(),
                title: "export failed".to_owned(),
                message: "0 rows written".to_owned(),
                dedup_key: None,
                created_at: 130,
            }],
            announcements: vec![Announcement {
                id: 1,
                title: "Fiber cut".to_owned(),
                body: "ETA 6pm".to_owned(),
                severity: "warning".to_owned(),
                until: None,
                created_at: 120,
            }],
            silences: vec![Silence {
                id: 1,
                monitor_id: "*".to_owned(),
                until: 710,
                reason: Some("deploying".to_owned()),
                created_at: 110,
            }],
        };

        let entries = merge(&sources, &names(), 0, 100);
        let kinds: Vec<&str> = entries.iter().map(|entry| entry.kind.as_str()).collect();
        assert_eq!(
            kinds,
            ["recovered", "alert", "announce", "silence", "down", "event"]
        );

        // The down entry resolves the display name, carries the reason and
        // the correlated change, and links its incident.
        let down = &entries[4];
        assert_eq!(down.title, "API down");
        assert_eq!(down.incident_id, Some(7));
        let detail = down.detail.as_deref().unwrap();
        assert!(detail.contains("connection refused") && detail.contains("recent change"));

        // The recovery phrases its duration; the silence its span and reason.
        assert_eq!(entries[0].detail.as_deref(), Some("after 1m 0s"));
        assert_eq!(entries[3].title, "silenced all monitors for 10m 0s");
        assert_eq!(entries[3].detail.as_deref(), Some("deploying"));
    }

    #[test]
    fn since_and_limit_bound_the_chronology() {
        // Started before the window but ended inside it: only the recovery shows.
        let sources = Sources {
            incidents: vec![incident(1, 50, Some(150))],
            ..Sources::default()
        };
        let entries = merge(&sources, &names(), 100, 100);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind.as_str(), "recovered");

        // The limit keeps the newest entries.
        let sources = Sources {
            events: (0..10)
                .map(|i| EventMarker {
                    id: i,
                    title: format!("event {i}"),
                    created_at: i,
                })
                .collect(),
            ..Sources::default()
        };
        let entries = merge(&sources, &names(), 0, 3);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].title, "event 9");
    }
}
