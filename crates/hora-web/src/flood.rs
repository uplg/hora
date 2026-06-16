//! In-memory anti-flood for pushed alerts (`POST /api/monitors/{id}/alert`).
//!
//! A producer that retries a failing operation would otherwise page on every
//! attempt. When it tags repeats with a stable `dedup_key`, the first one is
//! dispatched and any repeat of that key within the configured window is
//! *coalesced* - dropped and counted - so the channels (and the timeline) see
//! one alert, not a hundred. Moving the rate-limiting here means every producer
//! benefits without implementing its own.
//!
//! State is per-process and best-effort: a restart just resets the windows,
//! which at worst lets one extra alert through. The map holds only keys whose
//! window is still open (pruned on every call), so it stays bounded by the
//! number of *active* dedup keys.

use std::collections::HashMap;
use std::sync::Mutex;

/// What to do with an incoming pushed alert.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Dispatch and record it.
    Send,
    /// Drop it: the same key was dispatched within the window. `suppressed` is
    /// how many repeats have been coalesced since the last send (this one
    /// included); `retry_after` the seconds until the window reopens.
    Coalesce { suppressed: u64, retry_after: i64 },
}

/// Per-process coalescing state, guarded by a plain mutex - `admit` does no I/O
/// and never awaits, so the lock is held only for the map update.
#[derive(Default)]
pub(crate) struct Flood {
    inner: Mutex<HashMap<Key, Slot>>,
}

type Key = (String, String);

struct Slot {
    /// When the most recent *dispatched* alert for this key went out.
    sent_at: i64,
    /// Repeats coalesced since `sent_at`.
    suppressed: u64,
}

impl Flood {
    /// Decide an alert's fate. A missing/empty `dedup_key`, or a `window` of 0,
    /// always sends - there is then nothing to coalesce on. `now` is injected
    /// so the decision is deterministic and unit-testable.
    pub(crate) fn admit(
        &self,
        monitor_id: &str,
        dedup_key: Option<&str>,
        now: i64,
        window: i64,
    ) -> Admission {
        let Some(dedup_key) = dedup_key.filter(|key| !key.is_empty()) else {
            return Admission::Send;
        };
        if window <= 0 {
            return Admission::Send;
        }

        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Keep only keys whose window is still open, so the map can't grow
        // without bound across distinct keys.
        map.retain(|_, slot| now.saturating_sub(slot.sent_at) < window);

        let key = (monitor_id.to_owned(), dedup_key.to_owned());
        if let Some(slot) = map.get_mut(&key) {
            // Within the window: coalesce, counting the repeat.
            slot.suppressed += 1;
            Admission::Coalesce {
                suppressed: slot.suppressed,
                retry_after: (slot.sent_at + window - now).max(0),
            }
        } else {
            // Fresh key (or the window had elapsed and the slot was pruned):
            // send and open a new window.
            map.insert(
                key,
                Slot {
                    sent_at: now,
                    suppressed: 0,
                },
            );
            Admission::Send
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_dedup_key_always_sends() {
        let flood = Flood::default();
        assert_eq!(flood.admit("m", None, 0, 300), Admission::Send);
        assert_eq!(flood.admit("m", None, 1, 300), Admission::Send);
        assert_eq!(flood.admit("m", Some(""), 2, 300), Admission::Send);
    }

    #[test]
    fn window_zero_disables_coalescing() {
        let flood = Flood::default();
        assert_eq!(flood.admit("m", Some("k"), 0, 0), Admission::Send);
        assert_eq!(flood.admit("m", Some("k"), 1, 0), Admission::Send);
    }

    #[test]
    fn repeat_within_window_coalesces_and_counts() {
        let flood = Flood::default();
        assert_eq!(flood.admit("m", Some("k"), 0, 300), Admission::Send);
        assert_eq!(
            flood.admit("m", Some("k"), 100, 300),
            Admission::Coalesce {
                suppressed: 1,
                retry_after: 200
            }
        );
        assert_eq!(
            flood.admit("m", Some("k"), 200, 300),
            Admission::Coalesce {
                suppressed: 2,
                retry_after: 100
            }
        );
    }

    #[test]
    fn window_reopens_after_it_elapses() {
        let flood = Flood::default();
        assert_eq!(flood.admit("m", Some("k"), 0, 300), Admission::Send);
        assert!(matches!(
            flood.admit("m", Some("k"), 100, 300),
            Admission::Coalesce { .. }
        ));
        // At exactly `window` later the window has elapsed: send again, count reset.
        assert_eq!(flood.admit("m", Some("k"), 300, 300), Admission::Send);
        assert_eq!(
            flood.admit("m", Some("k"), 400, 300),
            Admission::Coalesce {
                suppressed: 1,
                retry_after: 200
            }
        );
    }

    #[test]
    fn keys_and_monitors_are_independent() {
        let flood = Flood::default();
        assert_eq!(flood.admit("m1", Some("k"), 0, 300), Admission::Send);
        // Different key, same monitor: its own window.
        assert_eq!(flood.admit("m1", Some("other"), 1, 300), Admission::Send);
        // Same key, different monitor: also its own window.
        assert_eq!(flood.admit("m2", Some("k"), 1, 300), Admission::Send);
        // The original key still coalesces.
        assert!(matches!(
            flood.admit("m1", Some("k"), 2, 300),
            Admission::Coalesce { .. }
        ));
    }

    #[test]
    fn elapsed_keys_are_pruned_from_the_map() {
        let flood = Flood::default();
        flood.admit("m", Some("k"), 0, 300);
        // A later, unrelated send past the first window prunes the stale slot.
        flood.admit("m", Some("k2"), 1000, 300);
        assert_eq!(flood.inner.lock().unwrap().len(), 1);
    }
}
