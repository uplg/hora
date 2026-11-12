//! HTTP endpoint handlers and their request/response types, one submodule per
//! area; the `OpenAPI` document and the static/ops endpoints live here.

pub(crate) mod annotations;
pub(crate) mod api;
pub(crate) mod assets;
pub(crate) mod badges;
pub(crate) mod maintenance;
pub(crate) mod pages;
pub(crate) mod peer;
pub(crate) mod push;

use std::sync::LazyLock;

use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use utoipa::OpenApi;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, Http, HttpAuthScheme, SecurityScheme};

use hora_core::config::{Config, Monitor};
use hora_core::db::Point;
use hora_core::mesh::wire::{HealthReport, HealthStatus, PeerSeen};

use crate::AppState;
use crate::error::AppError;
use crate::summary::{DayCell, IncidentView, MaintenanceView, MonitorView, Summary};
use crate::visibility::Visibility;

// `paths(...)` below names the handlers unqualified: a module-qualified path
// would become the operation's `OpenAPI` tag. utoipa resolves each
// through the `__path_*` type it generates next to the handler.
use annotations::{__path_announce, __path_announce_clear, __path_post_event, __path_silence};
use api::{__path_latency_json, __path_summary_json};
use badges::{__path_heatmap_svg, __path_status_badge, __path_uptime_badge};
use peer::{__path_peer_monitors, __path_peer_probe};
use push::{__path_post_alert, __path_push};

/// The `OpenAPI` document, generated once at startup (empty if generation fails).
pub(crate) static OPENAPI_JSON: LazyLock<String> = LazyLock::new(|| {
    ApiDoc::openapi().to_pretty_json().unwrap_or_else(|err| {
        tracing::error!("failed to generate OpenAPI document: {err}");
        String::new()
    })
});

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Hora API",
        description = "JSON API of a Hora uptime monitor: read-only status views, plus token-gated writes (heartbeats, alerts, silences, planned maintenance, announcements, events) and the peer-mesh endpoints."
    ),
    modifiers(&SecuritySchemes),
    paths(
        summary_json,
        latency_json,
        push,
        post_alert,
        silence,
        announce,
        announce_clear,
        post_event,
        maintenance::plan,
        maintenance::list,
        maintenance::cancel,
        peer_probe,
        peer_monitors,
        status_badge,
        uptime_badge,
        heatmap_svg,
        healthz
    ),
    components(schemas(
        Summary,
        MonitorView,
        IncidentView,
        MaintenanceView,
        DayCell,
        Point,
        HealthReport,
        PeerSeen,
        annotations::SilenceResponse,
        annotations::AnnounceResponse,
        annotations::AnnounceClearResponse,
        push::AlertRequest,
        push::AlertResponse,
        annotations::EventResponse,
        maintenance::PlannedWindow,
        maintenance::PlannedWindows,
        hora_core::mesh::wire::HeartbeatBody,
        hora_core::mesh::wire::AnnouncedWindow,
        hora_core::mesh::wire::ProbeRequest,
        hora_core::mesh::wire::ProbeResponse,
        hora_core::mesh::wire::PeerMonitors,
        hora_core::mesh::wire::PeerMonitor,
        crate::summary::VantageView
    ))
)]
struct ApiDoc;

/// The two credentials the API understands: the operator, read-only or
/// group token as `Authorization: Bearer`, and an item token (push monitor,
/// alerting monitor, mesh peer) as `X-Push-Token`. Headers only: the write
/// endpoints refuse `?token=`, so none of them documents it.
struct SecuritySchemes;

impl utoipa::Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
        components.add_security_scheme(
            "push_token",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new(
                crate::auth::PUSH_TOKEN_HEADER,
            ))),
        );
    }
}

#[utoipa::path(
    get,
    path = "/healthz",
    responses(
        (status = 200, description = "Node healthy; its view of watched peers", body = HealthReport),
        (status = 503, description = "Scheduler stalled or database unreachable (same body)", body = HealthReport)
    )
)]
pub(crate) async fn healthz(State(state): State<AppState>) -> Response {
    let config = state.config.borrow().clone();
    let report = hora_core::mesh::peer::report(&state.core, &config, &state.last_tick).await;
    // A degraded node answers 503 so plain health checks (Docker's
    // HEALTHCHECK, a load balancer) notice; the body is unchanged for the
    // peers and keyword monitors that read it.
    let status = if report.status == HealthStatus::Ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    // Before the scheduler's first tick (`last_tick_age` -1) the 503 is the
    // expected answer of a node still starting, not a failure: the access log
    // keeps it at debug (see `routes::Starting`). The status is unchanged, so
    // a HEALTHCHECK still waits for the first tick.
    let starting = !report.scheduler_ok && report.last_tick_age < 0 && report.db_ok;
    let mut response = (status, Json(report)).into_response();
    if starting {
        response.extensions_mut().insert(crate::routes::Starting);
    }
    response
}

pub(crate) async fn openapi() -> Response {
    if OPENAPI_JSON.is_empty() {
        // The document is static; an empty one means generation failed at startup.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OpenAPI generation failed",
        )
            .into_response();
    }
    (
        [(header::CONTENT_TYPE, "application/json")],
        OPENAPI_JSON.as_str(),
    )
        .into_response()
}

/// The configured monitor `id`, if this audience may see it. A private
/// monitor answers exactly like a missing one (404) - its existence is not
/// revealed either way.
pub(super) fn visible_monitor<'a>(
    config: &'a Config,
    visibility: &Visibility<'_>,
    id: &str,
) -> Result<&'a Monitor, AppError> {
    config
        .monitors
        .iter()
        .find(|monitor| monitor.id == id && visibility.can_see_monitor(monitor))
        .ok_or(AppError::NotFound("unknown monitor"))
}
