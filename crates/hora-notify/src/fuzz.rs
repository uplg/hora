//! The way in for the fuzz target `notify_render` (see `fuzz/` at the
//! repository root): what every channel would send for an event, without
//! sending it. Built with `--cfg fuzzing` only, which `cargo fuzz` sets.

use crate::Event;

/// One channel's rendering of an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub channel: &'static str,
    /// The title, subject or header, for the channels that have one.
    pub title: Option<String>,
    /// The text, or the serialized payload.
    pub body: String,
}

/// `event` as each built-in channel would send it.
#[must_use]
pub fn render_all(event: Event<'_>) -> Vec<Sent> {
    vec![
        crate::discord::fuzzed(event),
        crate::slack::fuzzed(event),
        crate::telegram::fuzzed(event),
        crate::matrix::fuzzed(event),
        crate::email::fuzzed(event),
        crate::freemobile::fuzzed(event),
        crate::ntfy::fuzzed(event),
        crate::gotify::fuzzed(event),
        crate::pushover::fuzzed(event),
        crate::webhook::fuzzed(event),
    ]
}
