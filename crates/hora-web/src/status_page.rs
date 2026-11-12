//! The status page's view, built from a [`Summary`]: the hero sentence that
//! answers "is it working?" with the hour, what needs attention first, then
//! the groups. Read-only over the summary (pure presentation), so it never
//! touches the database or the summary cache.
//!
//! Two layouts. Up to [`SCALE_AT`] monitors, every group is a framed list of
//! rows, each with its daily bar. Beyond, the page stays light: groups where
//! all is well fold into one summary row (a `<details>`), and a group lists
//! at most [`ROWS_PER_GROUP`] compact rows (no bar, no chart), problems first;
//! "Show all" is the group's own page.

use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use hora_core::config::Config;

use crate::display::DisplayState;
use crate::layout::{count_word, join_and, ms, pct};
use crate::render::{day_bar, meter, owl};
use crate::summary::{MonitorView, Summary};

/// Above this many monitors the page uses its scale layout.
pub(crate) const SCALE_AT: usize = 30;
/// Compact rows a group lists on the scale layout before "Show all".
pub(crate) const ROWS_PER_GROUP: usize = 24;
/// Rows in "Needs attention" before the rest is left to the groups.
pub(crate) const ATTENTION_ROWS: usize = 50;
/// Local-only monitors detailed in the operator's "why this is not an outage".
const LOCAL_DETAILED: usize = 5;

// A template's view model: the flags are independent facts the template
// reads (layout, audience, page), not a state machine.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct StatusPage<'a> {
    pub(crate) summary: &'a Summary,
    /// The page's state: `up`, `degraded`, `down`, `maint` or `none`.
    pub(crate) state: &'static str,
    /// The owl in that state's pose (one per page).
    pub(crate) owl: String,
    /// The hour, the hero's eyebrow.
    pub(crate) when: String,
    /// The one sentence: "Everything is running."
    pub(crate) headline: String,
    /// The line under it: `sub_lead` in bold, then `sub_rest`.
    pub(crate) sub_lead: String,
    pub(crate) sub_rest: String,
    pub(crate) chips: Vec<Chip>,
    /// The operator's announcements (`[[incidents]]`, `hora announce`).
    pub(crate) announcements: Vec<Callout>,
    /// The maintenance window the hero is about, when maintenance is the state.
    pub(crate) window: Option<Window>,
    /// Active maintenance windows (when something worse leads the hero).
    pub(crate) maintenances: Vec<Callout>,
    /// The next planned window, announced ahead.
    pub(crate) next: Option<Callout>,
    /// "Not an outage": monitors only this node sees down.
    pub(crate) local: Option<LocalNotice<'a>>,
    pub(crate) attention: Vec<Row<'a>>,
    pub(crate) attention_more: usize,
    pub(crate) scale: bool,
    pub(crate) groups: Vec<GroupBlock<'a>>,
    pub(crate) total: usize,
    /// Places watching (this node and its peers).
    pub(crate) places: usize,
    /// The days in the bar (`[page].history_days`), for its legend.
    pub(crate) days: u16,
    /// `nothing` (no monitor configured), `private` (none visible without a
    /// token) or empty.
    pub(crate) empty: &'static str,
    /// A token was given but opened nothing (the private door says so).
    pub(crate) bad_token: bool,
    /// One group's page (`/status/{group}`): every row, no folding.
    pub(crate) group_page: bool,
    /// The operator's view: node names, the mesh, the verdicts.
    pub(crate) operator: bool,
    /// The watched peers (operator only: node names stay off public pages).
    pub(crate) peers: Vec<PeerRow>,
    /// The latest incidents this audience may see.
    pub(crate) recent: &'a [crate::summary::RecentIncident],
    /// A newer Hora than this build (operator only).
    pub(crate) update: Option<UpdateNotice>,
}

/// The operator's "a new Hora is out" banner.
pub(crate) struct UpdateNotice {
    pub(crate) version: String,
    pub(crate) running: &'static str,
    pub(crate) summary: String,
    /// The release's page; GitHub's own, or the list of releases.
    pub(crate) url: String,
}

impl UpdateNotice {
    fn new(update: &hora_core::updates::Update) -> Self {
        // The page comes from GitHub's answer: link it only where it belongs.
        let url = if update.url.starts_with("https://github.com/") {
            update.url.clone()
        } else {
            format!(
                "https://github.com/{}/releases",
                hora_core::updates::PROJECT
            )
        };
        Self {
            version: update.version.clone(),
            running: hora_core::updates::RUNNING,
            summary: update.summary.clone(),
            url,
        }
    }
}

/// A watched peer, for the operator's "who keeps watch".
pub(crate) struct PeerRow {
    pub(crate) name: String,
    pub(crate) state: &'static str,
    pub(crate) word: &'static str,
    pub(crate) note: String,
}

/// A count or a fact in the hero, as a chip: shape, colour, word.
pub(crate) struct Chip {
    /// `up`, `degraded`, `down`, `maint`, `none`; empty for a neutral fact.
    pub(crate) state: &'static str,
    /// The sprite icon of a neutral chip (`pin`), empty otherwise.
    pub(crate) icon: &'static str,
    pub(crate) text: String,
    /// Where the chip leads (`#attention`), if anywhere.
    pub(crate) href: Option<String>,
}

/// An announcement or a maintenance notice: an icon and a word on a wash.
pub(crate) struct Callout {
    /// `info`, `warning`, `critical` or `maint` (the wash).
    pub(crate) class: &'static str,
    /// The glyph: `info`, `degraded`, `down`, `maint`.
    pub(crate) glyph: &'static str,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) when: Option<String>,
}

/// The maintenance window in the hero, with its progress.
pub(crate) struct Window {
    pub(crate) start: String,
    pub(crate) end: String,
    pub(crate) progress: String,
    /// The same as numbers, in minutes, for the progress bar's values.
    pub(crate) done: i64,
    pub(crate) length: i64,
    pub(crate) meter: String,
}

/// Monitors down from this node only, and what the page says about them.
pub(crate) struct LocalNotice<'a> {
    pub(crate) sentence: String,
    /// For the operator: where such a down goes, as configured.
    pub(crate) routing: String,
    /// The first few, detailed for the operator.
    pub(crate) monitors: Vec<LocalDetail<'a>>,
}

/// One local-only monitor in the operator's panel: who sees what.
pub(crate) struct LocalDetail<'a> {
    pub(crate) monitor: &'a MonitorView,
    pub(crate) owls: String,
    pub(crate) places: Vec<Place>,
}

/// One place's view of a monitor: the state, and its answer time.
pub(crate) struct Place {
    pub(crate) name: String,
    /// `this Hora` or `peer`.
    pub(crate) role: &'static str,
    pub(crate) state: &'static str,
    pub(crate) word: &'static str,
    pub(crate) answer: String,
}

/// A monitor's row: shape, name, word, why, and (on small pages) its bar.
pub(crate) struct Row<'a> {
    pub(crate) m: &'a MonitorView,
    /// The monitor id, URL-encoded for `/monitor/{id}`.
    pub(crate) href_id: String,
    pub(crate) state: &'static str,
    /// Where the row sorts when problems come first (see
    /// [`DisplayState::urgency`]).
    urgency: u8,
    pub(crate) word: &'static str,
    /// After the word: `· 99.98 %` or `· 1.8 s`; empty for none.
    pub(crate) detail: String,
    /// One line of why, for anything not plainly up.
    pub(crate) why: Option<String>,
    /// The group, named on the "Needs attention" rows.
    pub(crate) group: Option<&'a str>,
    /// The daily bar (full rows only), and its words for assistive tech.
    pub(crate) bar: String,
    pub(crate) bar_label: String,
    /// Where a down was seen from ("confirmed from").
    pub(crate) seen: Vec<Place>,
    pub(crate) seen_note: String,
}

/// A display group: a framed list (small pages) or a disclosure (scale).
pub(crate) struct GroupBlock<'a> {
    /// The heading ("Other services" for ungrouped monitors).
    pub(crate) name: String,
    /// The group page, URL-encoded (`/status/{group}`); empty when ungrouped.
    pub(crate) href: String,
    pub(crate) state: &'static str,
    /// "All 4 up", "1 down · 3 up".
    pub(crate) fact: String,
    /// The fact's lead (`1 down`), shown with the state's shape.
    pub(crate) fact_lead: String,
    pub(crate) fact_rest: String,
    /// Open on arrival: something is wrong in it, or it is the group's page.
    pub(crate) open: bool,
    /// The group's daily bar (scale layout), see [`group_day_classes`].
    pub(crate) bar: String,
    pub(crate) uptime: String,
    pub(crate) rows: Vec<Row<'a>>,
    pub(crate) total: usize,
}

#[derive(Default)]
struct Counts {
    up: usize,
    degraded: usize,
    down: usize,
    maint: usize,
    none: usize,
    retrying: usize,
}

impl Counts {
    fn add(&mut self, monitor: &MonitorView) {
        match monitor.display_state() {
            DisplayState::Up => self.up += 1,
            DisplayState::Degraded => {
                self.degraded += 1;
                if monitor.retrying() {
                    self.retrying += 1;
                }
            }
            DisplayState::Down => self.down += 1,
            DisplayState::Maint => self.maint += 1,
            DisplayState::None => self.none += 1,
        }
    }

    /// The page's state: the worst shown; `none` only when nothing has data.
    fn state(&self, total: usize) -> DisplayState {
        DisplayState::of_counts(self.down, self.degraded, self.maint, self.none, total)
    }
}

/// How a page is asked for, beside its summary.
pub(crate) struct Ask {
    pub(crate) operator: bool,
    /// A newer Hora than this build, if one is out (shown to the operator).
    pub(crate) update: Option<std::sync::Arc<hora_core::updates::Update>>,
    /// A `?token=` was presented (a wrong one, if nothing is visible).
    pub(crate) token_given: bool,
    pub(crate) group_page: bool,
}

/// Build the status page's view.
// One flat assembly of the page's parts; splitting it would only scatter it.
#[allow(clippy::too_many_lines)]
pub(crate) fn build<'a>(summary: &'a Summary, config: &Config, ask: &Ask) -> StatusPage<'a> {
    let now = Utc::now();
    let total = summary.monitors.len();
    let mut counts = Counts::default();
    for monitor in &summary.monitors {
        counts.add(monitor);
    }
    let state = counts.state(total);
    let scale = total > SCALE_AT;
    let places = summary.places();

    let local_monitors: Vec<&MonitorView> = summary
        .monitors
        .iter()
        .map(AsRef::as_ref)
        .filter(|monitor| monitor.local_only() && monitor.maintenance.is_none())
        .collect();

    let windows = active_windows(summary, now);
    let window = (state == DisplayState::Maint)
        .then(|| {
            windows
                .first()
                .map(|(start, end)| hero_window(*start, *end, now))
        })
        .flatten();

    let (headline, sub_lead, sub_rest) =
        sentence(summary, &counts, total, scale, places, &windows, now);

    let attention_all: Vec<Row> = attention_rows(summary);
    let attention_more = attention_all.len().saturating_sub(ATTENTION_ROWS);
    let attention = attention_all.into_iter().take(ATTENTION_ROWS).collect();

    let empty = if total > 0 {
        ""
    } else if config.monitors.is_empty() {
        "nothing"
    } else {
        "private"
    };

    StatusPage {
        summary,
        state: state.class(),
        owl: owl(state.class(), 96),
        when: summary.when(),
        headline,
        sub_lead,
        sub_rest,
        chips: chips(&counts, &local_monitors, summary, now),
        announcements: summary
            .incidents
            .iter()
            .map(|incident| {
                let (class, glyph) = match incident.severity {
                    "critical" => ("critical", "down"),
                    "warning" => ("warning", "degraded"),
                    _ => ("info", "info"),
                };
                Callout {
                    class,
                    glyph,
                    title: incident.title.clone(),
                    body: incident.body.clone(),
                    when: incident.at.as_ref().map(|at| format!("Posted {at}")),
                }
            })
            .collect(),
        window,
        maintenances: if state == DisplayState::Maint {
            Vec::new()
        } else {
            summary
                .maintenances
                .iter()
                .map(|window| Callout {
                    class: "maint",
                    glyph: "maint",
                    title: "Planned maintenance".to_owned(),
                    body: if window.reason.is_empty() {
                        format!("Affects {}.", window.monitors)
                    } else {
                        format!("{}. Affects {}.", window.reason, window.monitors)
                    },
                    when: window.note.clone(),
                })
                .collect()
        },
        next: next_window(summary, now).map(|window| Callout {
            class: "info",
            glyph: "info",
            title: "Next".to_owned(),
            body: format!(
                "{} to {}: {}.",
                window.start.format("%A %-d %B, %H:%M"),
                if window.end.date_naive() == window.start.date_naive() {
                    window.end.format("%H:%M UTC").to_string()
                } else {
                    window.end.format("%A %-d %B, %H:%M UTC").to_string()
                },
                if window.reason.is_empty() {
                    "planned maintenance"
                } else {
                    &window.reason
                }
            ),
            when: window.note.clone(),
        }),
        local: local_notice(&local_monitors, config),
        attention,
        attention_more,
        scale,
        groups: summary
            .groups
            .iter()
            .map(|group| group_block(&group.name, &group.monitors, scale, ask.group_page))
            .collect(),
        total,
        places,
        days: config.page.history_days,
        empty,
        bad_token: ask.token_given && empty == "private",
        group_page: ask.group_page,
        update: ask
            .update
            .as_deref()
            .filter(|_| ask.operator)
            .map(UpdateNotice::new),
        operator: ask.operator,
        peers: if ask.operator {
            summary
                .peers
                .iter()
                .map(|peer| {
                    let state = DisplayState::from(peer.status);
                    let mut note = peer.last_seen.as_ref().map_or_else(
                        || "never heard from".to_owned(),
                        |seen| format!("last heartbeat {seen}"),
                    );
                    if peer.pings {
                        note.push_str(" · this node heartbeats it too");
                    }
                    PeerRow {
                        name: peer.name.clone(),
                        state: state.class(),
                        word: match state {
                            DisplayState::Up => "Well",
                            DisplayState::Down => "Silent",
                            DisplayState::Degraded | DisplayState::Maint | DisplayState::None => {
                                "Not heard yet"
                            }
                        },
                        note,
                    }
                })
                .collect()
        } else {
            Vec::new()
        },
        recent: &summary.recent,
    }
}

/// The hero's words alone, for the plain-text page: "Everything is running.
/// All 14 services are up."
pub(crate) fn summary_sentence(summary: &Summary) -> String {
    let total = summary.monitors.len();
    let mut counts = Counts::default();
    for monitor in &summary.monitors {
        counts.add(monitor);
    }
    let (headline, lead, rest) = sentence(
        summary,
        &counts,
        total,
        total > SCALE_AT,
        summary.places(),
        &[],
        Utc::now(),
    );
    format!("{headline} {lead}{rest}")
}

/// The active maintenance windows the page is about, soonest end first.
fn active_windows(summary: &Summary, now: DateTime<Utc>) -> Vec<(DateTime<Utc>, DateTime<Utc>)> {
    // The summary holds the windows this page may announce (a group page
    // drops other groups'), each with its own hours. One may have ended
    // since the summary was built.
    let mut windows: Vec<(DateTime<Utc>, DateTime<Utc>)> = summary
        .maintenances
        .iter()
        .filter(|window| now >= window.start && now <= window.end)
        .map(|window| (window.start, window.end))
        .collect();
    windows.sort_by_key(|(_, end)| *end);
    windows
}

/// The next window that has not started, within a week: a config window, or
/// one planned at runtime (through the API, or announced by a peer). The
/// summary holds the ones this page may announce; one may have started
/// since it was built.
fn next_window(summary: &Summary, now: DateTime<Utc>) -> Option<&crate::summary::MaintenanceView> {
    summary
        .upcoming
        .iter()
        .filter(|window| {
            window.start > now
                && window.start <= now + chrono::TimeDelta::days(crate::summary::UPCOMING_DAYS)
        })
        .min_by_key(|window| window.start)
}

fn hero_window(start: DateTime<Utc>, end: DateTime<Utc>, now: DateTime<Utc>) -> Window {
    let length = (end - start).num_minutes().max(1);
    let done = (now - start).num_minutes().clamp(0, length);
    Window {
        start: start.format("%H:%M").to_string(),
        end: end.format("%H:%M").to_string(),
        progress: format!(
            "{} of {}",
            crate::layout::minutes(done),
            crate::layout::minutes(length)
        ),
        done,
        length,
        meter: meter(done, length, "meter maint"),
    }
}

/// The hero's sentence and the line under it.
// The voice's cases side by side (all up, down, slow, maintenance, waiting):
// easier to read and to reword as one function.
#[allow(clippy::too_many_lines)]
fn sentence(
    summary: &Summary,
    counts: &Counts,
    total: usize,
    scale: bool,
    places: usize,
    windows: &[(DateTime<Utc>, DateTime<Utc>)],
    now: DateTime<Utc>,
) -> (String, String, String) {
    let names = |state: DisplayState| -> Vec<&str> {
        summary
            .monitors
            .iter()
            .filter(|monitor| monitor.display_state() == state)
            .map(|monitor| monitor.name.as_str())
            .collect()
    };
    let is_are = |n: usize| if n == 1 { "is" } else { "are" };
    let problems = counts.down + counts.degraded;
    // At scale, a handful of problems among hundreds is "almost everything".
    let almost = scale && problems * 50 <= total;

    let headline = if total > 0 && counts.none == total {
        "Waiting for the first checks.".to_owned()
    } else if counts.down > 0 {
        if almost {
            "Almost everything is running.".to_owned()
        } else if counts.down <= 3 {
            let down = names(DisplayState::Down);
            format!("{} {} down.", join_and(&down), is_are(down.len()))
        } else if counts.down * 2 < total {
            "Some services are down.".to_owned()
        } else {
            "Most services are down.".to_owned()
        }
    } else if counts.degraded > 0 {
        let trouble = if counts.retrying > 0 {
            "having trouble"
        } else {
            "slow"
        };
        if almost {
            "Almost everything is running.".to_owned()
        } else if counts.degraded <= 3 {
            let slow = names(DisplayState::Degraded);
            format!("{} {} {trouble}.", join_and(&slow), is_are(slow.len()))
        } else {
            format!("Some services are {trouble}.")
        }
    } else if counts.maint > 0 {
        match windows.first() {
            Some((_, end)) if end.date_naive() == now.date_naive() => {
                format!("Planned maintenance, until {}.", end.format("%H:%M"))
            }
            Some((_, end)) => format!(
                "Planned maintenance, until {}.",
                end.format("%a %-d %b, %H:%M")
            ),
            None => "Planned maintenance.".to_owned(),
        }
    } else {
        "Everything is running.".to_owned()
    };

    let mut rest = String::new();
    let lead = if problems == 0 && counts.maint == 0 && counts.none == 0 {
        rest.push_str(if total == 1 { " is up." } else { " are up." });
        if total == 1 {
            "The one service".to_owned()
        } else {
            format!("All {total} services")
        }
    } else {
        rest.push_str(if total == 1 {
            " service is up."
        } else {
            " services are up."
        });
        let mut parts = Vec::new();
        if counts.down > 0 {
            parts.push(format!(
                "{} {} down",
                count_word(counts.down),
                is_are(counts.down)
            ));
        }
        if counts.degraded > 0 {
            let slow = counts.degraded - counts.retrying;
            if slow > 0 {
                parts.push(format!("{} {} slow", count_word(slow), is_are(slow)));
            }
            if counts.retrying > 0 {
                parts.push(format!(
                    "{} {} being checked again",
                    count_word(counts.retrying),
                    is_are(counts.retrying)
                ));
            }
        }
        if counts.maint > 0 {
            parts.push(format!(
                "{} {} in planned maintenance",
                count_word(counts.maint),
                is_are(counts.maint)
            ));
        }
        if counts.none > 0 && counts.none < total {
            parts.push(format!(
                "{} {} waiting for its first check",
                count_word(counts.none),
                is_are(counts.none)
            ));
        }
        if !parts.is_empty() {
            let joined = join_and(&parts);
            let mut chars = joined.chars();
            let capitalised: String = chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default();
            rest.push(' ');
            rest.push_str(&capitalised);
            rest.push_str(if counts.up > 0 {
                "; the rest is fine."
            } else {
                "."
            });
        }
        format!("{} of {total}", counts.up)
    };
    if places > 1 {
        let _ = write!(rest, " Checked from {places} places.");
    }
    (headline, lead, rest)
}

fn chips(
    counts: &Counts,
    local: &[&MonitorView],
    summary: &Summary,
    now: DateTime<Utc>,
) -> Vec<Chip> {
    let quiet_days = summary.quiet_days;
    let mut chips = Vec::new();
    let mut add = |state: DisplayState, n: usize, text: &str, href: Option<&str>| {
        if n > 0 {
            chips.push(Chip {
                state: state.class(),
                icon: "",
                text: format!("{n} {text}"),
                href: href.map(str::to_owned),
            });
        }
    };
    add(DisplayState::Down, counts.down, "down", Some("#attention"));
    add(
        DisplayState::Degraded,
        counts.degraded,
        if counts.retrying == counts.degraded {
            "retrying"
        } else {
            "slow"
        },
        Some("#attention"),
    );
    add(DisplayState::Maint, counts.maint, "in maintenance", None);
    add(DisplayState::None, counts.none, "waiting", None);
    add(DisplayState::Up, counts.up, "up", None);
    if !local.is_empty() {
        chips.push(Chip {
            state: "",
            icon: "pin",
            text: "One checkpoint: network issue".to_owned(),
            href: Some("#local".to_owned()),
        });
    }
    // A calm fact, only while nothing is down (an open incident clears it).
    if counts.down == 0
        && let Some(days) = quiet_days
    {
        chips.push(Chip {
            state: "",
            icon: "",
            text: format!(
                "No incident in {days} day{}",
                if days == 1 { "" } else { "s" }
            ),
            href: None,
        });
    }
    if counts.maint == 0
        && let Some(next) = next_window(summary, now)
    {
        chips.push(Chip {
            state: DisplayState::Maint.class(),
            icon: "",
            text: format!("Planned: {}", next.start.format("%a %-d %b, %H:%M UTC")),
            href: Some("#next".to_owned()),
        });
    }
    chips
}

/// "Not an outage": what the page says when only this node sees some
/// monitors down. Public visitors read one calm sentence; the operator also
/// gets who saw what.
fn local_notice<'a>(
    local: &[&'a MonitorView],
    config: &hora_core::config::Config,
) -> Option<LocalNotice<'a>> {
    let first = local.first()?;
    let sentence = if local.len() == 1 {
        let others: Vec<&str> = first.vantages.iter().map(|v| v.peer.as_str()).collect();
        format!(
            "One of our checkpoints can't reach {}; {} {} it normally, so the problem is on the \
             road from that checkpoint, not in the service.",
            first.name,
            join_and(&others),
            if others.len() == 1 {
                "reaches"
            } else {
                "reach"
            },
        )
    } else {
        format!(
            "One of our checkpoints can't reach {} services; the other places reach them \
             normally, so the problem is on the road from that checkpoint, not in the services.",
            local.len()
        )
    };
    let monitors = local
        .iter()
        .take(LOCAL_DETAILED)
        .map(|monitor| {
            let mut places = vec![Place {
                name: "This Hora".to_owned(),
                role: "this node",
                state: DisplayState::Down.class(),
                word: DisplayState::Down.word(),
                answer: monitor
                    .last_error
                    .clone()
                    .unwrap_or_else(|| "no answer".to_owned()),
            }];
            places.extend(monitor.vantages.iter().map(|vantage| {
                let state = DisplayState::from(vantage.status);
                Place {
                    name: vantage.peer.clone(),
                    role: "peer",
                    state: state.class(),
                    word: state.word(),
                    answer: vantage.p50_ms.map_or_else(|| "-".to_owned(), ms),
                }
            }));
            let pictured: Vec<(&str, &str)> = places
                .iter()
                .map(|place| (place.state, place.name.as_str()))
                .collect();
            let owls = crate::render::owls(
                &pictured,
                &format!(
                    "Only this Hora sees {} down; the other places see it up.",
                    monitor.name
                ),
            );
            LocalDetail {
                monitor,
                owls,
                places,
            }
        })
        .collect();
    Some(LocalNotice {
        sentence,
        routing: local_routing(config),
        monitors,
    })
}

/// What happens to a down seen from this node only, as configured - the
/// operator panel's footnote.
fn local_routing(config: &hora_core::config::Config) -> String {
    const LEAD: &str = "This node asks its peers before it alerts. A down seen from here only, \
        while every peer that answers sees the service up,";
    const QUIET: &str = "does not page the usual channels:";
    const TAIL: &str = "The peers are asked again every 5 minutes; once they see it down too, \
        or stop answering, the usual alert goes out.";
    match config.alerts.notify_unconfirmed.as_deref() {
        // The default never lets a peer's answer silence a down on its own.
        None => format!(
            "{LEAD} still goes to the monitor's channels, marked as seen from here only (set \
             alerts.notify_unconfirmed to send it to a quiet channel instead). {TAIL}"
        ),
        Some([]) => format!(
            "{LEAD} {QUIET} it is only recorded (alerts.notify_unconfirmed is empty). {TAIL}"
        ),
        Some(quiet) => format!(
            "{LEAD} {QUIET} it goes to {} instead (alerts.notify_unconfirmed). {TAIL}",
            join_and(quiet)
        ),
    }
}

/// The monitors that are not plainly up, worst first, each with its group.
fn attention_rows(summary: &Summary) -> Vec<Row<'_>> {
    let mut rows: Vec<Row> = summary
        .monitors
        .iter()
        .map(AsRef::as_ref)
        .filter(|monitor| monitor.display_state().notable())
        .map(|monitor| {
            let mut row = row(monitor, false);
            row.group = monitor.group.as_deref();
            row
        })
        .collect();
    rows.sort_by(|a, b| {
        a.urgency
            .cmp(&b.urgency)
            .then_with(|| a.m.name.cmp(&b.m.name))
    });
    rows
}

/// One monitor's row. `with_bar` draws its daily bar (small pages).
fn row(monitor: &MonitorView, with_bar: bool) -> Row<'_> {
    let state = monitor.display_state();
    let now = Utc::now().timestamp();
    let detail = match state {
        // Up over the bar's days, the figure its bar shows.
        DisplayState::Up | DisplayState::Maint => monitor
            .uptime_long_permille
            .or(monitor.uptime_permille)
            .map(pct)
            .unwrap_or_default(),
        DisplayState::Down => crate::monitor_page::down_for(monitor, now).map_or_else(
            || monitor.last_latency_ms.map(ms).unwrap_or_default(),
            crate::layout::minutes,
        ),
        DisplayState::Degraded => monitor.last_latency_ms.map(ms).unwrap_or_default(),
        DisplayState::None => String::new(),
    };
    let (seen, seen_note) = seen_from(monitor);
    Row {
        m: monitor,
        href_id: hora_core::fmt::percent_encode(&monitor.id),
        state: state.class(),
        urgency: state.urgency(),
        word: monitor.word(),
        detail,
        why: why(monitor),
        group: None,
        bar: if with_bar {
            day_bar(&day_classes(monitor))
        } else {
            String::new()
        },
        bar_label: bar_label(monitor),
        seen,
        seen_note,
    }
}

/// One line of why, in the visitor's words.
fn why(monitor: &MonitorView) -> Option<String> {
    let state = monitor.display_state();
    if monitor.local_only() {
        let others: Vec<&str> = monitor.vantages.iter().map(|v| v.peer.as_str()).collect();
        return Some(format!(
            "Up from {}. One checkpoint can't reach it: a network problem near that checkpoint.",
            join_and(&others)
        ));
    }
    let mut parts = Vec::new();
    match state {
        DisplayState::Maint => {
            let title = monitor.maintenance.as_deref().unwrap_or_default();
            parts.push(if title.is_empty() {
                "Planned maintenance.".to_owned()
            } else {
                format!("Planned maintenance: {title}.")
            });
        }
        DisplayState::Down => {
            if let Some(since) = crate::monitor_page::since_line(monitor, Utc::now().timestamp()) {
                parts.push(since);
            }
            if let Some(error) = &monitor.last_error {
                parts.push(format!("Last answer: {error}."));
            }
        }
        DisplayState::Degraded => {
            match (&monitor.last_error, monitor.last_latency_ms, monitor.p50_ms) {
                (Some(error), _, _) => parts.push(format!(
                    "The last check failed ({error}); checking again before calling it down."
                )),
                // Say the limit it crossed: a 4 ms answer reads "slow" only next
                // to the 1 ms the operator set.
                (None, Some(latency), _)
                    if monitor.slow_over_ms.is_some_and(|limit| latency > limit) =>
                {
                    parts.push(format!(
                        "Answers take {}, over its {} limit.",
                        ms(latency),
                        ms(monitor.slow_over_ms.unwrap_or_default())
                    ));
                }
                (None, Some(latency), Some(p50)) if p50 < latency => parts.push(format!(
                    "Answers take {} (usually {}).",
                    ms(latency),
                    ms(p50)
                )),
                (None, Some(latency), _) => parts.push(format!("Answers take {}.", ms(latency))),
                _ => {}
            }
        }
        DisplayState::None => parts.push("Waiting for the first check.".to_owned()),
        DisplayState::Up => {}
    }
    if let Some(cause) = &monitor.cause {
        parts.push(format!("Probably caused by {cause}."));
    }
    if !monitor.impacted.is_empty() {
        parts.push(format!(
            "{} {} on it.",
            join_and(&monitor.impacted),
            if monitor.impacted.len() == 1 {
                "depends"
            } else {
                "depend"
            }
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Where a down monitor was seen from: this node and each peer, and what
/// the agreement means. Empty without peers.
fn seen_from(monitor: &MonitorView) -> (Vec<Place>, String) {
    if monitor.vantages.is_empty() || monitor.display_state() != DisplayState::Down {
        return (Vec::new(), String::new());
    }
    let mut places = vec![Place {
        name: "Here".to_owned(),
        role: "this node",
        state: DisplayState::Down.class(),
        word: DisplayState::Down.word(),
        answer: String::new(),
    }];
    places.extend(monitor.vantages.iter().map(|vantage| {
        let state = DisplayState::from(vantage.status);
        Place {
            name: vantage.peer.clone(),
            role: "peer",
            state: state.class(),
            word: state.word(),
            answer: String::new(),
        }
    }));
    // This node, and every peer that sees it down too.
    let down = 1 + monitor
        .vantages
        .iter()
        .filter(|vantage| DisplayState::from(vantage.status) == DisplayState::Down)
        .count();
    let note = if down == places.len() {
        format!("{down} of {down} places: not a local problem")
    } else {
        format!("Seen down from {down} of {} places", places.len())
    };
    (places, note)
}

/// The monitor's daily bar classes, oldest first. A day a planned window
/// touched is maintenance unless it still was an outage; maintenance now
/// marks today.
fn day_classes(monitor: &MonitorView) -> Vec<u8> {
    let last = monitor.bar.len().saturating_sub(1);
    monitor
        .bar
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let now = index == last && monitor.maintenance.is_some();
            crate::summary::bar_class(cell.state, cell.maint, now)
        })
        .collect()
}

/// Below this mean availability over a day (per mille), a group's day reads
/// slow. A group's bar summarises many services: one failed check among 45
/// that day is the services' own bars' business, not the group's.
const GROUP_DAY_SLOW_BELOW_PERMILLE: i64 = 999;

/// A group's daily bar, one class per day: an outage (`x`) when any of its
/// services had a real outage that day (see `DAY_OUTAGE_BELOW_PERMILLE`), slow
/// (`d`) when the group's mean availability fell under 99.9 %, maintenance
/// (`m`) when a service was in a planned window and nothing went down, up
/// (`u`) otherwise, and no data (`n`) when none of them had any. Taking each
/// day's worst service instead painted a large group's bar slow every day.
fn group_day_classes(monitors: &[std::sync::Arc<MonitorView>]) -> Vec<u8> {
    let days: Vec<(Vec<u8>, Vec<Option<i64>>)> = monitors
        .iter()
        .map(|m| {
            (
                day_classes(m),
                m.bar.iter().map(|cell| cell.permille).collect(),
            )
        })
        .collect();
    fold_group_days(&days)
}

/// [`group_day_classes`] on each service's day classes and availabilities
/// (per mille), oldest day first; bars are aligned on today.
fn fold_group_days(services: &[(Vec<u8>, Vec<Option<i64>>)]) -> Vec<u8> {
    let length = services
        .iter()
        .map(|(days, _)| days.len())
        .max()
        .unwrap_or(0);
    (0..length)
        .map(|day| {
            let mut any = false;
            let mut outage = false;
            let mut maint = false;
            let (mut sum, mut measured) = (0_i64, 0_i64);
            for (days, permilles) in services {
                // A shorter bar (a newer service) starts later.
                let Some(index) = (day + days.len()).checked_sub(length) else {
                    continue;
                };
                match days[index] {
                    b'x' => outage = true,
                    b'm' => maint = true,
                    b'n' => continue,
                    _ => {}
                }
                any = true;
                if let Some(permille) = permilles.get(index).copied().flatten() {
                    sum += permille;
                    measured += 1;
                }
            }
            if outage {
                b'x'
            } else if measured > 0 && sum < GROUP_DAY_SLOW_BELOW_PERMILLE * measured {
                b'd'
            } else if maint {
                b'm'
            } else if any {
                b'u'
            } else {
                b'n'
            }
        })
        .collect()
}

/// The bar's words, read after the service's name in its link: the period,
/// the outages, the slow days.
fn bar_label(monitor: &MonitorView) -> String {
    let outages = monitor
        .bar
        .iter()
        .filter(|cell| cell.state == "down")
        .count();
    let slow = monitor
        .bar
        .iter()
        .filter(|cell| cell.state == "degraded")
        .count();
    let mut label = match monitor.uptime_long_permille {
        Some(permille) => format!("{} up over {} days, ", pct(permille), monitor.bar.len()),
        None => format!("Last {} days: ", monitor.bar.len()),
    };
    if outages == 0 && slow == 0 {
        label.push_str("no incident");
    } else {
        let mut parts = Vec::new();
        if outages > 0 {
            parts.push(format!(
                "{outages} day{} with an outage",
                if outages == 1 { "" } else { "s" }
            ));
        }
        if slow > 0 {
            parts.push(format!(
                "{slow} slow day{}",
                if slow == 1 { "" } else { "s" }
            ));
        }
        label.push_str(&join_and(&parts));
    }
    label.push_str(". Open its history");
    label
}

fn group_block<'a>(
    name: &str,
    monitors: &'a [std::sync::Arc<MonitorView>],
    scale: bool,
    group_page: bool,
) -> GroupBlock<'a> {
    let mut counts = Counts::default();
    for monitor in monitors {
        counts.add(monitor);
    }
    let total = monitors.len();
    let state = counts.state(total);

    let (fact_lead, fact_rest) = if counts.up == total {
        (format!("All {total} up"), String::new())
    } else {
        let mut lead = Vec::new();
        if counts.down > 0 {
            lead.push(format!("{} down", counts.down));
        }
        if counts.degraded > 0 {
            lead.push(format!(
                "{} {}",
                counts.degraded,
                if counts.retrying == counts.degraded {
                    "retrying"
                } else {
                    "slow"
                }
            ));
        }
        if counts.maint > 0 {
            lead.push(format!("{} in maintenance", counts.maint));
        }
        if counts.none > 0 {
            lead.push(format!("{} waiting", counts.none));
        }
        let rest = if counts.up > 0 {
            format!("{} up", counts.up)
        } else {
            String::new()
        };
        (lead.join(" · "), rest)
    };
    let fact = if fact_rest.is_empty() {
        fact_lead.clone()
    } else {
        format!("{fact_lead} · {fact_rest}")
    };

    // Small pages: every row with its bar. Scale: compact rows, problems
    // first then A to Z, at most a page of them unless this is the group's
    // own page (or the ungrouped monitors, which have no page of their own).
    let compact = scale || (group_page && total > SCALE_AT);
    let mut rows: Vec<Row> = monitors
        .iter()
        .map(|monitor| row(monitor, !compact))
        .collect();
    if compact {
        rows.sort_by(|a, b| {
            a.urgency
                .cmp(&b.urgency)
                .then_with(|| a.m.name.cmp(&b.m.name))
        });
        if !group_page && !name.is_empty() {
            rows.truncate(ROWS_PER_GROUP);
        }
    }

    let bar = if scale {
        day_bar(&group_day_classes(monitors))
    } else {
        String::new()
    };
    let measured: Vec<i64> = monitors.iter().filter_map(|m| m.uptime_permille).collect();
    let uptime = if measured.is_empty() {
        String::new()
    } else {
        let average = measured.iter().sum::<i64>() / i64::try_from(measured.len()).unwrap_or(1);
        format!("{} over 24 h", pct(average))
    };

    GroupBlock {
        name: if name.is_empty() {
            "Other services".to_owned()
        } else {
            name.to_owned()
        },
        href: if name.is_empty() {
            String::new()
        } else {
            format!("/status/{}", hora_core::fmt::percent_encode(name))
        },
        state: state.class(),
        fact,
        fact_lead,
        fact_rest,
        open: group_page || !matches!(state, DisplayState::Up | DisplayState::None),
        bar,
        uptime,
        rows,
        total,
    }
}

#[cfg(test)]
mod group_bar_tests {
    use super::fold_group_days;

    #[test]
    fn a_large_group_is_not_slow_for_one_service_failed_check() {
        // 45 services; one had a few failed checks (a slow day for its own
        // bar, 99.3 %), the others a clean day: the group reads up.
        let mut services = vec![(vec![b'u'], vec![Some(1000)]); 44];
        services.push((vec![b'd'], vec![Some(993)]));
        assert_eq!(fold_group_days(&services), vec![b'u']);
    }

    #[test]
    fn a_real_outage_or_a_low_mean_still_shows() {
        // One service under 90 % that day: the group shows the outage.
        let outage = vec![
            (vec![b'u'], vec![Some(1000)]),
            (vec![b'x'], vec![Some(400)]),
        ];
        assert_eq!(fold_group_days(&outage), vec![b'x']);
        // Two services slow most of the day: the mean falls under 99.9 %.
        let slow = vec![(vec![b'd'], vec![Some(950)]), (vec![b'd'], vec![Some(960)])];
        assert_eq!(fold_group_days(&slow), vec![b'd']);
    }

    #[test]
    fn maintenance_newer_services_and_empty_days() {
        // Today: one service in a planned window, nothing down. Yesterday: only
        // the older service existed. The day before: no data at all.
        let services = vec![
            (vec![b'n', b'u', b'm'], vec![None, Some(1000), Some(1000)]),
            (vec![b'u'], vec![Some(1000)]),
        ];
        assert_eq!(fold_group_days(&services), vec![b'n', b'u', b'm']);
    }
}

#[cfg(test)]
mod local_routing_tests {
    use super::local_routing;

    #[test]
    fn the_footnote_says_where_a_local_only_down_goes() {
        let config = |alerts: &str| {
            hora_core::config::parse(&format!(
                "[page]\n[server]\n[alerts]\n{alerts}\n\
                 [[channels]]\nname = \"low\"\ntype = \"webhook\"\nurl = \"https://example.com/h\"\n\
                 [[monitors]]\nid = \"web\"\nname = \"Web\"\n\
                 target = \"https://example.com\"\ninterval_secs = 60\n"
            ))
            .expect("config")
        };
        // Unset: never silent.
        let unset = local_routing(&config(""));
        assert!(
            unset.contains("still goes to the monitor's channels"),
            "{unset}"
        );
        assert!(!unset.contains("does not page"), "{unset}");
        let quiet = local_routing(&config("notify_unconfirmed = [\"low\"]"));
        assert!(
            quiet.contains("does not page the usual channels: it goes to low instead"),
            "{quiet}"
        );
        let silent = local_routing(&config("notify_unconfirmed = []"));
        assert!(
            silent.contains("does not page the usual channels: it is only recorded"),
            "{silent}"
        );
    }
}

#[cfg(test)]
mod maintenance_window_tests {
    use std::collections::HashMap;

    use chrono::{TimeDelta, Utc};

    use super::{Ask, build};
    use crate::summary::{BuildState, build_summary, for_group};

    /// Two windows with the same title, one per group, running at once. The
    /// hero used to find its window in the config by that title, so a
    /// group's page took whichever ended first: the other group's hours.
    #[tokio::test]
    async fn a_group_page_shows_its_own_windows_hours() {
        let now = Utc::now();
        let at = |hours: i64| {
            (now + TimeDelta::hours(hours))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        };
        let config = hora_core::config::parse(&format!(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "api"
            name = "API"
            group = "Alpha"
            target = "https://alpha.example.com"
            interval_secs = 60
            [[monitors]]
            id = "db"
            name = "Database"
            group = "Beta"
            target = "https://beta.example.com"
            interval_secs = 60
            [[maintenance]]
            title = "Upgrade"
            start = "{}"
            end = "{}"
            monitors = ["api"]
            [[maintenance]]
            title = "Upgrade"
            start = "{}"
            end = "{}"
            monitors = ["db"]
            "#,
            at(-1),
            at(5),
            at(-2),
            at(1),
        ))
        .expect("config");
        let store = hora_core::db::Store::in_memory().await;
        let built = build_summary(
            &store,
            &config,
            &mut BuildState::default(),
            &[],
            &HashMap::new(),
        )
        .await;
        let ask = Ask {
            operator: false,
            update: None,
            token_given: false,
            group_page: true,
        };

        // Alpha's window lasts six hours and ends in five; Beta's, three and one.
        let alpha = for_group(&built.summary, &config, "Alpha").expect("group");
        let page = build(&alpha, &config, &ask);
        let window = page.window.expect("the hero is the maintenance window");
        assert_eq!(window.length, 6 * 60);
        assert_eq!(
            window.end,
            (now + TimeDelta::hours(5)).format("%H:%M").to_string()
        );

        let beta = for_group(&built.summary, &config, "Beta").expect("group");
        let page = build(&beta, &config, &ask);
        assert_eq!(page.window.expect("window").length, 3 * 60);
    }
}
