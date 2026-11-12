//! Domain-expiry monitoring: the daily RDAP lookups (see [`crate::rdap`]) for
//! monitors with a `domain_expiry`, and the alert when one nears expiration.
//! Run by the certificate watcher's tick ([`crate::cert::spawn_watcher`]).

use crate::db::Store;
use hora_notify::Event;
use tracing::{info, warn};

use crate::SECONDS_PER_DAY;
use crate::alert_once::{self, STAGE_WARNING, Watch};
use crate::config::Config;
use crate::db;
use crate::notifications::Notifiers;

/// RDAP lookups happen at most this often per monitor (just under a day, so
/// the 12h ticks land on a daily cadence): registries rate-limit, the answer
/// moves yearly, and the gate is the stored `checked_at`, so a restart never
/// re-queries early.
const DOMAIN_CHECK_SECS: i64 = 20 * 3600;

/// One pass of the RDAP domain-expiry checks: for each monitor with a
/// `domain_expiry`, refresh the registry's expiration date at most daily
/// (gated on the stored `checked_at`, so restarts never re-query early) and
/// alert once when it enters the warning window - the same edge-triggered,
/// maintenance-muted policy as the certificate expiry in [`crate::cert`],
/// with the same stored mark (see [`crate::alert_once`]): no repeat after a
/// restart, a retry while no channel accepted the alert, and a new alert once
/// a renewed domain nears its next expiration.
pub(crate) async fn check_domains(
    store: &Store,
    snapshot: &Config,
    notifier: &Notifiers,
    client: &reqwest::Client,
    now: i64,
) {
    let threshold_days = i64::from(snapshot.alerts.domain_expiry_days);
    for monitor in &snapshot.monitors {
        let Some(domain) = &monitor.domain_expiry else {
            continue;
        };
        let stored = match db::domain_expiry(store, &monitor.id).await {
            Ok(stored) => stored,
            Err(err) => {
                warn!(monitor = %monitor.id, "failed to read domain expiry: {err:#}");
                continue;
            }
        };
        // A changed `domain_expiry` in the config re-queries immediately.
        let expires_at = match stored {
            Some((ref stored_domain, expires_at, checked_at))
                if stored_domain == domain && now - checked_at < DOMAIN_CHECK_SECS =>
            {
                expires_at
            }
            _ => match crate::rdap::domain_expiration(client, domain).await {
                Ok(expires_at) => {
                    if let Err(err) =
                        db::upsert_domain_expiry(store, &monitor.id, domain, expires_at, now).await
                    {
                        warn!(monitor = %monitor.id, "failed to store domain expiry: {err:#}");
                    }
                    let days_left = (expires_at - now) / SECONDS_PER_DAY;
                    info!(monitor = %monitor.id, domain, days_left, "checked domain expiry (RDAP)");
                    expires_at
                }
                Err(err) => {
                    warn!(monitor = %monitor.id, domain, "RDAP domain check failed: {err:#}");
                    continue;
                }
            },
        };

        // Mute (without recording the alert) during maintenance, so it still
        // fires once the window ends.
        if crate::planned::in_maintenance(store, snapshot, &monitor.id, chrono::Utc::now()).await {
            continue;
        }
        let days_left = (expires_at - now) / SECONDS_PER_DAY;
        if days_left > threshold_days {
            alert_once::rearm(store, Watch::DomainExpiry, &monitor.id).await;
            continue;
        }
        if !alert_once::should_alert(store, Watch::DomainExpiry, &monitor.id, STAGE_WARNING).await {
            continue;
        }
        let event = Event::DomainExpiring {
            monitor: &monitor.name,
            domain,
            secs_left: expires_at - now,
        };
        if alert_once::dispatch(notifier, event, monitor.notify.as_deref(), &monitor.id).await {
            alert_once::mark_delivered(store, Watch::DomainExpiry, &monitor.id, STAGE_WARNING)
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(hook: &str) -> Config {
        crate::config::parse(&format!(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "site"
            name = "Site"
            target = "https://localhost:1/"
            interval_secs = 60
            timeout_secs = 5
            domain_expiry = "example.com"
            [[channels]]
            name = "hook"
            type = "webhook"
            url = "{hook}"
            "#
        ))
        .expect("config")
    }

    /// The stored expiration is fresh, so no RDAP lookup happens: only the
    /// alert policy runs. Nothing is carried between the calls but the
    /// database, as across a restart.
    #[tokio::test]
    async fn an_expiring_domain_alerts_once_delivered_and_again_after_a_renewal() {
        let store = Store::in_memory().await;
        let client = crate::http::client(None).unwrap();
        let now = 1_800_000_000;
        let expires = now + 5 * SECONDS_PER_DAY;
        db::upsert_domain_expiry(&store, "site", "example.com", expires, now)
            .await
            .unwrap();

        // Every channel refuses: the alert stays owed.
        let refusing = config(&crate::testing::webhook_refusing().await);
        let notifier = crate::notifications::shared(&refusing, &client);
        check_domains(&store, &refusing, &notifier, &client, now).await;

        let (hook, posts) = crate::testing::webhook_sink().await;
        let working = config(&hook);
        let notifier = crate::notifications::shared(&working, &client);
        check_domains(&store, &working, &notifier, &client, now).await;
        check_domains(&store, &working, &notifier, &client, now).await;
        assert_eq!(crate::testing::events(&posts), ["domain_expiring"]);

        // Renewed for a year, then that year nearly over.
        let renewed = expires + 365 * SECONDS_PER_DAY;
        db::upsert_domain_expiry(&store, "site", "example.com", renewed, now)
            .await
            .unwrap();
        check_domains(&store, &working, &notifier, &client, now).await;
        assert_eq!(crate::testing::events(&posts).len(), 1);
        let later = renewed - SECONDS_PER_DAY;
        db::upsert_domain_expiry(&store, "site", "example.com", renewed, later)
            .await
            .unwrap();
        check_domains(&store, &working, &notifier, &client, later).await;
        assert_eq!(
            crate::testing::events(&posts),
            ["domain_expiring", "domain_expiring"]
        );
    }
}
