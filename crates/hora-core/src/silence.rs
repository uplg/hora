//! Ad-hoc silences ("mute api while I deploy"): one validation for the two
//! doors that create them, `POST /api/silence` and `hora silence`.

use crate::db::Store;

use crate::config::Config;
use crate::{MAX_SILENCE_REASON_CHARS, MAX_SILENCE_SECS, bounded, db, parse_duration};

/// The ids a silence was recorded for (`["*"]` for every monitor), and when it
/// expires (unix epoch seconds, UTC).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Silenced {
    pub ids: Vec<String>,
    pub until: i64,
}

/// Why a silence was refused.
#[derive(Debug)]
pub enum SilenceError {
    /// Not a positive duration, or longer than [`MAX_SILENCE_SECS`].
    InvalidDuration,
    /// An empty id list.
    NoIds,
    /// An id that is neither a configured monitor nor a watched peer.
    UnknownId(String),
    /// The insert failed.
    Database(crate::db::Error),
}

/// What an unparseable or too long duration is answered, wherever one is
/// asked for (a silence, a maintenance window).
pub const INVALID_DURATION: &str = "invalid duration (use e.g. 10m, 1h30m; max 7d)";

/// The stored id of a silence (and the selector of a maintenance window)
/// that covers every monitor.
pub const ALL: &str = "*";

impl SilenceError {
    /// The error in words fit for an API answer: fixed, so nothing a caller
    /// sent is echoed back.
    #[must_use]
    pub fn message(&self) -> &'static str {
        match self {
            Self::InvalidDuration => INVALID_DURATION,
            Self::NoIds => "no monitor ids given",
            Self::UnknownId(_) => "unknown monitor id",
            Self::Database(_) => "recording the silence failed",
        }
    }
}

impl std::fmt::Display for SilenceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidDuration | Self::NoIds => f.write_str(self.message()),
            Self::UnknownId(id) => write!(f, "unknown monitor id {id:?}"),
            Self::Database(err) => write!(f, "recording the silence failed: {err}"),
        }
    }
}

impl std::error::Error for SilenceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Database(err) => Some(err),
            _ => None,
        }
    }
}

/// Mute alerts for `ids` (comma-separated monitor ids, or `all` / `*` for
/// every monitor) for `duration` (`10m`, `1h30m`; at most 7 days), with an
/// optional `reason` (trimmed, capped at [`MAX_SILENCE_REASON_CHARS`]).
///
/// Every id is checked first, so a typo'd deploy hook fails loudly instead of
/// silencing nothing: a configured monitor id, or the listen id of a watched
/// peer (whose heartbeat alerts a silence mutes too). The rows are written in
/// one transaction - all of them or none.
///
/// # Errors
///
/// See [`SilenceError`].
pub async fn apply(
    store: &Store,
    config: &Config,
    ids: &str,
    duration: &str,
    reason: Option<&str>,
) -> Result<Silenced, SilenceError> {
    let duration_secs = parse_duration(duration)
        .filter(|secs| *secs <= MAX_SILENCE_SECS)
        .ok_or(SilenceError::InvalidDuration)?;
    let ids = resolve_ids(config, ids)?;
    let reason = reason
        .map(|reason| bounded(reason, MAX_SILENCE_REASON_CHARS))
        .filter(|reason| !reason.is_empty());
    let until = chrono::Utc::now()
        .timestamp()
        .saturating_add(i64::try_from(duration_secs).unwrap_or(i64::MAX));
    db::insert_silences(store, &ids, until, reason.as_deref())
        .await
        .map_err(SilenceError::Database)?;
    Ok(Silenced { ids, until })
}

/// The validated id list: `["*"]` for `all` / `*`, otherwise every listed id,
/// each a configured monitor or watched peer.
pub(crate) fn resolve_ids(config: &Config, ids: &str) -> Result<Vec<String>, SilenceError> {
    let ids = ids.trim();
    if ids == "all" || ids == ALL {
        return Ok(vec![ALL.to_owned()]);
    }
    let ids: Vec<String> = ids
        .split(',')
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .collect();
    if ids.is_empty() {
        return Err(SilenceError::NoIds);
    }
    if let Some(unknown) = ids.iter().find(|id| !is_silenceable(config, id)) {
        return Err(SilenceError::UnknownId(unknown.clone()));
    }
    Ok(ids)
}

/// Whether a silence on `id` would mute something: a monitor's alerts, or a
/// watched peer's missed-heartbeat alerts (keyed by its listen id).
fn is_silenceable(config: &Config, id: &str) -> bool {
    config.find_monitor(id).is_some()
        || config
            .peers
            .iter()
            .any(|peer| peer.is_watched() && peer.listen_id() == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str(
            r#"
            [page]
            [server]
            [health]
            id = "hora-a"
            [[monitors]]
            id = "api"
            name = "API"
            target = "https://example.com"
            interval_secs = 60
            [[peers]]
            id = "hora-b"
            name = "Paris"
            listen_id = "paris-in"
            expect_every_secs = 60
            listen_token = "a-long-enough-listen-token"
            "#,
        )
        .expect("config")
    }

    #[test]
    fn ids_resolve_wildcards_monitors_and_watched_peers() {
        let config = config();
        let peer_id = config.peers[0].listen_id().to_owned();
        assert_eq!(peer_id, "paris-in");
        assert_eq!(resolve_ids(&config, " all ").unwrap(), ["*"]);
        assert_eq!(resolve_ids(&config, "*").unwrap(), ["*"]);
        assert_eq!(
            resolve_ids(&config, &format!("api, {peer_id}")).unwrap(),
            ["api".to_owned(), peer_id]
        );
        assert!(matches!(
            resolve_ids(&config, " , "),
            Err(SilenceError::NoIds)
        ));
        assert!(matches!(
            resolve_ids(&config, "api,typo"),
            Err(SilenceError::UnknownId(id)) if id == "typo"
        ));
    }

    #[tokio::test]
    async fn apply_validates_then_records_every_id() {
        let store = Store::in_memory().await;
        let config = config();
        assert!(matches!(
            apply(&store, &config, "api", "8d", None).await,
            Err(SilenceError::InvalidDuration)
        ));
        assert!(matches!(
            apply(&store, &config, "api,typo", "10m", None).await,
            Err(SilenceError::UnknownId(_))
        ));
        // Nothing was written by the refused calls.
        let now = chrono::Utc::now().timestamp();
        assert!(db::active_silences(&store, now).await.unwrap().is_empty());

        let long = "é".repeat(MAX_SILENCE_REASON_CHARS + 50);
        let silenced = apply(&store, &config, "api", "10m", Some(&long))
            .await
            .unwrap();
        assert_eq!(silenced.ids, ["api"]);
        let active = db::active_silences(&store, now).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(
            active[0].reason.as_deref().map(|r| r.chars().count()),
            Some(MAX_SILENCE_REASON_CHARS)
        );
        assert!(db::is_silenced(&store, "api", now).await.unwrap());
    }

    #[test]
    fn a_silence_error_says_what_went_wrong() {
        assert_eq!(SilenceError::InvalidDuration.message(), INVALID_DURATION);
        assert_eq!(SilenceError::InvalidDuration.to_string(), INVALID_DURATION);
        assert_eq!(SilenceError::NoIds.message(), "no monitor ids given");
        assert_eq!(
            SilenceError::UnknownId("x".to_owned()).message(),
            "unknown monitor id"
        );
        assert_eq!(
            SilenceError::UnknownId("x".to_owned()).to_string(),
            "unknown monitor id \"x\""
        );
        assert!(std::error::Error::source(&SilenceError::NoIds).is_none());
        let database = SilenceError::Database(sqlx::Error::PoolClosed);
        assert!(std::error::Error::source(&database).is_some());
        assert_eq!(database.message(), "recording the silence failed");
    }
}
