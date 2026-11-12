//! SVG images: status and uptime badges, and the latency heatmap.

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use badgelib::Style;
use chrono::Utc;
use serde::Deserialize;

use hora_core::config::{Config, Monitor};
use hora_core::db::{self};

use super::visible_monitor;
use crate::AppState;
use crate::auth::Viewer;
use crate::error::AppError;
use crate::render::{badge, status_color, svg_response, uptime_color};
use crate::visibility::{Audience, Visibility};

#[utoipa::path(
    get,
    path = "/api/monitors/{id}/heatmap.svg",
    params(("id" = String, Path, description = "Monitor id")),
    security((), ("bearer" = [])),
    responses(
        (status = 200, description = "28-day hours-by-days latency heatmap (SVG)"),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn heatmap_svg(
    State(state): State<AppState>,
    Path(id): Path<String>,
    viewer: Viewer,
) -> Result<impl IntoResponse, AppError> {
    let config = &viewer.config;
    // Same visibility rule as the latency endpoint.
    let audience = super::pages::audience_for_monitor(&viewer, &id);
    let visibility = Visibility::new(config, &audience);
    let monitor = visible_monitor(config, &visibility, &id)?;
    // The image does not depend on who asks (visibility is settled above), and
    // it groups 28 days of checks: one render per monitor serves everyone for
    // a minute.
    let svg = state
        .heatmaps
        .get_or_build(&id, config, async {
            let now = Utc::now().timestamp();
            let since = (now / 86_400 - (crate::heatmap::HEATMAP_DAYS - 1)) * 86_400;
            let cells = db::latency_hourly(&state.store, &id, since).await?;
            Ok::<_, hora_core::db::Error>(crate::heatmap::render(&cells, now, &monitor.name, true))
        })
        .await?;
    Ok(svg_response(svg.as_ref().clone()))
}

#[derive(Deserialize)]
pub(crate) struct BadgeParams {
    style: Option<Style>,
}

impl BadgeParams {
    fn style(self) -> Style {
        self.style.unwrap_or(Style::Flat)
    }
}

#[utoipa::path(
    get,
    path = "/api/badge/{id}/status",
    params(
        ("id" = String, Path, description = "Monitor id"),
        ("style" = Option<String>, Query, description = "Badge style: flat, flat-square, or for-the-badge")
    ),
    responses(
        (status = 200, description = "Status badge (SVG)"),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn status_badge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<BadgeParams>,
) -> Result<impl IntoResponse, AppError> {
    // Badges are embeddable and unauthenticated: a private monitor's badge is
    // a 404, not a leak. They answer from two indexed single-monitor reads
    // (milliseconds) rather than the page summary - a badge on a README must
    // not pay for, or wait on, a full status-page rebuild.
    let config = state.config.borrow().clone();
    badge_monitor(&config, &id)?;
    let threshold = i64::from(config.fail_threshold(&id));
    let recent = db::recent_checks(&state.store, &id, threshold).await?;
    let status = db::derive_status(&recent, threshold).as_str();
    Ok(svg_response(badge(
        "status",
        status,
        status_color(status),
        params.style(),
    )))
}

#[utoipa::path(
    get,
    path = "/api/badge/{id}/uptime",
    params(
        ("id" = String, Path, description = "Monitor id"),
        ("style" = Option<String>, Query, description = "Badge style: flat, flat-square, or for-the-badge")
    ),
    responses(
        (status = 200, description = "24 h uptime badge (SVG)"),
        (status = 404, description = "Unknown monitor")
    )
)]
pub(crate) async fn uptime_badge(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(params): Query<BadgeParams>,
) -> Result<impl IntoResponse, AppError> {
    // Same single-monitor fast path as `status_badge`; the permille arithmetic
    // mirrors the summary's card so both always show the same figure.
    let config = state.config.borrow().clone();
    badge_monitor(&config, &id)?;
    let since = Utc::now().timestamp() - hora_core::SECONDS_PER_DAY;
    let (available, total) = db::availability(&state.store, &id, since).await?;
    let permille = (total > 0).then(|| available.saturating_mul(1000) / total);
    let (message, color) = match permille {
        Some(permille) => (hora_core::fmt::permille(permille), uptime_color(permille)),
        None => ("n/a".to_owned(), crate::render::BADGE_NONE),
    };
    Ok(svg_response(badge(
        "uptime",
        &message,
        color,
        params.style(),
    )))
}

/// The badge-visible monitor with `id`: badges are always the public view,
/// whoever embeds them - a private monitor's badge is indistinguishable from
/// an unknown one.
fn badge_monitor<'a>(config: &'a Config, id: &str) -> Result<&'a Monitor, AppError> {
    visible_monitor(config, &Visibility::new(config, &Audience::Public), id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_expose_public_monitors_only() {
        let config: Config = toml::from_str(
            r#"
            [page]
            [server]
            [[monitors]]
            id = "pub"
            name = "Public"
            target = "https://example.com"
            interval_secs = 60
            [[monitors]]
            id = "priv"
            name = "Private"
            target = "https://internal.example.com"
            interval_secs = 60
            public = false
        "#,
        )
        .unwrap();
        assert!(badge_monitor(&config, "pub").is_ok());
        // Private and unknown are the same 404 - the badge must not leak
        // which private ids exist.
        assert!(badge_monitor(&config, "priv").is_err());
        assert!(badge_monitor(&config, "nope").is_err());
    }
}
