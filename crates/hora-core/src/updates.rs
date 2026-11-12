//! Hora's own updates ("Hora v0.12.0 is out (running 0.11.0)").
//!
//! The release watch of the monitors ([`crate::release`]), pointed at Hora
//! itself: GitHub is asked for Hora's latest release at most every
//! [`CHECK_SECS`], and one newer than this build is told to the operator
//! twice over - a banner on their status page (read from the [`UpdateSlot`])
//! for as long as it is newer, and one message through the channels, once
//! per release (the release told is stored, so a restart does not repeat
//! it). `[updates] check = false` asks nothing.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use hora_notify::Event;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::config::Config;
use crate::db::{self, Store};
use crate::notifications::Notifiers;

/// Where Hora is published.
pub const PROJECT: &str = "uplg/hora";

/// This build's version.
pub const RUNNING: &str = env!("CARGO_PKG_VERSION");

/// Ask GitHub at most this often - across restarts too, the time of the last
/// answer being stored.
const CHECK_SECS: i64 = 24 * 3600;

/// How often the task wakes: a reloaded `[updates]` applies within the hour.
const WAKE_EVERY: Duration = Duration::from_hours(1);

/// The latest release seen, as stored (JSON).
const META_LATEST: &str = "hora_latest_release";
/// The release the channels were told about.
const META_TOLD: &str = "hora_release_told";

/// The longest summary of a release's notes, in characters.
const MAX_SUMMARY_CHARS: usize = 300;

/// A newer Hora than this build: what the operator's banner says.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    /// The release's tag, `v0.12.0`.
    pub version: String,
    /// The release's page.
    pub url: String,
    /// What it brings, from its notes (may be empty).
    pub summary: String,
    /// When GitHub was last asked (unix seconds).
    pub checked_at: i64,
}

/// The newer release, if there is one: written by the update task, read by
/// the web layer for the operator's banner.
pub type UpdateSlot = Arc<ArcSwapOption<Update>>;

/// An empty slot, for wiring before the first check (and for tests).
#[must_use]
pub fn new_slot() -> UpdateSlot {
    Arc::new(ArcSwapOption::empty())
}

/// Spawn the update task. It re-reads `[updates]` on every wake-up, so a
/// reload turning the check off (or on) applies within the hour; a shutdown
/// signal stops it between wake-ups.
#[must_use]
pub fn spawn(
    store: Store,
    config: watch::Receiver<Arc<Config>>,
    notifier: Notifiers,
    client: reqwest::Client,
    slot: UpdateSlot,
    mut shutdown: watch::Receiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let snapshot = config.borrow().clone();
            let now = chrono::Utc::now().timestamp();
            tokio::select! {
                () = check(&store, &snapshot, &notifier, &client, &slot, now) => {}
                _ = shutdown.changed() => break,
            }
            tokio::select! {
                () = tokio::time::sleep(WAKE_EVERY) => {}
                _ = shutdown.changed() => break,
            }
        }
    })
}

/// One pass: refresh the latest release (gated on the stored `checked_at`),
/// then tell the operator if it is newer than this build.
async fn check(
    store: &Store,
    config: &Config,
    notifier: &Notifiers,
    client: &reqwest::Client,
    slot: &UpdateSlot,
    now: i64,
) {
    if !config.updates.check {
        slot.store(None);
        return;
    }
    let stored = stored(store).await;
    let latest = match stored {
        Some(stored) if now - stored.checked_at < CHECK_SECS => stored,
        stored => match crate::release::latest(client, PROJECT).await {
            Ok(release) => {
                let latest = Update {
                    summary: summary(&release.notes),
                    version: release.tag,
                    url: release.url,
                    checked_at: now,
                };
                if let Ok(json) = serde_json::to_string(&latest)
                    && let Err(err) = db::meta_set(store, META_LATEST, &json).await
                {
                    warn!("failed to store Hora's latest release: {err:#}");
                }
                info!(latest = %latest.version, running = RUNNING, "checked Hora's latest release");
                latest
            }
            Err(err) => {
                warn!("could not check for a new Hora release: {err:#}");
                // Keep telling what was last known rather than nothing.
                match stored {
                    Some(stored) => stored,
                    None => return,
                }
            }
        },
    };
    tell(store, config, notifier, slot, latest, RUNNING).await;
}

/// The latest release as last stored, if any (an unreadable row is none).
async fn stored(store: &Store) -> Option<Update> {
    let json = db::meta_get(store, META_LATEST).await.ok()??;
    serde_json::from_str(&json).ok()
}

/// Show `latest` on the operator's page while it is newer than `running`,
/// and send it through the channels once.
async fn tell(
    store: &Store,
    config: &Config,
    notifier: &Notifiers,
    slot: &UpdateSlot,
    latest: Update,
    running: &str,
) {
    if !crate::release::is_newer(&latest.version, running) {
        slot.store(None);
        return;
    }
    let told = db::meta_get(store, META_TOLD).await.ok().flatten();
    let latest = Arc::new(latest);
    slot.store(Some(Arc::clone(&latest)));
    if told.as_deref() == Some(latest.version.as_str()) {
        return;
    }
    info!(latest = %latest.version, running, "a new Hora release is out");
    notifier
        .load_full()
        .dispatch(
            Event::ReleaseAvailable(hora_notify::Release {
                monitor: "Hora",
                project: PROJECT,
                current: running,
                latest: &latest.version,
                url: &latest.url,
                notes: (!latest.summary.is_empty()).then_some(latest.summary.as_str()),
            }),
            config.updates.notify.as_deref(),
        )
        .await;
    if let Err(err) = db::meta_set(store, META_TOLD, &latest.version).await {
        warn!("failed to record the Hora release told: {err:#}");
    }
}

/// What a release brings, in plain text: the opening paragraph of its notes
/// (Hora's changelog entries open with one), or else the bold leads of its
/// first items ("- **Your logo in the header.** ..."). Markdown links keep
/// their text; capped at [`MAX_SUMMARY_CHARS`] on a word.
fn summary(notes: &str) -> String {
    let paragraphs: Vec<String> = notes
        .split("\n\n")
        .map(|paragraph| {
            paragraph
                .lines()
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|paragraph| !paragraph.is_empty() && !paragraph.starts_with('#'))
        .collect();
    let text = match paragraphs.first() {
        Some(first) if !first.starts_with("- ") => plain(first),
        _ => {
            let leads: Vec<String> = notes
                .lines()
                .filter_map(|line| line.trim().strip_prefix("- **"))
                .filter_map(|rest| rest.split_once("**").map(|(lead, _)| plain(lead)))
                .take(3)
                .collect();
            leads.join(" ")
        }
    };
    cap(&text)
}

/// Markdown inline syntax off: `[text](url)` to `text`, no `**` nor backticks.
fn plain(markdown: &str) -> String {
    let mut out = String::with_capacity(markdown.len());
    let mut rest = markdown;
    while let Some(open) = rest.find('[') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after
            .split_once("](")
            .and_then(|(text, tail)| tail.find(')').map(|close| (text, &tail[close + 1..])))
        {
            Some((text, tail)) if !text.contains('[') => {
                out.push_str(text);
                rest = tail;
            }
            _ => {
                out.push('[');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace('`', "").trim().to_owned()
}

/// `text` within [`MAX_SUMMARY_CHARS`], cut on a word with an ellipsis.
fn cap(text: &str) -> String {
    if text.chars().count() <= MAX_SUMMARY_CHARS {
        return text.to_owned();
    }
    let cut: String = text.chars().take(MAX_SUMMARY_CHARS).collect();
    let cut = cut.rsplit_once(' ').map_or(cut.as_str(), |(head, _)| head);
    format!("{}…", cut.trim_end_matches([',', ';', ':', '.']))
}

/// The release-notes summary for the fuzz targets in `fuzz/` (built with
/// `--cfg fuzzing` only).
#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzz {
    /// [`super::summary`].
    #[must_use]
    pub fn summary(notes: &str) -> String {
        super::summary(notes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTES_0_11: &str = "A new face and a hardening release. Hora gets its brand (the night watch,\n\
        a little owl) and a status page redesigned around it. See\n\
        [UPGRADES.md](UPGRADES.md#0100--0110) for what to check before upgrading.\n\n\
        ### Added\n\n\
        - **A redesigned status page.** The hour and one sentence.\n";

    #[test]
    fn the_summary_is_the_opening_paragraph_in_plain_text() {
        assert_eq!(
            summary(NOTES_0_11),
            "A new face and a hardening release. Hora gets its brand (the night watch, \
             a little owl) and a status page redesigned around it. See UPGRADES.md \
             for what to check before upgrading."
        );
    }

    #[test]
    fn without_an_opening_paragraph_the_items_lead() {
        let notes = "### Added\n\n\
            - **Your logo in the header.** `[page] logo` shows it.\n\
            - **Update notices.** The operator hears of new releases.\n\n\
            ### Fixed\n\n\
            - A typo.\n";
        assert_eq!(summary(notes), "Your logo in the header. Update notices.");
        assert_eq!(summary(""), "");
    }

    #[test]
    fn a_long_summary_is_cut_on_a_word() {
        let long = "word ".repeat(100);
        let cut = summary(&long);
        assert!(cut.chars().count() <= MAX_SUMMARY_CHARS + 1, "{cut}");
        assert!(cut.ends_with("word…"), "{cut}");
    }

    #[test]
    fn plain_keeps_link_text_and_odd_brackets() {
        assert_eq!(
            plain("see [the guide](https://x/y) now"),
            "see the guide now"
        );
        assert_eq!(plain("an [unclosed bracket"), "an [unclosed bracket");
        assert_eq!(plain("**bold** and `code`"), "bold and code");
    }

    fn update(version: &str) -> Update {
        Update {
            version: version.to_owned(),
            url: format!("https://github.com/uplg/hora/releases/tag/{version}"),
            summary: "Your logo in the header.".to_owned(),
            checked_at: 0,
        }
    }

    /// A newer release shows in the slot and is sent once; the same one
    /// again (a later pass, a restart) is not resent; once running it, the
    /// banner goes.
    #[tokio::test]
    async fn a_newer_release_is_shown_and_sent_once() {
        let (url, posts) = crate::testing::webhook_sink().await;
        let config = crate::config::parse(&format!(
            r#"
            [page]
            [server]
            [[channels]]
            name = "ops"
            type = "webhook"
            url = "{url}"
            "#
        ))
        .expect("config");
        let store = Store::in_memory().await;
        let client = crate::http::client(None).expect("client");
        let notifier = crate::notifications::shared(&config, &client);
        let slot = new_slot();

        tell(
            &store,
            &config,
            &notifier,
            &slot,
            update("v0.12.0"),
            "0.11.0",
        )
        .await;
        assert_eq!(slot.load_full().expect("banner").version, "v0.12.0");
        tell(
            &store,
            &config,
            &notifier,
            &slot,
            update("v0.12.0"),
            "0.11.0",
        )
        .await;
        let sent = posts.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "{sent:?}");
        assert_eq!(sent[0]["event"], "release_available");
        assert_eq!(sent[0]["latest"], "v0.12.0");
        assert_eq!(sent[0]["message"], "Your logo in the header.");

        tell(
            &store,
            &config,
            &notifier,
            &slot,
            update("v0.12.0"),
            "0.12.0",
        )
        .await;
        assert!(slot.load_full().is_none());
        assert_eq!(posts.lock().unwrap().len(), 1);
    }

    /// `notify = []` keeps the banner but sends nothing; `check = false`
    /// clears the banner without asking GitHub.
    #[tokio::test]
    async fn the_channels_and_the_check_can_be_turned_off() {
        let (url, posts) = crate::testing::webhook_sink().await;
        let toml = |updates: &str| {
            format!(
                r#"
                [page]
                [server]
                [updates]
                {updates}
                [[channels]]
                name = "ops"
                type = "webhook"
                url = "{url}"
                "#
            )
        };
        let quiet = crate::config::parse(&toml("notify = []")).expect("config");
        let store = Store::in_memory().await;
        let client = crate::http::client(None).expect("client");
        let notifier = crate::notifications::shared(&quiet, &client);
        let slot = new_slot();
        tell(
            &store,
            &quiet,
            &notifier,
            &slot,
            update("v0.12.0"),
            "0.11.0",
        )
        .await;
        assert!(slot.load_full().is_some());
        assert!(posts.lock().unwrap().is_empty());

        let off = crate::config::parse(&toml("check = false")).expect("config");
        check(&store, &off, &notifier, &client, &slot, 0).await;
        assert!(slot.load_full().is_none());
    }

    #[test]
    fn unknown_channels_are_refused() {
        let err = crate::config::parse(
            r#"
            [page]
            [server]
            [updates]
            notify = ["nope"]
            "#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("nope"), "{err:#}");
    }
}
