//! Upstream release watch ("owner/repo v1.9.2 is out (running v1.9.1)").
//!
//! A monitor says which GitHub project it runs (`release.github`) and how the
//! running version is known: written down, or asked of the service itself. The
//! project's latest published release comes from GitHub's REST API, which
//! leaves drafts and prereleases out by itself. Anonymous calls are allowed 60
//! an hour per address, so the watcher gates on the stored `checked_at`
//! rather than asking on each tick.

use crate::db::Store;
use hora_notify::Event;
use serde_json_path::JsonPath;
use tracing::{info, warn};

use crate::config::{Config, Parsed, ReleaseWatch};
use crate::db;
use crate::http::{MAX_JSON_BYTES, get_json_following, read_all_capped};
use crate::notifications::Notifiers;

/// GitHub's REST API, repositories.
const API: &str = "https://api.github.com/repos";

/// How many redirects to follow: GitHub answers a renamed or transferred
/// repository with a 301 (followed by hand, whatever the client's policy).
const MAX_REDIRECTS: usize = 3;

/// The longest text taken for a version: an answer that is not a version (an
/// HTML error page) must not end up whole in an alert.
const MAX_VERSION_LEN: usize = 64;

/// Ask GitHub for a project's latest release at most this often. Under the
/// watcher's 12-hour tick, so every tick asks; over a restart loop, so a
/// crashing daemon does not spend the anonymous API's 60 requests an hour.
const RELEASE_CHECK_SECS: i64 = 6 * 3600;

/// A published release: its tag, the page that carries its notes, and the
/// notes themselves (Markdown, empty when the release has none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Release {
    pub tag: String,
    pub url: String,
    pub notes: String,
}

/// The latest published release of `project` (`owner/repo`).
///
/// # Errors
///
/// Returns an error if the lookup fails (network, rate limit, an unknown
/// repository) or the project has published no release: tags alone are not
/// releases.
pub(crate) async fn latest(client: &reqwest::Client, project: &str) -> anyhow::Result<Release> {
    let url = format!("{API}/{project}/releases/latest");
    let headers = [
        (
            reqwest::header::ACCEPT.as_str(),
            "application/vnd.github+json",
        ),
        ("X-GitHub-Api-Version", "2022-11-28"),
    ];
    let body = match get_json_following(client, &url, &headers, MAX_REDIRECTS).await? {
        Ok(body) => body,
        Err(reqwest::StatusCode::NOT_FOUND) => anyhow::bail!(
            "no such repository, or no published release (tags alone are not releases)"
        ),
        Err(status) => anyhow::bail!("GitHub answered HTTP {status}"),
    };
    release_of(&body).ok_or_else(|| anyhow::anyhow!("no tag_name in GitHub's answer"))
}

/// The tag, page and notes of a release object of GitHub's API.
fn release_of(body: &serde_json::Value) -> Option<Release> {
    let tag = body.get("tag_name")?.as_str()?.trim();
    let url = body.get("html_url")?.as_str()?;
    let notes = body.get("body").and_then(serde_json::Value::as_str);
    (!tag.is_empty()).then(|| Release {
        tag: tag.to_owned(),
        url: url.to_owned(),
        notes: notes.unwrap_or_default().to_owned(),
    })
}

/// The version that runs: the configured literal, or what the service answers
/// at `current_url` (through `current_query` when it answers JSON).
///
/// # Errors
///
/// Returns an error if the service cannot be asked, or answers no version.
pub(crate) async fn running(
    client: &reqwest::Client,
    watch: &ReleaseWatch,
) -> anyhow::Result<String> {
    if let Some(current) = &watch.current {
        return Ok(current.trim().to_owned());
    }
    let Some(url) = &watch.current_url else {
        // Config validation requires one of the two.
        anyhow::bail!("neither current nor current_url");
    };
    let response = client.get(url).send().await?;
    let status = response.status();
    anyhow::ensure!(status.is_success(), "{url} answered HTTP {status}");
    let body = read_all_capped(response, MAX_JSON_BYTES).await?;
    // A malformed query never gets here: validation refuses it at load.
    let query = watch.current_query.as_ref().and_then(Parsed::get);
    version_in(&String::from_utf8_lossy(&body), query)
        .ok_or_else(|| anyhow::anyhow!("no version in the answer of {url}"))
}

/// The version inside a service's answer: the first node `query` matches in a
/// JSON document, or without a query the first line of the text.
fn version_in(body: &str, query: Option<&JsonPath>) -> Option<String> {
    let version = match query {
        Some(path) => {
            let value: serde_json::Value = serde_json::from_str(body).ok()?;
            match path.query(&value).first()? {
                serde_json::Value::String(text) => text.trim().to_owned(),
                serde_json::Value::Number(number) => number.to_string(),
                _ => return None,
            }
        }
        None => body.lines().next()?.trim().to_owned(),
    };
    (!version.is_empty() && version.len() <= MAX_VERSION_LEN).then_some(version)
}

/// Whether `latest` is a newer version than `current`.
///
/// Versions are compared by their leading numbers ("v1.9.2", "1.9.2" and
/// "1.9.2 (a1b2c3)" are one version; "1.9" is "1.9.0"), which is what release
/// tags and the versions services report agree on. When either side has no
/// leading number ("release-42", "nightly"), any difference counts: better one
/// alert to look at than a release missed.
pub(crate) fn is_newer(latest: &str, current: &str) -> bool {
    match (numbers(latest), numbers(current)) {
        (Some(latest), Some(current)) => {
            let width = latest.len().max(current.len());
            let padded = |mut version: Vec<u64>| {
                version.resize(width, 0);
                version
            };
            padded(latest) > padded(current)
        }
        _ => latest.trim() != current.trim(),
    }
}

/// The leading dotted numbers of a version, past an optional `v`.
fn numbers(version: &str) -> Option<Vec<u64>> {
    let version = version.trim();
    let version = version.strip_prefix(['v', 'V']).unwrap_or(version);
    let end = version
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(version.len());
    let parts: Option<Vec<u64>> = version[..end]
        .trim_end_matches('.')
        .split('.')
        .map(|part| part.parse().ok())
        .collect();
    parts.filter(|parts| !parts.is_empty())
}

/// One pass of the release watches: for each monitor with a `release`, refresh
/// the project's latest release (gated on the stored `checked_at`), learn the
/// version that runs, and alert when the first is newer - once per release,
/// the release alerted for being stored once a channel accepted the alert
/// (see [`crate::alert_once::delivered`]), so a restart does not repeat it and
/// a failed one is retried.
/// Muted during maintenance without being recorded, like the certificate and
/// domain expiries.
pub(crate) async fn check_releases(
    store: &Store,
    snapshot: &Config,
    notifier: &Notifiers,
    client: &reqwest::Client,
    now: i64,
) {
    for monitor in &snapshot.monitors {
        let Some(watch) = &monitor.release else {
            continue;
        };
        let stored = match db::release_watch(store, &monitor.id).await {
            Ok(stored) => stored,
            Err(err) => {
                warn!(monitor = %monitor.id, "failed to read release watch: {err:#}");
                continue;
            }
        };
        // A changed `release.github` in the config asks again immediately.
        let stored = stored.filter(|stored| stored.project == watch.github);
        let (latest, url, alerted) = match stored {
            Some(stored) if now - stored.checked_at < RELEASE_CHECK_SECS => {
                (stored.latest, stored.url, stored.notified)
            }
            stored => match crate::release::latest(client, &watch.github).await {
                Ok(release) => {
                    if let Err(err) = db::upsert_release_watch(
                        store,
                        &monitor.id,
                        &watch.github,
                        &release.tag,
                        &release.url,
                        now,
                    )
                    .await
                    {
                        warn!(monitor = %monitor.id, "failed to store release watch: {err:#}");
                    }
                    info!(monitor = %monitor.id, project = %watch.github, latest = %release.tag, "checked latest release");
                    (release.tag, release.url, stored.and_then(|s| s.notified))
                }
                Err(err) => {
                    warn!(monitor = %monitor.id, project = %watch.github, "release check failed: {err:#}");
                    continue;
                }
            },
        };
        let current = match crate::release::running(client, watch).await {
            Ok(current) => current,
            Err(err) => {
                warn!(monitor = %monitor.id, "could not learn the running version: {err:#}");
                continue;
            }
        };
        if !crate::release::is_newer(&latest, &current)
            || alerted.as_deref() == Some(latest.as_str())
            || crate::scheduler::muted_under(store, snapshot, &monitor.id).await
        {
            continue;
        }
        let event = Event::ReleaseAvailable(hora_notify::Release {
            monitor: &monitor.name,
            project: &watch.github,
            current: &current,
            latest: &latest,
            url: &url,
            notes: None,
        });
        // Recorded only once a channel accepted it: an alert that failed
        // everywhere is sent again at the next sweep.
        if crate::alert_once::dispatch(notifier, event, monitor.notify.as_deref(), &monitor.id)
            .await
            && let Err(err) = db::mark_release_notified(store, &monitor.id, &latest).await
        {
            warn!(monitor = %monitor.id, "failed to record the release alert: {err:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn watching(hook: &str) -> Config {
        crate::config::parse(&format!(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "chat"
            name = "Chat"
            target = "https://localhost:1/"
            interval_secs = 60
            timeout_secs = 5
            release = {{ github = "owner/repo", current = "v1.9.1" }}
            [[channels]]
            name = "hook"
            type = "webhook"
            url = "{hook}"
            "#
        ))
        .expect("config")
    }

    /// The stored release is fresh (no call to GitHub) and newer than the
    /// literal `current`: only the alert policy runs.
    #[tokio::test]
    async fn a_release_alert_is_recorded_only_once_delivered() {
        let store = Store::in_memory().await;
        let client = crate::http::client(None).unwrap();
        let now = 1_800_000_000;
        db::upsert_release_watch(&store, "chat", "owner/repo", "v1.9.2", "https://x/r", now)
            .await
            .unwrap();
        let recorded = || async {
            let stored = db::release_watch(&store, "chat").await.unwrap();
            stored.and_then(|stored| stored.notified)
        };

        let refusing = watching(&crate::testing::webhook_refusing().await);
        let notifier = crate::notifications::shared(&refusing, &client);
        check_releases(&store, &refusing, &notifier, &client, now).await;
        assert_eq!(recorded().await, None, "a failed alert must stay owed");

        let (hook, posts) = crate::testing::webhook_sink().await;
        let working = watching(&hook);
        let notifier = crate::notifications::shared(&working, &client);
        check_releases(&store, &working, &notifier, &client, now).await;
        check_releases(&store, &working, &notifier, &client, now).await;
        assert_eq!(crate::testing::events(&posts), ["release_available"]);
        assert_eq!(recorded().await.as_deref(), Some("v1.9.2"));
    }

    #[test]
    fn a_later_release_is_newer_whatever_the_spelling() {
        assert!(is_newer("v1.9.2", "v1.9.1"));
        assert!(is_newer("v1.9.2", "1.9.1"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("v2.0", "1.99.99"));
        assert!(is_newer("26.7.4", "26.7.3 (build 1234)"));
    }

    #[test]
    fn the_same_or_an_older_release_is_not() {
        assert!(!is_newer("v1.9.2", "1.9.2"));
        assert!(!is_newer("v1.9", "1.9.0"));
        assert!(!is_newer("v1.9.1", "v1.9.2"));
        // Running ahead of the latest release (a build of main): nothing to say.
        assert!(!is_newer("v1.9.2", "1.10.0-dev"));
        assert!(!is_newer("1.9.2", "1.9.2 (a1b2c3)"));
    }

    #[test]
    fn without_numbers_any_difference_counts() {
        assert!(is_newer("release-43", "release-42"));
        assert!(!is_newer("nightly", " nightly "));
        assert!(is_newer("v1.2.3", "stable"));
    }

    #[test]
    fn reads_the_release_of_githubs_answer() {
        let body = serde_json::json!({
            "tag_name": "v1.9.2",
            "html_url": "https://github.com/matrix-construct/tuwunel/releases/tag/v1.9.2",
            "prerelease": false
        });
        assert_eq!(
            release_of(&body),
            Some(Release {
                tag: "v1.9.2".to_owned(),
                url: "https://github.com/matrix-construct/tuwunel/releases/tag/v1.9.2".to_owned(),
                notes: String::new(),
            })
        );
        assert_eq!(
            release_of(&serde_json::json!({ "message": "Not Found" })),
            None
        );
        assert_eq!(
            release_of(&serde_json::json!({ "tag_name": " ", "html_url": "x" })),
            None
        );
    }

    #[test]
    fn finds_the_version_a_service_answers() {
        let path = |query: &str| JsonPath::parse(query).expect("valid query");
        let json = r#"{"server":{"name":"Tuwunel","version":"1.9.1"}}"#;
        assert_eq!(
            version_in(json, Some(&path("$.server.version"))).as_deref(),
            Some("1.9.1")
        );
        assert_eq!(
            version_in(r#"{"v": 42}"#, Some(&path("$.v"))).as_deref(),
            Some("42")
        );
        // What a Matrix homeserver answers on /_matrix/client/versions (MSC4383).
        let matrix = r#"{"versions":["v1.19"],"net.zemos.msc4383.server":{"name":"Tuwunel","version":"1.9.1"}}"#;
        assert_eq!(
            version_in(matrix, Some(&path("$['net.zemos.msc4383.server'].version"))).as_deref(),
            Some("1.9.1")
        );
        assert_eq!(version_in("1.12.27\n", None).as_deref(), Some("1.12.27"));
        // Not a version: nothing matched, not JSON, an object, an HTML page.
        assert_eq!(version_in(json, Some(&path("$.nope"))), None);
        assert_eq!(version_in("<html>", Some(&path("$.v"))), None);
        assert_eq!(version_in(json, Some(&path("$.server"))), None);
        assert_eq!(version_in(&"x".repeat(MAX_VERSION_LEN + 1), None), None);
        assert_eq!(version_in("", None), None);
    }
}
