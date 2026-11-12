//! `hora tune`: threshold and degraded-latency advice from the stored history.

use hora_core::{config, fmt};

use super::{CliError, days_label, find_monitor, open_database, parse_days, usage};

/// `hora tune [monitor_id] [--days N]`: replay the stored check history against
/// alternative anti-flap settings and print per-monitor recommendations. Pure
/// read-only analytics over data that already exists - it never probes, never
/// writes. With an id, it focuses one monitor; without, every monitor that has
/// history. `--days` narrows the lookback (default: the monitor's retention).
pub(crate) async fn tune(args: &[String]) -> Result<(), CliError> {
    let mut only: Option<&str> = None;
    let mut days: Option<i64> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--days" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| usage("Usage: hora tune [monitor_id] [--days N]"))?;
                days = Some(parse_days(Some(raw))?);
            }
            other if only.is_none() => only = Some(other),
            other => {
                return Err(usage(format!(
                    "Unexpected argument {other:?} (usage: hora tune [monitor_id] [--days N])"
                )));
            }
        }
    }

    let (config, store) = open_database().await?;
    let selected: Vec<&hora_core::config::Monitor> = match only {
        Some(id) => vec![find_monitor(&config, id)?],
        None => config.monitors.iter().collect(),
    };

    let lookback = days.unwrap_or_else(|| {
        // The widest per-monitor retention bounds how far raw checks reach.
        selected
            .iter()
            .map(|monitor| i64::from(monitor.retention_days(config.alerts.default_retention_days)))
            .max()
            .unwrap_or(90)
    });
    println!(
        "hora tune - {} (replaying up to {})",
        config::path().display(),
        days_label(lookback)
    );

    let now = chrono::Utc::now().timestamp();
    let mut printed = 0_usize;
    for monitor in selected {
        let retention = i64::from(monitor.retention_days(config.alerts.default_retention_days));
        let window_days = days.map_or(retention, |d| d.min(retention));
        let since = now - window_days * hora_core::SECONDS_PER_DAY;
        let samples = hora_core::db::check_samples(&store, &monitor.id, since).await?;

        let ctx = hora_core::tune::MonitorContext {
            id: &monitor.id,
            name: &monitor.name,
            group: monitor.group.as_deref(),
            kind: monitor.kind().as_str(),
            interval_secs: monitor.interval_secs,
            current_threshold: config.fail_threshold(&monitor.id),
            current_degraded_over_ms: monitor.degraded_over_ms,
        };
        let tuning = hora_core::tune::analyze(&ctx, &samples);

        if tuning.checks == 0 {
            // Only nag about an empty window when one monitor was asked for;
            // when sweeping all, silently skip the ones with no history.
            if only.is_some() {
                println!();
                println!(
                    "{} has no checks in the last {}.",
                    tuning.name,
                    days_label(window_days)
                );
            }
            continue;
        }
        println!();
        print_tuning(&tuning);
        printed += 1;
    }
    if printed == 0 && only.is_none() {
        println!();
        println!("No check history yet - let the daemon run, then tune against real data.");
    }
    Ok(())
}

/// Render one monitor's tuning block (see [`hora_core::tune`] for the model).
fn print_tuning(t: &hora_core::tune::MonitorTuning) {
    println!(
        "{}  ({}, every {})",
        t.name,
        t.kind,
        fmt::period(t.interval_secs)
    );

    let window = t.window.map_or_else(String::new, |(first, last)| {
        format!("{} -> {}, ", fmt::utc(first), fmt::utc(last))
    });
    println!(
        "  {} checks, {window}{} down  [fail_threshold={}]",
        t.checks, t.down_checks, t.current_threshold
    );

    if t.runs.is_empty() {
        println!("  no failures in the window - nothing to tune for fail_threshold");
    } else {
        let lengths: Vec<String> = t.runs.iter().map(ToString::to_string).collect();
        println!("  failure runs: {}  ({})", t.runs.len(), lengths.join(", "));
        println!("  fail_threshold   alerts   detect after");
        for row in &t.table {
            let marker = if row.threshold == t.current_threshold {
                "*"
            } else {
                " "
            };
            let current = if row.threshold == t.current_threshold {
                "  (current)"
            } else {
                ""
            };
            println!(
                "    {marker} {:<2}            {:>4}    {:>8}{current}",
                row.threshold,
                row.alerts,
                fmt::duration(row.detect_after_secs),
            );
        }
        print_threshold_advice(t);
    }

    print_degraded_advice(t);

    println!(
        "  probe_retries: not replayable from history (only a probe's final attempt is stored); \
         {} single-check failure{} seen - fail_threshold absorbs those across ticks",
        t.single_check_failures,
        if t.single_check_failures == 1 {
            ""
        } else {
            "s"
        }
    );
}

/// The one-line `fail_threshold` recommendation under the replay table.
fn print_threshold_advice(t: &hora_core::tune::MonitorTuning) {
    let interval = i64::try_from(t.interval_secs).unwrap_or(0);
    let longest = t.advice.longest_run;

    // Every failure was an isolated single-check blip: this is the anti-flap
    // question, not the outage-detection one. A threshold above 1 filtering
    // these is the design working - never a reason to lower it.
    if longest == 1 {
        if t.current_threshold <= 1 {
            println!(
                "  -> every failure was a single-check blip; raise fail_threshold to 2 so a \
                 one-off never pages"
            );
            println!("{}", apply_hint(&t.id, 2));
        } else {
            println!(
                "  -> only single-check blips ({}) occurred; fail_threshold {} filtered them all \
                 - the anti-flap working as intended",
                t.single_check_failures, t.current_threshold
            );
        }
        return;
    }

    // A multi-check outage that the current threshold never confirmed: a real
    // misconfiguration, this monitor would have stayed silent through it.
    if t.never_alerts() {
        println!(
            "  ! fail_threshold {} never fires here: the longest outage was {longest} checks - \
             lower it to {longest} or less to catch real outages",
            t.current_threshold
        );
        println!("{}", apply_hint(&t.id, longest));
        return;
    }

    let Some(rec) = t.advice.recommended else {
        println!(
            "  -> no clear flap/outage split in the runs above - the table shows the trade-off; \
             current fail_threshold {} looks reasonable",
            t.current_threshold
        );
        return;
    };
    let flap_max = t.advice.flap_max.unwrap_or(0);
    let rec_alerts = t
        .table
        .iter()
        .find(|row| row.threshold == rec)
        .map_or(0, |row| row.alerts);

    match rec.cmp(&t.current_threshold) {
        std::cmp::Ordering::Equal => println!(
            "  -> fail_threshold {rec} looks right: flaps (runs <= {flap_max}) filtered, \
             longest outage ({} checks) still caught",
            t.advice.longest_run
        ),
        std::cmp::Ordering::Greater => {
            let delay = i64::from(rec - t.current_threshold).saturating_mul(interval);
            println!(
                "  -> raise fail_threshold to {rec}: {} alerts instead of {} over the window \
                 (drops flaps <= {flap_max}), same real outages, +{} to detect",
                rec_alerts,
                t.current_alerts,
                fmt::duration(delay)
            );
            println!("{}", apply_hint(&t.id, rec));
        }
        std::cmp::Ordering::Less => {
            let saved = i64::from(t.current_threshold - rec).saturating_mul(interval);
            println!(
                "  -> fail_threshold {rec} would catch the same outages {} sooner; \
                 the current {} only adds delay (no extra flaps above {flap_max} to filter)",
                fmt::duration(saved),
                t.current_threshold
            );
            println!("{}", apply_hint(&t.id, rec));
        }
    }
}

/// Where a recommended threshold goes: the recommendation is this monitor's,
/// drawn from its own history, so it is set on the monitor rather than in
/// `[alerts]`, where it would change every other monitor too.
fn apply_hint(id: &str, threshold: u32) -> String {
    format!(
        "     to apply: fail_threshold = {threshold} in the [[monitors]] entry with id = \"{id}\" \
         (this monitor only; [alerts] fail_threshold is the default of the others)"
    )
}

/// The one-line `degraded_over_ms` recommendation from the latency spread.
fn print_degraded_advice(t: &hora_core::tune::MonitorTuning) {
    let Some(stats) = &t.latency else {
        return;
    };
    println!(
        "  latency, up checks: p50 {} ms  p95 {} ms  p99 {} ms  max {} ms{}",
        stats.p50,
        stats.p95,
        stats.p99,
        stats.max,
        t.current_degraded_over_ms
            .map_or_else(String::new, |ms| format!("  [degraded_over_ms={ms}]"))
    );
    let Some(rec) = t.recommended_degraded_over_ms else {
        return;
    };
    match (t.current_degraded_over_ms, t.currently_degraded) {
        (Some(current), Some(flagged)) => {
            let pct = percent(flagged, stats.count);
            if current < stats.p95 {
                println!(
                    "    {current} ms flags {flagged} of {} up checks ({pct} %) - that is normal \
                     traffic; recommend degraded_over_ms {rec} (~p99, ~1 %)",
                    stats.count
                );
            } else {
                println!(
                    "    {current} ms flags {flagged} of {} up checks ({pct} %); ~p99 is {rec} ms",
                    stats.count
                );
            }
        }
        _ => println!(
            "    no degraded_over_ms set - recommend {rec} (~p99) to flag genuine slowness"
        ),
    }
}

/// Percentage of `part` in `whole`, one decimal place; `0.0` for an empty whole.
/// Integer math (tenths of a percent, rounded) to stay exact and lint-clean.
fn percent(part: usize, whole: usize) -> String {
    if whole == 0 {
        return "0.0".to_owned();
    }
    let tenths = (part * 1000 + whole / 2) / whole;
    format!("{}.{}", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::apply_hint;

    /// A recommendation is one monitor's: the hint names the per-monitor
    /// key and the monitor, not the global setting alone.
    #[test]
    fn a_recommended_threshold_says_where_to_set_it() {
        let hint = apply_hint("api", 2);
        assert!(hint.contains("fail_threshold = 2"), "{hint}");
        assert!(
            hint.contains("[[monitors]] entry with id = \"api\""),
            "{hint}"
        );
    }
}
