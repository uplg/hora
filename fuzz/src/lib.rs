//! What the fuzz targets share: reading fields out of the fuzzer's bytes, a
//! runtime for the async entry points, and the node the stateful targets run
//! against.

use std::borrow::Cow;
use std::sync::{Arc, LazyLock};

use hora_core::config::Config;

/// The fuzzer's bytes as text: invalid UTF-8 is replaced, not thrown away,
/// so that every input exercises something.
#[must_use]
pub fn text(data: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(data)
}

/// A cursor over the fuzzer's bytes. Running out yields zeros and empty
/// text: an input is never rejected for being short.
pub struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    #[must_use]
    pub fn new(data: &'a [u8]) -> Self {
        Self(data)
    }

    pub fn byte(&mut self) -> u8 {
        let Some((&first, rest)) = self.0.split_first() else {
            return 0;
        };
        self.0 = rest;
        first
    }

    pub fn flag(&mut self) -> bool {
        self.byte() & 1 == 1
    }

    pub fn u32(&mut self) -> u32 {
        u32::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }

    pub fn u64(&mut self) -> u64 {
        u64::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }

    /// Any `i64`: the extremes are what overflows.
    pub fn i64(&mut self) -> i64 {
        i64::from_le_bytes(std::array::from_fn(|_| self.byte()))
    }

    /// A plausible unix time, between 2001 and 2096: what a clock or a row
    /// written from one holds.
    pub fn time(&mut self) -> i64 {
        1_000_000_000 + i64::from(self.u32() % 3_000_000_000)
    }

    /// The bytes up to the next `separator` (which is consumed), or all
    /// that is left.
    pub fn until(&mut self, separator: u8) -> &'a [u8] {
        let end = self.0.iter().position(|byte| *byte == separator);
        let (field, rest) = match end {
            Some(end) => (&self.0[..end], &self.0[end + 1..]),
            None => (self.0, &[][..]),
        };
        self.0 = rest;
        field
    }

    /// The next line, as text.
    pub fn line(&mut self) -> Cow<'a, str> {
        text(self.until(b'\n'))
    }

    /// Everything left.
    pub fn rest(&mut self) -> &'a [u8] {
        std::mem::take(&mut self.0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Run `future` on a runtime of its own, dropped with everything it spawned
/// (the router's rate limiters keep a task each).
pub fn block_on<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

/// The operator's token of [`node`].
pub const AUTH_TOKEN: &str = "0123456789abcdef";
/// The listen token of the peer `peer-x`, which may announce maintenance.
pub const PEER_TOKEN: &str = "peertok-peertok-1";
/// The push token of the monitor `beat`.
pub const PUSH_TOKEN: &str = "s3cret-s3cret-16";

/// One node's configuration, with every kind of input door open: tokens for
/// each audience, a peer that is trusted to announce maintenance, push
/// monitors with and without a token, a client-address header read through
/// two proxies. No channel and no target outside this machine: nothing a
/// target does leaves it.
const NODE: &str = r#"
[page]
title = "Fuzz"
[server]
auth_token = "0123456789abcdef"
view_token = "viewviewviewview"
client_ip_header = "x-forwarded-for"
trusted_proxies = 2
rate_limit_burst = 1000000
[server.group_tokens]
App = "appappappappapp1"
[health]
id = "test-node"
host = "alpha"
[[peers]]
id = "peer-x"
name = "Peer X"
host = "beta"
expect_every_secs = 60
listen_token = "peertok-peertok-1"
accept_maintenance_secs = 900
[[peers]]
id = "peer-y"
name = "Peer Y"
expect_every_secs = 60
allow_unauthenticated = true
[[monitors]]
id = "web"
name = "Web"
target = "http://127.0.0.1:9/"
interval_secs = 60
timeout_secs = 1
group = "App"
host = "beta"
slo_uptime = 99.9
[[monitors]]
id = "intra"
name = "Intra"
kind = "tcp"
target = "127.0.0.1:9"
interval_secs = 60
timeout_secs = 1
group = "App"
host = "alpha"
public = false
depends_on = ["web"]
[[monitors]]
id = "beat"
name = "Beat"
kind = "push"
interval_secs = 60
push_token = "s3cret-s3cret-16"
[[monitors]]
id = "cron"
name = "Cron <job> & \"co\""
kind = "push"
interval_secs = 60
schedule = "*/5 * * * *"
allow_unauthenticated = true
"#;

/// The parsed [`NODE`], shared: a configuration is read-only.
#[must_use]
pub fn node() -> Arc<Config> {
    static CONFIG: LazyLock<Arc<Config>> =
        LazyLock::new(|| Arc::new(hora_core::config::parse(NODE).expect("the fuzz node's config")));
    Arc::clone(&CONFIG)
}
