//! The network side: the API client, the `/api/summary` shapes it reads, and
//! the background fetcher that keeps every request off the UI loop.

use std::time::Duration;

use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Per-request deadline: a hung server costs one stale round, never the UI.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to back off after the server rate-limited us (HTTP 429).
pub(super) const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(15);

/// What `hora top` reads from `/api/summary` (unknown fields ignored, so the
/// dashboard tolerates both older and newer servers).
#[derive(Deserialize)]
pub(super) struct Summary {
    pub(super) title: String,
    /// The worst monitor status (`up`/`degraded`/`down`/`unknown`): drives
    /// the header colour, while `overall_label` is only the wording.
    #[serde(default)]
    pub(super) overall: String,
    pub(super) overall_label: String,
    pub(super) monitors: Vec<Monitor>,
    /// The pinned status-page banners (config + `hora announce`).
    #[serde(default)]
    pub(super) incidents: Vec<Banner>,
    /// Notification-channel health (authenticated API only; empty/absent on
    /// older or unauthenticated servers).
    #[serde(default)]
    pub(super) channels: Vec<Channel>,
    /// The database size (authenticated API only; absent on older servers).
    #[serde(default)]
    pub(super) storage: Option<Storage>,
}

#[derive(Deserialize)]
pub(super) struct Storage {
    pub(super) db_bytes: u64,
    pub(super) reclaimable_bytes: u64,
}

#[derive(Deserialize)]
pub(super) struct Banner {
    pub(super) title: String,
    pub(super) severity: String,
}

#[derive(Deserialize)]
pub(super) struct Channel {
    pub(super) name: String,
    #[serde(default)]
    pub(super) failing: bool,
    #[serde(default)]
    pub(super) consecutive_failures: u32,
    #[serde(default)]
    pub(super) failing_for_secs: Option<u64>,
}

#[derive(Deserialize)]
pub(super) struct Monitor {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) status: String,
    /// The reason of the maintenance window it is in, if it is in one.
    #[serde(default)]
    pub(super) maintenance: Option<String>,
    #[serde(default)]
    pub(super) group: Option<String>,
    #[serde(default)]
    pub(super) last_error: Option<String>,
    #[serde(default, rename = "uptime_24h_permille")]
    pub(super) uptime_permille: Option<i64>,
    #[serde(default, rename = "latency_p50_ms")]
    pub(super) p50: Option<i64>,
    #[serde(default, rename = "latency_p95_ms")]
    pub(super) p95: Option<i64>,
    #[serde(default, rename = "latency_p99_ms")]
    pub(super) p99: Option<i64>,
}

#[derive(Deserialize)]
pub(super) struct Point {
    latency_ms: i64,
}

/// Why a request to the API failed - typed, so the loop reacts to the status
/// (a 429 backs off) rather than to the wording of a message.
#[derive(Debug)]
pub(super) enum FetchError {
    /// The server answered with a non-success status; `detail` is the start
    /// of its body (the API's reason for a refused write).
    Status {
        status: reqwest::StatusCode,
        detail: String,
    },
    /// No answer: connection refused, DNS, timeout.
    Transport(String),
    /// An answer that is not the JSON we expect.
    Shape,
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status { status, .. } => write!(f, "server answered HTTP {status}"),
            Self::Transport(reason) => write!(f, "request failed: {reason}"),
            Self::Shape => f.write_str("unexpected response shape"),
        }
    }
}

impl FetchError {
    pub(super) fn is_rate_limited(&self) -> bool {
        matches!(self, Self::Status { status, .. } if *status == reqwest::StatusCode::TOO_MANY_REQUESTS)
    }

    /// The wording for a refused action: the API's own reason when it gave
    /// one ("server refused (400 Bad Request): invalid duration").
    fn refusal(&self) -> String {
        match self {
            Self::Status { status, detail } => format!("server refused ({status}): {detail}"),
            other => other.to_string(),
        }
    }
}

/// The API endpoint plus credentials; cheap to clone into each fetch task
/// (`reqwest::Client` is reference-counted).
#[derive(Clone)]
pub(super) struct Api {
    pub(super) client: reqwest::Client,
    pub(super) url: String,
    pub(super) token: Option<String>,
}

impl Api {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let request = self
            .client
            .request(method, format!("{}{path}", self.url.trim_end_matches('/')))
            .timeout(REQUEST_TIMEOUT);
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// Send `request`, turning a non-success status into [`FetchError::Status`].
    async fn send(request: reqwest::RequestBuilder) -> Result<reqwest::Response, FetchError> {
        // The reqwest error embeds the URL (which may carry nothing secret
        // here, but stay consistent with the daemon's logging policy).
        let response = request
            .send()
            .await
            .map_err(|err| FetchError::Transport(err.without_url().to_string()))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let detail = response.text().await.unwrap_or_default();
        Err(FetchError::Status {
            status,
            detail: detail.chars().take(120).collect(),
        })
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, FetchError> {
        Self::send(self.request(reqwest::Method::GET, path))
            .await?
            .json::<T>()
            .await
            .map_err(|_| FetchError::Shape)
    }
}

/// One authenticated write, built from the keyboard and sent in the
/// background.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Action {
    pub(super) method: reqwest::Method,
    pub(super) path: &'static str,
    pub(super) params: Vec<(&'static str, String)>,
    /// The notice shown once the server accepted it.
    pub(super) done: String,
}

/// A finished background fetch, delivered to the event loop.
pub(super) enum Fetched {
    Summary(Result<Summary, FetchError>),
    /// The sparkline of `monitor_id` - dropped if the selection moved on.
    Spark {
        monitor_id: String,
        result: Result<Vec<u64>, FetchError>,
    },
    /// An action's outcome, as the notice to show.
    Action(Result<String, String>),
}

/// What the event loop should start after applying a [`Fetched`].
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FollowUp {
    Nothing,
    /// Fetch this monitor's sparkline.
    Spark(String),
    /// Refetch the summary (an action changed server state).
    Refresh,
}

/// Starts the background fetches, at most one in flight per kind: a slow
/// server never accumulates a queue of summary requests behind it, and a new
/// selection replaces (aborts) the previous sparkline fetch.
pub(super) struct Fetcher {
    api: Api,
    tx: mpsc::Sender<Fetched>,
    summary: Option<JoinHandle<()>>,
    spark: Option<JoinHandle<()>>,
    action: Option<JoinHandle<()>>,
}

fn in_flight(task: Option<&JoinHandle<()>>) -> bool {
    task.is_some_and(|task| !task.is_finished())
}

impl Fetcher {
    pub(super) fn new(api: Api, tx: mpsc::Sender<Fetched>) -> Self {
        Self {
            api,
            tx,
            summary: None,
            spark: None,
            action: None,
        }
    }

    /// Fetch `/api/summary` unless a fetch is already running; `false` when
    /// skipped.
    pub(super) fn summary(&mut self) -> bool {
        if in_flight(self.summary.as_ref()) {
            return false;
        }
        let (api, tx) = (self.api.clone(), self.tx.clone());
        self.summary = Some(tokio::spawn(async move {
            let result = api.get::<Summary>("/api/summary").await;
            let _ = tx.send(Fetched::Summary(result)).await;
        }));
        true
    }

    /// Fetch the 24h latency series of `monitor_id`, abandoning any older
    /// sparkline fetch (its monitor is no longer the selected one).
    pub(super) fn spark(&mut self, monitor_id: String) {
        if let Some(previous) = self.spark.take() {
            previous.abort();
        }
        let (api, tx) = (self.api.clone(), self.tx.clone());
        self.spark = Some(tokio::spawn(async move {
            let path = format!("/api/monitors/{monitor_id}/latency?hours=24");
            let result = api.get::<Vec<Point>>(&path).await.map(|points| {
                points
                    .iter()
                    .map(|point| u64::try_from(point.latency_ms.max(0)).unwrap_or(0))
                    .collect()
            });
            let _ = tx.send(Fetched::Spark { monitor_id, result }).await;
        }));
    }

    /// Send an action in the background. Refused up front (with the notice
    /// to show) without a token, or while the previous action is in flight.
    pub(super) fn action(&mut self, action: Action) -> Result<(), String> {
        if self.api.token.is_none() {
            return Err(
                "this action needs a token: run hora top --token ... (or HORA_TOKEN)".to_owned(),
            );
        }
        if in_flight(self.action.as_ref()) {
            return Err("still sending the previous action - try again in a moment".to_owned());
        }
        let (api, tx) = (self.api.clone(), self.tx.clone());
        self.action = Some(tokio::spawn(async move {
            let request = api
                .request(action.method, action.path)
                .query(&action.params);
            let outcome = Api::send(request)
                .await
                .map(|_| action.done)
                .map_err(|err| err.refusal());
            let _ = tx.send(Fetched::Action(outcome)).await;
        }));
        Ok(())
    }
}

impl Drop for Fetcher {
    /// Leaving the dashboard abandons whatever is still in flight.
    fn drop(&mut self) {
        for task in [&self.summary, &self.spark, &self.action]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
    }
}

impl Monitor {
    /// What the status page shows for it (see
    /// [`hora_core::display::DisplayState::shown`]): a monitor inside a
    /// maintenance window is in maintenance, whatever its checks say. The
    /// summary does not say which downs are seen from this node only.
    pub(super) fn shown(&self) -> hora_core::display::DisplayState {
        hora_core::display::DisplayState::shown(
            hora_core::status::MonitorState::from_word(&self.status),
            self.maintenance.is_some(),
            false,
        )
    }
}
