//! Shared reqwest client construction (user-agent, timeout backstops, proxy).

use std::time::Duration;

use reqwest::Client;

/// Build an HTTP client with Hora's user-agent and timeout backstops, optionally
/// routed through a `proxy` (`http(s)://…` or `socks5://…`). Per-monitor probe
/// clients are built this way so each can carry its own proxy.
///
/// # Errors
///
/// Returns an error if the proxy URL is invalid or the client cannot be built.
pub fn client(proxy: Option<&str>) -> reqwest::Result<Client> {
    let mut builder = Client::builder()
        .user_agent(concat!("hora/", env!("CARGO_PKG_VERSION")))
        // Backstop for requests without a per-request timeout (notifiers); probes
        // override this with the monitor's own timeout.
        .timeout(Duration::from_secs(15))
        .connect_timeout(Duration::from_secs(10));
    if let Some(proxy) = proxy {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    builder.build()
}
