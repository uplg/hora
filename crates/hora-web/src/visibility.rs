//! Who may see what: the single home of Hora's visibility rules.
//!
//! Every view (status page, group page, history, timeline, feed, post-mortem,
//! report, latency/heatmap API) asks the same [`Visibility`] instead of
//! re-deciding what "authenticated" means - which is how a group token once
//! ended up with the operator's view.

use std::collections::HashSet;

use hora_core::config::{Config, Monitor};
use hora_core::db::{Incident, PushedAlert};

/// Who is looking. Resolved once per request from the presented credential.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Audience {
    /// No (valid) credential: public monitors only, sanitized.
    Public,
    /// A `server.group_tokens` holder: everything of its own group (private
    /// monitors and full failure detail included), plus what the public sees -
    /// but never the operator's streams (deploy markers, silences, channel
    /// health) nor the name of a private monitor outside the group.
    Group(String),
    /// The `server.view_token` holder: every configured monitor with its
    /// full failure detail, private ones included - a group token for the
    /// whole instance. Read-only, and like a group it never gets the
    /// operator's streams.
    Reader,
    /// The `server.auth_token` holder: everything.
    Operator,
}

/// The visibility rules for one audience over one configuration. Built once
/// per request (or summary build) so the per-row checks are set lookups.
pub(crate) struct Visibility<'a> {
    audience: &'a Audience,
    /// Monitor ids this audience may see (unused for the operator, who sees
    /// every id - including monitors since removed from the config).
    visible: HashSet<&'a str>,
    /// Ids *and* display names this audience may read in an annotation
    /// (topology `cause`/`impacted` store names; older rows stored ids).
    nameable: HashSet<&'a str>,
    /// Monitor ids whose raw failure detail (reason, captured response) this
    /// audience may read.
    detailed: HashSet<&'a str>,
    /// Monitor ids that opted into publishing their detail
    /// (`public_error_detail`): their correlated event and vantage verdict are
    /// published too.
    published: HashSet<&'a str>,
}

impl<'a> Visibility<'a> {
    pub(crate) fn new(config: &'a Config, audience: &'a Audience) -> Self {
        let mut visible = HashSet::new();
        let mut nameable = HashSet::new();
        let mut detailed = HashSet::new();
        let mut published = HashSet::new();
        if *audience != Audience::Operator {
            for monitor in &config.monitors {
                let in_group = match audience {
                    Audience::Reader => true,
                    Audience::Group(group) => monitor.group.as_deref() == Some(group.as_str()),
                    Audience::Public | Audience::Operator => false,
                };
                if !(monitor.public || in_group) {
                    continue;
                }
                visible.insert(monitor.id.as_str());
                nameable.insert(monitor.id.as_str());
                nameable.insert(monitor.name.as_str());
                let opted_in = monitor.public && monitor.public_error_detail;
                if opted_in {
                    published.insert(monitor.id.as_str());
                }
                if in_group || opted_in {
                    detailed.insert(monitor.id.as_str());
                }
            }
        }
        Self {
            audience,
            visible,
            nameable,
            detailed,
            published,
        }
    }

    pub(crate) fn is_operator(&self) -> bool {
        *self.audience == Audience::Operator
    }

    /// Whether the operator-only streams are shown: event markers (deploy
    /// titles), silences, notification-channel health, alert-free-text of
    /// monitors that did not opt in. Not for the read-only token: it opens
    /// the monitors, not the operator's desk.
    pub(crate) fn sees_operator_streams(&self) -> bool {
        self.is_operator()
    }

    /// Whether this audience may see the monitor with this id at all. A
    /// private monitor answers exactly like an unknown one.
    pub(crate) fn can_see(&self, monitor_id: &str) -> bool {
        self.is_operator() || self.visible.contains(monitor_id)
    }

    /// [`Self::can_see`] for a configured monitor.
    pub(crate) fn can_see_monitor(&self, monitor: &Monitor) -> bool {
        self.can_see(&monitor.id)
    }

    /// Whether a monitor id or display name may appear in an annotation.
    pub(crate) fn can_name(&self, id_or_name: &str) -> bool {
        self.is_operator() || self.nameable.contains(id_or_name)
    }

    /// Whether the monitor's raw failure detail (reason, captured response)
    /// reaches this audience, rather than the safe category.
    pub(crate) fn detailed(&self, monitor_id: &str) -> bool {
        self.is_operator() || self.detailed.contains(monitor_id)
    }

    /// Collapse an incident to what this audience may read (callers have
    /// already checked [`Self::can_see`]): without detail the reason falls
    /// back to its safe category and the captured response is dropped; the
    /// correlated event ("deploy api v2.3") and the vantage verdict (which
    /// names the mesh's nodes) are operator streams, published only when the
    /// monitor opted in with `public_error_detail`; topology annotations keep
    /// only nameable monitors. Operator notes (`hora annotate`) deliberately
    /// survive: they are written *for* visitors.
    pub(crate) fn sanitize_incident(&self, incident: &mut Incident) {
        if self.is_operator() {
            return;
        }
        let id = incident.monitor_id.as_str();
        if !self.detailed(id) {
            incident.error = incident
                .error
                .as_deref()
                .map(|reason| hora_core::probe::public_reason(incident.reason, reason).to_owned());
            incident.snapshot = None;
        }
        if !self.published.contains(id) {
            incident.event = None;
            incident.vantage = None;
        }
        incident.cause = incident.cause.take().filter(|cause| self.can_name(cause));
        // `impacted` is a JSON list of names; keep the nameable ones only.
        incident.impacted = incident.impacted.as_deref().and_then(|json| {
            let names: Vec<String> = serde_json::from_str::<Vec<String>>(json)
                .unwrap_or_default()
                .into_iter()
                .filter(|name| self.can_name(name))
                .collect();
            (!names.is_empty()).then(|| serde_json::to_string(&names).unwrap_or_default())
        });
    }

    /// Collapse a pushed alert to what this audience may read: the title and
    /// severity always show (a short headline, like an incident's existence);
    /// the free-form message (which can carry producer detail like file
    /// paths) only with detail.
    pub(crate) fn sanitize_alert(&self, alert: &mut PushedAlert) {
        if !self.detailed(&alert.monitor_id) {
            alert.message.clear();
        }
    }

    /// Keep the incidents this audience may see, sanitized.
    pub(crate) fn filter_incidents(&self, incidents: &mut Vec<Incident>) {
        incidents.retain(|incident| self.can_see(&incident.monitor_id));
        for incident in incidents.iter_mut() {
            self.sanitize_incident(incident);
        }
    }

    /// Keep the pushed alerts this audience may see, sanitized.
    pub(crate) fn filter_alerts(&self, alerts: &mut Vec<PushedAlert>) {
        alerts.retain(|alert| self.can_see(&alert.monitor_id));
        for alert in alerts.iter_mut() {
            self.sanitize_alert(alert);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        hora_core::config::parse(
            r#"
            [page]
            [server]
            auth_token = "0123456789abcdef"
            view_token = "fedcba9876543210"
            [server.group_tokens]
            A = "aaaaaaaaaaaaaaaa"
            [[monitors]]
            id = "a-pub"
            name = "A Public"
            target = "https://a.example.com"
            interval_secs = 60
            group = "A"
            [[monitors]]
            id = "a-priv"
            name = "A Private"
            target = "https://a.internal"
            interval_secs = 60
            group = "A"
            public = false
            [[monitors]]
            id = "b-priv"
            name = "B Private"
            target = "https://b.internal"
            interval_secs = 60
            group = "B"
            public = false
            [[monitors]]
            id = "open"
            name = "Open"
            target = "https://open.example.com"
            interval_secs = 60
            public_error_detail = true
            "#,
        )
        .expect("config")
    }

    fn incident(monitor_id: &str) -> Incident {
        Incident {
            id: 1,
            monitor_id: monitor_id.to_owned(),
            started_at: 0,
            ended_at: None,
            duration_s: None,
            cause: Some("B Private".to_owned()),
            impacted: Some(r#"["A Private","B Private","Open"]"#.to_owned()),
            error: Some("HTTP 503: stack trace".to_owned()),
            reason: None,
            note: Some("fiber cut".to_owned()),
            snapshot: Some("HTTP/2 503".to_owned()),
            event: Some("deploy api v2.3, 3 min before".to_owned()),
            vantage: Some("seen UP by Hora B".to_owned()),
            created_at: 0,
        }
    }

    #[test]
    fn public_sees_and_names_public_monitors_only() {
        let config = config();
        let audience = Audience::Public;
        let vis = Visibility::new(&config, &audience);
        assert!(vis.can_see("a-pub") && vis.can_see("open"));
        assert!(!vis.can_see("a-priv") && !vis.can_see("b-priv") && !vis.can_see("gone"));
        assert!(vis.can_name("A Public") && !vis.can_name("A Private"));
        assert!(!vis.detailed("a-pub") && vis.detailed("open"));
        assert!(!vis.sees_operator_streams());

        let mut row = incident("a-pub");
        vis.sanitize_incident(&mut row);
        assert_eq!(row.error.as_deref(), Some("HTTP 503"));
        assert!(row.snapshot.is_none() && row.event.is_none() && row.vantage.is_none());
        assert!(row.cause.is_none());
        assert_eq!(row.impacted.as_deref(), Some(r#"["Open"]"#));
        assert_eq!(row.note.as_deref(), Some("fiber cut"));
    }

    #[test]
    fn group_gets_its_own_detail_but_no_operator_streams_or_foreign_names() {
        let config = config();
        let audience = Audience::Group("A".to_owned());
        let vis = Visibility::new(&config, &audience);
        assert!(vis.can_see("a-priv") && vis.can_see("a-pub") && vis.can_see("open"));
        assert!(!vis.can_see("b-priv"));
        assert!(vis.can_name("A Private") && !vis.can_name("B Private"));
        assert!(vis.detailed("a-priv") && vis.detailed("a-pub"));
        assert!(!vis.sees_operator_streams());

        let mut row = incident("a-priv");
        vis.sanitize_incident(&mut row);
        // Its own monitor: the raw reason and capture survive...
        assert_eq!(row.error.as_deref(), Some("HTTP 503: stack trace"));
        assert!(row.snapshot.is_some());
        // ...but not the deploy title, the mesh names, nor another group's
        // private monitor.
        assert!(row.event.is_none() && row.vantage.is_none());
        assert!(row.cause.is_none());
        assert_eq!(row.impacted.as_deref(), Some(r#"["A Private","Open"]"#));
    }

    #[test]
    fn reader_gets_every_monitor_in_detail_but_no_operator_streams() {
        let config = config();
        let audience = Audience::Reader;
        let vis = Visibility::new(&config, &audience);
        for id in ["a-pub", "a-priv", "b-priv", "open"] {
            assert!(vis.can_see(id) && vis.detailed(id), "{id}");
        }
        assert!(vis.can_name("A Private") && vis.can_name("B Private"));
        // Not the operator: no streams, and no monitor the config forgot.
        assert!(!vis.is_operator() && !vis.sees_operator_streams());
        assert!(!vis.can_see("gone"));

        let mut row = incident("b-priv");
        vis.sanitize_incident(&mut row);
        assert_eq!(row.error.as_deref(), Some("HTTP 503: stack trace"));
        assert!(row.snapshot.is_some());
        assert_eq!(row.cause.as_deref(), Some("B Private"));
        assert_eq!(
            row.impacted.as_deref(),
            Some(r#"["A Private","B Private","Open"]"#)
        );
        // The deploy title and the mesh's node names stay the operator's.
        assert!(row.event.is_none() && row.vantage.is_none());
    }

    #[test]
    fn operator_sees_everything_untouched() {
        let config = config();
        let audience = Audience::Operator;
        let vis = Visibility::new(&config, &audience);
        assert!(vis.can_see("b-priv") && vis.can_see("gone"));
        assert!(vis.can_name("B Private") && vis.detailed("b-priv"));
        let mut row = incident("b-priv");
        vis.sanitize_incident(&mut row);
        assert_eq!(row.cause.as_deref(), Some("B Private"));
        assert!(row.event.is_some() && row.vantage.is_some());
    }

    #[test]
    fn published_detail_includes_event_and_vantage() {
        let config = config();
        let audience = Audience::Public;
        let vis = Visibility::new(&config, &audience);
        let mut row = incident("open");
        vis.sanitize_incident(&mut row);
        assert_eq!(row.error.as_deref(), Some("HTTP 503: stack trace"));
        assert!(row.event.is_some() && row.vantage.is_some());
    }

    #[test]
    fn alerts_keep_message_only_with_detail() {
        let config = config();
        let audience = Audience::Public;
        let vis = Visibility::new(&config, &audience);
        let alert = |monitor_id: &str| PushedAlert {
            id: 1,
            monitor_id: monitor_id.to_owned(),
            severity: "error".to_owned(),
            title: "t".to_owned(),
            message: "detail".to_owned(),
            dedup_key: None,
            created_at: 0,
        };
        let mut alerts = vec![alert("a-pub"), alert("open"), alert("a-priv")];
        vis.filter_alerts(&mut alerts);
        assert_eq!(alerts.len(), 2);
        assert_eq!(alerts[0].message, "");
        assert_eq!(alerts[1].message, "detail");
    }
}
