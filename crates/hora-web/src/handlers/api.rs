//! The read-only JSON API and the Prometheus metrics.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use serde::Deserialize;

use hora_core::db::{self, Point};

use super::visible_monitor;
use crate::auth::Viewer;
use crate::error::AppError;
use crate::metrics;
use crate::snapshot::Body;
use crate::summary::Summary;
use crate::visibility::Visibility;
use crate::{AppState, MAX_LATENCY_HOURS, MAX_LATENCY_POINTS, SECONDS_PER_HOUR};

#[utoipa::path(
    get,
    path = "/api/summary",
    security((), ("bearer" = [])),
    responses((status = 200, description = "Status of every monitor", body = Summary))
)]
pub(crate) async fn summary_json(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    // Serialized once per snapshot and audience, then served from memory.
    let body = cached_body(
        &state,
        &viewer,
        Body::Json(viewer.audience.clone()),
        |summary| serde_json::to_vec(summary).map_err(AppError::from),
    )
    .await?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

pub(crate) async fn metrics_prometheus(
    State(state): State<AppState>,
    viewer: Viewer,
) -> Result<Response, AppError> {
    let body = cached_body(
        &state,
        &viewer,
        Body::Metrics(viewer.audience.clone()),
        |summary| Ok(metrics::render(summary).into_bytes()),
    )
    .await?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
        .into_response())
}

/// A body rendered from the viewer's summary, once per snapshot.
async fn cached_body(
    state: &AppState,
    viewer: &Viewer,
    key: Body,
    render: impl FnOnce(&Summary) -> Result<Vec<u8>, AppError>,
) -> Result<Bytes, AppError> {
    let snapshot = state.snapshot().await;
    let body = snapshot.body(&viewer.config, key, || {
        let summary = snapshot.summary(&viewer.config, &viewer.audience);
        render(&summary).map(|bytes| Some(Bytes::from(bytes)))
    });
    let body = body.await?;
    Ok(body.unwrap_or_default())
}

#[derive(Debug, Deserialize)]
pub(crate) struct LatencyQuery {
    #[serde(default = "default_hours")]
    hours: i64,
}

pub(crate) fn default_hours() -> i64 {
    24
}

#[utoipa::path(
    get,
    path = "/api/monitors/{id}/latency",
    params(
        ("id" = String, Path, description = "Monitor id"),
        ("hours" = Option<i64>, Query, description = "Look-back window in hours (1..=720)")
    ),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "Latency samples, oldest first", body = [Point]),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn latency_json(
    State(state): State<AppState>,
    Path(id): Path<String>,
    viewer: Viewer,
    Query(query): Query<LatencyQuery>,
) -> Result<Json<Vec<Point>>, AppError> {
    let audience = super::pages::audience_for_monitor(&viewer, &id);
    let visibility = Visibility::new(&viewer.config, &audience);
    visible_monitor(&viewer.config, &visibility, &id)?;
    let window = query.hours.clamp(1, MAX_LATENCY_HOURS) * SECONDS_PER_HOUR;
    let since = Utc::now().timestamp() - window;
    // Average into at most MAX_LATENCY_POINTS buckets in SQL, so a 10s-interval
    // monitor over 720h (~260k raw rows) never materializes more than the cap.
    // Ceiling division keeps the bucket count under the cap even for short
    // windows, where flooring would produce up to ~2x the buckets.
    let max_points = i64::try_from(MAX_LATENCY_POINTS).expect("MAX_LATENCY_POINTS fits in i64");
    // (Manual ceil: `i64::div_ceil` is still unstable.)
    let bucket_secs = ((window + max_points - 1) / max_points).max(1);
    let points = db::latency_series(&state.store, &id, since, bucket_secs).await?;
    // The SQL already respects the cap; downsample stays as a pure backstop.
    Ok(Json(downsample(points, MAX_LATENCY_POINTS)))
}

/// Sample a series down to at most `max` points, keeping its overall shape.
pub(crate) fn downsample(points: Vec<Point>, max: usize) -> Vec<Point> {
    if points.len() <= max || max == 0 {
        return points;
    }
    let step = points.len().div_ceil(max);
    points.into_iter().step_by(step).collect()
}
