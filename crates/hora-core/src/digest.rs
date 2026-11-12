//! The periodic digest: a recap of the last seven days through the
//! notification channels - "this week: 99.97 %, 2 incidents, budget 18 min of
//! 43 min left". All the data already exists (checks, incidents, SLOs); this
//! just words it. By construction it never alerts, so it carries zero
//! false-positive risk - the one notification that is allowed to be routine.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use crate::db::Store;
use hora_notify::Event;
use tokio::sync::watch;
use tracing::{info, warn};

use crate::SECONDS_PER_DAY;
use crate::config::Config;
use crate::notifications::Notifiers;
use crate::{db, fmt, slo};

/// Where the last-sent timestamp persists, so a restart neither double-sends
/// nor forgets - and a send missed while the daemon was down catches up.
const META_KEY: &str = "digest_last_sent";

/// The window the digest covers.
const DIGEST_DAYS: i64 = 7;

/// Cap on the rendered summary, comfortably under the strictest channel
/// limit (Discord embeds: 4096 chars).
const MAX_SUMMARY_CHARS: usize = 3500;

/// Spawn the digest task. It self-gates on the `[digest]` config section and
/// re-reads it on every wake-up, so adding (or editing) the section on a live
/// reload takes effect without a restart. A shutdown signal stops it between
/// ticks.
#[must_use]
pub fn spawn(
    store: Store,
    config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let snapshot = config.borrow().clone();
            let sleep_secs = match &snapshot.digest {
                Some(digest) => tick(&store, &snapshot, digest, &notifier).await,
                // No [digest] yet: check back occasionally for a live reload.
                None => 300,
            };
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(sleep_secs)) => {}
                _ = shutdown.changed() => break,
            }
        }
    })
}

/// One evaluation: send if a scheduled occurrence has passed since the last
/// send, and return how long to sleep. Sleeps are capped at an hour so a
/// reloaded schedule applies reasonably soon.
async fn tick(
    store: &Store,
    config: &Config,
    digest: &crate::config::Digest,
    notifier: &Notifiers,
) -> u64 {
    // Validated at config load; a parse failure here is defensive only.
    let Ok(cron) = crate::config::parse_cron(&digest.schedule) else {
        warn!("invalid digest schedule {:?}", digest.schedule);
        return 3600;
    };
    let now = chrono::Utc::now().timestamp();

    // First run ever: baseline now, so enabling the digest mid-week waits for
    // the next scheduled slot instead of firing immediately.
    let last_sent = if let Some(last_sent) = read_last_sent(store).await {
        last_sent
    } else {
        store_last_sent(store, now).await;
        now
    };

    let Some(due) = next_occurrence(&cron, last_sent) else {
        warn!(
            "digest schedule {:?} has no next occurrence",
            digest.schedule
        );
        return 3600;
    };
    if due > now {
        return u64::try_from(due - now).unwrap_or(3600).clamp(1, 3600);
    }

    // Due (or missed while the daemon was down): send one digest, however
    // many occurrences were missed, and move the baseline to now.
    match build_summary(store, config, now).await {
        Ok((period, summary)) => {
            info!(%period, "sending digest");
            let dispatcher = notifier.load_full();
            let failed = dispatcher
                .dispatch(
                    Event::Digest {
                        period: &period,
                        summary: &summary,
                    },
                    digest.notify.as_deref(),
                )
                .await;
            if !failed.is_empty() {
                warn!("digest delivery failed on: {}", failed.join(", "));
            }
        }
        Err(err) => warn!("failed to build the digest: {err:#}"),
    }
    // Advance even on a failed build/delivery: the failure is logged, and
    // retrying every minute until next week would spam a broken channel.
    store_last_sent(store, now).await;
    60
}

/// The first scheduled occurrence strictly after `last_sent`.
fn next_occurrence(cron: &croner::Cron, last_sent: i64) -> Option<i64> {
    let from = chrono::DateTime::from_timestamp(last_sent, 0)?;
    Some(cron.find_next_occurrence(&from, false).ok()?.timestamp())
}

async fn read_last_sent(store: &Store) -> Option<i64> {
    match db::meta_get(store, META_KEY).await {
        Ok(value) => value.and_then(|value| value.parse().ok()),
        Err(err) => {
            warn!("failed to read digest state: {err:#}");
            None
        }
    }
}

async fn store_last_sent(store: &Store, now: i64) {
    if let Err(err) = db::meta_set(store, META_KEY, &now.to_string()).await {
        warn!("failed to store digest state: {err:#}");
    }
}

/// Build the digest for the seven days ending at `now`: `(period, summary)`.
/// One line per monitor in configuration order - uptime, incidents in the
/// window, error budget when an SLO is configured.
///
/// # Errors
///
/// Returns an error if a database read fails.
pub async fn build_summary(
    store: &Store,
    config: &Config,
    now: i64,
) -> anyhow::Result<(String, String)> {
    let since = now - DIGEST_DAYS * SECONDS_PER_DAY;
    let period = format!(
        "{} \u{2192} {}",
        fmt::short_date(since),
        fmt::short_date(now)
    );

    // A week of checks: the hourly roll-ups plus the raw edges, not a scan
    // of every raw row in the window.
    let availability: std::collections::HashMap<String, (i64, i64)> =
        db::window_stats_all(store, since)
            .await?
            .into_iter()
            .map(|(id, stats)| (id, (stats.available, stats.total)))
            .collect();
    // Incidents that overlapped the window: still open, ended inside it, or
    // started inside it. Selected in SQL, so a busy week is never truncated.
    let incidents = db::incidents_between(store, since, now).await?;
    let ongoing = incidents.iter().filter(|i| i.ended_at.is_none()).count();

    let (mut up_sum, mut total_sum) = (0_i64, 0_i64);
    let mut lines = Vec::with_capacity(config.monitors.len());
    for monitor in &config.monitors {
        let (up, total) = availability.get(&monitor.id).copied().unwrap_or((0, 0));
        up_sum += up;
        total_sum += total;

        let mut line = format!("- {}: {}", monitor.name, fmt::pct(up, total));
        let count = incidents
            .iter()
            .filter(|incident| incident.monitor_id == monitor.id)
            .count();
        if count > 0 {
            let plural = if count > 1 { "s" } else { "" };
            let _ = write!(line, ", {count} incident{plural}");
        }
        if let Some(slo_bp) = monitor.slo_uptime {
            line.push_str(&budget_phrase(store, monitor, slo_bp, now).await);
        }
        lines.push(line);
    }

    let incident_count = incidents.len();
    let plural = if incident_count == 1 { "" } else { "s" };
    let mut summary = format!(
        "{} overall, {incident_count} incident{plural}",
        fmt::pct(up_sum, total_sum)
    );
    if ongoing > 0 {
        let _ = write!(summary, " ({ongoing} ongoing)");
    }
    for line in lines {
        // Bound the whole text under the strictest channel limit; the page
        // has the full picture, the digest is the headline.
        if summary.len() + line.len() + 1 > MAX_SUMMARY_CHARS {
            summary.push_str("\n(more monitors omitted)");
            break;
        }
        summary.push('\n');
        summary.push_str(&line);
    }
    Ok((period, summary))
}

/// `", budget 18 min of 43 min left (30 d)"` for a monitor with an SLO - or how far
/// over budget it is. An unreadable history drops the clause rather than
/// publishing a wrong number.
async fn budget_phrase(
    store: &Store,
    monitor: &crate::config::Monitor,
    slo_bp: u32,
    now: i64,
) -> String {
    let window_days = monitor.slo_window_days();
    let since = now - i64::from(window_days) * SECONDS_PER_DAY;
    let Ok((available, total)) = db::availability(store, &monitor.id, since).await else {
        return String::new();
    };
    let budget = slo::budget_minutes(window_days, slo_bp);
    let remaining = slo::remaining_minutes(window_days, slo_bp, available, total);
    if remaining >= 0 {
        format!(
            ", budget {} of {} left ({window_days} d)",
            fmt::duration(remaining.saturating_mul(60)),
            fmt::duration(budget.saturating_mul(60))
        )
    } else {
        format!(
            ", budget exhausted ({} over, {window_days} d)",
            fmt::duration(remaining.saturating_neg().saturating_mul(60))
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_occurrence_is_strictly_after_last_sent() {
        let cron: croner::Cron = "0 8 * * 1".parse().unwrap(); // Mondays 08:00
        // 2026-06-08 was a Monday; sent exactly at 08:00 UTC.
        let sent = chrono::DateTime::parse_from_rfc3339("2026-06-08T08:00:00Z")
            .unwrap()
            .timestamp();
        let next = next_occurrence(&cron, sent).unwrap();
        let expected = chrono::DateTime::parse_from_rfc3339("2026-06-15T08:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(next, expected);
    }
}
