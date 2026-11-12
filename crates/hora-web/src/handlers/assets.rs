//! The compiled-in static assets: the one stylesheet, the brand fonts, the
//! mark, the favicon and the home-screen icons, plus the web app manifest. Every page links them with a `?v=` content hash,
//! so they are served `immutable`: a new build changes the hash, never the
//! bytes behind an old URL.

use std::sync::LazyLock;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

use hora_core::config::Logo;

use crate::AppState;

/// The favicon, also served at the conventional `/favicon.svg`.
pub(crate) const FAVICON_SVG: &str = include_str!("../../assets/favicon.svg");
const MARK_SVG: &str = include_str!("../../assets/mark.svg");
/// The stylesheet as written; `__V__` stands for [`ASSET_VERSION`] in its
/// font URLs, so a font change busts the stylesheet's cache too.
const CSS_SOURCE: &str = include_str!("../../assets/hora.css");

const FRAUNCES: &[u8] = include_bytes!("../../assets/fonts/Fraunces-VF.woff2");
const CAL_SANS: &[u8] = include_bytes!("../../assets/fonts/CalSansVF.woff2");
/// Mona Sans Mono: "Mona" is a Reserved Font Name under the OFL, so the file
/// ships exactly as released (not subset, converted or renamed).
const MONA_SANS_MONO: &[u8] = include_bytes!("../../assets/fonts/MonaSansMono.woff2");
/// The home-screen icons: the brand's maskable mark (full-bleed petrol, the
/// owl inside the safe zone), rendered to PNG as the platforms want it.
const ICON_192: &[u8] = include_bytes!("../../assets/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../../assets/icon-512.png");
const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../../assets/apple-touch-icon.png");

/// Every binary asset by its path under `/assets/`, with its media type.
const BINARY: &[(&str, &[u8], &str)] = &[
    ("fonts/Fraunces-VF.woff2", FRAUNCES, "font/woff2"),
    ("fonts/CalSansVF.woff2", CAL_SANS, "font/woff2"),
    ("fonts/MonaSansMono.woff2", MONA_SANS_MONO, "font/woff2"),
    ("mark.svg", MARK_SVG.as_bytes(), "image/svg+xml"),
    ("favicon.svg", FAVICON_SVG.as_bytes(), "image/svg+xml"),
    ("icon-192.png", ICON_192, "image/png"),
    ("icon-512.png", ICON_512, "image/png"),
    ("apple-touch-icon.png", APPLE_TOUCH_ICON, "image/png"),
];

/// A short hash of every asset's bytes: the `?v=` cache buster. FNV-1a, so
/// it is stable across builds and toolchains (std's hasher is not).
pub(crate) static ASSET_VERSION: LazyLock<String> = LazyLock::new(|| {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let sources = BINARY
        .iter()
        .map(|(_, bytes, _)| *bytes)
        .chain(std::iter::once(CSS_SOURCE.as_bytes()));
    for bytes in sources {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("{:08x}", hash >> 32)
});

/// The served stylesheet: the source with its version placeholder filled.
static CSS: LazyLock<String> = LazyLock::new(|| CSS_SOURCE.replace("__V__", &ASSET_VERSION));

const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// `GET /assets/{*path}`: a compiled-in asset, or 404.
pub(crate) async fn asset(Path(path): Path<String>) -> Response {
    if path == "hora.css" {
        return (
            [
                (header::CONTENT_TYPE, "text/css; charset=utf-8"),
                (header::CACHE_CONTROL, IMMUTABLE),
            ],
            CSS.as_str(),
        )
            .into_response();
    }
    match BINARY.iter().find(|(name, _, _)| *name == path) {
        Some((_, bytes, kind)) => (
            [
                (header::CONTENT_TYPE, *kind),
                (header::CACHE_CONTROL, IMMUTABLE),
            ],
            *bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "unknown asset").into_response(),
    }
}

/// `GET /logo`: the operator's `[page] logo`, or 404 without one. Pages link
/// it with the digest of its bytes, so it is `immutable` like the rest. An
/// SVG cannot run anything here: every response carries the `script-src
/// 'none'` policy.
pub(crate) async fn logo(State(state): State<AppState>) -> Response {
    let logo = state.config.borrow().page.logo_image.clone();
    serve_logo(logo)
}

/// `GET /logo-dark`: the dark theme's `[page] logo_dark`, as [`logo`].
pub(crate) async fn logo_dark(State(state): State<AppState>) -> Response {
    let logo = state.config.borrow().page.logo_dark_image.clone();
    serve_logo(logo)
}

fn serve_logo(logo: Option<Logo>) -> Response {
    match logo {
        Some(logo) => (
            [
                (header::CONTENT_TYPE, logo.content_type),
                (header::CACHE_CONTROL, IMMUTABLE),
            ],
            axum::body::Bytes::from_owner(logo.bytes),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no logo").into_response(),
    }
}

/// `GET /favicon.svg`, where browsers look first. Not versioned (the URL is
/// fixed by convention), so cached for a day only.
pub(crate) async fn favicon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/svg+xml"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        FAVICON_SVG,
    )
}

/// `GET /apple-touch-icon.png`, where iOS looks when a page names none.
pub(crate) async fn apple_touch_icon() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        APPLE_TOUCH_ICON,
    )
}

/// `GET /manifest.webmanifest`: what a phone needs to put the status page on
/// its home screen as an app - the operator's name, the owl, the ground
/// colour - opening on the status page, standalone. The pages run no script,
/// so there is no service worker: the app shows the live page, never a
/// stale copy.
pub(crate) async fn manifest(State(state): State<AppState>) -> Response {
    let title = state.config.borrow().page.title.clone();
    let icon = |size: u32, purpose: &str| {
        serde_json::json!({
            "src": format!("/assets/icon-{size}.png?v={}", *ASSET_VERSION),
            "sizes": format!("{size}x{size}"),
            "type": "image/png",
            "purpose": purpose,
        })
    };
    let body = serde_json::json!({
        "name": format!("{title} status"),
        "short_name": title,
        "description": format!("Live status of {title}."),
        "start_url": "/",
        "scope": "/",
        "display": "standalone",
        "background_color": "#F2F0EA",
        "theme_color": "#0E5E68",
        "icons": [icon(192, "any"), icon(512, "any"), icon(512, "maskable")],
    });
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            // The title can change with a config reload.
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_is_short_hex_and_fills_the_stylesheet() {
        assert_eq!(ASSET_VERSION.len(), 8);
        assert!(ASSET_VERSION.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!CSS.contains("__V__"));
        assert!(CSS.contains(&format!("CalSansVF.woff2?v={}", *ASSET_VERSION)));
    }

    #[test]
    fn stylesheet_needs_no_inline_style_or_remote_origin() {
        // Fonts are same-origin (CSP font-src 'self'); nothing is fetched elsewhere.
        assert!(!CSS.contains("http://") && !CSS.contains("https://"));
    }
}
