//! Reading the history: incidents, notes, postmortems, the timeline and the
//! monthly report.

use std::fmt::Write as _;

use hora_core::fmt;

use super::{
    CliError, days_label, no_such_incident, open_database, parse_days, resolve_incident_id, usage,
};

/// Print the monthly SLA report as text - the terminal twin of the printable
/// `/report/{month}` page. Defaults to last month: "here is your May report".
pub(crate) async fn report(month: Option<&str>) -> Result<(), CliError> {
    let (config, store) = open_database().await?;
    let month = month.map_or_else(
        || hora_core::report::previous_month(chrono::Utc::now().timestamp()),
        str::to_owned,
    );
    let report = hora_core::report::build(&store, &config, &month)
        .await
        .map_err(|err| CliError::Failed(format!("{err:#}")))?;

    println!("SLA report - {} ({})", report.label, config.page.title);
    let mut current_group: Option<&str> = None;
    for row in &report.rows {
        let group = row.group.as_deref().unwrap_or("");
        if current_group != Some(group) {
            current_group = Some(group);
            println!();
            println!("{}", if group.is_empty() { "Monitors" } else { group });
        }
        let uptime = row
            .uptime_bp
            .map_or_else(|| "no data".to_owned(), fmt::pct_bp);
        let mut line = format!("  {}: {uptime}", row.name);
        if row.incidents > 0 {
            let plural = if row.incidents > 1 { "s" } else { "" };
            let _ = write!(
                line,
                ", {} incident{plural}, {} down",
                row.incidents,
                fmt::duration(row.downtime_secs)
            );
        }
        if let Some(mttr) = row.mttr_secs {
            let _ = write!(line, ", MTTR {}", fmt::duration(mttr));
        }
        if let (Some(slo_bp), Some(met)) = (row.slo_bp, row.slo_met) {
            let _ = write!(
                line,
                ", SLO {} {}",
                fmt::pct_bp(i64::from(slo_bp)),
                if met { "met" } else { "MISSED" }
            );
        }
        if let (Some(consumed), Some(budget)) = (row.budget_consumed_minutes, row.budget_minutes) {
            let _ = write!(
                line,
                ", budget {} of {}",
                fmt::duration(consumed.saturating_mul(60)),
                fmt::duration(budget.saturating_mul(60))
            );
        }
        println!("{line}");
    }
    Ok(())
}

/// `hora timeline [--days N]`: the unified chronology - down/recovered
/// transitions, operator events, pushed alerts, announcements, silences,
/// planned maintenance - merged newest-first. "What happened this week?" in one command.
pub(crate) async fn timeline(args: &[String]) -> Result<(), CliError> {
    let mut days: i64 = 7;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--days" => days = parse_days(iter.next())?,
            other => {
                return Err(usage(format!(
                    "Unexpected argument {other:?} (usage: hora timeline [--days N])"
                )));
            }
        }
    }

    let (config, store) = open_database().await?;
    let since = chrono::Utc::now().timestamp() - days * hora_core::SECONDS_PER_DAY;
    let sources = hora_core::timeline::fetch(&store, since, 500).await?;
    // Peers by id (a planned window names the one that announced it), the
    // monitors on top.
    let names: std::collections::HashMap<String, String> = config
        .peers
        .iter()
        .map(|peer| (peer.id.clone(), peer.name.clone()))
        .chain(
            config
                .monitors
                .iter()
                .map(|monitor| (monitor.id.clone(), monitor.name.clone())),
        )
        .collect();
    let entries = hora_core::timeline::merge(&sources, &names, since, 200);

    println!("hora timeline - last {}", days_label(days));
    if entries.is_empty() {
        println!();
        println!("Nothing recorded in the window.");
        return Ok(());
    }
    for entry in entries {
        let link = entry.incident_id.map_or_default(|id| format!("  (#{id})"));
        println!(
            "{}  {:<9}  {}{link}",
            fmt::utc(entry.at),
            entry.kind.as_str(),
            entry.title
        );
        if let Some(detail) = entry.detail {
            // Aligned under the title column (timestamp + gap + kind + gap).
            println!("{:>36}{detail}", "");
        }
    }
    Ok(())
}

/// `hora postmortem <id|last>`: print the auto-generated markdown post-mortem
/// of an incident - everything Hora already recorded about it, ready to paste
/// into a ticket. The web twin is `/incident/{id}`.
pub(crate) async fn postmortem(id_arg: &str) -> Result<(), CliError> {
    let (config, store) = open_database().await?;
    let id = resolve_incident_id(&store, id_arg).await?;
    let Some(incident) = hora_core::db::incident_by_id(&store, id).await? else {
        return Err(no_such_incident(id));
    };
    let name = config.monitor_name(&incident.monitor_id);
    print!(
        "{}",
        fmt::printable(&hora_core::postmortem::render(&incident, name))
    );
    Ok(())
}

/// List recent incidents with their ids - the lookup companion of `annotate`.
pub(crate) async fn list_incidents(limit: i64) -> Result<(), CliError> {
    let (config, store) = open_database().await?;
    let incidents = hora_core::db::recent_incidents(&store, limit).await?;
    if incidents.is_empty() {
        println!("No incidents recorded.");
        return Ok(());
    }
    for incident in incidents {
        let name = config.monitor_name(&incident.monitor_id);
        let span = match incident.ended_at {
            Some(ended) => format!(
                "{} -> {} ({})",
                fmt::utc(incident.started_at),
                fmt::utc(ended),
                fmt::duration(incident.duration_s.unwrap_or(0))
            ),
            None => format!("{} -> ongoing", fmt::utc(incident.started_at)),
        };
        println!("#{}  {name}  {span}", incident.id);
        if let Some(error) = &incident.error {
            println!("      error: {}", fmt::printable(error));
        }
        // The full snapshot lives on /history; the status line is enough here.
        if let Some(first_line) = incident
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.lines().next())
        {
            println!("      answered: {}", fmt::printable(first_line));
        }
        if let Some(note) = &incident.note {
            println!("      note:  {}", fmt::printable(note));
        }
    }
    Ok(())
}

/// Attach (or clear, with an empty note) an annotation on an incident, shown
/// on /history and in the Atom feed. `last` targets the most recent incident.
pub(crate) async fn annotate(id_arg: &str, note: &str) -> Result<(), CliError> {
    let (_, store) = open_database().await?;
    let id = resolve_incident_id(&store, id_arg).await?;
    if !hora_core::db::set_incident_note(&store, id, note).await? {
        return Err(no_such_incident(id));
    }
    if note.is_empty() {
        println!("Cleared the note on incident #{id}.");
    } else {
        println!("Annotated incident #{id}: {note}");
    }
    Ok(())
}
