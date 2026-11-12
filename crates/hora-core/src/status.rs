//! Typed status values: the verdict of one check, and what a monitor is now.
//!
//! [`CheckStatus`] is what a probe, a push or a recorded miss produces and what
//! `checks.status` stores (0 down, 1 up, 2 degraded). [`MonitorState`] is the
//! current state derived from the latest checks, which adds `unknown` (no data)
//! and only calls a monitor down once the failure threshold confirms it.

use serde::{Deserialize, Serialize};

/// The verdict of one check, as stored in `checks.status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, sqlx::Type)]
#[repr(i64)]
pub enum CheckStatus {
    /// The check failed.
    Down = 0,
    /// The check passed.
    Up = 1,
    /// The check passed, too slowly (or with a warning).
    Degraded = 2,
}

impl CheckStatus {
    /// Up, or degraded when `slow`.
    #[must_use]
    pub fn up_unless(slow: bool) -> Self {
        if slow { Self::Degraded } else { Self::Up }
    }

    /// Whether the service answered: up or degraded. A degraded check counts
    /// as available everywhere (uptime, the down state machine, quorum).
    #[must_use]
    pub fn is_available(self) -> bool {
        self != Self::Down
    }

    /// The value stored in `checks.status`.
    #[must_use]
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    /// The wire word: `down`, `up` or `degraded`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Up => "up",
            Self::Degraded => "degraded",
        }
    }

    /// Parse a wire word (`?status=` on a push). `None` for anything else: a
    /// typo must fail loudly, never read as up.
    #[must_use]
    pub fn parse(word: &str) -> Option<Self> {
        match word {
            "down" => Some(Self::Down),
            "up" => Some(Self::Up),
            "degraded" => Some(Self::Degraded),
            _ => None,
        }
    }
}

/// What a monitor is now, from its recent checks (see
/// [`crate::db::derive_status`]). Serialized as the lowercase word; a word
/// this version does not know (from a newer peer) reads as unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MonitorState {
    Up,
    Degraded,
    Down,
    /// No check recorded yet.
    #[serde(other)]
    Unknown,
}

impl MonitorState {
    /// The state [`Self::as_str`] names; anything else is `Unknown`.
    #[must_use]
    pub fn from_word(word: &str) -> Self {
        match word {
            "up" => Self::Up,
            "degraded" => Self::Degraded,
            "down" => Self::Down,
            _ => Self::Unknown,
        }
    }

    /// The word the page, the API and the badges use.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for MonitorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stored_values_and_words_are_the_documented_ones() {
        for (status, value, word) in [
            (CheckStatus::Down, 0, "down"),
            (CheckStatus::Up, 1, "up"),
            (CheckStatus::Degraded, 2, "degraded"),
        ] {
            assert_eq!(status.as_i64(), value);
            assert_eq!(status.as_str(), word);
            assert_eq!(CheckStatus::parse(word), Some(status));
        }
        assert_eq!(CheckStatus::parse("dwon"), None);
        assert_eq!(
            serde_json::to_string(&MonitorState::Unknown).unwrap(),
            "\"unknown\""
        );
        // A newer peer's word does not break the decode of its whole listing.
        assert_eq!(
            serde_json::from_str::<MonitorState>("\"flapping\"").unwrap(),
            MonitorState::Unknown
        );
    }
}
