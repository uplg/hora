//! The view model the page template and the JSON API render.

use std::sync::Arc;

use askama::Template;
use chrono::{DateTime, Utc};
use hora_core::probe::FailureKind;
use hora_core::status::MonitorState;
use serde::Serialize;
use utoipa::ToSchema;

use crate::display::DisplayState;

// --- View model ----------------------------------------------------------

#[derive(Serialize, ToSchema)]
pub(crate) struct Summary {
    pub(crate) title: String,
    pub(crate) overall: super::Overall,
    pub(crate) overall_label: &'static str,
    pub(crate) generated_at: String,
    #[serde(skip)]
    pub(crate) updated_utc: String,
    pub(crate) incidents: Vec<IncidentView>,
    pub(crate) maintenances: Vec<MaintenanceView>,
    /// The windows that have not started yet and start within a week, for
    /// the page's "Next" line: the ones this audience may be told about.
    #[serde(skip)]
    pub(crate) upcoming: Vec<MaintenanceView>,
    /// Shared with the groups' card lists (and the per-group pages derived
    /// from this summary), so a view - chart SVG included - is built once.
    #[serde(serialize_with = "serialize_views")]
    #[schema(value_type = Vec<MonitorView>)]
    pub(crate) monitors: Vec<Arc<MonitorView>>,
    /// Monitor groups: `(group_name, monitors)`. Ungrouped monitors appear under
    /// an empty-string key, always last.
    pub(crate) groups: Vec<GroupView>,
    pub(crate) peers: Vec<PeerView>,
    /// Notification-channel health (operator only; empty in the public view).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) channels: Vec<ChannelView>,
    /// The database's size (operator only; absent for every other audience).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) storage: Option<StorageView>,
    /// The last 24 h's event markers ("deploy api v2.3"), drawn on the
    /// monitor charts: an operator stream, empty for every other audience.
    #[serde(skip)]
    pub(crate) events: Arc<[hora_core::db::EventMarker]>,
    /// The latest incidents this audience may see, newest first (the status
    /// page's "Recent incidents").
    #[serde(skip)]
    pub(crate) recent: Vec<RecentIncident>,
    /// Every recent incident this audience may see, newest first: what
    /// `recent` was cut from. A group's page picks its own newest from here,
    /// rather than from a cut that other groups' incidents may have filled.
    #[serde(skip)]
    pub(crate) recent_pool: Vec<RecentIncident>,
    /// Whole days since the last incident this audience may see ended (or
    /// since its oldest data, without one); `None` while one is ongoing.
    #[serde(skip)]
    pub(crate) quiet_days: Option<i64>,
}

/// One past or ongoing incident, as the status page lists it.
#[derive(Clone)]
pub(crate) struct RecentIncident {
    pub(crate) id: i64,
    pub(crate) monitor_id: String,
    /// `2026-09-18`, and `18 Sep`.
    pub(crate) date: String,
    pub(crate) day: String,
    /// "Public API was down for 41 minutes", "Public API is down".
    pub(crate) title: String,
    /// The operator's note on it, else what it answered (sanitized).
    pub(crate) note: Option<String>,
    pub(crate) resolved: bool,
}

#[derive(Serialize, ToSchema)]
pub(crate) struct GroupView {
    pub(crate) name: String,
    /// Member monitor ids, in display order. The full monitor objects live once in
    /// the top-level `monitors`; the API carries only ids here so the response is
    /// not doubled, and a caller maps ids back to the monitors to render sections.
    pub(crate) ids: Vec<String>,
    /// The rendered cards, for the server-side template only; skipped from the API
    /// (it would duplicate every monitor object).
    #[serde(skip)]
    pub(crate) monitors: Vec<Arc<MonitorView>>,
}

/// Serialize shared views as plain objects (serde's `rc` feature is off).
fn serialize_views<S: serde::Serializer>(
    views: &[Arc<MonitorView>],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.collect_seq(views.iter().map(AsRef::as_ref))
}

#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct IncidentView {
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) severity: &'static str,
    pub(crate) at: Option<String>,
}

#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct MaintenanceView {
    pub(crate) reason: String,
    pub(crate) monitors: String,
    /// The window's own hours, for the page's hero. Carried with the banner
    /// so the page never has to find the window again in the config: looked
    /// up by title, two windows with the same (or no) title were one, and a
    /// group's page showed another group's hours.
    #[serde(skip)]
    pub(crate) start: DateTime<Utc>,
    #[serde(skip)]
    pub(crate) end: DateTime<Utc>,
    /// The window covers everything.
    #[serde(skip)]
    pub(crate) all: bool,
    /// Otherwise, the monitor ids behind `monitors`: only those this
    /// audience may be told about, so a narrower page (a group's) can cut
    /// the list again without learning anything.
    #[serde(skip)]
    pub(crate) ids: Vec<String>,
    /// A `[[maintenance]]` window of the config.
    #[serde(skip)]
    pub(crate) configured: bool,
    /// For the operator: the host it was planned for, and the peer that
    /// announced it. Never set for another audience.
    #[serde(skip)]
    pub(crate) note: Option<String>,
}

/// A watched peer's view for the status page: just its current status and when it
/// was last seen. Peers are deliberately rendered apart from monitors.
#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct PeerView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) status: MonitorState,
    pub(crate) last_seen: Option<String>,
    /// Whether this node also heartbeats the peer (the OUT side is configured).
    pub(crate) pings: bool,
}

/// Notification-channel health, for the operator view only (`top`, authenticated
/// `/api/summary`). The public status page never sees this: channel names and
/// delivery health are the operator's business, not a client's.
#[derive(Serialize, ToSchema)]
pub(crate) struct ChannelView {
    pub(crate) name: String,
    pub(crate) failing: bool,
    pub(crate) consecutive_failures: u32,
    /// Seconds since the first failure in the current streak; `null` when the
    /// channel is healthy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failing_for_secs: Option<u64>,
}

/// The database's size, for `hora top`'s hint to run `hora compact`. Read
/// from the file's header counters (O(1)), not measured page by page.
#[derive(Serialize, ToSchema)]
pub(crate) struct StorageView {
    /// The database file, in bytes (without its WAL).
    pub(crate) db_bytes: u64,
    /// Its free pages, in bytes: what `hora compact` gives back at the least
    /// (fragmentation inside used pages comes on top).
    pub(crate) reclaimable_bytes: u64,
}

/// A card. Cloned only to adjust a few fields for another audience (see
/// `summary::derive`): the heavy parts - the latency series and the daily
/// bar - are shared, not copied.
#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct MonitorView {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) status: MonitorState,
    pub(crate) last_latency_ms: Option<i64>,
    /// Failure reason of the most recent check, when it was not up: surfaces
    /// the *why* behind a degraded/down card without opening the database.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) last_error: Option<String>,
    /// What kind of failure `last_error` describes, kept to derive the
    /// public form for the other audiences.
    #[serde(skip)]
    pub(crate) last_reason: Option<FailureKind>,
    pub(crate) last_checked: Option<String>,
    #[serde(rename = "uptime_24h_permille")]
    pub(crate) uptime_permille: Option<i64>,
    #[serde(skip)]
    pub(crate) uptime_label: Option<String>,
    #[serde(rename = "latency_p50_ms")]
    pub(crate) p50_ms: Option<i64>,
    #[serde(rename = "latency_p95_ms")]
    pub(crate) p95_ms: Option<i64>,
    #[serde(rename = "latency_p99_ms")]
    pub(crate) p99_ms: Option<i64>,
    #[serde(rename = "slo_latency_ms")]
    pub(crate) slo_latency_ms: Option<i64>,
    #[serde(skip)]
    pub(crate) slo_state: super::status::SloState,
    /// Availability SLO target, percent (e.g. 99.9).
    #[serde(rename = "slo_uptime_pct", skip_serializing_if = "Option::is_none")]
    pub(crate) slo_uptime_pct: Option<f64>,
    #[serde(
        rename = "error_budget_minutes_total",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) budget_minutes_total: Option<i64>,
    #[serde(
        rename = "error_budget_minutes_left",
        skip_serializing_if = "Option::is_none"
    )]
    pub(crate) budget_minutes_left: Option<i64>,
    #[serde(skip)]
    pub(crate) budget_label: Option<String>,
    #[serde(skip)]
    pub(crate) budget_title: String,
    #[serde(skip)]
    pub(crate) budget_state: super::status::BudgetState,
    /// Active maintenance window title (alerts muted); `None` outside a window.
    pub(crate) maintenance: Option<String>,
    /// When the open incident started (confirmed down), while it is down.
    #[serde(skip)]
    pub(crate) down_since: Option<i64>,
    /// The latency above which it reads slow (`degraded_over_ms`), if set.
    #[serde(skip)]
    pub(crate) slow_over_ms: Option<i64>,
    /// Its incident log in short: the last end, the open one's start.
    #[serde(skip)]
    pub(crate) marks: hora_core::db::IncidentMarks,
    /// Availability over the daily bar's days (`[page].history_days`),
    /// permille; `None` without checks.
    #[serde(skip)]
    pub(crate) uptime_long_permille: Option<i64>,
    /// Whole days until the certificate expires: at least 1 while it is
    /// valid, negative once it has expired (see `status::cert_days_left`).
    #[serde(rename = "cert_expiry_days")]
    pub(crate) cert_days: Option<i64>,
    /// The same in seconds (negative once expired), when the build ran: the
    /// last day reads in hours, and "expired" means it.
    #[serde(skip)]
    pub(crate) cert_secs_left: Option<i64>,
    #[serde(skip)]
    pub(crate) cert_label: Option<String>,
    #[serde(skip)]
    pub(crate) cert_state: super::status::CertState,
    #[serde(rename = "history")]
    #[schema(value_type = Vec<DayCell>)]
    pub(crate) bar: DayBar,
    /// The 24 h latency series, drawn by the monitor's own page only (the
    /// status page shows no chart), shared with every audience's copy.
    #[serde(skip)]
    pub(crate) spark: Arc<[hora_core::db::Point]>,
    /// Display group this monitor belongs to.
    pub(crate) group: Option<String>,
    /// Upstream monitor name causing this failure (topology annotation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cause: Option<String>,
    /// Downstream monitor names impacted by this root-cause failure.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) impacted: Vec<String>,
    /// The peers' view of this target ("80 ms from EU"), when the mesh
    /// monitors it too. Empty without peers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) vantages: Vec<VantageView>,
}

/// One peer's view of a monitor's target, for the card and the API.
#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct VantageView {
    /// The peer's display name.
    pub(crate) peer: String,
    /// `up` | `degraded` | `down` | `unknown` from that vantage.
    pub(crate) status: MonitorState,
    /// That vantage's 24h median latency, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) p50_ms: Option<i64>,
    /// The pre-formatted card label ("Hora B: 220 ms" / "Hora B: down").
    #[serde(skip)]
    pub(crate) label: String,
}

/// The daily uptime bar, shared between a card and its copies for other
/// audiences. Iterates and serializes like the plain list of cells.
#[derive(Clone)]
pub(crate) struct DayBar(Arc<[DayCell]>);

impl From<Vec<DayCell>> for DayBar {
    fn from(cells: Vec<DayCell>) -> Self {
        Self(cells.into())
    }
}

impl std::ops::Deref for DayBar {
    type Target = [DayCell];

    fn deref(&self) -> &[DayCell] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a DayBar {
    type Item = &'a DayCell;
    type IntoIter = std::slice::Iter<'a, DayCell>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl Serialize for DayBar {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.0.iter())
    }
}

#[derive(Clone, Serialize, ToSchema)]
pub(crate) struct DayCell {
    pub(crate) date: String,
    pub(crate) state: &'static str,
    /// The day's availability, permille; `None` without checks. (Kept as a
    /// number: a formatted title per cell was 90 strings per monitor and
    /// build, the largest share of the summary's memory and build time.)
    #[serde(skip)]
    pub(crate) permille: Option<i64>,
    /// A configured maintenance window covering this monitor touched the day.
    #[serde(skip)]
    pub(crate) maint: bool,
}

// --- Presentation helpers, derived from the fields above only --------------

impl Summary {
    /// When the summary was built, as the page's eyebrow says the hour:
    /// "Saturday 3 October, 14:32 UTC".
    pub(crate) fn when(&self) -> String {
        DateTime::parse_from_rfc3339(&self.generated_at).map_or_else(
            |_| self.updated_utc.clone(),
            |at| {
                at.with_timezone(&Utc)
                    .format("%A %-d %B, %H:%M UTC")
                    .to_string()
            },
        )
    }

    /// How many places watch: this node plus every peer that reported a view
    /// of at least one shared target. 1 without a mesh.
    pub(crate) fn places(&self) -> usize {
        let mut peers: Vec<&str> = self
            .monitors
            .iter()
            .flat_map(|monitor| monitor.vantages.iter().map(|v| v.peer.as_str()))
            .collect();
        peers.sort_unstable();
        peers.dedup();
        1 + peers.len()
    }
}

impl MonitorView {
    /// The state the page shows: its status, or maintenance inside a planned
    /// window. A local-only down (see [`Self::local_only`]) shows as up: the
    /// service is up for its users.
    pub(crate) fn display_state(&self) -> DisplayState {
        DisplayState::shown(self.status, self.maintenance.is_some(), self.local_only())
    }

    /// Down from this node only: every peer that watches the same target
    /// sees it up (or slow). A network problem near this node, not an outage.
    pub(crate) fn local_only(&self) -> bool {
        self.status == MonitorState::Down
            && !self.vantages.is_empty()
            && self
                .vantages
                .iter()
                .all(|vantage| DisplayState::from(vantage.status).answers())
    }

    /// The last check failed but the failure threshold is not reached yet
    /// (degraded with an error): it is being checked again, not slow.
    pub(crate) fn retrying(&self) -> bool {
        self.display_state() == DisplayState::Degraded && self.last_error.is_some()
    }

    /// The state's word on the page. "Retrying" for [`Self::retrying`],
    /// "Slow" when it answered above its latency threshold.
    pub(crate) fn word(&self) -> &'static str {
        if self.retrying() {
            "Retrying"
        } else {
            self.display_state().word()
        }
    }
}

/// The status page (whole, or one group's): the page chrome plus the status
/// view built from the summary (see `crate::status_page`).
#[derive(Template)]
#[template(path = "status.html")]
pub(crate) struct StatusTemplate<'a> {
    pub(crate) chrome: crate::layout::Chrome,
    pub(crate) page: crate::status_page::StatusPage<'a>,
}
