//! The whole HTTP surface, through the real router on an in-memory database:
//! anonymous page views and badges, the token-bearing writes (silences,
//! announcements, events, maintenance), push heartbeats and pushed alerts,
//! a peer's heartbeat body and probe request, the client-address header.
//! Whatever a request stored is then rendered by every page, as HTML and as
//! text, for the operator and for a visitor.
//!
//! Input: sections separated by a NUL byte. The first is what a peer
//! disclosed on `/api/peer/monitors` (JSON; shown per vantage on the pages),
//! each following one is a request:
//!
//! ```text
//! POST /api/push/beat?status=down&msg=disk
//! x-push-token: s3cret-s3cret-16
//!
//! body
//! ```
#![no_main]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{HeaderName, HeaderValue, Method, Request, Uri};
use hora_core::mesh::vantage::{self, PeerVantage};
use hora_core::mesh::wire::PeerMonitors;
use libfuzzer_sys::fuzz_target;
use tower::ServiceExt as _;

const MAX_REQUESTS: usize = 6;

/// The views rendered once the requests are in, for the operator (who sees
/// everything): each shows something a request can have stored.
const VIEWS: [&str; 12] = [
    "/",
    "/history",
    "/history.atom",
    "/timeline",
    "/watchers",
    "/monitor/web",
    "/monitor/beat",
    "/status/App",
    "/incident/1",
    "/api/summary",
    "/metrics",
    "/api/maintenance",
];

/// The ones a visitor sees another rendering of.
const PUBLIC_VIEWS: [&str; 4] = ["/", "/history", "/monitor/web", "/api/summary"];

fuzz_target!(|data: &[u8]| {
    hora_fuzz::block_on(run(data));
});

async fn run(data: &[u8]) {
    let mut sections = data.split(|byte| *byte == 0);
    let disclosed = sections.next().unwrap_or_default();
    let app = app(disclosed).await;

    let mut wrote = false;
    for section in sections.take(MAX_REQUESTS) {
        let Some(request) = request(section) else {
            continue;
        };
        let written = request.method() != Method::GET;
        let response = send(&app, request).await;
        wrote |= written && response.is_success();
    }
    if !wrote && disclosed.is_empty() {
        return;
    }
    let view = |uri: &str, operator: bool, accept: &str| {
        let mut request = Request::builder().uri(uri).header("accept", accept);
        if operator {
            request = request.header("authorization", format!("Bearer {}", hora_fuzz::AUTH_TOKEN));
        }
        request.body(Body::empty()).expect("a request")
    };
    for uri in VIEWS {
        send(&app, view(uri, true, "text/html")).await;
    }
    for uri in PUBLIC_VIEWS {
        send(&app, view(uri, false, "text/html")).await;
    }
    // The status page has a text rendering too.
    send(&app, view("/", true, "text/plain")).await;
    send(&app, view("/status/App", false, "text/plain")).await;
}

/// The fuzz node behind its router, on an empty database, with `disclosed`
/// as the peers' view of its targets.
async fn app(disclosed: &[u8]) -> Router {
    let config = hora_fuzz::node();
    let vantages = vantage::new_map();
    if let Ok(answer) = serde_json::from_slice::<PeerMonitors>(disclosed) {
        // As the poller folds a peer's answer into the map.
        let mut map: HashMap<String, Vec<PeerVantage>> = HashMap::new();
        for monitor in answer.monitors.iter().take(64) {
            map.entry(vantage::key(monitor.kind, &monitor.target))
                .or_default()
                .push(PeerVantage {
                    peer: "Peer X".to_owned(),
                    status: monitor.status,
                    p50_ms: monitor.p50_ms,
                });
        }
        vantages.store(Arc::new(map));
    }
    let store = hora_core::db::Store::in_memory().await;
    let client = hora_core::http::client(None).expect("a client");
    let notifier = hora_core::notifications::shared(&config, &client);
    let (sender, receiver) = tokio::sync::watch::channel(config);
    // The router reads the configuration for as long as it lives.
    std::mem::forget(sender);
    let tick = u64::try_from(chrono::Utc::now().timestamp()).unwrap_or(0);
    hora_web::router(
        hora_web::AppState::new(store, receiver, Arc::new(AtomicU64::new(tick)), notifier)
            .with_vantage(vantages),
    )
}

/// One section as a request; `None` when HTTP itself would not carry it
/// (the server's parser refuses those before the router sees them).
fn request(section: &[u8]) -> Option<Request<Body>> {
    let mut input = hora_fuzz::Input::new(section);
    let line = input.line();
    let (method, uri) = line.trim_end_matches('\r').split_once(' ')?;
    let mut request = Request::builder()
        .method(Method::from_bytes(method.as_bytes()).ok()?)
        .uri(uri.parse::<Uri>().ok()?)
        .body(Body::empty())
        .ok()?;
    loop {
        let header = input.until(b'\n');
        let Some(colon) = header.iter().position(|byte| *byte == b':') else {
            break;
        };
        let (name, value) = (&header[..colon], header[colon + 1..].trim_ascii());
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name),
            HeaderValue::from_bytes(value),
        ) {
            request.headers_mut().append(name, value);
        }
    }
    *request.body_mut() = Body::from(input.rest().to_vec());
    Some(request)
}

/// Send one request and read the whole answer; its status.
async fn send(app: &Router, mut request: Request<Body>) -> axum::http::StatusCode {
    // The rate limiter falls back on the connection's address.
    request
        .extensions_mut()
        .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
    let (method, uri) = (request.method().clone(), request.uri().clone());
    let response = app.clone().oneshot(request).await.expect("infallible");
    let status = response.status();
    let _ = axum::body::to_bytes(response.into_body(), usize::MAX).await;
    // No request, and nothing a request stored, is a reason for a handler
    // to fail: the database is in memory and answers.
    assert_ne!(
        status,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "{method} {uri} answered 500"
    );
    status
}
