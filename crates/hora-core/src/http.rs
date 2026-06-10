//! Shared reqwest client construction (user-agent, timeout backstops, proxy).

use std::time::Duration;

use reqwest::Client;
use reqwest::redirect::Policy;

/// Build an HTTP client with Hora's user-agent and timeout backstops, optionally
/// routed through a `proxy` (`http(s)://…` or `socks5://…`). Used for notifiers;
/// follows redirects with reqwest's default policy.
///
/// # Errors
///
/// Returns an error if the proxy URL is invalid or the client cannot be built.
pub fn client(proxy: Option<&str>) -> reqwest::Result<Client> {
    build(proxy, Policy::default())
}

/// Like [`client`], but never auto-follows redirects: probes follow them
/// manually (see `probe::http`) so per-monitor credential headers are dropped on
/// cross-origin hops. reqwest strips only its own well-known sensitive headers
/// across hosts, so without this an arbitrary `X-Api-Key` would be re-sent to
/// whatever host a malicious target redirects to.
///
/// # Errors
///
/// Returns an error if the proxy URL is invalid or the client cannot be built.
pub fn probe_client(proxy: Option<&str>) -> reqwest::Result<Client> {
    build(proxy, Policy::none())
}

fn build(proxy: Option<&str>, redirect: Policy) -> reqwest::Result<Client> {
    let mut builder = Client::builder()
        .user_agent(concat!("hora/", env!("CARGO_PKG_VERSION")))
        .redirect(redirect)
        // Backstop for requests without a per-request timeout (notifiers); probes
        // override this with the monitor's own timeout.
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    builder.build()
}
