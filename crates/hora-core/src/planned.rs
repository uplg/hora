//! Planned maintenance: the one place that answers "is this monitor, or this
//! peer, in maintenance at this time - and in which window".
//!
//! A window comes from three sources: the `[[maintenance]]` tables of the
//! config, the operator at runtime (`POST /api/maintenance`, `hora
//! maintenance plan`), and a peer announcing on its heartbeat that its host
//! is about to go down. The last two are stored (see
//! [`db::MaintenanceWindow`]). A [`Schedule`] merges the three for one
//! configuration; the scheduler, the peer watches, the status page, the
//! monthly report and the timeline all read it, so none of them can disagree
//! on what is muted.
//!
//! A stored window says what it covers as a [`Selector`], resolved against
//! the configuration each time a schedule is built: a monitor that gains the
//! `host` label is covered from the next read, and a peer that loses its
//! `accept_maintenance_secs` mutes nothing more.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};

use chrono::{DateTime, Utc};
use tracing::{error, info, warn};

use crate::config::{Config, Peer};
use crate::db::{self, MaintenanceWindow, NewMaintenanceWindow, Store};
use crate::mesh::wire::AnnouncedWindow;
use crate::{MAX_SILENCE_REASON_CHARS, MAX_SILENCE_SECS, SECONDS_PER_DAY, bounded, parse_duration};

/// How far ahead the API plans a window: a year. Past that it is a typo (a
/// timestamp in milliseconds), not a plan.
pub const MAX_PLAN_AHEAD_SECS: i64 = 365 * SECONDS_PER_DAY;

/// How far ahead a window is announced to the peers, and accepted from one:
/// a week. A reboot is known hours ahead; a window months away has time to
/// be announced later, and a peer should not hold a mute it was handed long
/// ago.
pub const MAX_ANNOUNCE_AHEAD_SECS: i64 = 7 * SECONDS_PER_DAY;

/// The most windows one heartbeat announces, and the most a peer's
/// announcement is read for. A node plans a handful; more is a bug or an
/// attempt to fill the receiver's table.
pub const MAX_ANNOUNCED_WINDOWS: usize = 32;

/// How much a peer may mute in any 24 hours, in windows of its accepted
/// length: with `accept_maintenance_secs = 900`, an hour a day. The length
/// bounds one window; without this a peer that announces one after the
/// other - a script that plans "now, for ten minutes" every minute is
/// enough - would keep its host silent for good.
pub const PEER_BUDGET_WINDOWS: i64 = 4;

/// The origin of a window planned on this node, as stored.
const ORIGIN_API: &str = "api";
/// The prefix of the origin of a window a peer announced (`peer:<peer id>`).
const ORIGIN_PEER_PREFIX: &str = "peer:";

/// What a stored window covers, as it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Everything: every monitor and every peer watch (`*`).
    All,
    /// These ids: monitors, or watched peers' listen ids (`ids:a,b`).
    Ids(Vec<String>),
    /// The monitors labelled with this host (`host:<name>`).
    Host(String),
    /// Nothing beyond the watch of the peer that announced it (`peer`).
    Peer,
}

impl Selector {
    /// From the stored text; `None` for a row this version cannot read.
    #[must_use]
    pub fn parse(stored: &str) -> Option<Self> {
        if stored == "*" {
            Some(Self::All)
        } else if stored == "peer" {
            Some(Self::Peer)
        } else if let Some(ids) = stored.strip_prefix("ids:") {
            Some(Self::Ids(ids.split(',').map(str::to_owned).collect()))
        } else {
            stored
                .strip_prefix("host:")
                .map(|host| Self::Host(host.to_owned()))
        }
    }
}

impl std::fmt::Display for Selector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::All => f.write_str("*"),
            Self::Peer => f.write_str("peer"),
            Self::Ids(ids) => write!(f, "ids:{}", ids.join(",")),
            Self::Host(host) => write!(f, "host:{host}"),
        }
    }
}

/// Where a window comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// A `[[maintenance]]` table of the config.
    Config,
    /// Planned on this node, through the API or the CLI.
    Api,
    /// Announced by this peer (its `[[peers]]` id) on its heartbeat.
    Peer(String),
}

impl Origin {
    fn parse(stored: &str) -> Option<Self> {
        if stored == ORIGIN_API {
            Some(Self::Api)
        } else {
            stored
                .strip_prefix(ORIGIN_PEER_PREFIX)
                .map(|peer| Self::Peer(peer.to_owned()))
        }
    }
}

/// The stored origin of the windows `peer` announces.
fn peer_origin(peer: &Peer) -> String {
    format!("{ORIGIN_PEER_PREFIX}{}", peer.id)
}

/// One maintenance window, from any source, resolved against a configuration:
/// it names the ids it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// The stored window's id; `None` for one of the config.
    pub id: Option<i64>,
    /// The config window's title, or the reason given when it was planned.
    pub title: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// Covers every monitor and every peer watch.
    pub all: bool,
    /// Otherwise, the ids it covers: monitors, and watched peers' listen ids.
    pub monitors: Vec<String>,
    /// The host it was planned for, when it was planned for one.
    pub host: Option<String>,
    pub origin: Origin,
}

impl Window {
    /// Whether the window covers this monitor id (or peer listen id).
    #[must_use]
    pub fn covers(&self, id: &str) -> bool {
        self.all || self.monitors.iter().any(|covered| covered == id)
    }

    /// Whether `at` falls inside the window, both bounds included.
    #[must_use]
    pub fn is_active(&self, at: DateTime<Utc>) -> bool {
        at >= self.start && at <= self.end
    }

    /// The watched peers whose listen id the window covers, by name: what it
    /// mutes beyond monitors.
    #[must_use]
    pub fn covered_peers<'c>(&self, config: &'c Config) -> Vec<&'c str> {
        config
            .peers
            .iter()
            .filter(|peer| peer.is_watched() && !self.all && self.covers(peer.listen_id()))
            .map(|peer| peer.name.as_str())
            .collect()
    }
}

/// Resolve a stored window against `config`: which ids it covers today.
/// `None` for a row that mutes nothing here - one this version cannot read,
/// or one announced by a peer that is no longer trusted to announce.
///
/// This is where a peer's reach is bounded, whatever was stored: its window
/// covers the watch of that peer, plus this node's monitors labelled with the
/// peer's own configured `host` when that is the host it announced - and it
/// never lasts longer than the peer's `accept_maintenance_secs`.
#[must_use]
pub fn resolve(config: &Config, row: &MaintenanceWindow) -> Option<Window> {
    let origin = Origin::parse(&row.origin)?;
    let selector = Selector::parse(&row.selector)?;
    let start = DateTime::from_timestamp(row.starts_at, 0)?;
    let end = DateTime::from_timestamp(row.ends_at, 0)?;
    let mut window = Window {
        id: Some(row.id),
        title: row.reason.clone(),
        start,
        end,
        all: false,
        monitors: Vec::new(),
        host: None,
        origin: origin.clone(),
    };
    match (&origin, selector) {
        // Only the loader makes these.
        (Origin::Config, _) | (Origin::Api, Selector::Peer) => return None,
        (Origin::Api, Selector::All) => window.all = true,
        (Origin::Api, Selector::Ids(ids)) => window.monitors = ids,
        (Origin::Api, Selector::Host(host)) => {
            window.monitors = ids_on_host(config, &host);
            window.host = Some(host);
        }
        (Origin::Peer(peer_id), selector) => {
            let peer = config.peers.iter().find(|peer| &peer.id == peer_id)?;
            let accepted = trusted_secs(peer)?;
            window.end = end.min(start + chrono::TimeDelta::seconds(accepted));
            window.monitors.push(peer.listen_id().to_owned());
            // A peer declares maintenance for its own host and nothing else:
            // not for another host, never for monitors it would name.
            if let Selector::Host(host) = selector
                && peer.host.as_deref() == Some(host.as_str())
            {
                window.monitors.extend(
                    config
                        .monitors
                        .iter()
                        .filter(|monitor| monitor.host.as_deref() == Some(host.as_str()))
                        .map(|monitor| monitor.id.clone()),
                );
                window.host = Some(host);
            }
        }
    }
    Some(window)
}

/// For how long `peer` may mute alerts here with one announced window, in
/// seconds; `None` when its announcements count for nothing. Config
/// validation already refuses `accept_maintenance_secs` on a peer that is
/// not watched or not authenticated; this holds even if it did not.
fn trusted_secs(peer: &Peer) -> Option<i64> {
    (peer.is_watched() && peer.listen_token.is_some() && !peer.allow_unauthenticated)
        .then(|| peer.accepted_maintenance_secs())
        .flatten()
}

/// What a window planned here for `host` covers: the monitors labelled with
/// it, and the watch of a peer that runs on it (that node goes down with its
/// host).
fn ids_on_host(config: &Config, host: &str) -> Vec<String> {
    config
        .monitors
        .iter()
        .filter(|monitor| monitor.host.as_deref() == Some(host))
        .map(|monitor| monitor.id.clone())
        .chain(
            config
                .peers
                .iter()
                .filter(|peer| peer.is_watched() && peer.host.as_deref() == Some(host))
                .map(|peer| peer.listen_id().to_owned()),
        )
        .collect()
}

/// Every maintenance window that applies under one configuration: the
/// config's own and the stored ones, resolved.
#[derive(Debug, Clone, Default)]
pub struct Schedule {
    windows: Vec<Window>,
}

impl Schedule {
    /// The config's windows followed by the stored ones that apply (see
    /// [`resolve`]).
    #[must_use]
    pub fn merged(config: &Config, stored: &[MaintenanceWindow]) -> Self {
        let configured = config.maintenance.iter().map(|window| Window {
            id: None,
            title: window.title.clone(),
            start: window.start,
            end: window.end,
            all: window.monitors.is_empty(),
            monitors: window.monitors.clone(),
            host: None,
            origin: Origin::Config,
        });
        Self {
            windows: configured
                .chain(stored.iter().filter_map(|row| resolve(config, row)))
                .collect(),
        }
    }

    /// Every window, the config's first.
    #[must_use]
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// The window covering `id` (a monitor id, or a watched peer's listen
    /// id) at `at`, if any.
    #[must_use]
    pub fn active(&self, id: &str, at: DateTime<Utc>) -> Option<&Window> {
        self.windows
            .iter()
            .find(|window| window.is_active(at) && window.covers(id))
    }

    /// Whether `id` is inside a maintenance window at `at` (alerts muted).
    #[must_use]
    pub fn in_maintenance(&self, id: &str, at: DateTime<Utc>) -> bool {
        self.active(id, at).is_some()
    }
}

/// The schedule of the windows that overlap `[from, to]` (unix seconds).
/// A read error leaves the config's windows only, and says so: a database
/// hiccup must not invent a mute, and the page still renders.
pub async fn schedule_between(store: &Store, config: &Config, from: i64, to: i64) -> Schedule {
    let stored = db::maintenance_windows_between(store, from, to)
        .await
        .unwrap_or_else(|err| {
            error!("failed to read the planned maintenance windows: {err:#}");
            Vec::new()
        });
    Schedule::merged(config, &stored)
}

/// The window covering `id` at `now`, from the config or the database.
pub async fn active_window(
    store: &Store,
    config: &Config,
    id: &str,
    now: DateTime<Utc>,
) -> Option<Window> {
    let at = now.timestamp();
    schedule_between(store, config, at, at)
        .await
        .active(id, now)
        .cloned()
}

/// Whether `id` (a monitor id, or a watched peer's listen id) is inside a
/// maintenance window at `now`: the question every alert asks before it goes
/// out. Checks are recorded either way.
pub async fn in_maintenance(store: &Store, config: &Config, id: &str, now: DateTime<Utc>) -> bool {
    active_window(store, config, id, now).await.is_some()
}

// --- Planning on this node ------------------------------------------------

/// A window to plan, as the API and the CLI receive it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Plan<'a> {
    /// Comma-separated monitor ids (or watched peers' listen ids), or `all`.
    pub monitors: Option<&'a str>,
    /// A host name: the monitors labelled with it. Exactly one of the two.
    pub host: Option<&'a str>,
    /// When it starts: RFC 3339 (`2026-11-13T05:00:00Z`) or unix seconds;
    /// `None` = now.
    pub starts_at: Option<&'a str>,
    /// How long it lasts (`10m`, `1h30m`), at most 7 days.
    pub duration: &'a str,
    /// Why, shown on the status page.
    pub reason: &'a str,
}

/// Why a window was not planned.
#[derive(Debug)]
pub enum PlanError {
    /// Neither `monitors` nor `host`, or both.
    Selector,
    /// `starts_at` is neither RFC 3339 nor unix seconds, or lies more than a
    /// year ahead.
    InvalidStart,
    /// Not a positive duration, or longer than [`MAX_SILENCE_SECS`].
    InvalidDuration,
    /// The window would already be over.
    AlreadyOver,
    /// No reason given.
    NoReason,
    /// An empty id list.
    NoIds,
    /// An id that is neither a configured monitor nor a watched peer.
    UnknownId(String),
    /// A host no monitor, no peer and not this node is labelled with.
    UnknownHost(String),
    /// The insert failed.
    Database(db::Error),
}

impl PlanError {
    /// The message every door (API, CLI) answers with.
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            Self::Selector => "give exactly one of monitors=<ids|all> or host=<name>",
            Self::InvalidStart => {
                "invalid starts_at (RFC 3339 such as 2026-11-13T05:00:00Z, or unix seconds; \
                 at most a year ahead)"
            }
            Self::InvalidDuration => "invalid duration (use e.g. 10m, 1h30m; max 7d)",
            Self::AlreadyOver => "the window would already be over",
            Self::NoReason => "reason must not be empty",
            Self::NoIds => "no monitor ids given",
            Self::UnknownId(_) => "unknown monitor id",
            Self::UnknownHost(_) => "unknown host (no monitor, peer or [health] carries it)",
            Self::Database(_) => "recording the maintenance window failed",
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownId(id) => write!(f, "unknown monitor id {id:?}"),
            Self::UnknownHost(host) => write!(f, "unknown host {host:?}"),
            Self::Database(err) => write!(f, "{}: {err}", self.message()),
            other => f.write_str(other.message()),
        }
    }
}

impl std::error::Error for PlanError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            _ => None,
        }
    }
}

/// A planned window, and whether this call created it (`false`: the same
/// window was already planned, and is returned as it was).
#[derive(Debug, Clone)]
pub struct Planned {
    pub window: MaintenanceWindow,
    pub created: bool,
}

/// A window's start: RFC 3339, or unix seconds. `None` for anything else.
#[must_use]
pub fn parse_start(raw: &str) -> Option<i64> {
    let raw = raw.trim();
    if !raw.is_empty() && raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return raw.parse().ok();
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|at| at.timestamp())
}

/// Plan a maintenance window on this node, at `now`.
///
/// Everything is checked before anything is written, so a typo'd host script
/// fails loudly instead of planning nothing. Planning the same window again -
/// same selector, start and end - returns the one already stored: a script
/// that runs every few minutes must pass an explicit `starts_at` for that to
/// hold (the default, now, moves).
///
/// # Errors
///
/// See [`PlanError`].
pub async fn plan(
    store: &Store,
    config: &Config,
    plan: Plan<'_>,
    now: i64,
) -> Result<Planned, PlanError> {
    let selector = match (plan.monitors, plan.host.map(str::trim)) {
        (Some(ids), None) => match crate::silence::resolve_ids(config, ids) {
            Ok(ids) if ids == ["*"] => Selector::All,
            Ok(mut ids) => {
                // One spelling per set of ids, so `web,api` repeats `api,web`.
                ids.sort_unstable();
                ids.dedup();
                Selector::Ids(ids)
            }
            Err(crate::silence::SilenceError::UnknownId(id)) => {
                return Err(PlanError::UnknownId(id));
            }
            Err(_) => return Err(PlanError::NoIds),
        },
        (None, Some(host)) if config.knows_host(host) => Selector::Host(host.to_owned()),
        (None, Some(host)) => return Err(PlanError::UnknownHost(bounded(host, 64))),
        _ => return Err(PlanError::Selector),
    };
    let starts_at = match plan.starts_at {
        None => now,
        Some(raw) => parse_start(raw)
            .filter(|start| *start <= now.saturating_add(MAX_PLAN_AHEAD_SECS))
            .ok_or(PlanError::InvalidStart)?,
    };
    let duration = parse_duration(plan.duration)
        .filter(|secs| *secs <= MAX_SILENCE_SECS)
        .and_then(|secs| i64::try_from(secs).ok())
        .ok_or(PlanError::InvalidDuration)?;
    let ends_at = starts_at.saturating_add(duration);
    if ends_at <= now {
        return Err(PlanError::AlreadyOver);
    }
    let reason = bounded(plan.reason, MAX_SILENCE_REASON_CHARS);
    if reason.is_empty() {
        return Err(PlanError::NoReason);
    }
    let (window, created) = db::insert_maintenance_window(
        store,
        NewMaintenanceWindow {
            starts_at,
            ends_at,
            reason: &reason,
            selector: &selector.to_string(),
            origin: ORIGIN_API,
            remote_id: None,
        },
    )
    .await
    .map_err(PlanError::Database)?;
    Ok(Planned { window, created })
}

/// Why a window was not cancelled.
#[derive(Debug)]
pub enum CancelError {
    /// No window with this id.
    NotFound,
    /// A peer announced it: deleting the row would last one heartbeat. It is
    /// cancelled on that peer, or by no longer accepting its announcements.
    FromPeer(String),
    /// The deletion failed.
    Database(db::Error),
}

impl std::fmt::Display for CancelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such maintenance window"),
            Self::FromPeer(peer) => write!(
                f,
                "this window was announced by peer {peer}: cancel it there, or remove \
                 accept_maintenance_secs from that peer"
            ),
            Self::Database(err) => write!(f, "cancelling the maintenance window failed: {err}"),
        }
    }
}

impl std::error::Error for CancelError {}

/// Cancel a window planned on this node, whether it has started or not, and
/// return it. The peers it was announced to drop it at the next heartbeat,
/// which no longer lists it.
///
/// # Errors
///
/// See [`CancelError`].
pub async fn cancel(store: &Store, id: i64) -> Result<MaintenanceWindow, CancelError> {
    let window = db::maintenance_window(store, id)
        .await
        .map_err(CancelError::Database)?
        .ok_or(CancelError::NotFound)?;
    if let Some(Origin::Peer(peer)) = Origin::parse(&window.origin) {
        return Err(CancelError::FromPeer(peer));
    }
    db::delete_maintenance_window(store, id)
        .await
        .map_err(CancelError::Database)?;
    Ok(window)
}

// --- Telling the peers ----------------------------------------------------

/// What this node tells its peers on a heartbeat at `now`: the windows
/// planned here that have not ended and start within a week, soonest first.
///
/// A window for this node's own host (`[health] host`) is announced with the
/// host, so a peer also mutes its monitors of the services hosted there. One
/// for named monitors, or for everything, is announced without: the peer
/// then only stops expecting this node's heartbeat. A window for another
/// host is this node's business alone.
///
/// # Errors
///
/// Returns an error if the windows cannot be read.
pub async fn announcements(
    store: &Store,
    config: &Config,
    now: i64,
) -> db::Result<Vec<AnnouncedWindow>> {
    let horizon = now.saturating_add(MAX_ANNOUNCE_AHEAD_SECS);
    let own_host = config.own_host();
    Ok(db::pending_maintenance_windows(store, now)
        .await?
        .into_iter()
        .filter(|row| row.starts_at <= horizon)
        .filter_map(|row| {
            let Reach::Peers(host) = reach(own_host, &row) else {
                return None;
            };
            Some(AnnouncedWindow {
                id: row.id,
                starts_at: row.starts_at,
                ends_at: row.ends_at,
                reason: row.reason,
                host,
            })
        })
        .take(MAX_ANNOUNCED_WINDOWS)
        .collect())
}

/// How far a stored window travels.
enum Reach {
    /// It stays on this node: a peer's own announcement is never passed on,
    /// nor is a window for another host than this node's.
    Local,
    /// It is told to the peers - for this node's host, or (`None`) without
    /// one.
    Peers(Option<String>),
}

/// Whether a stored window is told to the peers, and with which host.
fn reach(own_host: Option<&str>, row: &MaintenanceWindow) -> Reach {
    if row.origin != ORIGIN_API {
        return Reach::Local;
    }
    match Selector::parse(&row.selector) {
        Some(Selector::Host(host)) if own_host == Some(host.as_str()) => Reach::Peers(Some(host)),
        Some(Selector::All | Selector::Ids(_)) => Reach::Peers(None),
        Some(Selector::Host(_) | Selector::Peer) | None => Reach::Local,
    }
}

/// Whether this node tells its peers about `row` on its heartbeats (from a
/// week before it starts): a window planned here, for this node's own host
/// or for monitors - see [`announcements`].
#[must_use]
pub fn is_announced(config: &Config, row: &MaintenanceWindow) -> bool {
    config.peers.iter().any(|peer| peer.ping_url.is_some())
        && matches!(reach(config.own_host(), row), Reach::Peers(_))
}

/// A heartbeat's announcement could not be read at all (not JSON, or larger
/// than a heartbeat has any reason to be): nothing is applied and nothing
/// withdrawn, and the log says so, at most once an hour.
pub fn refuse_unreadable(peer: &Peer, now: i64) {
    log_refusals(
        peer,
        &["the heartbeat's body is not a readable announcement"],
        now,
    );
}

/// What became of one heartbeat's announcement on this node.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// Windows stored by this heartbeat.
    pub accepted: Vec<MaintenanceWindow>,
    /// Windows the peer no longer announces, dropped.
    pub withdrawn: Vec<MaintenanceWindow>,
    /// Why the announcement, or one of its windows, was not applied.
    pub refused: Vec<&'static str>,
}

/// Apply what `peer` announced on a heartbeat received at `now`: the list is
/// the whole of what it plans, so a window it listed before and no longer
/// does is withdrawn (a heartbeat without a list - a 0.11 node, a node with
/// nothing planned - says "none").
///
/// Nothing is applied unless the operator said to trust this peer
/// (`accept_maintenance_secs`) and the heartbeat carried its `listen_token`
/// (`authenticated`). A window is cut to the accepted length, and refused
/// when it starts more than a week ahead. What it then mutes is decided when
/// it is read ([`resolve`]): the watch of this peer, and the monitors on the
/// peer's own host when that is the host announced - a host that is not the
/// peer's is not stored at all.
///
/// Acceptances and withdrawals are logged once, as they happen; a refusal
/// repeats on every heartbeat, so it is logged once an hour.
///
/// # Errors
///
/// Returns an error if the stored windows cannot be read or written.
pub async fn apply_announcement(
    store: &Store,
    peer: &Peer,
    authenticated: bool,
    announced: &[AnnouncedWindow],
    now: i64,
) -> db::Result<Applied> {
    let mut applied = Applied::default();
    let Some(accepted_secs) = trusted_secs(peer) else {
        if !announced.is_empty() {
            applied
                .refused
                .push("accept_maintenance_secs is not set for this peer");
        }
        log_refusals(peer, &applied.refused, now);
        return Ok(applied);
    };
    if !authenticated {
        if !announced.is_empty() {
            applied
                .refused
                .push("the heartbeat did not carry the peer's listen_token");
        }
        log_refusals(peer, &applied.refused, now);
        return Ok(applied);
    }
    if announced.len() > MAX_ANNOUNCED_WINDOWS {
        applied
            .refused
            .push("more windows than a heartbeat may announce");
    }

    let origin = peer_origin(peer);
    let wanted = wanted_windows(peer, accepted_secs, announced, now, &mut applied.refused);
    // Everything this peer was granted that can still weigh on a window
    // announced now: back to a day before the earliest.
    let earliest = wanted.iter().map(|new| new.starts_at).min().unwrap_or(now);
    let granted: Vec<MaintenanceWindow> =
        db::maintenance_windows_between(store, earliest.min(now) - SECONDS_PER_DAY, i64::MAX)
            .await?
            .into_iter()
            .filter(|row| row.origin == origin)
            .collect();
    let wanted = within_budget(
        wanted,
        &granted,
        accepted_secs.saturating_mul(PEER_BUDGET_WINDOWS),
        now,
        &mut applied.refused,
    );

    let stored: Vec<&MaintenanceWindow> = granted.iter().filter(|row| row.ends_at >= now).collect();
    for row in &stored {
        if !wanted.iter().any(|new| new.is(row)) {
            // What it already muted stays on record, against the budget;
            // it mutes nothing from this second on.
            let last = now - 1;
            if row.starts_at < last {
                db::end_maintenance_window(store, row.id, last).await?;
            } else {
                db::delete_maintenance_window(store, row.id).await?;
            }
            info!(
                peer = %peer.id,
                window = row.remote_id,
                "maintenance window withdrawn by the peer"
            );
            applied.withdrawn.push((*row).clone());
        }
    }
    for new in &wanted {
        if stored.iter().any(|row| new.is(row)) {
            continue;
        }
        let (row, created) = db::insert_maintenance_window(
            store,
            NewMaintenanceWindow {
                starts_at: new.starts_at,
                ends_at: new.ends_at,
                reason: &new.reason,
                selector: &new.selector,
                origin: &origin,
                remote_id: Some(new.remote_id),
            },
        )
        .await?;
        if created {
            info!(
                peer = %peer.id,
                window = row.remote_id,
                starts = %crate::fmt::utc(row.starts_at),
                ends = %crate::fmt::utc(row.ends_at),
                covers = %row.selector,
                reason = %row.reason,
                "maintenance window accepted from the peer"
            );
            applied.accepted.push(row);
        }
    }
    log_refusals(peer, &applied.refused, now);
    Ok(applied)
}

/// What `peer`'s announcement amounts to on this node: each window as it
/// would be stored, cut to `accepted_secs`, the ones already over left out.
/// What is refused, and why, is added to `refused`.
fn wanted_windows(
    peer: &Peer,
    accepted_secs: i64,
    announced: &[AnnouncedWindow],
    now: i64,
    refused: &mut Vec<&'static str>,
) -> Vec<Wanted> {
    let mut wanted: Vec<Wanted> = Vec::new();
    for window in announced.iter().take(MAX_ANNOUNCED_WINDOWS) {
        if window.ends_at <= window.starts_at {
            refused.push("a window that ends before it starts");
            continue;
        }
        if window.starts_at > now.saturating_add(MAX_ANNOUNCE_AHEAD_SECS) {
            refused.push("a window that starts more than 7 days ahead");
            continue;
        }
        // Cut, not refused: the peer's reboot is real whatever it asked for.
        let ends_at = window
            .ends_at
            .min(window.starts_at.saturating_add(accepted_secs));
        if ends_at <= now {
            continue;
        }
        // The peer's own host, or nothing but its watch.
        let selector = match &window.host {
            Some(host) if peer.host.as_deref() == Some(host.as_str()) => {
                Selector::Host(host.clone())
            }
            Some(_) => {
                refused.push("a host that is not this peer's (only its watch is muted)");
                Selector::Peer
            }
            None => Selector::Peer,
        };
        wanted.push(Wanted {
            remote_id: window.id,
            starts_at: window.starts_at,
            ends_at,
            selector: selector.to_string(),
            reason: bounded(&window.reason, MAX_SILENCE_REASON_CHARS),
        });
    }
    wanted
}

/// Cut `wanted` to what the peer's budget still allows: `budget` seconds of
/// muted time in the 24 hours before each window starts, counting what it
/// was `granted` before (as far as it ran: a window withdrawn or replaced
/// counts until `now`) and the windows that start earlier in this same
/// announcement. Time already muted is not counted twice, so windows that
/// overlap cost what they add. A window left with nothing is refused.
fn within_budget(
    mut wanted: Vec<Wanted>,
    granted: &[MaintenanceWindow],
    budget: i64,
    now: i64,
    refused: &mut Vec<&'static str>,
) -> Vec<Wanted> {
    wanted.sort_by_key(|new| (new.starts_at, new.remote_id));
    // A window announced again is weighed afresh, whole: its stored row
    // would count its beginning twice.
    let mut muted: Vec<(i64, i64)> = granted
        .iter()
        .filter(|row| {
            !wanted
                .iter()
                .any(|new| row.remote_id == Some(new.remote_id) && row.starts_at == new.starts_at)
        })
        .map(|row| (row.starts_at, row.ends_at.min(now)))
        .filter(|(start, end)| end > start)
        .collect();
    let mut kept = Vec::with_capacity(wanted.len());
    for mut new in wanted {
        let spent = measure(&muted, new.starts_at - SECONDS_PER_DAY, new.starts_at);
        let ends_at = end_within(&muted, new.starts_at, new.ends_at, budget - spent);
        if ends_at <= new.starts_at.max(now) {
            refused.push("this peer has used its maintenance budget for 24 hours");
            continue;
        }
        new.ends_at = ends_at;
        muted.push((new.starts_at, new.ends_at));
        kept.push(new);
    }
    kept
}

/// `spans` cut to `[from, to]`, sorted and merged.
fn merged(spans: &[(i64, i64)], from: i64, to: i64) -> Vec<(i64, i64)> {
    let mut cut: Vec<(i64, i64)> = spans
        .iter()
        .map(|&(start, end)| (start.max(from), end.min(to)))
        .filter(|(start, end)| end > start)
        .collect();
    cut.sort_unstable();
    let mut out: Vec<(i64, i64)> = Vec::with_capacity(cut.len());
    for (start, end) in cut {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// How many seconds of `[from, to]` the `spans` cover.
fn measure(spans: &[(i64, i64)], from: i64, to: i64) -> i64 {
    merged(spans, from, to)
        .iter()
        .map(|(start, end)| end - start)
        .sum()
}

/// Where a window from `start` to `end` must stop to add at most
/// `allowance` seconds to what `spans` already cover.
fn end_within(spans: &[(i64, i64)], start: i64, end: i64, allowance: i64) -> i64 {
    let mut left = allowance.max(0);
    let mut at = start;
    for (covered_from, covered_to) in merged(spans, start, end) {
        let gap = covered_from - at;
        if gap > left {
            return at + left;
        }
        left -= gap;
        at = covered_to;
    }
    if end - at > left { at + left } else { end }
}

/// One announced window as this node would store it.
struct Wanted {
    remote_id: i64,
    starts_at: i64,
    ends_at: i64,
    selector: String,
    reason: String,
}

impl Wanted {
    /// Whether `row` is this window, already stored as it should be.
    fn is(&self, row: &MaintenanceWindow) -> bool {
        row.remote_id == Some(self.remote_id)
            && row.starts_at == self.starts_at
            && row.ends_at == self.ends_at
            && row.selector == self.selector
            && row.reason == self.reason
    }
}

/// How often the same refusal for the same peer is logged again.
const REFUSAL_LOG_EVERY_SECS: i64 = 3600;

/// When each (peer, refusal) was last logged, unix seconds. Bounded by the
/// configured peers times the handful of refusal reasons.
static REFUSALS_LOGGED: LazyLock<Mutex<HashMap<(String, &'static str), i64>>> =
    LazyLock::new(Mutex::default);

/// Whether a refusal for this peer is due a log line at `now`, and note that
/// it got one.
fn refusal_due(peer_id: &str, why: &'static str, now: i64) -> bool {
    let mut logged = REFUSALS_LOGGED
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    let last = logged.entry((peer_id.to_owned(), why)).or_insert(i64::MIN);
    let due = now.saturating_sub(*last) >= REFUSAL_LOG_EVERY_SECS;
    if due {
        *last = now;
    }
    due
}

/// Warn about each refusal, at most once per [`REFUSAL_LOG_EVERY_SECS`]: the
/// announcement comes back with every heartbeat.
fn log_refusals(peer: &Peer, refused: &[&'static str], now: i64) {
    for why in refused {
        if refusal_due(&peer.id, why, now) {
            warn!(
                peer = %peer.id,
                "maintenance announcement refused: {why} (logged at most once an hour)"
            );
        }
    }
}

#[cfg(test)]
mod tests;
