//! The JSON shapes exchanged between Hora instances: the `/healthz` report a
//! witness reads, the body of a heartbeat, the confirmation probe of
//! `/api/peer/probe`, and the monitor listing of `/api/peer/monitors`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::{Kind, redact_url_secrets};
use crate::status::MonitorState;

// --- /healthz report ------------------------------------------------------

/// The header carrying a push monitor's, a peer's or a pushed alert's token:
/// a header, because it stays out of access logs. Sent by the mesh, read by
/// the web layer.
pub const PUSH_TOKEN_HEADER: &str = "x-push-token";

/// The body of `/healthz`: this node's own health plus its view of every peer it
/// watches. `status` is `"ok"` only when the node is fully healthy, so an external
/// keyword monitor (e.g. `UptimeRobot` matching `"ok"`) detects trouble; the rest of
/// the document is ignored by such pollers but read by other Hora for quorum.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HealthReport {
    /// `"ok"` when the scheduler ticks, the database answers and the checks
    /// can run, else `"degraded"`.
    pub status: HealthStatus,
    pub scheduler_ok: bool,
    pub db_ok: bool,
    /// Seconds since the most recent scheduler tick; `-1` if it has not ticked yet.
    pub last_tick_age: i64,
    /// Whether this node's checks can run: `false` while it is out of file
    /// descriptors or memory and skips them (absent from an older node,
    /// which reads as `true`).
    #[serde(default = "yes")]
    pub probes_ok: bool,
    /// This node's identity (`[health].id`), absent if no `[health]` section.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// This node's view of each watched peer, keyed by the peer's global id.
    pub peers: HashMap<String, PeerSeen>,
}

fn yes() -> bool {
    true
}

/// A node's own health on `/healthz`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    /// The scheduler ticks and the database answers.
    Ok,
    /// Anything else - including a word a newer peer may send one day: a
    /// report this node cannot read is not a healthy one.
    #[serde(other)]
    Degraded,
}

impl HealthStatus {
    /// The wire word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
        }
    }
}

/// How a node sees a peer it watches, from that peer's heartbeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PeerState {
    /// Heartbeating within its expected interval.
    Up,
    /// Its last heartbeat is overdue.
    Down,
    /// Never heard from (or a word this node does not know).
    #[serde(other)]
    Unknown,
}

impl PeerState {
    /// The wire word.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
            Self::Unknown => "unknown",
        }
    }
}

/// One node's view of a peer, as reported on `/healthz`.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PeerSeen {
    /// `"up"`, `"down"`, or `"unknown"` (never seen).
    pub state: PeerState,
    /// Seconds since the peer's last heartbeat; `-1` if never seen.
    pub age: i64,
}

// --- Heartbeat (/api/push/{listen_id}) -------------------------------------

/// The JSON body of a heartbeat between two Hora. A heartbeat is otherwise a
/// bare `POST` with `?status=up&msg=...`, and stays one for every other
/// receiver: the body is only sent when there is something in it. A 0.11
/// node sends none and reads none - its handler never looks at the body -
/// so either side may be the older one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct HeartbeatBody {
    /// Every maintenance window the sender plans, the ones already under
    /// way included. The list is complete: a window that was in an earlier
    /// heartbeat and is no longer listed has been cancelled.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub maintenance: Vec<AnnouncedWindow>,
}

/// One maintenance window a node announces to its peers: "expect me to go
/// silent from `starts_at` to `ends_at`", and, with `host`, "and the
/// services on that host with me".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AnnouncedWindow {
    /// The sender's id for the window, never reused: the same window keeps
    /// it from one heartbeat to the next.
    pub id: i64,
    /// Unix epoch seconds (UTC).
    pub starts_at: i64,
    /// Unix epoch seconds (UTC).
    pub ends_at: i64,
    #[serde(default)]
    pub reason: String,
    /// The sender's own host, when the window is for the whole machine. The
    /// receiver only honours it when it is the host it has configured for
    /// this peer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

// --- Peer API (/api/peer/*) --------------------------------------------------

/// What one node asks another to probe. The responder matches `kind` +
/// `target` (and `check`, when the request carries one) against its own
/// monitors and refuses anything else.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProbeRequest {
    /// The requesting node's `[health].id`; the responder authenticates it
    /// against that peer's `listen_token`.
    pub from: String,
    pub kind: Kind,
    /// The target, secrets masked (see [`disclosed_target`]): a peer that
    /// does not have this monitor must not learn its credentials by being
    /// asked. A 0.11 node still sends it verbatim; the responder masks both
    /// sides before comparing.
    pub target: String,
    /// The fingerprint of the requester's check (see
    /// [`check_fingerprint`](super::fingerprint::check_fingerprint)): the
    /// responder then answers only with a monitor that checks the same way.
    /// Absent from a 0.11 node, and for an ad-hoc `hora probe <target>
    /// --confirm`, which asks about the target and nothing more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// The vantage's verdict on one probe.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ProbeResponse {
    pub up: bool,
    /// The failure reason when not up, bounded by the responder.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// What a node discloses to an authenticated peer on `GET /api/peer/monitors`:
/// its probeable monitors with their live view and the fingerprint of what
/// each asserts - enough for `hora peers diff` (are our configs aligned?) and
/// the per-vantage latency display, and nothing else (no names, notes or
/// credentials).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PeerMonitors {
    pub monitors: Vec<PeerMonitor>,
}

/// A monitor's target as it is disclosed to a peer, and compared between
/// nodes: with the secrets a URL may carry masked (`user:pass@`, the values
/// of `?api_key=...`; see [`redact_url_secrets`]). A peer authenticates with
/// the mesh token, which is no reason to hand it every monitor's credentials.
///
/// Whoever compares targets across nodes (`hora peers diff`, the per-vantage
/// view) passes *both* sides through here: the masking is idempotent, so a
/// target a 0.11 node still sends verbatim compares equal to the same target
/// masked locally. Two monitors that differ only by a secret are one target
/// to a peer - which is all a peer should be able to tell.
#[must_use]
pub fn disclosed_target(target: &str) -> String {
    redact_url_secrets(target).into_owned()
}

/// One probeable monitor as seen from a peer's vantage.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct PeerMonitor {
    pub kind: Kind,
    /// The target, secrets masked (see [`disclosed_target`]).
    pub target: String,
    /// `up` | `degraded` | `down` | `unknown`, from that node's view.
    pub status: MonitorState,
    /// That node's 24h median latency to the target, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_ms: Option<i64>,
    /// The fingerprint of that node's check on the target (see
    /// [`check_fingerprint`](super::fingerprint::check_fingerprint)); absent
    /// from a 0.11 node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}
