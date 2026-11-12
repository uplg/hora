//! The peer-mesh endpoints: confirmation probes and the monitor listing.

use axum::Json;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use serde::Deserialize;

use hora_core::config::Kind;
use hora_core::mesh::fingerprint::check_fingerprint;
use hora_core::mesh::wire::disclosed_target;
use hora_core::status::MonitorState;

use crate::AppState;
use crate::auth::authorize_peer;
use crate::error::AppError;

#[utoipa::path(
    post,
    path = "/api/peer/probe",
    request_body = hora_core::mesh::wire::ProbeRequest,
    security(("push_token" = [])),
    responses(
        (status = 200, description = "This vantage's verdict on the target", body = hora_core::mesh::wire::ProbeResponse),
        (status = 400, description = "Exec and push monitors are not network probes"),
        (status = 401, description = "Unknown requesting peer, or missing/wrong X-Push-Token"),
        (status = 404, description = "The target is not in this node's configuration, or not checked the way the request's `check` says")
    )
)]
pub(crate) async fn peer_probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<hora_core::mesh::wire::ProbeRequest>,
) -> Result<Json<hora_core::mesh::wire::ProbeResponse>, AppError> {
    let config = state.config.borrow().clone();
    // Probing is strictly more sensitive than a push heartbeat: the peer must
    // be known here with a listen_token, and the header must match.
    authorize_peer(&config, &headers, &request.from)?;

    // Exec and push monitors have no network target: every exec monitor has
    // the same empty one, so matching it would run *some* local exec monitor
    // and answer its verdict (or a bogus "down") for another node's check.
    if matches!(request.kind, Kind::Exec | Kind::Push) {
        return Err(AppError::BadRequest(
            "exec and push monitors are not network probes",
        ));
    }

    // The SSRF guard: only targets present in THIS node's configuration are
    // probed - a peer is a vantage point, never a proxy. The matched
    // monitor's own settings (timeout, assertions, proxy) drive the probe.
    //
    // Targets are compared as peers disclose them, secrets masked on both
    // sides: a 0.12 node sends its target masked, a 0.11 node verbatim, and
    // either finds the monitor. When the request says what it checks (its
    // fingerprint), only a monitor that checks the same way answers: another
    // monitor on the same URL - another expected status, another Host header -
    // would confirm or contradict a verdict it knows nothing about. Without
    // one (a 0.11 node, an ad-hoc `hora probe --confirm`) the first monitor
    // on the target answers, as it always did.
    let target = disclosed_target(&request.target);
    let monitor = config
        .monitors
        .iter()
        .find(|monitor| {
            monitor.kind() == request.kind
                && disclosed_target(monitor.target()) == target
                && request
                    .check
                    .as_ref()
                    .is_none_or(|check| check_fingerprint(monitor).as_ref() == Some(check))
        })
        .ok_or(AppError::NotFound(
            "target not in this node's configuration",
        ))?;

    // Single attempt (no retries): the requester wants a fast vantage check,
    // not this node's anti-flap pipeline. The outer timeout is a backstop on
    // top of the probe's own; if even that elapses, the target is
    // unresponsive *from here*, which is a down verdict in its own right.
    let mut probe_monitor = monitor.clone();
    probe_monitor.probe_retries = Some(0);
    let client = state
        .probe_clients
        .get_or_build(&config, monitor.proxy(), hora_core::http::probe_client)
        .map_err(|err| AppError::Internal(err.into()))?;
    // The kind check above leaves only the kinds with a live probe.
    let Some(probe) = probe_monitor.spec.network() else {
        return Err(AppError::BadRequest(
            "exec and push monitors are not network probes",
        ));
    };
    let outcome = match tokio::time::timeout(
        hora_core::mesh::confirm::PROBE_DEADLINE,
        hora_core::probe::run(&client, &probe_monitor, probe),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            return Ok(Json(hora_core::mesh::wire::ProbeResponse {
                up: false,
                error: Some("probe timed out at this vantage".to_owned()),
            }));
        }
    };
    // Out of descriptors or memory here: this vantage has no verdict, and
    // "down" would be a false one. No answer is what the requester expects
    // of a vantage that cannot see.
    if outcome.is_local_fault() {
        return Err(AppError::Unavailable(
            "this node cannot run checks right now",
        ));
    }
    Ok(Json(hora_core::mesh::wire::ProbeResponse {
        up: outcome.is_up(),
        // Bounded: the reason crosses the wire into another node's logs.
        error: outcome.error.map(|error| error.chars().take(200).collect()),
    }))
}

#[derive(Debug, Deserialize)]
pub(crate) struct PeerMonitorsQuery {
    /// The requesting node's `[health].id`.
    from: String,
}

#[utoipa::path(
    get,
    path = "/api/peer/monitors",
    params(("from" = String, Query, description = "The requesting peer's [health].id")),
    security(("push_token" = [])),
    responses(
        (status = 200, description = "This node's probeable monitors, with its own view of each (status, 24 h median)", body = hora_core::mesh::wire::PeerMonitors),
        (status = 401, description = "Unknown requesting peer, or missing/wrong X-Push-Token")
    )
)]
pub(crate) async fn peer_monitors(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PeerMonitorsQuery>,
) -> Result<Json<hora_core::mesh::wire::PeerMonitors>, AppError> {
    let config = state.config.borrow().clone();
    // Same strict authentication as /api/peer/probe.
    authorize_peer(&config, &headers, &query.from)?;

    // What a mesh member may know: kind + target (it can probe those anyway
    // via /api/peer/probe), the fingerprint of what each asserts (a digest:
    // comparable, not readable) and this node's live view of each - never names,
    // notes or credentials, including the ones a target URL carries
    // (`user:pass@`, `?api_key=`), which are masked. Push/exec monitors have
    // no probeable target.
    // Answered from the status snapshot the page is served from (its
    // statuses and 24h medians), never from the database.
    let summary = state
        .snapshot()
        .await
        .summary(&config, &crate::visibility::Audience::Operator);
    let views: std::collections::HashMap<&str, &crate::summary::MonitorView> = summary
        .monitors
        .iter()
        .map(|view| (view.id.as_str(), view.as_ref()))
        .collect();
    let monitors = config
        .monitors
        .iter()
        .filter(|monitor| !matches!(monitor.kind(), Kind::Push | Kind::Exec))
        .map(|monitor| {
            let view = views.get(monitor.id.as_str());
            hora_core::mesh::wire::PeerMonitor {
                kind: monitor.kind(),
                target: disclosed_target(monitor.target()),
                status: view.map_or(MonitorState::Unknown, |view| view.status),
                p50_ms: view.and_then(|view| view.p50_ms),
                check: check_fingerprint(monitor),
            }
        })
        .collect();
    Ok(Json(hora_core::mesh::wire::PeerMonitors { monitors }))
}
