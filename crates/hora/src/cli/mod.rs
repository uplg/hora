//! The one-shot subcommands, one file per family, and what they share: the
//! error type, argument parsing helpers and the database opener.

pub(crate) mod admin;
pub(crate) mod annotations;
pub(crate) mod compact;
pub(crate) mod history;
pub(crate) mod peers;
pub(crate) mod probe;
#[cfg(test)]
mod tests;
pub(crate) mod tune;

use std::fmt::Write as _;
use std::process::ExitCode;

use anyhow::Context as _;
use hora_core::config;

/// Why a command ended unsuccessfully, turned into stderr output and an exit
/// code by [`CliError::report`].
#[derive(Debug)]
pub(crate) enum CliError {
    /// A malformed invocation (missing argument, bad flag value, unknown id):
    /// the message, usually a usage line, goes to stderr verbatim.
    Usage(String),
    /// The command ran and reached a negative verdict (delivery failed, a
    /// capability is missing, no such incident): the message goes to stderr
    /// verbatim.
    Failed(String),
    /// A negative verdict already printed on stdout (a down probe, peer
    /// drift): nothing more to say, only the exit code.
    Silent,
    /// Anything else (config, database, I/O), printed exactly as an error
    /// returned from `main` always was: `Error: ...` with its causes.
    Other(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for CliError {
    fn from(err: E) -> Self {
        Self::Other(err.into())
    }
}

impl CliError {
    /// Print the error and return the process exit code. Every failure exits
    /// 1, usage errors included: scripts already gate on that (`hora probe url
    /// && deploy`), so the conventional 2 for usage would be a breaking change.
    pub(crate) fn report(self) -> ExitCode {
        match self {
            Self::Usage(message) | Self::Failed(message) => eprintln!("{message}"),
            Self::Silent => {}
            Self::Other(err) => eprintln!("Error: {err:?}"),
        }
        ExitCode::FAILURE
    }
}

/// A [`CliError::Usage`] from any message.
pub(crate) fn usage(message: impl Into<String>) -> CliError {
    CliError::Usage(message.into())
}

/// An optional `[limit]` argument (default 20, at least 1); `usage_line` is
/// the error when it is not a number.
pub(crate) fn parse_limit(raw: Option<&String>, usage_line: &str) -> Result<i64, CliError> {
    raw.map_or(Ok(20), |raw| raw.parse::<i64>())
        .map(|limit| limit.max(1))
        .map_err(|_| usage(usage_line))
}

/// A `--days N` value: a positive whole number of days.
pub(crate) fn parse_days(raw: Option<&String>) -> Result<i64, CliError> {
    raw.and_then(|raw| raw.parse::<i64>().ok())
        .filter(|&days| days > 0)
        .ok_or_else(|| usage("--days must be a positive number of days"))
}

/// The configured monitor `id`, or a usage error listing the configured ids:
/// a typo'd id fails loudly rather than testing, tuning or silencing nothing.
pub(crate) fn find_monitor<'c>(
    config: &'c hora_core::config::Config,
    id: &str,
) -> Result<&'c hora_core::config::Monitor, CliError> {
    config
        .find_monitor(id)
        .ok_or_else(|| unknown_monitor(config, id))
}

/// The usage error for an unknown monitor id, listing the configured ones.
pub(crate) fn unknown_monitor(config: &hora_core::config::Config, id: &str) -> CliError {
    let mut message = format!("Unknown monitor {id:?}. Configured ids:");
    for monitor in &config.monitors {
        let _ = write!(message, "\n  {}", monitor.id);
    }
    usage(message)
}

/// Resolve an incident argument: a numeric id, or `last` for the most recent
/// incident.
pub(crate) async fn resolve_incident_id(
    store: &hora_core::db::Store,
    arg: &str,
) -> Result<i64, CliError> {
    if arg == "last" {
        return hora_core::db::latest_incident_id(store)
            .await?
            .ok_or_else(|| CliError::Failed("No incidents recorded yet.".to_owned()));
    }
    arg.parse().map_err(|_| {
        usage(format!(
            "Invalid incident id {arg:?} (a number, or 'last')."
        ))
    })
}

/// The error for an incident id that matches no row.
pub(crate) fn no_such_incident(id: i64) -> CliError {
    CliError::Failed(format!(
        "No incident #{id}. 'hora incidents' lists the recent ones."
    ))
}

pub(crate) fn print_help() {
    println!("Hora - a tiny self-hosted uptime monitor");
    println!();
    println!("Usage: hora [COMMAND]");
    println!();
    println!("Commands:");
    println!("  import kuma <file>  Convert an Uptime Kuma backup JSON to Hora TOML (stdout)");
    println!("  check [--strict]    Validate the configuration, print its warnings and exit");
    println!("                      (--strict: a warning fails the check too)");
    println!("  doctor              Diagnose the runtime environment (IPv6, ICMP socket,");
    println!("                      DNS resolver, listen port, database)");
    println!("  tune [id] [--days N]  Replay stored history against other fail_threshold /");
    println!("                      degraded_over_ms settings and recommend per monitor");
    println!("  probe <id|target> [--confirm] [--kind http|tcp|icmp|dns]");
    println!("                      One-shot ad-hoc probe; --confirm asks the peers their verdict");
    println!("                      (an ad-hoc http target also takes --method GET|HEAD and");
    println!("                      --expect 200,401)");
    println!("  test-alert [id]     Send a test down + recovered through the configured");
    println!("                      channels (all of them, or the routed ones of monitor [id])");
    println!("  silence <ids> <for> [reason]  Mute alerts for monitors (comma-separated ids");
    println!("                      or 'all') for a duration like 10m or 1h30m (max 7d)");
    println!("  silence list        Show the active silences");
    println!("  announce <title> [body] [--severity s] [--until 4h|18:00]");
    println!("                      Pin a public banner on the status page");
    println!("  announce list / clear  Show or remove the pinned announcements");
    println!("                      ('hora announce -- clear ...' pins a title starting");
    println!("                      with list or clear)");
    println!("  silence clear       Remove every silence");
    println!("  maintenance plan (--host <name> | --monitors <ids|all>) [--at <when>]");
    println!("                      --for <duration> <reason...>");
    println!("                      Plan a maintenance window (shown on the page, alerts");
    println!("                      muted, announced to the peers for this node's host)");
    println!("  maintenance list    Show the planned and active windows");
    println!("  maintenance cancel <id>  Cancel one");
    println!("  top [--url U] [--token T] [--interval S]");
    println!("                      Live terminal dashboard over the JSON API");
    println!("  digest              Print the weekly digest (a dry run of [digest])");
    println!("  report [YYYY-MM]    Print the monthly SLA report (default: last month;");
    println!("                      the printable page is /report/YYYY-MM)");
    println!("  incidents [limit]   List recent incidents with their ids");
    println!("  annotate <id> <note>  Attach a note to an incident ('last' targets the");
    println!("                      most recent one; an empty note clears it)");
    println!("  event <title...>    Record an event marker (\"deploy api v2.3\"): shown on");
    println!("                      the charts and correlated into incidents");
    println!("  event list [limit]  List the recent event markers ('hora event -- list ...'");
    println!("                      records a title that starts with the word list)");
    println!("  postmortem <id|last>  Print an incident's auto-generated markdown");
    println!("                      post-mortem (the web twin is /incident/{{id}})");
    println!("  timeline [--days N] Unified chronology: downs/recoveries, events,");
    println!("                      pushed alerts, announcements, silences (default 7d)");
    println!("  peers diff          Compare this node's monitors with each peer's (the");
    println!("                      alignment multi-vantage confirmation relies on)");
    println!("  backup <dest.db>    Snapshot the database with VACUUM INTO");
    println!("  compact [--dry-run] [--purge-removed] [--keep]");
    println!("                      Apply the retention now and shrink the file (stop");
    println!("                      the daemon first; --keep leaves the original as .bak).");
    println!("                      --dry-run measures each table and the gain instead");
    println!("  --version, -V       Show the version");
    println!("  --help, -h          Show this help message");
}

/// The CLI's hold on `<db>.writers.lock`, kept for the life of the process
/// (one command per process): see [`hora_core::db::lock_writers`].
static WRITERS_LOCK: std::sync::OnceLock<Option<hora_core::db::DbLock>> =
    std::sync::OnceLock::new();

/// How [`open_database_with`] treats `<db>.writers.lock`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritersLock {
    /// Take it shared, like every command: refuses while `hora compact`
    /// rewrites the file.
    Shared,
    /// The caller (`hora compact`) already holds it exclusive.
    HeldExclusive,
}

/// Open the daemon's database for a CLI subcommand. Refuses to *create* one: a
/// missing file means the config points somewhere the daemon never wrote (a
/// different working directory, usually), and silently creating an empty
/// database there would only hide the mistake.
pub(crate) async fn open_database()
-> anyhow::Result<(hora_core::config::Config, hora_core::db::Store)> {
    let config = load_config()?;
    open_database_with(config, WritersLock::Shared).await
}

/// The configuration the daemon would load.
pub(crate) fn load_config() -> anyhow::Result<hora_core::config::Config> {
    config::load_from(&config::path()).context("loading configuration")
}

/// [`open_database`] for an already loaded config, taking (or not) the writers
/// lock.
pub(crate) async fn open_database_with(
    config: hora_core::config::Config,
    writers: WritersLock,
) -> anyhow::Result<(hora_core::config::Config, hora_core::db::Store)> {
    let path = &config.server.database_path;
    if path != ":memory:" && !path.starts_with("file:") && !std::path::Path::new(path).exists() {
        anyhow::bail!(
            "database {path} not found - run from the daemon's working directory, \
             or point HORA_CONFIG at its config"
        );
    }
    if writers == WritersLock::Shared && WRITERS_LOCK.get().is_none() {
        let lock = hora_core::db::lock_writers(path, false).map_err(|err| match err {
            hora_core::db::LockError::Busy(_) => {
                anyhow::anyhow!("hora compact is rewriting {path} - try again once it has finished")
            }
            other @ hora_core::db::LockError::Io(..) => anyhow::Error::new(other),
        })?;
        let _ = WRITERS_LOCK.set(lock);
    }
    let store = hora_core::db::connect(path)
        .await
        .context("opening database")?;
    Ok((config, store))
}

/// `"1 day"` / `"30 days"` - the only place the lookback is pluralised.
pub(crate) fn days_label(days: i64) -> String {
    format!("{days} day{}", if days == 1 { "" } else { "s" })
}
