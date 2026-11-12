//! Operating the instance: config check, test alert, backup, digest preview,
//! doctor and the Uptime Kuma import.

use anyhow::Context as _;
use hora_core::config;

use super::{CliError, find_monitor, open_database, usage};

/// `hora import kuma <backup.json>`: convert an Uptime Kuma backup to Hora
/// TOML on stdout.
pub(crate) fn import_kuma(args: &[String]) -> Result<(), CliError> {
    let [kind, json_path, ..] = args else {
        return Err(usage("Usage: hora import kuma <backup.json>"));
    };
    if kind != "kuma" {
        return Err(usage("Usage: hora import kuma <backup.json>"));
    }
    let json_str =
        std::fs::read_to_string(json_path).with_context(|| format!("reading {json_path}"))?;
    let toml_out = hora_core::import::convert_kuma_to_hora(&json_str)?;
    println!("{toml_out}");
    Ok(())
}

/// `hora check [--strict]`: validate the config and exit non-zero on error -
/// meant for CI and pre-deploy hooks. What the loader only warns about (a
/// timeout longer than its interval, a channel over cleartext http) is printed
/// and counted; `--strict` makes one warning a failure, for a pipeline that
/// wants a config the daemon starts on without a word.
pub(crate) fn check_config(args: &[String]) -> Result<(), CliError> {
    let strict = match args {
        [] => false,
        [flag] if flag == "--strict" => true,
        _ => return Err(usage("Usage: hora check [--strict]")),
    };
    let config_path = config::path();
    let (loaded, warnings) = collecting_warnings(|| {
        let loaded = config::load_from(&config_path);
        // What the daemon would say at startup about a config mounted in a
        // way that defeats live reload: one of its warnings, so said here.
        hora_core::config_watch::warn_caveats(&config_path);
        loaded
    });
    // The daemon would log these on stderr at startup: same place here, so
    // stdout stays the one-line verdict.
    for warning in &warnings {
        eprintln!("warning: {warning}");
    }
    let verdict = check_verdict(
        &config_path.display().to_string(),
        loaded.map(drop),
        warnings.len(),
        strict,
    )?;
    println!("{verdict}");
    Ok(())
}

/// The outcome of `hora check` for a load that ended in `loaded` after
/// `warnings` warnings: the line to print, or the failure.
fn check_verdict(
    path: &str,
    loaded: anyhow::Result<()>,
    warnings: usize,
    strict: bool,
) -> Result<String, CliError> {
    if let Err(err) = loaded {
        return Err(CliError::Failed(format!("Configuration error: {err:#}")));
    }
    let count = match warnings {
        0 => return Ok(format!("{path} is valid.")),
        1 => "1 warning".to_owned(),
        n => format!("{n} warnings"),
    };
    if strict {
        return Err(CliError::Failed(format!(
            "{path} is valid, {count}: failing because of --strict."
        )));
    }
    Ok(format!("{path} is valid, {count}."))
}

/// Run `load` and return, with its result, the message of every `WARN` or
/// `ERROR` event it emitted. `hora check` installs no global subscriber, so
/// without this the loader's warnings went nowhere and a config that warns
/// was simply "valid". The collector is the default subscriber for the
/// duration of the closure only, on this thread - which is where the loader
/// runs.
fn collecting_warnings<T>(load: impl FnOnce() -> T) -> (T, Vec<String>) {
    use tracing_subscriber::layer::SubscriberExt as _;

    let collected = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(WarningCollector {
        collected: std::sync::Arc::clone(&collected),
    });
    let result = tracing::subscriber::with_default(subscriber, load);
    let warnings = collected
        .lock()
        .map_or_default(|mut warnings| std::mem::take(&mut *warnings));
    (result, warnings)
}

/// A `tracing` layer that keeps the text of `WARN` and `ERROR` events.
struct WarningCollector {
    collected: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for WarningCollector {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        // `Level` orders by verbosity: ERROR < WARN < INFO.
        if *event.metadata().level() > tracing::Level::WARN {
            return;
        }
        let mut text = EventText::default();
        event.record(&mut text);
        if let Ok(mut collected) = self.collected.lock() {
            collected.push(text.0);
        }
    }
}

/// An event as one line: its message, then any structured field as
/// `name=value`.
#[derive(Default)]
struct EventText(String);

impl tracing::field::Visit for EventText {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            // The message is `format_args!`: its `Debug` is the text itself.
            let rest = std::mem::take(&mut self.0);
            let _ = write!(self.0, "{value:?}");
            if !rest.is_empty() {
                self.0.push(' ');
                self.0.push_str(&rest);
            }
        } else {
            if !self.0.is_empty() {
                self.0.push(' ');
            }
            let _ = write!(self.0, "{}={value:?}", field.name());
        }
    }
}

/// Send a test `Down` then `Recovered` through the real notification chain, so
/// an operator verifies delivery *before* the first real incident instead of
/// during it. Without an id every configured channel is exercised; with one,
/// the monitor's `notify` routing applies - testing exactly what would fire.
/// Failures surface as the notifiers' own per-channel warnings *and* as a
/// non-zero exit, so a CI pipeline can gate on the notification chain.
pub(crate) async fn test_alert(monitor_id: Option<&str>) -> Result<(), CliError> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;

    let (name, notify) = match monitor_id {
        None => ("Hora test".to_owned(), None),
        Some(id) => {
            let monitor = find_monitor(&config, id)?;
            (monitor.name.clone(), monitor.notify.clone())
        }
    };

    let client = hora_core::http::client(None).context("building HTTP client")?;
    let dispatcher = hora_core::notifications::build(&config, &client, None);
    let targeted: Vec<&str> = dispatcher
        .names()
        .filter(|channel| {
            notify
                .as_ref()
                .is_none_or(|only| only.iter().any(|name| name == channel))
        })
        .collect();
    if targeted.is_empty() {
        return Err(CliError::Failed(
            "No notification channel to test (none configured, or none routed).".to_owned(),
        ));
    }

    println!(
        "Sending a test alert (down + recovered) as {name:?} to: {}",
        targeted.join(", ")
    );
    let event = hora_core::notifications::Event::down(
        &name,
        Some("test alert sent by `hora test-alert` - not a real incident"),
    );
    let mut failed = dispatcher.dispatch(event, notify.as_deref()).await;
    let failed_recovery = dispatcher
        .dispatch(
            hora_core::notifications::Event::Recovered {
                monitor: &name,
                local_only: false,
            },
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
        Err(CliError::Failed(format!(
            "Delivery failed on: {} (the warnings above say why).",
            failed.join(", ")
        )))
    }
}

/// Snapshot the database to `dest` via `VACUUM INTO`: consistent and compacted,
/// safe while the daemon runs. Meant for cron ("a one-statement answer to 'what
/// if I lose a year of history?'").
pub(crate) async fn backup(dest: &str) -> Result<(), CliError> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    let source = &config.server.database_path;
    hora_core::db::backup_into(source, dest).await?;
    let size = std::fs::metadata(dest).map_or(0, |meta| meta.len());
    println!("Backed up {source} to {dest} ({} KiB).", size / 1024);
    Ok(())
}

/// Print the digest exactly as the `[digest]` task would send it - a dry run
/// to check the wording (and the data) without notifying anyone.
pub(crate) async fn digest_preview() -> Result<(), CliError> {
    let (config, store) = open_database().await?;
    let now = chrono::Utc::now().timestamp();
    let (period, summary) = hora_core::digest::build_summary(&store, &config, now).await?;
    println!("Hora digest ({period})");
    println!("{summary}");
    Ok(())
}

/// Diagnose the runtime environment against what the config needs: `hora
/// check` says the config is sound, `hora doctor` says the *host* can honour
/// it. Exits non-zero when a needed capability is missing.
pub(crate) async fn doctor() -> Result<(), CliError> {
    let config_path = config::path();
    let config = config::load_from(&config_path).context("loading configuration")?;
    println!(
        "hora doctor - {} ({} monitors)",
        config_path.display(),
        config.monitors.len()
    );
    println!();

    let findings = hora_core::doctor::run(&config, &config_path).await;
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
        return Err(CliError::Failed(
            "Some capabilities the configuration needs are missing.".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Warns once at load: the timeout outlives the interval.
    const WARNS: &str = "[page]\n[server]\n[[monitors]]\nid = \"api\"\nname = \"API\"\n\
         target = \"https://example.com\"\ninterval_secs = 30\ntimeout_secs = 60\n";

    fn failure(result: Result<String, CliError>) -> String {
        match result {
            Err(CliError::Failed(message)) => message,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// `hora check` used to print "valid" and nothing else for this config:
    /// the loader's warning had no subscriber to land in.
    #[test]
    fn check_collects_the_loaders_warnings() {
        let (loaded, warnings) = collecting_warnings(|| config::parse(WARNS));
        assert!(loaded.is_ok());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("monitor api: timeout_secs (60) exceeds interval_secs (30)"),
            "{warnings:?}"
        );

        let quiet = WARNS.replace("timeout_secs = 60", "timeout_secs = 10");
        let (loaded, warnings) = collecting_warnings(|| config::parse(&quiet));
        assert!(loaded.is_ok());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn collected_events_keep_their_fields_and_skip_info() {
        let ((), warnings) = collecting_warnings(|| {
            tracing::info!("not a warning");
            tracing::warn!(peer = "b", "link is slow");
            tracing::error!("broken");
        });
        assert_eq!(warnings, ["link is slow peer=\"b\"", "broken"]);
    }

    #[test]
    fn check_counts_warnings_and_strict_fails_on_one() {
        let verdict = |warnings, strict| check_verdict("config.toml", Ok(()), warnings, strict);
        assert_eq!(verdict(0, false).unwrap(), "config.toml is valid.");
        assert_eq!(verdict(0, true).unwrap(), "config.toml is valid.");
        assert_eq!(
            verdict(1, false).unwrap(),
            "config.toml is valid, 1 warning."
        );
        assert_eq!(
            verdict(2, false).unwrap(),
            "config.toml is valid, 2 warnings."
        );
        assert_eq!(
            failure(verdict(2, true)),
            "config.toml is valid, 2 warnings: failing because of --strict."
        );
        // An error is an error, warnings or not.
        let broken = check_verdict("config.toml", Err(anyhow::anyhow!("bad")), 1, false);
        assert_eq!(failure(broken), "Configuration error: bad");
    }

    #[test]
    fn check_takes_strict_and_nothing_else() {
        let unknown = check_config(&["--strikt".to_owned()]);
        assert!(matches!(unknown, Err(CliError::Usage(_))), "{unknown:?}");
    }
}
