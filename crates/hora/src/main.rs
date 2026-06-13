//! Hora - a tiny self-hosted uptime monitor.
//!
//! Wires the pieces together: load config, open the database, start the
//! supervisor (which owns the live config and notification channels), spawn the
//! certificate watcher and pruner, and serve the status page and JSON API.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use anyhow::Context as _;
use hora_core::config;

mod top;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if run_subcommand().await? {
        return Ok(());
    }

    init_tracing();
    serve().await
}

/// Handle a CLI subcommand. `Ok(true)` means one ran and the process should
/// exit; plain `hora` (no arguments) returns `Ok(false)` and starts the
/// monitor.
async fn run_subcommand() -> anyhow::Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        match args[1].as_str() {
            "import" => {
                if args.len() < 4 || args[2] != "kuma" {
                    eprintln!("Usage: hora import kuma <backup.json>");
                    std::process::exit(1);
                }
                let json_path = &args[3];
                let json_str = std::fs::read_to_string(json_path)
                    .with_context(|| format!("reading {json_path}"))?;
                let toml_out = hora_core::import::convert_kuma_to_hora(&json_str)?;
                println!("{toml_out}");
            }
            "check" => {
                // Validate the config and exit non-zero on error: meant for CI
                // and pre-deploy hooks.
                let config_path = config::path();
                match config::load_from(&config_path) {
                    Ok(_) => println!("{} is valid.", config_path.display()),
                    Err(err) => {
                        eprintln!("Configuration error: {err:#}");
                        std::process::exit(1);
                    }
                }
            }
            "test-alert" => {
                // Tracing first: delivery failures surface as per-channel
                // warnings from the notifiers, and that is the whole point.
                init_tracing();
                test_alert(args.get(2).map(String::as_str)).await?;
            }
            "backup" => {
                let Some(dest) = args.get(2) else {
                    eprintln!("Usage: hora backup <destination.db>");
                    std::process::exit(1);
                };
                backup(dest).await?;
            }
            "incidents" => {
                let limit = args
                    .get(2)
                    .map_or(Ok(20), |raw| raw.parse::<i64>())
                    .unwrap_or_else(|_| {
                        eprintln!("Usage: hora incidents [limit]");
                        std::process::exit(1);
                    });
                list_incidents(limit.max(1)).await?;
            }
            "annotate" => {
                if args.len() < 4 {
                    eprintln!("Usage: hora annotate <incident-id|last> <note>");
                    eprintln!("An empty note (\"\") clears the annotation.");
                    std::process::exit(1);
                }
                annotate(&args[2], &args[3..].join(" ")).await?;
            }
            "silence" => {
                silence(&args[2..]).await?;
            }
            "digest" => {
                digest_preview().await?;
            }
            "report" => {
                report(args.get(2).map(String::as_str)).await?;
            }
            "doctor" => {
                doctor().await?;
            }
            "tune" => {
                tune(&args[2..]).await?;
            }
            "announce" => {
                announce(&args[2..]).await?;
            }
            "top" => {
                top::run(&args[2..]).await?;
            }
            "--version" | "-V" => println!("hora {}", env!("CARGO_PKG_VERSION")),
            "--help" | "-h" => print_help(),
            _ => {
                eprintln!("Unknown command: {}", args[1]);
                eprintln!("Run 'hora --help' for usage information.");
                std::process::exit(1);
            }
        }
        return Ok(true);
    }
    Ok(false)
}

/// Send a test `Down` then `Recovered` through the real notification chain, so
/// an operator verifies delivery *before* the first real incident instead of
/// during it. Without an id every configured channel is exercised; with one,
/// the monitor's `notify` routing applies - testing exactly what would fire.
/// Failures surface as the notifiers' own per-channel warnings *and* as a
/// non-zero exit, so a CI pipeline can gate on the notification chain.
async fn test_alert(monitor_id: Option<&str>) -> anyhow::Result<()> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;

    let (name, notify) = match monitor_id {
        None => ("Hora test".to_owned(), None),
        Some(id) => {
            let Some(monitor) = config.monitors.iter().find(|monitor| monitor.id == id) else {
                eprintln!("Unknown monitor {id:?}. Configured ids:");
                for monitor in &config.monitors {
                    eprintln!("  {}", monitor.id);
                }
                std::process::exit(1);
            };
            (monitor.name.clone(), monitor.notify.clone())
        }
    };

    let client = hora_core::http::client(None).context("building HTTP client")?;
    let dispatcher = hora_core::notifications::build(&config, &client);
    let targeted: Vec<&str> = dispatcher
        .names()
        .filter(|channel| {
            notify
                .as_ref()
                .is_none_or(|only| only.iter().any(|name| name == channel))
        })
        .collect();
    if targeted.is_empty() {
        eprintln!("No notification channel to test (none configured, or none routed).");
        std::process::exit(1);
    }

    println!(
        "Sending a test alert (down + recovered) as {name:?} to: {}",
        targeted.join(", ")
    );
    let event = hora_core::notifications::Event::Down {
        monitor: &name,
        error: Some("test alert sent by `hora test-alert` - not a real incident"),
        cause: None,
        impacted: &[],
        vantage: None,
    };
    let mut failed = dispatcher.dispatch(event, notify.as_deref()).await;
    let failed_recovery = dispatcher
        .dispatch(
            hora_core::notifications::Event::Recovered { monitor: &name },
            notify.as_deref(),
        )
        .await;
    // One verdict per channel: failing either delivery counts as failed.
    for channel in failed_recovery {
        if !failed.contains(&channel) {
            failed.push(channel);
        }
    }
    if failed.is_empty() {
        println!("Done. Every channel accepted both notifications.");
        Ok(())
    } else {
        eprintln!(
            "Delivery failed on: {} (the warnings above say why).",
            failed.join(", ")
        );
        std::process::exit(1);
    }
}

fn print_help() {
    println!("Hora - a tiny self-hosted uptime monitor");
    println!();
    println!("Usage: hora [COMMAND]");
    println!();
    println!("Commands:");
    println!("  import kuma <file>  Convert an Uptime Kuma backup JSON to Hora TOML (stdout)");
    println!("  check               Validate the configuration and exit");
    println!("  doctor              Diagnose the runtime environment (IPv6, ICMP socket,");
    println!("                      DNS resolver, listen port, database)");
    println!("  tune [id] [--days N]  Replay stored history against other fail_threshold /");
    println!("                      degraded_over_ms settings and recommend per monitor");
    println!("  test-alert [id]     Send a test down + recovered through the configured");
    println!("                      channels (all of them, or the routed ones of monitor [id])");
    println!("  silence <ids> <for> [reason]  Mute alerts for monitors (comma-separated ids");
    println!("                      or 'all') for a duration like 10m or 1h30m (max 7d)");
    println!("  silence list        Show the active silences");
    println!("  announce <title> [body] [--severity s] [--until 4h|18:00]");
    println!("                      Pin a public banner on the status page");
    println!("  announce list / clear  Show or remove the pinned announcements");
    println!("  silence clear       Remove every silence");
    println!("  top [--url U] [--token T] [--interval S]");
    println!("                      Live terminal dashboard over the JSON API");
    println!("  digest              Print the weekly digest (a dry run of [digest])");
    println!("  report [YYYY-MM]    Print the monthly SLA report (default: last month;");
    println!("                      the printable page is /report/YYYY-MM)");
    println!("  incidents [limit]   List recent incidents with their ids");
    println!("  annotate <id> <note>  Attach a note to an incident ('last' targets the");
    println!("                      most recent one; an empty note clears it)");
    println!("  backup <dest.db>    Snapshot the database with VACUUM INTO");
    println!("  --version, -V       Show the version");
    println!("  --help, -h          Show this help message");
}

/// Open the daemon's database for a CLI subcommand. Refuses to *create* one: a
/// missing file means the config points somewhere the daemon never wrote (a
/// different working directory, usually), and silently creating an empty
/// database there would only hide the mistake.
async fn open_database() -> anyhow::Result<(hora_core::config::Config, hora_core::db::SqlitePool)> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    let path = &config.server.database_path;
    if path != ":memory:" && !path.starts_with("file:") && !std::path::Path::new(path).exists() {
        anyhow::bail!(
            "database {path} not found - run from the daemon's working directory, \
             or point HORA_CONFIG at its config"
        );
    }
    let pool = hora_core::db::connect(path)
        .await
        .context("opening database")?;
    Ok((config, pool))
}

/// Snapshot the database to `dest` via `VACUUM INTO`: consistent and compacted,
/// safe while the daemon runs. Meant for cron ("a one-statement answer to 'what
/// if I lose a year of history?'").
async fn backup(dest: &str) -> anyhow::Result<()> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    let source = &config.server.database_path;
    hora_core::db::backup_into(source, dest).await?;
    let size = std::fs::metadata(dest).map_or(0, |meta| meta.len());
    println!("Backed up {source} to {dest} ({} KiB).", size / 1024);
    Ok(())
}

/// `hora announce`: pin (or list/clear) a public banner on the status page -
/// the communication side of incidents, written straight into the daemon's
/// database and live within the summary cache TTL (~5s). The HTTP twin is
/// `POST /api/announce`.
async fn announce(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let (_, pool) = open_database().await?;
            let now = chrono::Utc::now().timestamp();
            let pinned = hora_core::db::active_announcements(&pool, now).await?;
            if pinned.is_empty() {
                println!("No announcements pinned.");
            }
            for item in pinned {
                let until = item.until.map_or_else(
                    || "until cleared".to_owned(),
                    |ts| format!("until {}", format_epoch(ts)),
                );
                println!("#{} [{}] {} ({until})", item.id, item.severity, item.title);
                if !item.body.is_empty() {
                    println!("      {}", item.body);
                }
            }
        }
        Some("clear") => {
            let (_, pool) = open_database().await?;
            let cleared =
                hora_core::db::clear_announcements(&pool, chrono::Utc::now().timestamp()).await?;
            println!("Cleared {cleared} announcement(s).");
        }
        Some(_) => {
            let (title, body, severity, until) = parse_announce_args(args)?;
            let (_, pool) = open_database().await?;
            hora_core::db::insert_announcement(&pool, &title, &body, severity, until).await?;
            let expiry = until.map_or_else(
                || "until `hora announce clear`".to_owned(),
                |ts| format!("until {}", format_epoch(ts)),
            );
            println!("Pinned [{severity}] {title:?} ({expiry}).");
        }
        None => {
            eprintln!(
                "Usage: hora announce <title> [body...] [--severity info|warning|critical|resolved] [--until 4h|18:00]"
            );
            eprintln!("       hora announce list");
            eprintln!("       hora announce clear");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Split `hora announce` arguments: `--severity`/`--until` flags anywhere,
/// the first free word is the title, the rest joins into the body.
fn parse_announce_args(
    args: &[String],
) -> anyhow::Result<(String, String, &'static str, Option<i64>)> {
    let mut severity = "info";
    let mut until = None;
    let mut words: Vec<&str> = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--severity" => {
                severity = match iter.next().map(String::as_str) {
                    Some("info") => "info",
                    Some("warning") => "warning",
                    Some("critical") => "critical",
                    Some("resolved") => "resolved",
                    other => anyhow::bail!(
                        "--severity must be info, warning, critical or resolved (got {other:?})"
                    ),
                };
            }
            "--until" => {
                let raw = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--until needs a value (e.g. 4h or 18:00)"))?;
                until = Some(parse_until(raw, chrono::Utc::now().timestamp()).ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "invalid --until {raw:?} (use a duration like 4h, or HH:MM UTC)"
                        )
                    },
                )?);
            }
            word => words.push(word),
        }
    }
    let Some((title, body)) = words.split_first() else {
        anyhow::bail!("announce needs a title");
    };
    Ok((
        title.trim().chars().take(200).collect(),
        body.join(" ").chars().take(500).collect(),
        severity,
        until,
    ))
}

/// `--until` accepts a duration (`4h`, `90m`) or a UTC clock time (`18:00`,
/// meaning the next occurrence - today if still ahead, tomorrow otherwise).
fn parse_until(raw: &str, now: i64) -> Option<i64> {
    if let Some(secs) = hora_core::parse_duration(raw) {
        return Some(now + i64::try_from(secs).unwrap_or(i64::MAX));
    }
    let (hours, minutes) = raw.split_once(':')?;
    let (hours, minutes): (i64, i64) = (hours.parse().ok()?, minutes.parse().ok()?);
    if !(0..24).contains(&hours) || !(0..60).contains(&minutes) {
        return None;
    }
    let today = (now / 86_400) * 86_400 + hours * 3600 + minutes * 60;
    Some(if today > now { today } else { today + 86_400 })
}

/// Print the digest exactly as the `[digest]` task would send it/// Print the digest exactly as the `[digest]` task would send it - a dry run
/// to check the wording (and the data) without notifying anyone.
async fn digest_preview() -> anyhow::Result<()> {
    let (config, pool) = open_database().await?;
    let now = chrono::Utc::now().timestamp();
    let (period, summary) = hora_core::digest::build_summary(&pool, &config, now).await?;
    println!("Hora digest ({period})");
    println!("{summary}");
    Ok(())
}

/// Diagnose the runtime environment against what the config needs: `hora
/// check` says the config is sound, `hora doctor` says the *host* can honour
/// it. Exits non-zero when a needed capability is missing.
async fn doctor() -> anyhow::Result<()> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    println!(
        "hora doctor - {} ({} monitors)",
        config_path.display(),
        config.monitors.len()
    );
    println!();

    let findings = hora_core::doctor::run(&config).await;
    let mut failed = false;
    for finding in &findings {
        failed = failed || finding.status == hora_core::doctor::Status::Fail;
        println!(
            "  {:<10} {:<5} {}",
            finding.name,
            finding.status.label(),
            finding.detail
        );
    }
    if failed {
        println!();
        eprintln!("Some capabilities the configuration needs are missing.");
        std::process::exit(1);
    }
    Ok(())
}

/// Print the monthly SLA report as text - the terminal twin of the printable
/// `/report/{month}` page. Defaults to last month: "here is your May report".
async fn report(month: Option<&str>) -> anyhow::Result<()> {
    let (config, pool) = open_database().await?;
    let month = month.map_or_else(
        || hora_core::report::previous_month(chrono::Utc::now().timestamp()),
        str::to_owned,
    );
    let report = match hora_core::report::build(&pool, &config, &month).await {
        Ok(report) => report,
        Err(err) => {
            eprintln!("{err:#}");
            std::process::exit(1);
        }
    };

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
            .map_or_else(|| "no data".to_owned(), hora_core::report::format_bp);
        let mut line = format!("  {}: {uptime}", row.name);
        if row.incidents > 0 {
            let plural = if row.incidents > 1 { "s" } else { "" };
            let _ = write!(
                line,
                ", {} incident{plural}, {} down",
                row.incidents,
                hora_core::report::format_secs(row.downtime_secs)
            );
        }
        if let Some(mttr) = row.mttr_secs {
            let _ = write!(line, ", MTTR {}", hora_core::report::format_secs(mttr));
        }
        if let (Some(slo_bp), Some(met)) = (row.slo_bp, row.slo_met) {
            let _ = write!(
                line,
                ", SLO {} {}",
                hora_core::report::format_bp(i64::from(slo_bp)),
                if met { "met" } else { "MISSED" }
            );
        }
        if let (Some(consumed), Some(budget)) = (row.budget_consumed_minutes, row.budget_minutes) {
            let _ = write!(line, ", budget {consumed}m of {budget}m");
        }
        println!("{line}");
    }
    Ok(())
}

/// `hora tune [monitor_id] [--days N]`: replay the stored check history against
/// alternative anti-flap settings and print per-monitor recommendations. Pure
/// read-only analytics over data that already exists - it never probes, never
/// writes. With an id, it focuses one monitor; without, every monitor that has
/// history. `--days` narrows the lookback (default: the monitor's retention).
async fn tune(args: &[String]) -> anyhow::Result<()> {
    let mut only: Option<&str> = None;
    let mut days: Option<i64> = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--days" => {
                let raw = iter.next().unwrap_or_else(|| {
                    eprintln!("Usage: hora tune [monitor_id] [--days N]");
                    std::process::exit(1);
                });
                let parsed = raw.parse::<i64>().ok().filter(|&d| d > 0);
                let Some(parsed) = parsed else {
                    eprintln!("--days must be a positive number of days");
                    std::process::exit(1);
                };
                days = Some(parsed);
            }
            other if only.is_none() => only = Some(other),
            other => {
                eprintln!(
                    "Unexpected argument {other:?} (usage: hora tune [monitor_id] [--days N])"
                );
                std::process::exit(1);
            }
        }
    }

    let (config, pool) = open_database().await?;
    let selected: Vec<&hora_core::config::Monitor> = match only {
        Some(id) => {
            let Some(monitor) = config.monitors.iter().find(|monitor| monitor.id == id) else {
                eprintln!("Unknown monitor {id:?}. Configured ids:");
                for monitor in &config.monitors {
                    eprintln!("  {}", monitor.id);
                }
                std::process::exit(1);
            };
            vec![monitor]
        }
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
        let samples = hora_core::db::check_samples(&pool, &monitor.id, since).await?;

        let ctx = hora_core::tune::MonitorContext {
            id: &monitor.id,
            name: &monitor.name,
            group: monitor.group.as_deref(),
            kind: monitor.kind.as_str(),
            interval_secs: monitor.interval_secs,
            current_threshold: config.alerts.fail_threshold,
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
    println!("{}  ({}, every {}s)", t.name, t.kind, t.interval_secs);

    let window = t.window.map_or_else(String::new, |(first, last)| {
        format!("{} -> {}, ", format_epoch(first), format_epoch(last))
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
                format_secs(row.detect_after_secs),
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
                format_secs(delay)
            );
        }
        std::cmp::Ordering::Less => {
            let saved = i64::from(t.current_threshold - rec).saturating_mul(interval);
            println!(
                "  -> fail_threshold {rec} would catch the same outages {} sooner; \
                 the current {} only adds delay (no extra flaps above {flap_max} to filter)",
                format_secs(saved),
                t.current_threshold
            );
        }
    }
}

/// The one-line `degraded_over_ms` recommendation from the latency spread.
fn print_degraded_advice(t: &hora_core::tune::MonitorTuning) {
    let Some(stats) = &t.latency else {
        return;
    };
    println!(
        "  latency, up checks: p50 {}ms  p95 {}ms  p99 {}ms  max {}ms{}",
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
                    "    {current}ms flags {flagged} of {} up checks ({pct}%) - that is normal \
                     traffic; recommend degraded_over_ms {rec} (~p99, ~1%)",
                    stats.count
                );
            } else {
                println!(
                    "    {current}ms flags {flagged} of {} up checks ({pct}%); ~p99 is {rec}ms",
                    stats.count
                );
            }
        }
        _ => println!(
            "    no degraded_over_ms set - recommend {rec} (~p99) to flag genuine slowness"
        ),
    }
}

/// `"1 day"` / `"30 days"` - the only place the lookback is pluralised.
fn days_label(days: i64) -> String {
    format!("{days} day{}", if days == 1 { "" } else { "s" })
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

/// List recent incidents with their ids - the lookup companion of `annotate`.
async fn list_incidents(limit: i64) -> anyhow::Result<()> {
    let (config, pool) = open_database().await?;
    let incidents = hora_core::db::recent_incidents(&pool, limit).await?;
    if incidents.is_empty() {
        println!("No incidents recorded.");
        return Ok(());
    }
    for incident in incidents {
        let name = config
            .monitors
            .iter()
            .find(|monitor| monitor.id == incident.monitor_id)
            .map_or(incident.monitor_id.as_str(), |monitor| {
                monitor.name.as_str()
            });
        let span = match incident.ended_at {
            Some(ended) => format!(
                "{} -> {} ({})",
                format_epoch(incident.started_at),
                format_epoch(ended),
                format_secs(incident.duration_s.unwrap_or(0))
            ),
            None => format!("{} -> ongoing", format_epoch(incident.started_at)),
        };
        println!("#{}  {name}  {span}", incident.id);
        if let Some(error) = &incident.error {
            println!("      error: {error}");
        }
        // The full snapshot lives on /history; the status line is enough here.
        if let Some(first_line) = incident
            .snapshot
            .as_deref()
            .and_then(|snapshot| snapshot.lines().next())
        {
            println!("      answered: {first_line}");
        }
        if let Some(note) = &incident.note {
            println!("      note:  {note}");
        }
    }
    Ok(())
}

/// Attach (or clear, with an empty note) an annotation on an incident, shown
/// on /history and in the Atom feed. `last` targets the most recent incident.
async fn annotate(id_arg: &str, note: &str) -> anyhow::Result<()> {
    let (_, pool) = open_database().await?;
    let id = if id_arg == "last" {
        let Some(id) = hora_core::db::latest_incident_id(&pool).await? else {
            eprintln!("No incidents recorded yet.");
            std::process::exit(1);
        };
        id
    } else {
        id_arg.parse().unwrap_or_else(|_| {
            eprintln!("Invalid incident id {id_arg:?} (a number, or 'last').");
            std::process::exit(1);
        })
    };
    if !hora_core::db::set_incident_note(&pool, id, note).await? {
        eprintln!("No incident #{id}. 'hora incidents' lists the recent ones.");
        std::process::exit(1);
    }
    if note.is_empty() {
        println!("Cleared the note on incident #{id}.");
    } else {
        println!("Annotated incident #{id}: {note}");
    }
    Ok(())
}

/// `hora silence <ids|all> <duration> [reason]` / `list` / `clear`: ad-hoc
/// alert muting (a deploy window) written straight into the daemon's database,
/// picked up on its next tick. The HTTP counterpart is `POST /api/silence`.
async fn silence(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("list") => {
            let (_, pool) = open_database().await?;
            let now = chrono::Utc::now().timestamp();
            let silences = hora_core::db::active_silences(&pool, now).await?;
            if silences.is_empty() {
                println!("No active silences.");
            }
            for silence in silences {
                let target = if silence.monitor_id == "*" {
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
                    format_epoch(silence.until),
                    format_secs(silence.until - now)
                );
            }
        }
        Some("clear") => {
            let (_, pool) = open_database().await?;
            let cleared =
                hora_core::db::clear_silences(&pool, chrono::Utc::now().timestamp()).await?;
            println!("Cleared {cleared} active silence(s).");
        }
        Some(ids) if args.len() >= 2 => {
            let Some(duration_secs) = hora_core::parse_duration(&args[1])
                .filter(|secs| *secs <= hora_core::MAX_SILENCE_SECS)
            else {
                eprintln!(
                    "Invalid duration {:?} (use e.g. 10m, 1h30m; max 7d).",
                    args[1]
                );
                std::process::exit(1);
            };
            let (config, pool) = open_database().await?;
            let monitors: Vec<&str> = if ids == "all" || ids == "*" {
                vec!["*"]
            } else {
                let ids: Vec<&str> = ids.split(',').map(str::trim).collect();
                // Fail on a typo'd id rather than silencing nothing.
                for id in &ids {
                    if !config.monitors.iter().any(|monitor| monitor.id == *id) {
                        eprintln!("Unknown monitor {id:?}. Configured ids:");
                        for monitor in &config.monitors {
                            eprintln!("  {}", monitor.id);
                        }
                        std::process::exit(1);
                    }
                }
                ids
            };
            let reason = (args.len() > 2).then(|| args[2..].join(" "));
            let until =
                chrono::Utc::now().timestamp() + i64::try_from(duration_secs).unwrap_or(i64::MAX);
            for id in &monitors {
                hora_core::db::insert_silence(&pool, id, until, reason.as_deref()).await?;
            }
            let target = if monitors == ["*"] {
                "all monitors".to_owned()
            } else {
                monitors.join(", ")
            };
            println!("Silenced {target} until {}.", format_epoch(until));
        }
        _ => {
            eprintln!("Usage: hora silence <ids|all> <duration> [reason]");
            eprintln!("       hora silence list");
            eprintln!("       hora silence clear");
            std::process::exit(1);
        }
    }
    Ok(())
}

fn format_epoch(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, 0).map_or_else(
        || timestamp.to_string(),
        |dt| dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn format_secs(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

/// Run the monitor: load config, open the database, start the supervisor and
/// background tasks, and serve the status page until a shutdown signal.
async fn serve() -> anyhow::Result<()> {
    let config_path = config::path();
    let initial = config::load_from(&config_path).context("loading configuration")?;
    let pool = hora_core::db::connect(&initial.server.database_path)
        .await
        .context("opening database")?;
    // The notifier client (no proxy); per-monitor probe clients are built by the
    // supervisor so each can carry its own proxy.
    let client = hora_core::http::client(None).context("building HTTP client")?;

    // A shutdown signal lets the background tasks stop cleanly (finishing their
    // current iteration) instead of being aborted when the runtime drops.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    // The scheduler's liveness beacon: each monitor tick bumps it, and the
    // dead-man heartbeat and /healthz read it to tell a live scheduler from a
    // wedged one. Shared with the supervisor (writers) and the web layer (reader).
    let last_tick = Arc::new(AtomicU64::new(0));

    // The supervisor owns the live config + notification channels and reconciles
    // monitor tasks on reload; other components read through its handles.
    let handle = hora_core::supervisor::start(
        initial,
        config_path,
        pool.clone(),
        client.clone(),
        Arc::clone(&last_tick),
        shutdown_rx.clone(),
    );
    let cert_task = hora_core::cert::spawn_watcher(
        pool.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        client.clone(),
        shutdown_rx.clone(),
    );

    // Mutual surveillance: the outbound dead-man heartbeat. It self-gates on the
    // [health] section and reads it live, so it is always spawned (and activates if
    // [health] is added on reload). The inbound peer-watch tasks are owned and
    // hot-reloaded by the supervisor alongside the monitors.
    let heartbeat_task = hora_core::peer::spawn_heartbeat(
        handle.config.clone(),
        pool.clone(),
        client,
        Arc::clone(&last_tick),
        shutdown_rx.clone(),
    );

    let digest_task = hora_core::digest::spawn(
        pool.clone(),
        handle.config.clone(),
        handle.notifier.clone(),
        shutdown_rx.clone(),
    );

    let prune_task = hora_core::db::spawn_pruner(&pool, handle.config.clone(), shutdown_rx);

    let bind = handle.config.borrow().server.bind.clone();
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding {bind}"))?;
    tracing::info!(
        "hora {} listening on http://{bind}",
        env!("CARGO_PKG_VERSION")
    );

    let state = hora_web::AppState::new(pool, handle.config.clone(), Arc::clone(&last_tick));
    // Connect-info gives the rate limiter a peer IP to fall back on when there
    // is no `X-Forwarded-For` (i.e. direct access, not behind a proxy).
    let app = hora_web::router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("running HTTP server")?;

    // The HTTP server has drained; now stop the background tasks and wait briefly
    // for them to finish their current iteration before the runtime drops.
    let _ = shutdown_tx.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), async {
        let _ = tokio::join!(
            handle.task,
            cert_task,
            prune_task,
            heartbeat_task,
            digest_task
        );
    })
    .await;
    Ok(())
}

fn init_tracing() {
    // Distinguish "unset" (silent default) from "set but invalid" (warn, so a
    // typo'd filter isn't silently ignored). Tracing isn't up yet, so use stderr.
    let filter = match std::env::var("HORA_LOG") {
        Ok(value) => EnvFilter::try_new(&value).unwrap_or_else(|err| {
            eprintln!("warning: invalid HORA_LOG {value:?} ({err}); using info");
            EnvFilter::new("info")
        }),
        Err(_) => EnvFilter::new("info"),
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

/// Resolve when the process receives a shutdown signal. Listens for Ctrl-C on
/// every platform and, on Unix, also `SIGTERM` - the signal `docker stop` and
/// most init systems send - so the server drains in-flight requests cleanly
/// instead of being killed after the grace period.
async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!("failed to listen for Ctrl-C: {err}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::error!("failed to listen for SIGTERM: {err}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|&part| part.to_owned()).collect()
    }

    #[test]
    fn announce_args_split_flags_title_and_body() {
        let (title, body, severity, until) = parse_announce_args(&strings(&[
            "Fiber",
            "cut,",
            "ETA",
            "6pm",
            "--severity",
            "warning",
            "--until",
            "4h",
        ]))
        .expect("parse");
        assert_eq!(title, "Fiber");
        assert_eq!(body, "cut, ETA 6pm");
        assert_eq!(severity, "warning");
        assert!(until.is_some());

        assert!(parse_announce_args(&strings(&["--severity", "warning"])).is_err());
        assert!(parse_announce_args(&strings(&["t", "--severity", "panic"])).is_err());
        assert!(parse_announce_args(&strings(&["t", "--until", "nope"])).is_err());
    }

    #[test]
    fn until_takes_durations_and_next_clock_time() {
        let noon = 86_400 * 10 + 12 * 3600; // some UTC noon
        assert_eq!(parse_until("4h", noon), Some(noon + 4 * 3600));
        // 18:00 is still ahead today.
        assert_eq!(parse_until("18:00", noon), Some(noon + 6 * 3600));
        // 09:00 already passed: tomorrow.
        assert_eq!(parse_until("09:00", noon), Some(noon + 21 * 3600));
        assert_eq!(parse_until("25:00", noon), None);
        assert_eq!(parse_until("garbage", noon), None);
    }
}
