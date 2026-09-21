//! Upstream release watch ("owner/repo v1.9.2 is out (running v1.9.1)").
//!
//! A monitor says which GitHub project it runs (`release.github`) and how the
//! running version is known: written down, or asked of the service itself. The
//! project's latest published release comes from GitHub's REST API, which
//! leaves drafts and prereleases out by itself. Anonymous calls are allowed 60
//! an hour per address, so the watcher gates on the stored `checked_at`
//! rather than asking on each tick.

use crate::config::ReleaseWatch;

/// GitHub's REST API, repositories.
const API: &str = "https://api.github.com/repos";

/// How many redirects to follow by hand. The shared HTTP client never
/// auto-follows (probe headers must not cross origins), and GitHub answers a
/// renamed or transferred repository with a 301.
const MAX_REDIRECTS: usize = 3;

/// The longest text taken for a version: an answer that is not a version (an
/// HTML error page) must not end up whole in an alert.
const MAX_VERSION_LEN: usize = 64;

/// A published release: its tag, and the page that carries its notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Release {
    pub tag: String,
    pub url: String,
}

/// The latest published release of `project` (`owner/repo`).
///
/// # Errors
///
/// Returns an error if the lookup fails (network, rate limit, an unknown
/// repository) or the project has published no release: tags alone are not
/// releases.
pub(crate) async fn latest(client: &reqwest::Client, project: &str) -> anyhow::Result<Release> {
    let mut url = format!("{API}/{project}/releases/latest");
    for _ in 0..=MAX_REDIRECTS {
        let response = client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        let status = response.status();
        if status.is_redirection() {
            let Some(next) = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
            else {
                anyhow::bail!("redirect without a Location header");
            };
            url = next.to_owned();
            continue;
        }
        anyhow::ensure!(
            status != reqwest::StatusCode::NOT_FOUND,
            "no such repository, or no published release (tags alone are not releases)"
        );
        anyhow::ensure!(status.is_success(), "GitHub answered HTTP {status}");
        let body: serde_json::Value = response.json().await?;
        return release_of(&body).ok_or_else(|| anyhow::anyhow!("no tag_name in GitHub's answer"));
    }
    anyhow::bail!("too many redirects")
}

/// The tag and page of a release object of GitHub's API.
fn release_of(body: &serde_json::Value) -> Option<Release> {
    let tag = body.get("tag_name")?.as_str()?.trim();
    let url = body.get("html_url")?.as_str()?;
    (!tag.is_empty()).then(|| Release {
        tag: tag.to_owned(),
        url: url.to_owned(),
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
    let body = response.text().await?;
    version_in(&body, watch.current_query.as_deref())
        .ok_or_else(|| anyhow::anyhow!("no version in the answer of {url}"))
}

/// The version inside a service's answer: the first node `query` matches in a
/// JSON document, or without a query the first line of the text.
fn version_in(body: &str, query: Option<&str>) -> Option<String> {
    let version = match query {
        Some(query) => {
            let value: serde_json::Value = serde_json::from_str(body).ok()?;
            // The query is validated at config load.
            let path = serde_json_path::JsonPath::parse(query).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
        let json = r#"{"server":{"name":"Tuwunel","version":"1.9.1"}}"#;
        assert_eq!(
            version_in(json, Some("$.server.version")).as_deref(),
            Some("1.9.1")
        );
        assert_eq!(
            version_in(r#"{"v": 42}"#, Some("$.v")).as_deref(),
            Some("42")
        );
        // What a Matrix homeserver answers on /_matrix/client/versions (MSC4383).
        let matrix = r#"{"versions":["v1.19"],"net.zemos.msc4383.server":{"name":"Tuwunel","version":"1.9.1"}}"#;
        assert_eq!(
            version_in(matrix, Some("$['net.zemos.msc4383.server'].version")).as_deref(),
            Some("1.9.1")
        );
        assert_eq!(version_in("1.12.27\n", None).as_deref(), Some("1.12.27"));
        // Not a version: nothing matched, not JSON, an object, an HTML page.
        assert_eq!(version_in(json, Some("$.nope")), None);
        assert_eq!(version_in("<html>", Some("$.v")), None);
        assert_eq!(version_in(json, Some("$.server")), None);
        assert_eq!(version_in(&"x".repeat(MAX_VERSION_LEN + 1), None), None);
        assert_eq!(version_in("", None), None);
    }
}
