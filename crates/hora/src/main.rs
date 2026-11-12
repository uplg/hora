//! Hora - a tiny self-hosted uptime monitor.
//!
//! Wires the pieces together: load config, open the database, start the
//! supervisor (which owns the live config and notification channels), spawn the
//! certificate watcher and pruner, and serve the status page and JSON API.
//! Plain `hora` runs the monitor ([`serve()`]); anything else is a one-shot
//! subcommand (the [`cli`] module), dispatched here.

mod cli;
mod listener;
mod serve;
mod top;

use std::process::ExitCode;

use cli::{CliError, usage};
use serve::{init_tracing, serve};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // Plain `hora` (no arguments) starts the monitor; anything else is a
    // one-shot subcommand. Either way the outcome comes back here instead of a
    // `process::exit` deep inside, so destructors run (the SQLite pool closes,
    // the terminal is restored) before the process ends.
    let outcome = if args.len() > 1 {
        run_subcommand(&args).await
    } else {
        init_tracing();
        serve().await.map_err(CliError::from)
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => err.report(),
    }
}

/// Dispatch a CLI subcommand (`args[1]`; the caller checked it exists).
async fn run_subcommand(args: &[String]) -> Result<(), CliError> {
    let rest = &args[2..];
    match args[1].as_str() {
        "import" => cli::admin::import_kuma(rest),
        "check" => cli::admin::check_config(rest),
        "test-alert" => {
            // Tracing first: delivery failures surface as per-channel
            // warnings from the notifiers, and that is the whole point.
            init_tracing();
            cli::admin::test_alert(rest.first().map(String::as_str)).await
        }
        "backup" => {
            let dest = rest
                .first()
                .ok_or_else(|| usage("Usage: hora backup <destination.db>"))?;
            cli::admin::backup(dest).await
        }
        "compact" => cli::compact::compact(rest).await,
        "incidents" => {
            let limit = cli::parse_limit(rest.first(), "Usage: hora incidents [limit]")?;
            cli::history::list_incidents(limit).await
        }
        "annotate" => match rest {
            [id, note @ ..] if !note.is_empty() => {
                cli::history::annotate(id, &note.join(" ")).await
            }
            _ => Err(usage(
                "Usage: hora annotate <incident-id|last> <note>\n\
                 An empty note (\"\") clears the annotation.",
            )),
        },
        "event" => cli::annotations::event(rest).await,
        "timeline" => cli::history::timeline(rest).await,
        "peers" => cli::peers::peers(rest).await,
        "postmortem" => {
            let id = rest
                .first()
                .ok_or_else(|| usage("Usage: hora postmortem <incident-id|last>"))?;
            cli::history::postmortem(id).await
        }
        "silence" => cli::annotations::silence(rest).await,
        "maintenance" => cli::annotations::maintenance(rest).await,
        "digest" => cli::admin::digest_preview().await,
        "report" => cli::history::report(rest.first().map(String::as_str)).await,
        "doctor" => cli::admin::doctor().await,
        "tune" => cli::tune::tune(rest).await,
        "probe" => cli::probe::probe(rest).await,
        "announce" => cli::annotations::announce(rest).await,
        "top" => top::run(rest).await.map_err(CliError::from),
        "--version" | "-V" => {
            println!("hora {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--help" | "-h" => {
            cli::print_help();
            Ok(())
        }
        unknown => Err(usage(format!(
            "Unknown command: {unknown}\nRun 'hora --help' for usage information."
        ))),
    }
}
