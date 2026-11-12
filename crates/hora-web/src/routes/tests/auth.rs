//! Who a token makes you: the read-only `server.view_token`, and the log
//! line a refused credential leaves.

use std::sync::Mutex;

use super::*;

const VIEW: &str = "viewviewviewview";

fn bearer(uri: &str, token: &str) -> Request<Body> {
    let mut request = get(uri);
    request.headers_mut().insert(
        header::AUTHORIZATION,
        format!("Bearer {token}").parse().expect("header value"),
    );
    request
}

/// The read-only token opens every monitor, private ones included, through
/// the header or a kiosk's `?token=`.
#[tokio::test]
async fn the_view_token_reads_private_monitors() {
    let app = test_app().await;
    let anonymous = body_text(app.clone().oneshot(get("/api/summary")).await.unwrap()).await;
    assert!(!anonymous.contains("\"intra\""), "{anonymous}");

    for request in [
        bearer("/api/summary", VIEW),
        get(&format!("/api/summary?token={VIEW}")),
    ] {
        let res = app.clone().oneshot(request).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        // A credentialed answer is the viewer's own.
        assert_eq!(
            res.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
        let body = body_text(res).await;
        assert!(
            body.contains("\"intra\"") && body.contains("\"web\""),
            "{body}"
        );
    }
    for uri in ["/", "/status/App", "/monitor/intra", "/metrics"] {
        let res = app
            .clone()
            .oneshot(get(&format!("{uri}?token={VIEW}")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        let body = body_text(res).await;
        assert!(
            body.contains("Intra") || body.contains("intra"),
            "{uri}: {body}"
        );
    }
    // Its links carry it from page to page, like the operator's.
    let page = body_text(
        app.clone()
            .oneshot(get(&format!("/?token={VIEW}")))
            .await
            .unwrap(),
    )
    .await;
    assert!(page.contains(&format!("/history?token={VIEW}")), "{page}");
    // Without it the private monitor is not there at all.
    let res = app.oneshot(get("/monitor/intra")).await.unwrap();
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

/// Reading the monitors is all it is for: the operator's streams (deploy
/// markers, the mesh) stay the operator's.
#[tokio::test]
async fn the_view_token_gets_no_operator_streams() {
    let (app, store) = test_app_with_pool().await;
    hora_core::db::insert_event(&store, "deploy api v2.3")
        .await
        .unwrap();
    let fetch = |uri: String| {
        let app = app.clone();
        async move { app.oneshot(get(&uri)).await.unwrap() }
    };

    for uri in ["/history", "/timeline", "/"] {
        let viewer = body_text(fetch(format!("{uri}?token={VIEW}")).await).await;
        assert!(!viewer.contains("deploy api v2.3"), "{uri}: {viewer}");
    }
    let operator = body_text(fetch("/history?token=0123456789abcdef".to_owned()).await).await;
    assert!(operator.contains("deploy api v2.3"), "{operator}");

    // The mesh page is not theirs, and the watched peers stay off the page.
    let res = fetch(format!("/watchers?token={VIEW}")).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let page = body_text(fetch(format!("/?token={VIEW}")).await).await;
    assert!(!page.contains("Peer X"), "{page}");
    assert!(!page.contains("/watchers"), "{page}");
}

/// Every event logged on this thread, as `LEVEL field=value ...`.
struct Capture(Arc<Mutex<Vec<String>>>);

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl tracing::Subscriber for Capture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        // Span fields count too: a token must not hide in the request span.
        let mut fields = Fields("SPAN".to_owned());
        attributes.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut fields = Fields(event.metadata().level().to_string());
        event.record(&mut fields);
        self.0.lock().unwrap().push(fields.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// A 401 to a request that showed a credential leaves one warning: who
/// (the address), where (the route) - and never the credential itself.
#[tokio::test]
async fn a_refused_credential_is_logged_without_the_credential() {
    let app = test_app().await;
    let lines = Arc::new(Mutex::new(Vec::new()));
    let _guard = tracing::subscriber::set_default(Capture(Arc::clone(&lines)));
    let failures = |lines: &Mutex<Vec<String>>| -> Vec<String> {
        lines
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains("authentication failed"))
            .cloned()
            .collect()
    };

    // Nothing presented, or the right thing presented: nothing to report.
    for request in [
        push("/api/event?title=x"),
        write("/api/event?title=x"),
        beat("/api/push/beat"),
        get("/api/summary?token=0123456789abcdef"),
    ] {
        app.clone().oneshot(request).await.unwrap();
    }
    assert_eq!(failures(&lines), Vec::<String>::new());

    for (request, route) in [
        (
            push_with(
                "/api/event?title=x",
                "authorization",
                "Bearer hunter2-hunter2-hunter2",
            ),
            "/api/event",
        ),
        (
            push_with("/api/push/beat", "x-push-token", "hunter2-hunter2-hunter2"),
            "/api/push/{id}",
        ),
        // The read-only token on a write, and a token left in a write's URL.
        (
            push_with(
                "/api/silence?monitors=web&duration=1m",
                "authorization",
                "Bearer viewviewviewview",
            ),
            "/api/silence",
        ),
        (
            push("/api/event?title=x&token=hunter2-hunter2-hunter2"),
            "/api/event",
        ),
    ] {
        let before = failures(&lines).len();
        let res = app.clone().oneshot(request).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{route}");
        let logged = failures(&lines);
        assert_eq!(logged.len(), before + 1, "{route}: {logged:?}");
        let line = &logged[before];
        assert!(line.starts_with("WARN"), "{line}");
        assert!(line.contains("client=127.0.0.1"), "{line}");
        assert!(line.contains("method=POST"), "{line}");
        assert!(line.contains(&format!("route=\"{route}\"")), "{line}");
    }
    // Not in that line, nor in any other the requests produced.
    for line in lines.lock().unwrap().iter() {
        assert!(!line.contains("hunter2"), "{line}");
        assert!(!line.contains("viewviewviewview"), "{line}");
        assert!(!line.contains("token="), "{line}");
    }
}
