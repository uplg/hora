//! Operator annotations from the shell: announcements, event markers,
//! silences and planned maintenance.

use hora_core::announce::{self, Announcement};
use hora_core::silence::SilenceError;
use hora_core::{MAX_EVENT_TITLE_CHARS, fmt};

use super::{CliError, open_database, unknown_monitor, usage};

/// `hora announce`: pin (or list/clear) a public banner on the status page -
/// the communication side of incidents, written straight into the daemon's
/// database and live within the summary cache TTL (~5s). The HTTP twin is
/// `POST /api/announce`. A leading `--` ends the subcommand words, so a banner
/// titled "clear skies" is `hora announce -- clear skies`.
pub(crate) async fn announce(args: &[String]) -> Result<(), CliError> {
    match args.first().map(String::as_str) {
        Some("--") if args.len() > 1 => pin_announcement(&args[1..]).await?,
        Some("list") => {
            let (_, store) = open_database().await?;
            let now = chrono::Utc::now().timestamp();
            let pinned = hora_core::db::active_announcements(&store, now).await?;
            if pinned.is_empty() {
                println!("No announcements pinned.");
            }
            for item in pinned {
                let until = item.until.map_or_else(
                    || "until cleared".to_owned(),
                    |ts| format!("until {}", fmt::utc(ts)),
                );
                println!("#{} [{}] {} ({until})", item.id, item.severity, item.title);
                if !item.body.is_empty() {
                    println!("      {}", item.body);
                }
            }
        }
        Some("clear") => {
            let (_, store) = open_database().await?;
            let cleared =
                hora_core::db::clear_announcements(&store, chrono::Utc::now().timestamp()).await?;
            println!("Cleared {cleared} announcement(s).");
        }
        Some(first) if first != "--" => pin_announcement(args).await?,
        _ => {
            return Err(usage(concat!(
                "Usage: hora announce <title> [body...] [--severity info|warning|critical|resolved] [--until 4h|18:00]\n",
                "       hora announce list\n",
                "       hora announce clear",
            )));
        }
    }
    Ok(())
}

async fn pin_announcement(args: &[String]) -> anyhow::Result<()> {
    let announcement = parse_announce_args(args, chrono::Utc::now().timestamp())?;
    let (_, store) = open_database().await?;
    announcement.pin(&store).await?;
    let expiry = announcement.until.map_or_else(
        || "until `hora announce clear`".to_owned(),
        |ts| format!("until {}", fmt::utc(ts)),
    );
    println!(
        "Pinned [{}] {:?} ({expiry}).",
        announcement.severity, announcement.title
    );
    Ok(())
}

/// Split `hora announce` arguments: `--severity`/`--until` flags anywhere
/// (see [`announce::split_flags`]), the first free word is the title, the rest
/// joins into the body.
pub(super) fn parse_announce_args(args: &[String], now: i64) -> anyhow::Result<Announcement> {
    let flags = announce::split_flags(args.iter().map(String::as_str))?;
    let Some((title, body)) = flags.words.split_first() else {
        anyhow::bail!("announce needs a title");
    };
    let until = flags.until.and_then(|raw| announce::parse_until(raw, now));
    Ok(Announcement::new(
        title,
        &body.join(" "),
        flags.severity.unwrap_or_default(),
        until,
    )?)
}

/// `hora event <title...>` / `hora event list [N]`: record (or list) an event
/// marker - "deploy api v2.3" - the same gesture as a silence, written
/// straight into the daemon's database. Markers overlay the latency
/// sparklines, list on /history, and a down confirming within the hour is
/// annotated "recent change: ...". The HTTP twin is `POST /api/event`.
pub(crate) async fn event(args: &[String]) -> Result<(), CliError> {
    match parse_event_args(args)? {
        EventCommand::List(limit) => {
            let (_, store) = open_database().await?;
            let events = hora_core::db::recent_events(&store, limit).await?;
            if events.is_empty() {
                println!("No events recorded.");
            }
            for event in events {
                println!("{}  {}", fmt::utc(event.created_at), event.title);
            }
        }
        EventCommand::Record(title) => {
            let (_, store) = open_database().await?;
            hora_core::db::insert_event(&store, &title).await?;
            println!("Recorded event: {title}");
        }
    }
    Ok(())
}

/// What `hora event` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum EventCommand {
    /// `list [limit]`.
    List(i64),
    /// Record a marker with this title (trimmed, at most 200 chars).
    Record(String),
}

/// Parse `hora event` arguments. `list` is the subcommand only when followed
/// by nothing or a number: `hora event list deploy` is a usage error that
/// points at `--` rather than guessing - a leading `--` ends the subcommand
/// words, so `hora event -- list deploy` records "list deploy".
pub(super) fn parse_event_args(args: &[String]) -> Result<EventCommand, CliError> {
    let title_words = match args {
        [] => {
            return Err(usage(
                "Usage: hora event <title...>\n       hora event list [limit]",
            ));
        }
        [first, rest @ ..] if first == "list" => {
            return match rest.first().map(|raw| raw.parse::<i64>()) {
                None => Ok(EventCommand::List(20)),
                Some(Ok(limit)) => Ok(EventCommand::List(limit.max(1))),
                Some(Err(_)) => Err(usage(format!(
                    "Usage: hora event list [limit]\n\
                     To record an event whose title starts with \"list\": hora event -- {}",
                    args.join(" ")
                ))),
            };
        }
        [first, rest @ ..] if first == "--" => rest,
        _ => args,
    };
    let title = hora_core::bounded(&title_words.join(" "), MAX_EVENT_TITLE_CHARS);
    if title.is_empty() {
        return Err(usage("Usage: hora event <title...>"));
    }
    Ok(EventCommand::Record(title))
}

/// `hora silence <ids|all> <duration> [reason]` / `list` / `clear`: ad-hoc
/// alert muting (a deploy window) written straight into the daemon's database,
/// picked up on its next tick. The HTTP counterpart is `POST /api/silence`.
pub(crate) async fn silence(args: &[String]) -> Result<(), CliError> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let (_, store) = open_database().await?;
            let now = chrono::Utc::now().timestamp();
            let silences = hora_core::db::active_silences(&store, now).await?;
            if silences.is_empty() {
                println!("No active silences.");
            }
            for silence in silences {
                let target = if silence.monitor_id == hora_core::silence::ALL {
                    "all monitors"
                } else {
                    &silence.monitor_id
                };
                let reason = silence
                    .reason
                    .map(|reason| format!(" - {reason}"))
                    .unwrap_or_default();
                println!(
                    "{target}: until {} ({} left){reason}",
                    fmt::utc(silence.until),
                    fmt::duration(silence.until - now)
                );
            }
        }
        Some("clear") => {
            let (_, store) = open_database().await?;
            let cleared =
                hora_core::db::clear_silences(&store, chrono::Utc::now().timestamp()).await?;
            println!("Cleared {cleared} active silence(s).");
        }
        Some(ids) if args.len() >= 2 => {
            let (config, store) = open_database().await?;
            let reason = (args.len() > 2).then(|| args[2..].join(" "));
            let silenced =
                match hora_core::silence::apply(&store, &config, ids, &args[1], reason.as_deref())
                    .await
                {
                    Ok(silenced) => silenced,
                    Err(SilenceError::InvalidDuration) => {
                        return Err(usage(format!(
                            "Invalid duration {:?} (use e.g. 10m, 1h30m; max 7d).",
                            args[1]
                        )));
                    }
                    Err(SilenceError::UnknownId(id)) => return Err(unknown_monitor(&config, &id)),
                    Err(SilenceError::NoIds) => return Err(usage(SILENCE_USAGE)),
                    Err(err @ SilenceError::Database(_)) => {
                        return Err(CliError::Other(err.into()));
                    }
                };
            let until = silenced.until;
            let all = silenced.ids == [hora_core::silence::ALL];
            let target = hora_core::planned::covered(all, &silenced.ids);
            println!("Silenced {target} until {}.", fmt::utc(until));
        }
        _ => return Err(usage(SILENCE_USAGE)),
    }
    Ok(())
}

/// What `hora silence` answers a malformed invocation with.
const SILENCE_USAGE: &str = concat!(
    "Usage: hora silence <ids|all> <duration> [reason]\n",
    "       hora silence list\n",
    "       hora silence clear",
);

/// `hora maintenance plan|list|cancel`: maintenance windows planned at
/// runtime, written straight into the daemon's database and picked up on its
/// next tick (and told to the peers on its next heartbeat). The HTTP twins
/// are `POST`, `GET` and `DELETE /api/maintenance`.
pub(crate) async fn maintenance(args: &[String]) -> Result<(), CliError> {
    match args.split_first() {
        Some((word, rest)) if word == "plan" => plan_maintenance(rest).await,
        Some((word, [])) if word == "list" => list_maintenance().await,
        Some((word, [id])) if word == "cancel" => {
            let id: i64 = id
                .parse()
                .map_err(|_| usage(format!("Invalid window id {id:?} (a number).")))?;
            let (_, store) = open_database().await?;
            match hora_core::planned::cancel(&store, id).await {
                Ok(window) => {
                    println!(
                        "Cancelled maintenance window #{}: {}.",
                        window.id, window.reason
                    );
                    Ok(())
                }
                Err(err @ hora_core::planned::CancelError::Database(_)) => {
                    Err(CliError::Other(err.into()))
                }
                Err(err) => Err(CliError::Failed(format!(
                    "{err} ('hora maintenance list' shows the windows)."
                ))),
            }
        }
        _ => Err(usage(MAINTENANCE_USAGE)),
    }
}

/// What `hora maintenance` answers a malformed invocation with.
const MAINTENANCE_USAGE: &str = concat!(
    "Usage: hora maintenance plan (--host <name> | --monitors <ids|all>) [--at <when>] --for <duration> <reason...>\n",
    "       hora maintenance list\n",
    "       hora maintenance cancel <id>\n",
    "<when>: 2026-11-13T05:00:00Z, unix seconds, or 05:00 (its next occurrence, UTC); default now.\n",
    "<duration>: 10m, 1h30m (max 7d).",
);

/// `hora maintenance plan` arguments, flags taken out of the reason's words.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct PlanArgs {
    pub(super) host: Option<String>,
    pub(super) monitors: Option<String>,
    /// The start, resolved to unix seconds; `None` = now.
    pub(super) starts_at: Option<i64>,
    pub(super) duration: String,
    pub(super) reason: String,
}

/// Parse `hora maintenance plan` arguments at `now`. `--at` takes what the
/// API's `starts_at` takes, plus a UTC clock time (`05:00`): its next
/// occurrence, a convenience for a person at a shell - a script that runs
/// again after 05:00 would plan tomorrow's, so scripts pass the full
/// timestamp.
pub(super) fn parse_plan_args(args: &[String], now: i64) -> Result<PlanArgs, CliError> {
    let mut plan = PlanArgs::default();
    let mut words = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut value = |flag: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| usage(format!("{flag} needs a value.\n{MAINTENANCE_USAGE}")))
        };
        match arg.as_str() {
            "--host" => plan.host = Some(value("--host")?),
            "--monitors" => plan.monitors = Some(value("--monitors")?),
            "--for" => plan.duration = value("--for")?,
            "--at" => {
                let raw = value("--at")?;
                let at = hora_core::planned::parse_start(&raw)
                    .or_else(|| {
                        // A clock time only: `parse_until` also reads a
                        // duration, which `--for` already is.
                        raw.contains(':')
                            .then(|| announce::parse_until(&raw, now))
                            .flatten()
                    })
                    .ok_or_else(|| usage(format!("Invalid --at {raw:?}.\n{MAINTENANCE_USAGE}")))?;
                plan.starts_at = Some(at);
            }
            word => words.push(word),
        }
    }
    plan.reason = words.join(" ");
    if plan.duration.is_empty() {
        return Err(usage(MAINTENANCE_USAGE));
    }
    Ok(plan)
}

async fn plan_maintenance(args: &[String]) -> Result<(), CliError> {
    use hora_core::planned::{Plan, PlanError};

    let now = chrono::Utc::now().timestamp();
    let args = parse_plan_args(args, now)?;
    let (config, store) = open_database().await?;
    let starts_at = args.starts_at.map(|at| at.to_string());
    let planned = match hora_core::planned::plan(
        &store,
        &config,
        Plan {
            monitors: args.monitors.as_deref(),
            host: args.host.as_deref(),
            starts_at: starts_at.as_deref(),
            duration: &args.duration,
            reason: &args.reason,
        },
        now,
    )
    .await
    {
        Ok(planned) => planned,
        Err(PlanError::UnknownId(id)) => return Err(unknown_monitor(&config, &id)),
        Err(err @ PlanError::Database(_)) => return Err(CliError::Other(err.into())),
        Err(err) => return Err(usage(format!("{err}.\n{MAINTENANCE_USAGE}"))),
    };
    let window = &planned.window;
    println!(
        "{} maintenance window #{}: {} to {} ({}).",
        if planned.created {
            "Planned"
        } else {
            "Already planned,"
        },
        window.id,
        fmt::utc(window.starts_at),
        fmt::utc(window.ends_at),
        fmt::duration(window.ends_at - window.starts_at),
    );
    println!("  {}", covered(&config, window));
    if hora_core::planned::is_announced(&config, window) {
        println!("  Announced to the peers on the next heartbeats.");
    }
    Ok(())
}

async fn list_maintenance() -> Result<(), CliError> {
    let (config, store) = open_database().await?;
    let now = chrono::Utc::now().timestamp();
    let windows = hora_core::db::pending_maintenance_windows(&store, now).await?;
    if windows.is_empty() {
        println!("No maintenance window planned.");
    }
    for window in windows {
        let state = if window.starts_at > now {
            format!("starts in {}", fmt::duration(window.starts_at - now))
        } else {
            format!("active, {} left", fmt::duration(window.ends_at - now))
        };
        println!(
            "#{}  {} to {}  ({state})  {}",
            window.id,
            fmt::utc(window.starts_at),
            fmt::utc(window.ends_at),
            window.reason
        );
        let origin = match hora_core::planned::announcing_peer(&window.origin) {
            Some(peer) => format!(" Announced by peer {peer}."),
            None => String::new(),
        };
        println!("      {}{origin}", covered(&config, &window));
    }
    Ok(())
}

/// What a stored window covers on this node, in a sentence.
fn covered(
    config: &hora_core::config::Config,
    window: &hora_core::db::MaintenanceWindow,
) -> String {
    let Some(resolved) = hora_core::planned::resolve(config, window) else {
        return "Ignored: its peer is no longer accepted maintenance from.".to_owned();
    };
    let host = resolved
        .host
        .as_ref()
        .map(|host| format!("Host {host}: "))
        .unwrap_or_default();
    format!(
        "{host}covers {}.",
        hora_core::planned::covered(resolved.all, &resolved.monitors)
    )
}
