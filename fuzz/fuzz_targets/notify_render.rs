//! An alert put into words for every channel, with hostile text in every
//! field: a monitor's name, a server's error, a pushed alert's title and
//! message, release notes, a digest. Each channel's rendering must fit the
//! limits of its API (past them the alert is refused, i.e. lost) and must
//! not let the text out of its markup.
//!
//! Input: the event's kind and its numbers, then one text field per line.
#![no_main]

use hora_notify::{AlertSeverity, Event, Release};
use libfuzzer_sys::fuzz_target;

/// `value` when this bit of the flags is set.
fn optional<T>(flags: u8, bit: u8, value: T) -> Option<T> {
    (flags >> bit & 1 == 1).then_some(value)
}

fuzz_target!(|data: &[u8]| {
    let mut input = hora_fuzz::Input::new(data);
    let kind = input.byte();
    let flags = input.byte();
    let (number, other) = (input.i64(), input.i64());
    let fields: Vec<_> = (0..6).map(|_| input.line()).collect();
    let [a, b, c, d, e, f] = [0, 1, 2, 3, 4, 5].map(|index| fields[index].as_ref());
    let rest = hora_fuzz::text(input.rest());
    let impacted: Vec<&str> = rest.lines().take(40).collect();

    let event = match kind % 12 {
        0 => Event::Down {
            monitor: a,
            error: optional(flags, 0, b),
            cause: optional(flags, 1, c),
            impacted: &impacted,
            vantage: optional(flags, 2, d),
            event: optional(flags, 3, e),
            local_only: flags & 16 != 0,
        },
        1 => Event::Degraded {
            monitor: a,
            latency_ms: optional(flags, 0, number),
            detail: optional(flags, 1, b),
        },
        2 => Event::Recovered {
            monitor: a,
            local_only: flags & 1 != 0,
        },
        3 => Event::CertExpiring {
            monitor: a,
            secs_left: number,
        },
        4 => Event::CertUnreadable {
            monitor: a,
            error: b,
        },
        5 => Event::DomainExpiring {
            monitor: a,
            domain: b,
            secs_left: number,
        },
        6 => Event::ReleaseAvailable(Release {
            monitor: a,
            project: b,
            current: c,
            latest: d,
            url: e,
            notes: optional(flags, 0, f),
        }),
        7 => Event::PeerLinkDegraded {
            peer: a,
            witness: b,
        },
        8 => Event::CertChanged {
            monitor: a,
            old_fingerprint: b,
            new_fingerprint: c,
        },
        9 => Event::Digest {
            period: a,
            summary: rest.as_ref(),
        },
        10 => Event::BudgetBurn {
            monitor: a,
            burn_rate_x10: number,
            window: b,
            exhausted_in_secs: optional(flags, 0, other),
        },
        _ => Event::Alert {
            monitor: a,
            severity: [
                AlertSeverity::Info,
                AlertSeverity::Warning,
                AlertSeverity::Error,
                AlertSeverity::Critical,
            ][usize::from(flags % 4)],
            title: b,
            message: rest.as_ref(),
        },
    };

    for sent in hora_notify::fuzz::render_all(event) {
        let title = sent.title.as_deref().unwrap_or_default();
        let (title_chars, body_chars) = (title.chars().count(), sent.body.chars().count());
        match sent.channel {
            // An embed: 256 characters of title, 4096 of description.
            "discord" => assert!(
                title_chars <= 256 && body_chars <= 4096,
                "discord: {title_chars} / {body_chars}"
            ),
            // Slack cuts `text` at 40 000 characters.
            "slack" => assert!(body_chars <= 40_000, "slack: {body_chars}"),
            "telegram" => {
                // Nothing of the event's may read as a tag: once the tags
                // Hora writes are taken out, no angle bracket is left.
                let mut text = sent.body.clone();
                for tag in ["<b>", "</b>", "<code>", "</code>"] {
                    text = text.replace(tag, "");
                }
                assert!(!text.contains(['<', '>']), "telegram: {}", sent.body);
            }
            // Over 4096 bytes ntfy makes the body an attachment.
            "ntfy" => assert!(sent.body.len() <= 4096, "ntfy: {}", sent.body.len()),
            // Pushover: 250 characters of title, 1024 of message.
            "pushover" => assert!(
                title_chars <= 250 && body_chars <= 1024,
                "pushover: {title_chars} / {body_chars}"
            ),
            "webhook" => assert!(
                serde_json::from_str::<serde_json::Value>(&sent.body).is_ok(),
                "webhook: {}",
                sent.body
            ),
            _ => {}
        }
    }
});
