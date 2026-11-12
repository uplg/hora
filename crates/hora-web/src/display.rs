//! The state a page shows for a monitor, a peer's view of one, or a whole
//! page: one enum, so every place that words, draws or sorts a state matches
//! it exhaustively. As class names passed around and matched with a `_ =>`
//! fallback, a typo read as "No data" without a word from the compiler.

use hora_core::status::MonitorState;

/// What the page shows: the four statuses a check can give, plus planned
/// maintenance (which hides the status under it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DisplayState {
    Up,
    /// Slow, or a failed check being retried before it is called down.
    Degraded,
    Down,
    /// Inside a planned maintenance window.
    Maint,
    /// No check recorded yet.
    None,
}

impl DisplayState {
    /// The state's name in the markup: its CSS class and its glyph in the
    /// sprite (`#g-down`). The templates compare and print this.
    pub(crate) const fn class(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Maint => "maint",
            Self::None => "none",
        }
    }

    /// The state's word: the word is the truth, the shape and the colour help.
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Up => "Up",
            Self::Degraded => "Slow",
            Self::Down => "Down",
            Self::Maint => "Maintenance",
            Self::None => "No data",
        }
    }

    /// The one-character shape for plain text, the same shapes as the page's:
    /// a disc, a half disc, a diamond, a clock, a dashed ring. Always beside
    /// its word.
    pub(crate) const fn symbol(self) -> &'static str {
        match self {
            Self::Up => "●",
            Self::Degraded => "◐",
            Self::Down => "◆",
            Self::Maint => "◷",
            Self::None => "◌",
        }
    }

    /// How urgent the state is, for "problems first" (lowest first).
    pub(crate) const fn urgency(self) -> u8 {
        match self {
            Self::Down => 0,
            Self::Degraded => 1,
            Self::Maint => 2,
            Self::None => 3,
            Self::Up => 4,
        }
    }

    /// Whether the target answers (up or slow), so an answer time means
    /// something.
    pub(crate) const fn answers(self) -> bool {
        match self {
            Self::Up | Self::Degraded => true,
            Self::Down | Self::Maint | Self::None => false,
        }
    }

    /// Worth a look: down, slow or in maintenance.
    pub(crate) const fn notable(self) -> bool {
        match self {
            Self::Down | Self::Degraded | Self::Maint => true,
            Self::Up | Self::None => false,
        }
    }
}

/// A monitor's or a peer's status as a display state (`unknown` has no data
/// yet: `none`).
impl From<MonitorState> for DisplayState {
    fn from(status: MonitorState) -> Self {
        match status {
            MonitorState::Up => Self::Up,
            MonitorState::Degraded => Self::Degraded,
            MonitorState::Down => Self::Down,
            MonitorState::Unknown => Self::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [DisplayState; 5] = [
        DisplayState::Up,
        DisplayState::Degraded,
        DisplayState::Down,
        DisplayState::Maint,
        DisplayState::None,
    ];

    /// The names the stylesheet, the sprite and the templates know: renaming
    /// one here without them breaks the page silently.
    #[test]
    fn classes_words_and_shapes_are_the_pages() {
        let classes: Vec<&str> = ALL.iter().map(|state| state.class()).collect();
        assert_eq!(classes, ["up", "degraded", "down", "maint", "none"]);
        let words: Vec<&str> = ALL.iter().map(|state| state.word()).collect();
        assert_eq!(words, ["Up", "Slow", "Down", "Maintenance", "No data"]);
        let shapes: Vec<&str> = ALL.iter().map(|state| state.symbol()).collect();
        assert_eq!(shapes, ["●", "◐", "◆", "◷", "◌"]);
    }

    #[test]
    fn a_status_without_data_shows_as_none() {
        assert_eq!(
            DisplayState::from(MonitorState::Unknown),
            DisplayState::None
        );
        assert_eq!(DisplayState::from(MonitorState::Down), DisplayState::Down);
        assert_eq!(DisplayState::from(MonitorState::Unknown).word(), "No data");
    }

    #[test]
    fn problems_sort_first() {
        let mut states = ALL;
        states.sort_by_key(|state| state.urgency());
        assert_eq!(
            states,
            [
                DisplayState::Down,
                DisplayState::Degraded,
                DisplayState::Maint,
                DisplayState::None,
                DisplayState::Up,
            ]
        );
    }
}
