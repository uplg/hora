//! A check that could not run because *this host* is out of something - file
//! descriptors, socket buffers, memory - says nothing about its target.
//!
//! Recorded as a failure it would turn every monitor down at once, and page
//! for all of them, whenever the daemon's own descriptors run out (a flood
//! of connections on the status page is enough). Such a check is skipped
//! instead - no row, no failure counted - and remembered here, so the node
//! reports itself degraded on `/healthz` and its peers raise the one alert
//! that is true: this node can no longer watch.

use std::error::Error;
use std::sync::atomic::{AtomicI64, Ordering};

/// How long after the last skipped check the node still reports itself
/// degraded. Under lasting exhaustion checks are skipped every few seconds
/// and the report never clears; once they run again it clears within this.
const MEMORY_SECS: i64 = 30;

/// At most one log line per this many seconds: under exhaustion every
/// monitor skips every tick.
const LOG_EVERY_SECS: i64 = 60;

// The same numbers on Linux, macOS and the BSDs.
const ENOMEM: i32 = 12;
const ENFILE: i32 = 23;
const EMFILE: i32 = 24;
const ENOBUFS: i32 = if cfg!(target_os = "linux") { 105 } else { 55 };

/// Whether `err` is this host running out of a resource, rather than
/// anything the other end did.
pub(crate) fn is_exhaustion(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::OutOfMemory
        || matches!(err.raw_os_error(), Some(ENOMEM | ENFILE | EMFILE | ENOBUFS))
}

/// `err` and every error behind it, outermost first. An `io::Error` that
/// wraps another (hyper-rustls nests a rustls error that way) is looked
/// into: its own `source()` skips the error it wraps, so the chain alone
/// never reaches it.
pub(crate) fn causes<'a>(
    err: &'a (dyn Error + 'static),
) -> impl Iterator<Item = &'a (dyn Error + 'static)> {
    std::iter::successors(Some(err), |&current| {
        current
            .downcast_ref::<std::io::Error>()
            .and_then(std::io::Error::get_ref)
            .map(|wrapped| wrapped as &(dyn Error + 'static))
            .or_else(|| current.source())
    })
}

/// The exhaustion in `err`'s chain of causes, if any, in words.
pub(crate) fn exhaustion_in(err: &(dyn Error + 'static)) -> Option<String> {
    causes(err)
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .find(|io| is_exhaustion(io))
        .map(ToString::to_string)
}

/// Whether this host cannot open a socket right now for lack of resources:
/// asked after a failed check, whose own error does not always carry the
/// cause (a resolver that could not open its socket reports a failed
/// lookup). Opening a socket is the test - it is what a check needs first.
pub(crate) fn exhausted_now() -> Option<String> {
    match std::net::UdpSocket::bind((std::net::Ipv4Addr::LOCALHOST, 0)) {
        Err(err) if is_exhaustion(&err) => Some(err.to_string()),
        _ => None,
    }
}

/// When checks were last skipped, and when that was last said in the log.
/// A type of its own so that its two rules can be tested away from the
/// process-wide instance.
#[derive(Default)]
struct Faults {
    last: AtomicI64,
    logged: AtomicI64,
}

impl Faults {
    /// Remember a skipped check at `now`. `true` when it is time to say so
    /// in the log: at most once per [`LOG_EVERY_SECS`], however many
    /// monitors skip.
    fn note(&self, now: i64) -> bool {
        self.last.fetch_max(now, Ordering::Relaxed);
        let logged = self.logged.load(Ordering::Relaxed);
        now - logged >= LOG_EVERY_SECS
            && self
                .logged
                .compare_exchange(logged, now, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
    }

    /// Whether a check was skipped within [`MEMORY_SECS`] of `now`.
    fn recent(&self, now: i64) -> bool {
        let last = self.last.load(Ordering::Relaxed);
        last != 0 && now - last <= MEMORY_SECS
    }
}

static FAULTS: Faults = Faults {
    last: AtomicI64::new(0),
    logged: AtomicI64::new(0),
};

/// Remember that a check was skipped at `now`, and say so in the log (once
/// per [`LOG_EVERY_SECS`], however many monitors skip).
pub(crate) fn note(monitor_id: &str, detail: &str, now: i64) {
    if FAULTS.note(now) {
        tracing::error!(
            monitor = %monitor_id,
            error = detail,
            "checks cannot run: this host is out of file descriptors or memory. \
             They are skipped, not counted as failures, and this node reports \
             itself degraded until they run again (raise the open-files limit, \
             or lower server.max_connections)"
        );
    }
}

/// Whether a check was skipped for lack of local resources recently enough
/// that this node should not call itself healthy.
#[must_use]
pub fn recent(now: i64) -> bool {
    FAULTS.recent(now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_exhaustion_is_excused() {
        assert!(is_exhaustion(&std::io::Error::from_raw_os_error(EMFILE)));
        assert!(is_exhaustion(&std::io::Error::from_raw_os_error(ENFILE)));
        assert!(is_exhaustion(&std::io::Error::from_raw_os_error(ENOBUFS)));
        assert!(is_exhaustion(&std::io::Error::from_raw_os_error(ENOMEM)));
        // What the other end, or the network, did.
        for kind in [
            std::io::ErrorKind::ConnectionRefused,
            std::io::ErrorKind::ConnectionReset,
            std::io::ErrorKind::TimedOut,
            std::io::ErrorKind::HostUnreachable,
        ] {
            assert!(!is_exhaustion(&std::io::Error::from(kind)), "{kind:?}");
        }
    }

    #[test]
    fn exhaustion_is_found_under_wrappers() {
        #[derive(Debug)]
        struct Wrapper(std::io::Error);
        impl std::fmt::Display for Wrapper {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("request failed")
            }
        }
        impl Error for Wrapper {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.0)
            }
        }
        assert!(exhaustion_in(&Wrapper(std::io::Error::from_raw_os_error(EMFILE))).is_some());
        assert!(
            exhaustion_in(&Wrapper(std::io::Error::from(
                std::io::ErrorKind::ConnectionRefused
            )))
            .is_none()
        );
        // Wrapped in an io::Error, where `source()` does not look.
        let nested = std::io::Error::other(std::io::Error::from_raw_os_error(EMFILE));
        assert!(exhaustion_in(&nested).is_some());
    }

    #[test]
    fn a_skipped_check_degrades_the_node_for_a_while_and_no_longer() {
        let faults = Faults::default();
        assert!(
            !faults.recent(1_000),
            "degraded before any check was skipped"
        );
        faults.note(1_000);
        assert!(faults.recent(1_000));
        assert!(faults.recent(1_000 + MEMORY_SECS));
        assert!(!faults.recent(1_000 + MEMORY_SECS + 1));
        // The latest skip counts, whatever order they were noted in.
        faults.note(2_000);
        faults.note(1_500);
        assert!(faults.recent(2_000 + MEMORY_SECS));
        assert!(!faults.recent(2_000 + MEMORY_SECS + 1));
    }

    #[test]
    fn skipped_checks_are_logged_once_a_minute() {
        let faults = Faults::default();
        assert!(faults.note(1_000), "the first one is said");
        // Three hundred monitors skipping in the same minute: one line.
        assert!(!faults.note(1_000));
        assert!(!faults.note(1_000 + LOG_EVERY_SECS - 1));
        assert!(faults.note(1_000 + LOG_EVERY_SECS));
        assert!(!faults.note(1_000 + LOG_EVERY_SECS + 1));
    }

    #[test]
    fn a_host_with_room_is_not_exhausted() {
        assert_eq!(exhausted_now(), None);
    }
}
