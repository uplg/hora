//! Shared reqwest client construction (user-agent, timeout backstops, proxy),
//! the redirect policies, and bounded body reads.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use reqwest::redirect::{Attempt, Policy};
use reqwest::{Client, RequestBuilder, Response, Url};
use serde::de::DeserializeOwned;

/// How many redirects the shared client follows before giving up (reqwest's
/// own default).
const MAX_REDIRECTS: usize = 10;

/// Slack on top of a request's own deadline for the outer guard in
/// [`fetch_json_capped`]: reqwest's timeout already covers the body, this only
/// catches a stall the per-request timer somehow missed.
const OUTER_DEADLINE_SLACK: Duration = Duration::from_secs(2);

/// Build an HTTP client with Hora's user-agent and timeout backstops, optionally
/// routed through a `proxy` (`http(s)://…` or `socks5://…`). Used for notifiers
/// and for the peer mesh (heartbeats, witness polls, confirmation probes, the
/// vantage poll).
///
/// Follows redirects only within the request's origin (plus an `http` to
/// `https` upgrade on the same host): these requests carry custom secret
/// headers (`X-Push-Token`, `X-Gotify-Key`, ...) that reqwest does not strip on
/// cross-origin hops, so following one would hand the secret to whatever host a
/// misconfigured or compromised endpoint points at. A cross-origin redirect is
/// returned as-is (a 3xx response), which every caller treats as a failure.
///
/// # Errors
///
/// Returns an error if the proxy URL is invalid or the client cannot be built.
pub fn client(proxy: Option<&str>) -> reqwest::Result<Client> {
    build(
        proxy,
        Policy::custom(same_origin_redirect),
        None,
        Reuse::Pooled,
    )
}

/// Like [`client`], but never auto-follows redirects: probes follow them
/// manually (see `probe::http`) so per-monitor credential headers are dropped on
/// cross-origin hops. reqwest strips only its own well-known sensitive headers
/// across hosts, so without this an arbitrary `X-Api-Key` would be re-sent to
/// whatever host a malicious target redirects to.
///
/// A probe client keeps no connection between requests: every probe opens its
/// own (TCP, TLS, then the request), as a new visitor would. A pooled
/// connection that stops answering without closing - seen in production on an
/// HTTP/2 connection kept alive by a 60s probe - would otherwise fail every
/// later probe and its retry until the process restarts, on a target that
/// answers anyone else.
///
/// # Errors
///
/// Returns an error if the proxy URL is invalid or the client cannot be built.
pub fn probe_client(proxy: Option<&str>) -> reqwest::Result<Client> {
    build(proxy, Policy::none(), None, Reuse::Fresh)
}

/// Like [`probe_client`], but restricted to one address family by binding the
/// local end to `local` (the family's unspecified address): the connector then
/// only attempts resolved addresses of that family. Used by dual-stack probes;
/// never proxied - config validation rejects the combination, since through a
/// proxy only the proxy hop's family would be tested.
///
/// # Errors
///
/// Returns an error if the client cannot be built.
pub fn probe_client_family(local: std::net::IpAddr) -> reqwest::Result<Client> {
    build(None, Policy::none(), Some(local), Reuse::Fresh)
}

/// Whether a client keeps idle connections for its next request.
#[derive(Clone, Copy)]
enum Reuse {
    /// Notifiers and the peer mesh: many requests to few hosts.
    Pooled,
    /// Probes: one connection per request (see [`probe_client`]).
    Fresh,
}

fn build(
    proxy: Option<&str>,
    redirect: Policy,
    local: Option<std::net::IpAddr>,
    reuse: Reuse,
) -> reqwest::Result<Client> {
    let mut builder = Client::builder()
        .user_agent(concat!("hora/", env!("CARGO_PKG_VERSION")))
        .redirect(redirect)
        .local_address(local)
        // Backstop for requests without a per-request timeout (notifiers); probes
        // override this with the monitor's own timeout.
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10));
    if matches!(reuse, Reuse::Fresh) {
        builder = builder.pool_max_idle_per_host(0);
    }
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    builder.build()
}

/// The redirect policy of [`client`]: follow a hop only while it stays on the
/// origin of the *initial* request (so a chain cannot launder its way out one
/// hop at a time), up to [`MAX_REDIRECTS`].
fn same_origin_redirect(attempt: Attempt) -> reqwest::redirect::Action {
    let allowed = attempt
        .previous()
        .first()
        .is_none_or(|initial| redirect_allowed(initial, attempt.url()));
    if !allowed {
        return attempt.stop();
    }
    if attempt.previous().len() > MAX_REDIRECTS {
        return attempt.error("too many redirects");
    }
    attempt.follow()
}

/// Whether a redirect from `initial` to `next` keeps the secret headers on the
/// same server: same scheme, host and port - or the same host upgraded from
/// `http` to `https` (the port then changes with the scheme, which is the
/// point of the upgrade, not a different server).
fn redirect_allowed(initial: &Url, next: &Url) -> bool {
    if initial.host_str() != next.host_str() {
        return false;
    }
    if initial.scheme() == next.scheme() {
        return initial.port_or_known_default() == next.port_or_known_default();
    }
    initial.scheme() == "http" && next.scheme() == "https" && next.port().is_none()
}

/// Whether a redirect from `from` to `to` leaves TLS: an `https` hop sent
/// anywhere but to `https`. The one downgrade rule of both redirect followers
/// (the probe's and [`get_json_following`]).
pub(crate) fn is_downgrade(from: &Url, to: &Url) -> bool {
    from.scheme() == "https" && to.scheme() != "https"
}

/// Whether `ip` is an address of the inside: loopback (`127.0.0.0/8`, `::1`),
/// link-local (`169.254.0.0/16`, `fe80::/10`), private (`10.0.0.0/8`,
/// `172.16.0.0/12`, `192.168.0.0/16`, `fc00::/7`), carrier-grade NAT
/// (`100.64.0.0/10`) or unspecified (`0.0.0.0/8`, `::`). An IPv4 address
/// carried in an IPv6 one (`::ffff:a.b.c.d`, NAT64's `64:ff9b::a.b.c.d`) is
/// judged as the IPv4 address it reaches.
pub(crate) fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 0
                || v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || (a == 100 && (64..128).contains(&b))
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_internal(IpAddr::V4(v4));
            }
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let [a, b] = segments[6].to_be_bytes();
                let [c, d] = segments[7].to_be_bytes();
                return is_internal(IpAddr::V4(Ipv4Addr::new(a, b, c, d)));
            }
            v6.is_unspecified()
                || v6.is_loopback()
                || segments[0] & 0xffc0 == 0xfe80
                || segments[0] & 0xfe00 == 0xfc00
        }
    }
}

/// Where a URL's host leads, as far as this host can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// At least one of its addresses [`is_internal`].
    Internal,
    Public,
    /// The name does not resolve here.
    Unknown,
}

/// Classify `url`'s host: an IP literal by itself, a name by every address it
/// resolves to - one internal address among public ones makes it internal,
/// since the connector may pick any of them.
async fn reach(url: &Url) -> Reach {
    let of = |internal: bool| {
        if internal {
            Reach::Internal
        } else {
            Reach::Public
        }
    };
    let Some(host) = url.host_str() else {
        return Reach::Unknown;
    };
    // An IPv6 literal comes bracketed; odd IPv4 spellings (`0x7f.1`, `2130706433`)
    // were already normalised by the URL parser.
    let literal = host.strip_prefix('[').and_then(|h| h.strip_suffix(']'));
    if let Ok(ip) = literal.unwrap_or(host).parse::<IpAddr>() {
        return of(is_internal(ip));
    }
    let port = url.port_or_known_default().unwrap_or(80);
    let Ok(addrs) = tokio::net::lookup_host((host, port)).await else {
        return Reach::Unknown;
    };
    let mut resolved = false;
    for addr in addrs {
        if is_internal(addr.ip()) {
            return Reach::Internal;
        }
        resolved = true;
    }
    if resolved {
        Reach::Public
    } else {
        Reach::Unknown
    }
}

/// A redirect refused because it leads from a public target to an internal
/// address (see [`RedirectGuard`]).
#[derive(Debug)]
pub(crate) struct InternalRedirect;

impl std::fmt::Display for InternalRedirect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("redirected to a private address")
    }
}

impl std::error::Error for InternalRedirect {}

/// Keeps a redirect chain that started on a public target from ending on an
/// internal one. A monitored site that is compromised (or merely hostile) can
/// answer `302 Location: http://169.254.169.254/...` or `http://10.0.0.5/admin`:
/// the probe would fetch it from inside the operator's network, then store and
/// show what it answered. A target that is itself internal - the usual case of
/// monitoring one's own services - may redirect anywhere, as before.
///
/// Two checks, one on each side of the connection:
///
/// - [`check_target`](Self::check_target), before a hop is requested: the
///   redirect's host is resolved here and refused if any of its addresses
///   [`is_internal`];
/// - [`check_peer`](Self::check_peer), on the hop's response: the address the
///   connection actually reached is judged again, and an internal one is
///   refused before a byte of the body is read.
///
/// The client resolves the name a second time to connect, so a DNS answer
/// that changes between the two (rebinding) gets one request - a `GET`
/// without the monitor's headers - sent to the internal address; the second
/// check then drops the response, so nothing it says is read, stored or
/// shown. Closing that too needs a resolver inside the client that pins the
/// checked address.
///
/// Through a proxy the proxy resolves and connects, and the peer of the
/// connection is the proxy: only the first check runs, on this host's view of
/// DNS, and a name that does not resolve here (the target's or the
/// redirect's) is let through - Hora cannot tell where the proxy takes it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RedirectGuard {
    /// The original target is internal (or, proxied, unknown): no restriction.
    open: bool,
    proxied: bool,
}

impl RedirectGuard {
    /// The guard for a chain that started at `target` and got `first` as its
    /// first response. Unproxied, the target's reach is the address that
    /// response came from - what was connected to, not what DNS says now.
    pub(crate) async fn new(target: &Url, first: &Response, proxied: bool) -> Self {
        let open = match first.remote_addr().filter(|_| !proxied) {
            Some(peer) => is_internal(peer.ip()),
            None => match reach(target).await {
                Reach::Internal => true,
                Reach::Public => false,
                Reach::Unknown => proxied,
            },
        };
        Self { open, proxied }
    }

    /// Whether the chain may go on to `next`.
    ///
    /// # Errors
    ///
    /// [`InternalRedirect`] when `next` leads to an internal address and the
    /// original target did not.
    pub(crate) async fn check_target(self, next: &Url) -> Result<(), InternalRedirect> {
        if !self.open && reach(next).await == Reach::Internal {
            return Err(InternalRedirect);
        }
        Ok(())
    }

    /// Whether the response of a redirected hop may be used.
    ///
    /// # Errors
    ///
    /// [`InternalRedirect`] when the hop connected to an internal address
    /// after all (the name changed its answer since
    /// [`check_target`](Self::check_target)).
    pub(crate) fn check_peer(self, response: &Response) -> Result<(), InternalRedirect> {
        let internal = response
            .remote_addr()
            .is_some_and(|peer| is_internal(peer.ip()));
        if !self.open && !self.proxied && internal {
            return Err(InternalRedirect);
        }
        Ok(())
    }
}

/// A response body read under a byte cap.
#[derive(Debug)]
pub struct CappedBody {
    /// At most `cap` bytes of the body.
    pub bytes: Vec<u8>,
    /// Whether the body was longer than the cap (the rest was not read).
    pub truncated: bool,
}

/// Read a response body, keeping at most `cap` bytes, so a huge (or hostile,
/// endless) body can't exhaust memory. Stops reading as soon as the cap is
/// passed and reports it in [`CappedBody::truncated`]; whether a truncated body
/// is still usable is the caller's call (a failure snapshot is, a JSON answer
/// is not).
///
/// # Errors
///
/// Returns the transport error when the body fails mid-stream: a partial body
/// is not a clean end of stream and must not be judged as if it were complete.
pub async fn read_capped(response: &mut Response, cap: usize) -> reqwest::Result<CappedBody> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        // Copy at most the remaining budget so one huge chunk can't blow the bound.
        let take = (cap - bytes.len()).min(chunk.len());
        bytes.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            return Ok(CappedBody {
                bytes,
                truncated: true,
            });
        }
    }
    Ok(CappedBody {
        bytes,
        truncated: false,
    })
}

/// The largest JSON document [`get_json_following`] reads. RDAP domain
/// objects and GitHub release objects run to tens of KiB; anything far beyond
/// is not the answer we want.
pub(crate) const MAX_JSON_BYTES: usize = 1 << 20;

/// Read a whole response body, failing once it exceeds `cap` bytes: for
/// answers that are useless cut short (a JSON document, a version string).
///
/// # Errors
///
/// Returns an error on a transport failure or an oversized body.
pub(crate) async fn read_all_capped(mut response: Response, cap: usize) -> anyhow::Result<Vec<u8>> {
    let body = read_capped(&mut response, cap).await?;
    anyhow::ensure!(!body.truncated, "answer larger than {cap} bytes");
    Ok(body.bytes)
}

/// GET a JSON document, following up to `max_redirects` redirects itself:
/// whatever the client's own redirect policy, a 30x that reaches us is
/// followed here, with a relative `Location` resolved against the current
/// URL (`Url::join`). A hop from `https` down to plain `http` is refused, so
/// a redirect can never strip transport security (the probes' rule,
/// [`is_downgrade`]), and so is one from a public server to an internal
/// address ([`RedirectGuard`], also shared with the probes). `headers` are
/// sent on every hop, cross-origin included, so they must never carry a
/// secret. Used by the RDAP lookup and the release watch, on the unproxied
/// shared client.
///
/// Returns the parsed body of a 2xx answer (read up to [`MAX_JSON_BYTES`]),
/// or `Err(status)` for any other final status, for the caller to word.
///
/// # Errors
///
/// Returns an error on a network failure, a redirect without a usable
/// `Location`, a downgrade, a redirect to an internal address, too many
/// redirects, or an oversized or invalid JSON body.
pub(crate) async fn get_json_following(
    client: &Client,
    url: &str,
    headers: &[(&'static str, &'static str)],
    max_redirects: usize,
) -> anyhow::Result<Result<serde_json::Value, reqwest::StatusCode>> {
    let target = Url::parse(url)?;
    let mut url = target.clone();
    let mut guard: Option<RedirectGuard> = None;
    for _ in 0..=max_redirects {
        let mut request = client.get(url.clone());
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await?;
        let guard = match guard {
            Some(guard) => {
                guard.check_peer(&response)?;
                guard
            }
            None => *guard.insert(RedirectGuard::new(&target, &response, false).await),
        };
        let status = response.status();
        if status.is_redirection() {
            let location = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("redirect without a Location header"))?;
            let next = url.join(location)?;
            anyhow::ensure!(
                !is_downgrade(&url, &next),
                "refusing a redirect from https to {}",
                next.scheme()
            );
            guard.check_target(&next).await?;
            url = next;
            continue;
        }
        if !status.is_success() {
            return Ok(Err(status));
        }
        let body = read_all_capped(response, MAX_JSON_BYTES).await?;
        return Ok(Ok(serde_json::from_slice(&body)?));
    }
    anyhow::bail!("too many redirects")
}

/// Send `request` and decode its JSON answer, bounded in time (`deadline`,
/// covering the body too) and size (`cap` bytes). Every failure - transport, a
/// non-2xx status (including an unfollowed redirect), an oversized, truncated
/// or malformed body, a timeout - is `None`: the mesh callers are all
/// fail-open, and an answer that could not be fully read must not be trusted.
pub async fn fetch_json_capped<T: DeserializeOwned>(
    request: RequestBuilder,
    cap: usize,
    deadline: Duration,
) -> Option<T> {
    fetch_json_or_status(request, cap, deadline).await.ok()
}

/// Like [`fetch_json_capped`], but a non-2xx answer is told apart from the
/// other failures: `Err(Some(status))` is the server's answer (a peer saying
/// 404 "not one of my targets" did answer), `Err(None)` everything else.
pub(crate) async fn fetch_json_or_status<T: DeserializeOwned>(
    request: RequestBuilder,
    cap: usize,
    deadline: Duration,
) -> Result<T, Option<reqwest::StatusCode>> {
    let request = request.timeout(deadline);
    let fetch = async {
        let mut response = request.send().await.map_err(|_| None)?;
        if !response.status().is_success() {
            return Err(Some(response.status()));
        }
        let body = read_capped(&mut response, cap).await.map_err(|_| None)?;
        if body.truncated {
            return Err(None);
        }
        serde_json::from_slice::<T>(&body.bytes).map_err(|_| None)
    };
    tokio::time::timeout(deadline + OUTER_DEADLINE_SLACK, fetch)
        .await
        .unwrap_or(Err(None))
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;

    fn url(raw: &str) -> Url {
        raw.parse().expect("valid url")
    }

    #[test]
    fn redirects_stay_on_the_initial_origin() {
        let initial = url("https://peer.example/api/push/a");
        assert!(redirect_allowed(
            &initial,
            &url("https://peer.example/api/push/a/")
        ));
        assert!(redirect_allowed(
            &initial,
            &url("https://peer.example:443/elsewhere")
        ));
        // Another host, another port or a downgrade: refused.
        assert!(!redirect_allowed(&initial, &url("https://evil.example/")));
        assert!(!redirect_allowed(
            &initial,
            &url("https://peer.example:8443/")
        ));
        assert!(!redirect_allowed(&initial, &url("http://peer.example/")));
        // A subdomain is a different origin too.
        assert!(!redirect_allowed(&initial, &url("https://x.peer.example/")));
        // The same host upgraded to https (default port) is the same server.
        let plain = url("http://peer.example/api");
        assert!(redirect_allowed(&plain, &url("https://peer.example/api")));
        assert!(!redirect_allowed(
            &plain,
            &url("https://peer.example:8443/api")
        ));
    }

    /// A one-shot HTTP server answering every connection with `response`;
    /// returns its base URL and a counter of the requests that carried
    /// `x-push-token`.
    async fn serve(response: String) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let tokens = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&tokens);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                if head.contains("x-push-token") {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), tokens)
    }

    #[tokio::test]
    async fn cross_origin_redirect_is_not_followed_with_the_token() {
        // The "attacker" origin: counts token-bearing requests.
        let (evil, evil_tokens) =
            serve("HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}".to_owned())
                .await;
        // The peer redirects to it (127.0.0.1 vs localhost: another host).
        let evil_host = evil.replace("127.0.0.1", "localhost");
        let (peer, _) = serve(format!(
            "HTTP/1.1 302 Found\r\nlocation: {evil_host}/steal\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
        ))
        .await;

        let client = client(None).expect("client");
        let response = client
            .post(format!("{peer}/api/push/a"))
            .header("x-push-token", "s3cret")
            .send()
            .await
            .expect("response");
        // The 302 comes back unfollowed, and the other origin never saw the token.
        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
        assert_eq!(evil_tokens.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn internal_addresses_are_the_listed_ranges() {
        let internal = [
            "127.0.0.1",
            "127.255.255.254",
            "10.0.0.5",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "0.0.0.0",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "fd12:3456::1",
            "::ffff:10.0.0.5",
            "64:ff9b::7f00:1",
        ];
        for ip in internal {
            assert!(is_internal(ip.parse().unwrap()), "{ip}");
        }
        let public = [
            "1.1.1.1",
            "172.32.0.1",
            "100.128.0.1",
            "100.63.255.255",
            "169.253.0.1",
            "192.169.0.1",
            "2606:4700:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::101:101",
            "fe00::1",
        ];
        for ip in public {
            assert!(!is_internal(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn a_public_chain_may_not_turn_inwards() {
        let guard = RedirectGuard {
            open: false,
            proxied: false,
        };
        for inward in [
            "http://127.0.0.1:8080/admin",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[::ffff:192.168.0.10]/",
            "http://localhost:9000/",
        ] {
            assert!(guard.check_target(&url(inward)).await.is_err(), "{inward}");
        }
        assert!(guard.check_target(&url("https://1.1.1.1/")).await.is_ok());
        // A target that is itself internal redirects wherever it likes.
        let open = RedirectGuard {
            open: true,
            proxied: false,
        };
        assert!(open.check_target(&url("http://10.0.0.5/")).await.is_ok());
    }

    /// The response of a hop that reached an internal address is refused when
    /// the chain started outside, whatever the name resolved to beforehand.
    #[tokio::test]
    async fn a_hop_that_connected_inwards_is_dropped() {
        let (inside, _) = serve(
            "HTTP/1.1 200 OK\r\ncontent-length: 6\r\nconnection: close\r\n\r\nsecret".to_owned(),
        )
        .await;
        let response = client(None).unwrap().get(&inside).send().await.unwrap();
        let closed = RedirectGuard {
            open: false,
            proxied: false,
        };
        assert!(closed.check_peer(&response).is_err());
        // The same response as the first of a chain: its target is internal.
        let guard = RedirectGuard::new(&url(&inside), &response, false).await;
        assert!(guard.open);
        assert!(guard.check_peer(&response).is_ok());
    }

    #[tokio::test]
    async fn fetch_json_capped_rejects_oversized_and_accepts_small() {
        let (small, _) = serve(
            "HTTP/1.1 200 OK\r\ncontent-length: 11\r\nconnection: close\r\n\r\n{\"up\":true}"
                .to_owned(),
        )
        .await;
        let client = client(None).expect("client");
        let value: Option<serde_json::Value> =
            fetch_json_capped(client.get(&small), 64, Duration::from_secs(5)).await;
        assert_eq!(value, Some(serde_json::json!({"up": true})));

        // Same body, cap below its size: refused rather than parsed partially.
        let value: Option<serde_json::Value> =
            fetch_json_capped(client.get(&small), 4, Duration::from_secs(5)).await;
        assert_eq!(value, None);
    }
}
