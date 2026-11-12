//! The one place an [`Event`] is put into words.
//!
//! Every human-facing channel renders the same channel-neutral [`Message`]:
//! a headline (a `subject` the rich channels set in bold, then the `rest` of the
//! sentence), an optional preformatted `code` block (a down monitor's error)
//! and detail [`Line`]s. A channel only chooses its icon/colour/priority from
//! the [`Kind`] and [`Urgency`], applies its own markup and escaping, and calls
//! [`Message::fit`] with its length limits. Adding an `Event` variant therefore
//! means one new arm here, not ten diverging copies.
//!
//! The generic JSON webhook is the exception: its payload is a structured
//! public contract, built from the event itself (see `webhook.rs`).

use crate::util::{Unit, human_duration, measure, truncate};
use crate::{AlertSeverity, Event, Release};

/// What happened, for the channels that pick an icon, colour or tag per event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Down,
    Degraded,
    Recovered,
    CertExpiring,
    DomainExpiring,
    Release,
    Digest,
    PeerLinkDegraded,
    CertChanged,
    CertUnreadable,
    BudgetBurn,
    Alert(AlertSeverity),
}

/// How loudly to notify, mapped by each push channel onto its native priority
/// (ntfy 1-5, Gotify 0-10, Pushover -1..1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Urgency {
    /// Good news or a recap: recovered, digest, info alert.
    Quiet,
    /// Worth knowing, nothing is failing: a new upstream release.
    Notice,
    /// Something needs attention soon: slow, expiring, link degraded.
    Warning,
    /// Something is broken now: down, certificate swapped, budget burning.
    High,
    /// A producer-flagged critical alert.
    Critical,
}

impl Kind {
    /// The emoji the chat channels prefix the headline with.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            Self::Down => "\u{1F534}",
            Self::Degraded => "\u{1F7E0}",
            Self::Recovered => "\u{1F7E2}",
            Self::CertExpiring | Self::CertUnreadable => "\u{1F510}",
            Self::DomainExpiring => "\u{1F310}",
            Self::Release => "\u{1F4E6}",
            Self::Digest => "\u{1F4CA}",
            Self::PeerLinkDegraded => "\u{1F7E1}",
            Self::CertChanged => "\u{26A0}\u{FE0F}",
            Self::BudgetBurn => "\u{1F525}",
            Self::Alert(_) => "\u{1F514}",
        }
    }

    /// Short uppercase label for an e-mail subject (`[DOWN] …`).
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Down => "DOWN",
            Self::Degraded => "SLOW",
            Self::Recovered => "OK",
            Self::CertExpiring | Self::CertChanged | Self::CertUnreadable => "TLS",
            Self::DomainExpiring => "DOMAIN",
            Self::Release => "RELEASE",
            Self::Digest => "DIGEST",
            Self::PeerLinkDegraded => "LINK",
            Self::BudgetBurn => "BUDGET",
            Self::Alert(severity) => severity_label(severity),
        }
    }

    pub(crate) fn urgency(self) -> Urgency {
        match self {
            Self::Recovered | Self::Digest | Self::Alert(AlertSeverity::Info) => Urgency::Quiet,
            Self::Release => Urgency::Notice,
            Self::Degraded
            | Self::CertExpiring
            | Self::CertUnreadable
            | Self::DomainExpiring
            | Self::PeerLinkDegraded
            | Self::Alert(AlertSeverity::Warning) => Urgency::Warning,
            Self::Down
            | Self::CertChanged
            | Self::BudgetBurn
            | Self::Alert(AlertSeverity::Error) => Urgency::High,
            Self::Alert(AlertSeverity::Critical) => Urgency::Critical,
        }
    }
}

fn severity_label(severity: AlertSeverity) -> &'static str {
    match severity {
        AlertSeverity::Info => "INFO",
        AlertSeverity::Warning => "WARNING",
        AlertSeverity::Error => "ERROR",
        AlertSeverity::Critical => "CRITICAL",
    }
}

/// A chat channel's markup: how it escapes text, sets the subject in bold and
/// shows code. Applied by [`Message::text_with`] after [`Message::fit`].
pub(crate) struct Markup {
    /// Escapes prose (headline, lines).
    pub text: fn(&str) -> String,
    /// Escapes the content of a code block or inline code.
    pub code: fn(&str) -> String,
    pub bold: (&'static str, &'static str),
    /// Around the code block, which sits on its own line.
    pub block: (&'static str, &'static str),
    /// Around a field's value.
    pub inline: (&'static str, &'static str),
}

impl Markup {
    /// No markup at all, for plain-text bodies (Matrix `m.text`).
    pub(crate) const PLAIN: Self = Self {
        text: str::to_owned,
        code: str::to_owned,
        bold: ("", ""),
        block: ("", ""),
        inline: ("", ""),
    };
}

/// One detail line under the headline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Line {
    /// Prose; may itself span several lines (a digest, a pushed alert's detail).
    Text(String),
    /// `name: value`, the value set in monospace where the channel can
    /// (certificate fingerprints).
    Field { name: &'static str, value: String },
}

impl Line {
    fn len(&self, unit: Unit) -> usize {
        match self {
            Self::Text(text) => measure(text, unit),
            Self::Field { name, value } => measure(name, unit) + 2 + measure(value, unit),
        }
    }

    /// The line as plain text, for channels without markup.
    pub(crate) fn plain(&self) -> String {
        match self {
            Self::Text(text) => text.clone(),
            Self::Field { name, value } => format!("{name}: {value}"),
        }
    }
}

/// A rendered event, before any channel markup. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Message {
    pub kind: Kind,
    /// The part of the headline the rich channels set in bold: the monitor (or
    /// peer) name, `[ERROR] API` for a pushed alert, `Hora digest`.
    pub subject: String,
    /// The rest of the headline, including its leading separator
    /// (`" is DOWN"`, `": owner/repo v2 is out (running v1)"`).
    pub rest: String,
    /// Preformatted detail shown as a code block (a down monitor's error).
    pub code: Option<String>,
    pub lines: Vec<Line>,
}

impl Message {
    /// Put `event` into words. The single source of every notification text.
    pub(crate) fn render(event: Event<'_>) -> Self {
        let (kind, subject, rest) = match event {
            Event::Down { monitor, .. } => (Kind::Down, monitor.to_owned(), " is DOWN".to_owned()),
            Event::Degraded {
                monitor,
                latency_ms,
                ..
            } => (
                Kind::Degraded,
                monitor.to_owned(),
                format!(
                    " is slow{}",
                    latency_ms.map_or_else(String::new, |ms| format!(" ({ms} ms)"))
                ),
            ),
            Event::Recovered { monitor, .. } => {
                (Kind::Recovered, monitor.to_owned(), " recovered".to_owned())
            }
            Event::CertExpiring { monitor, secs_left } => (
                Kind::CertExpiring,
                monitor.to_owned(),
                format!(" TLS certificate {}", expiry_phrase(secs_left)),
            ),
            Event::DomainExpiring {
                monitor,
                domain,
                secs_left,
            } => (
                Kind::DomainExpiring,
                monitor.to_owned(),
                format!(" domain {domain} {}", expiry_phrase(secs_left)),
            ),
            Event::ReleaseAvailable(release) => (
                Kind::Release,
                release.monitor.to_owned(),
                format!(": {}", release_phrase(&release)),
            ),
            Event::Digest { period, .. } => (
                Kind::Digest,
                "Hora digest".to_owned(),
                format!(" ({period})"),
            ),
            Event::PeerLinkDegraded { peer, .. } => (
                Kind::PeerLinkDegraded,
                peer.to_owned(),
                " link degraded".to_owned(),
            ),
            Event::CertUnreadable { monitor, .. } => (
                Kind::CertUnreadable,
                monitor.to_owned(),
                " TLS certificate could not be read".to_owned(),
            ),
            Event::CertChanged { monitor, .. } => (
                Kind::CertChanged,
                monitor.to_owned(),
                " TLS certificate changed unexpectedly".to_owned(),
            ),
            Event::BudgetBurn {
                monitor,
                burn_rate_x10,
                window,
                exhausted_in_secs,
            } => (
                Kind::BudgetBurn,
                monitor.to_owned(),
                format!(
                    " {}",
                    budget_burn_phrase(burn_rate_x10, window, exhausted_in_secs)
                ),
            ),
            Event::Alert {
                monitor,
                severity,
                title,
                ..
            } => (
                Kind::Alert(severity),
                format!("[{}] {monitor}", severity_label(severity)),
                format!(": {title}"),
            ),
        };
        let (code, lines) = details(event);
        Self {
            kind,
            subject,
            rest,
            code,
            lines,
        }
    }

    pub(crate) fn urgency(&self) -> Urgency {
        self.kind.urgency()
    }

    /// The headline as plain text: `"API is DOWN"`.
    pub(crate) fn headline(&self) -> String {
        format!("{}{}", self.subject, self.rest)
    }

    /// The headline behind its [`Kind::tag`] (`"[DOWN] API is DOWN"`), for an
    /// e-mail subject. A pushed alert's headline already starts with its
    /// severity label, so it is not tagged twice.
    pub(crate) fn tagged_headline(&self) -> String {
        match self.kind {
            Kind::Alert(_) => self.headline(),
            _ => format!("[{}] {}", self.kind.tag(), self.headline()),
        }
    }

    /// Everything under the headline as plain text, one piece per line.
    pub(crate) fn plain_body(&self) -> String {
        self.code
            .iter()
            .cloned()
            .chain(self.lines.iter().map(Line::plain))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The headline and the body apart, for a channel with a title field
    /// (Gotify, ntfy, Pushover): a phone's notification list then names the
    /// monitor on every line instead of repeating one fixed title. An event
    /// without detail (a recovery) repeats its headline as the body, which
    /// these APIs refuse to take empty.
    pub(crate) fn titled(&self) -> (String, String) {
        let title = self.headline();
        let body = self.plain_body();
        if body.is_empty() {
            (title.clone(), title)
        } else {
            (title, body)
        }
    }

    /// The whole message as plain text: the headline, then the body.
    pub(crate) fn plain(&self) -> String {
        let body = self.plain_body();
        if body.is_empty() {
            self.headline()
        } else {
            format!("{}\n{body}", self.headline())
        }
    }

    /// The headline in `markup`, behind the kind's icon: `🔴 <b>API</b> is DOWN`.
    pub(crate) fn headline_with(&self, markup: &Markup) -> String {
        format!(
            "{} {}{}{}{}",
            self.kind.icon(),
            markup.bold.0,
            (markup.text)(&self.subject),
            markup.bold.1,
            (markup.text)(&self.rest),
        )
    }

    /// Everything under the headline in `markup`, one piece per line.
    pub(crate) fn body_with(&self, markup: &Markup) -> String {
        let code = self.code.iter().map(|code| {
            format!(
                "{}{}{}",
                markup.block.0,
                (markup.code)(code),
                markup.block.1
            )
        });
        let lines = self.lines.iter().map(|line| match line {
            Line::Text(text) => (markup.text)(text),
            Line::Field { name, value } => format!(
                "{name}: {}{}{}",
                markup.inline.0,
                (markup.code)(value),
                markup.inline.1
            ),
        });
        code.chain(lines).collect::<Vec<_>>().join("\n")
    }

    /// The headline and the body in `markup`.
    pub(crate) fn text_with(&self, markup: &Markup) -> String {
        let body = self.body_with(markup);
        let head = self.headline_with(markup);
        if body.is_empty() {
            head
        } else {
            format!("{head}\n{body}")
        }
    }

    /// Shrink the message to a channel's limits, measured in `unit`, *before*
    /// markup is applied (cutting rendered HTML or a code fence would break
    /// it): the headline to `head`, the body (code block, then lines, one
    /// newline between pieces) to `body`. A cut piece ends with an ellipsis
    /// and the pieces after it are dropped. Channels leave a margin in their
    /// limits for their own icon and markup.
    ///
    /// Without this, a long error or digest is answered with an HTTP 400 by
    /// the API, i.e. the alert is lost.
    #[must_use]
    pub(crate) fn fit(mut self, head: usize, body: usize, unit: Unit) -> Self {
        self.subject = truncate(&self.subject, head, unit);
        self.rest = truncate(
            &self.rest,
            head.saturating_sub(measure(&self.subject, unit)),
            unit,
        );

        let mut budget = body;
        if let Some(code) = self.code.take() {
            let fitted = truncate(&code, budget, unit);
            budget = budget.saturating_sub(measure(&fitted, unit) + 1);
            if fitted != code {
                // Cut: what follows would read as part of the error.
                self.lines.clear();
            }
            self.code = (!fitted.is_empty()).then_some(fitted);
        }
        let mut kept = Vec::with_capacity(self.lines.len());
        for line in std::mem::take(&mut self.lines) {
            let len = line.len(unit);
            if len <= budget {
                budget = budget.saturating_sub(len + 1);
                kept.push(line);
                continue;
            }
            let cut = match line {
                Line::Text(text) => Some(Line::Text(truncate(&text, budget, unit))),
                Line::Field { name, value } => {
                    let room = budget.saturating_sub(measure(name, unit) + 2);
                    (room > 0).then(|| Line::Field {
                        name,
                        value: truncate(&value, room, unit),
                    })
                }
            };
            kept.extend(cut.filter(|line| line.len(unit) > 0));
            break;
        }
        self.lines = kept;
        self
    }
}

/// The body of each event: its code block and detail lines.
fn details(event: Event<'_>) -> (Option<String>, Vec<Line>) {
    match event {
        Event::Down {
            error,
            cause,
            impacted,
            vantage,
            event,
            ..
        } => {
            let mut lines = Vec::new();
            if let Some(cause) = cause {
                lines.push(Line::Text(format!("caused by {cause}")));
            } else if !impacted.is_empty() {
                lines.push(Line::Text(format!(
                    "impacts {}: {}",
                    impacted.len(),
                    impacted.join(", ")
                )));
            }
            if let Some(vantage) = vantage {
                lines.push(Line::Text(vantage.to_owned()));
            }
            if let Some(event) = event {
                lines.push(Line::Text(format!("recent change: {event}")));
            }
            (Some(error.unwrap_or("no response").to_owned()), lines)
        }
        Event::ReleaseAvailable(release) => (
            None,
            release
                .notes
                .into_iter()
                .chain([release.url])
                .map(|line| Line::Text(line.to_owned()))
                .collect(),
        ),
        Event::Digest { summary, .. } => (None, vec![Line::Text(summary.to_owned())]),
        Event::PeerLinkDegraded { witness, .. } => (
            None,
            vec![Line::Text(format!(
                "unreachable from here, but still seen up by {witness} (likely a network partition)"
            ))],
        ),
        Event::CertChanged {
            old_fingerprint,
            new_fingerprint,
            ..
        } => (
            None,
            vec![
                Line::Field {
                    name: "old",
                    value: old_fingerprint.to_owned(),
                },
                Line::Field {
                    name: "new",
                    value: new_fingerprint.to_owned(),
                },
                Line::Text(
                    "This may indicate a MITM attack or an unexpected certificate renewal."
                        .to_owned(),
                ),
            ],
        ),
        Event::CertUnreadable { error, .. } => (
            Some(error.to_owned()),
            vec![Line::Text(
                "Its expiry is not watched until it can be read again.".to_owned(),
            )],
        ),
        Event::Alert { message, .. } if !message.is_empty() => {
            (None, vec![Line::Text(message.to_owned())])
        }
        Event::Degraded {
            detail: Some(detail),
            ..
        } if !detail.is_empty() => (None, vec![Line::Text(detail.to_owned())]),
        Event::Degraded { .. }
        | Event::Recovered { .. }
        | Event::CertExpiring { .. }
        | Event::DomainExpiring { .. }
        | Event::BudgetBurn { .. }
        | Event::Alert { .. } => (None, Vec::new()),
    }
}

/// `"expires in 3 d"`, `"expires in 30 h"`, `"expires in 10 h"`,
/// `"expires in 45 min"`, `"has expired"`. Under two days the hours count,
/// and the minutes in the last hour: "has expired" with ten hours still to
/// go sends someone to renew in a panic, or not at all.
fn expiry_phrase(secs_left: i64) -> String {
    match u64::try_from(secs_left) {
        Ok(secs) if secs > 0 => format!("expires in {}", human_duration(secs)),
        _ => "has expired".to_owned(),
    }
}

/// A burn window as a person reads it: the wire's `"1h"` is `"1 h"`. The
/// webhook payload keeps the compact form.
fn window_phrase(window: &str) -> String {
    match window.find(|c: char| !c.is_ascii_digit()) {
        Some(unit) if unit > 0 => format!("{} {}", &window[..unit], &window[unit..]),
        _ => window.to_owned(),
    }
}

/// `"owner/repo v1.9.2 is out (running v1.9.1)"`.
fn release_phrase(release: &Release<'_>) -> String {
    format!(
        "{} {} is out (running {})",
        release.project, release.latest, release.current
    )
}

/// `"burning error budget at 14.4x (1 h) - exhausted in ~23 h at this rate"`.
fn budget_burn_phrase(burn_rate_x10: i64, window: &str, exhausted_in_secs: Option<i64>) -> String {
    let rate = if burn_rate_x10 % 10 == 0 {
        format!("{}x", burn_rate_x10 / 10)
    } else {
        format!("{}.{}x", burn_rate_x10 / 10, burn_rate_x10 % 10)
    };
    let eta = exhausted_in_secs.map_or_else(String::new, |secs| match u64::try_from(secs) {
        Ok(secs) if secs > 0 => format!(" - exhausted in ~{} at this rate", human_duration(secs)),
        // Zero, or a negative estimate from a clock hiccup: already spent.
        _ => " - budget already exhausted".to_owned(),
    });
    format!(
        "burning error budget at {rate} ({}){eta}",
        window_phrase(window)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn down<'a>(
        error: Option<&'a str>,
        cause: Option<&'a str>,
        impacted: &'a [&'a str],
    ) -> Event<'a> {
        Event::Down {
            monitor: "API",
            error,
            cause,
            impacted,
            vantage: None,
            event: None,
            local_only: false,
        }
    }

    #[test]
    fn down_headline_code_and_topology() {
        let msg = Message::render(Event::Down {
            monitor: "API",
            error: Some("boom"),
            cause: Some("DB"),
            impacted: &[],
            vantage: Some("confirmed down from 3/3 vantage points"),
            event: Some("deploy api v2.3, 3 min before"),
            local_only: false,
        });
        assert_eq!(msg.headline(), "API is DOWN");
        assert_eq!(msg.tagged_headline(), "[DOWN] API is DOWN");
        assert_eq!(msg.urgency(), Urgency::High);
        assert_eq!(
            msg.plain(),
            "API is DOWN\nboom\ncaused by DB\nconfirmed down from 3/3 vantage points\n\
             recent change: deploy api v2.3, 3 min before"
        );

        let root = Message::render(down(None, None, &["API", "Web"]));
        assert_eq!(root.code.as_deref(), Some("no response"));
        assert_eq!(root.plain_body(), "no response\nimpacts 2: API, Web");
    }

    #[test]
    fn every_event_has_one_wording() {
        let cases = [
            (
                Event::Degraded {
                    monitor: "API",
                    latency_ms: Some(1234),
                    detail: None,
                },
                "API is slow (1234 ms)",
            ),
            (
                Event::Recovered {
                    monitor: "API",
                    local_only: false,
                },
                "API recovered",
            ),
            (
                Event::CertExpiring {
                    monitor: "API",
                    secs_left: 3 * 86_400,
                },
                "API TLS certificate expires in 3 d",
            ),
            (
                Event::DomainExpiring {
                    monitor: "API",
                    domain: "example.com",
                    secs_left: 86_400,
                },
                "API domain example.com expires in 24 h",
            ),
            (
                Event::ReleaseAvailable(Release {
                    monitor: "Chat",
                    project: "o/r",
                    current: "v1",
                    latest: "v2",
                    url: "https://x/r",
                    notes: None,
                }),
                "Chat: o/r v2 is out (running v1)\nhttps://x/r",
            ),
            (
                Event::ReleaseAvailable(Release {
                    monitor: "Hora",
                    project: "uplg/hora",
                    current: "0.11.0",
                    latest: "v0.12.0",
                    url: "https://x/r",
                    notes: Some("Your logo in the header."),
                }),
                "Hora: uplg/hora v0.12.0 is out (running 0.11.0)\nYour logo in the header.\nhttps://x/r",
            ),
            (
                Event::Digest {
                    period: "week",
                    summary: "all good",
                },
                "Hora digest (week)\nall good",
            ),
            (
                Event::PeerLinkDegraded {
                    peer: "Hora B",
                    witness: "Hora C",
                },
                "Hora B link degraded\nunreachable from here, but still seen up by Hora C \
                 (likely a network partition)",
            ),
            (
                Event::BudgetBurn {
                    monitor: "API",
                    burn_rate_x10: 144,
                    window: "1h",
                    exhausted_in_secs: Some(0),
                },
                "API burning error budget at 14.4x (1 h) - budget already exhausted",
            ),
            (
                Event::Alert {
                    monitor: "API",
                    severity: AlertSeverity::Error,
                    title: "boom",
                    message: "stack trace",
                },
                "[ERROR] API: boom\nstack trace",
            ),
            (
                Event::Alert {
                    monitor: "API",
                    severity: AlertSeverity::Info,
                    title: "deploy started",
                    message: "",
                },
                "[INFO] API: deploy started",
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(Message::render(event).plain(), expected);
        }
    }

    #[test]
    fn cert_changed_keeps_fingerprints() {
        let msg = Message::render(Event::CertChanged {
            monitor: "API",
            old_fingerprint: "aa:bb",
            new_fingerprint: "cc:dd",
        });
        assert_eq!(msg.headline(), "API TLS certificate changed unexpectedly");
        assert!(msg.plain().contains("old: aa:bb\nnew: cc:dd"));
    }

    #[test]
    fn unreadable_cert_names_the_error() {
        let msg = Message::render(Event::CertUnreadable {
            monitor: "Mail",
            error: "starttls negotiation timed out",
        });
        assert_eq!(
            msg.tagged_headline(),
            "[TLS] Mail TLS certificate could not be read"
        );
        assert_eq!(msg.code.as_deref(), Some("starttls negotiation timed out"));
        assert_eq!(msg.urgency(), Urgency::Warning);
    }

    #[test]
    fn alert_subject_is_not_tagged_twice() {
        let msg = Message::render(Event::Alert {
            monitor: "API",
            severity: AlertSeverity::Critical,
            title: "disk full",
            message: "",
        });
        assert_eq!(msg.tagged_headline(), "[CRITICAL] API: disk full");
        assert_eq!(msg.urgency(), Urgency::Critical);
    }

    #[test]
    fn expiry_and_burn_phrasing() {
        const DAY: i64 = 86_400;
        assert_eq!(expiry_phrase(-1), "has expired");
        assert_eq!(expiry_phrase(0), "has expired");
        assert_eq!(expiry_phrase(59 * 60), "expires in 59 min");
        assert_eq!(expiry_phrase(59), "expires in 59 s");
        assert_eq!(expiry_phrase(45 * 60), "expires in 45 min");
        assert_eq!(expiry_phrase(3_600), "expires in 1 h");
        assert_eq!(expiry_phrase(10 * 3_600), "expires in 10 h");
        assert_eq!(expiry_phrase(DAY), "expires in 24 h");
        assert_eq!(expiry_phrase(3 * DAY + 5), "expires in 3 d");
        assert_eq!(expiry_phrase(3 * DAY + 7 * 3_600), "expires in 3 d 7 h");
        assert_eq!(
            budget_burn_phrase(60, "6h", Some(200_000)),
            "burning error budget at 6x (6 h) - exhausted in ~2 d 7 h at this rate"
        );
        // A negative estimate reads as spent, not as "~-5 s".
        assert!(budget_burn_phrase(144, "1h", Some(-5)).ends_with("budget already exhausted"));
    }

    #[test]
    fn fit_leaves_short_messages_alone() {
        let msg = Message::render(down(Some("boom"), Some("DB"), &[]));
        assert_eq!(msg.clone().fit(100, 100, Unit::Chars), msg);
    }

    #[test]
    fn fit_cuts_a_long_error_and_drops_what_follows() {
        let error = "é".repeat(5000);
        let msg = Message::render(down(Some(&error), Some("DB"), &[])).fit(50, 100, Unit::Chars);
        let code = msg.code.expect("code kept");
        assert_eq!(code.chars().count(), 100);
        assert!(code.ends_with('…'));
        assert!(msg.lines.is_empty(), "lines after a cut piece are dropped");
    }

    #[test]
    fn fit_cuts_lines_within_the_body_budget() {
        let summary = "x".repeat(10_000);
        let msg = Message::render(Event::Digest {
            period: "week",
            summary: &summary,
        })
        .fit(50, 4000, Unit::Chars);
        assert_eq!(msg.plain_body().chars().count(), 4000);

        // Measured in bytes for byte-limited channels (ntfy).
        let summary = "€".repeat(2000); // 3 bytes each
        let msg = Message::render(Event::Digest {
            period: "week",
            summary: &summary,
        })
        .fit(50, 1000, Unit::Bytes);
        assert!(msg.plain_body().len() <= 1000);
    }

    #[test]
    fn fit_cuts_the_headline() {
        let title = "t".repeat(300);
        let msg = Message::render(Event::Alert {
            monitor: "API",
            severity: AlertSeverity::Error,
            title: &title,
            message: "",
        })
        .fit(100, 100, Unit::Chars);
        assert_eq!(msg.headline().chars().count(), 100);
        assert!(msg.subject.starts_with("[ERROR] API"));
    }
}
