//! The public-visibility policy for failure reasons.

use crate::config::DnsRecord;

/// What kind of failure a check reports, stored next to its detail
/// (`checks.reason`, `incidents.reason`) so the public form of a reason is a
/// lookup, not a guess from the wording. The detail itself keeps the operator
/// view (body snippets, DNS answers, raw socket errors, the job's message).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FailureKind {
    /// The whole request (or ping) ran out of time.
    Timeout,
    /// The HTTP connection could not be established.
    ConnectionFailed,
    /// The TCP connection ran out of time.
    ConnectionTimedOut,
    TooManyRedirects,
    /// An `https` request was redirected to plain `http`.
    InsecureRedirect,
    /// A public target redirected to a loopback, link-local or private
    /// address (see `crate::http::RedirectGuard`).
    PrivateRedirect,
    /// The body could not be read or decoded.
    InvalidBody,
    /// Any other HTTP client error.
    RequestError,
    /// The target's name did not resolve.
    Unresolvable,
    /// The answer came back with an unexpected HTTP status (in `status_code`).
    Http,
    /// A keyword, JSON or number assertion failed (the detail names the
    /// configured keyword or query).
    Content,
    /// A DNS answer differed from the pinned one (the detail holds it).
    DnsMismatch,
    /// The DNS query failed.
    DnsFailed,
    DnsTimeout,
    /// The DNS query returned no record of the asked type.
    NoRecords,
    /// The ICMP socket or the ping failed.
    Ping,
    /// An exec plugin failed or warned (the detail is its own output).
    Plugin,
    /// Dual-stack: IPv4 broken, IPv6 fine.
    Ipv4Failing,
    /// Dual-stack: IPv6 broken, IPv4 fine.
    Ipv6Failing,
    /// Dual-stack: both families broken.
    BothFamiliesFailing,
    /// No heartbeat within the expected interval.
    MissingHeartbeat,
    /// No heartbeat for a scheduled (cron) run.
    MissedSchedule,
    /// No heartbeat ever, past the first deadline.
    NoHeartbeatYet,
    /// A push reported down or degraded; the detail is the job's own message.
    Pushed,
    /// The check could not run for lack of resources on this host (file
    /// descriptors, memory): nothing is known about the target. Never
    /// stored: the scheduler skips such a check (see `probe::local`).
    Local,
    /// Anything else: raw socket errors, setup failures.
    Other,
}

impl FailureKind {
    pub(crate) const ALL: [Self; 26] = [
        Self::Timeout,
        Self::ConnectionFailed,
        Self::ConnectionTimedOut,
        Self::TooManyRedirects,
        Self::InsecureRedirect,
        Self::PrivateRedirect,
        Self::InvalidBody,
        Self::RequestError,
        Self::Unresolvable,
        Self::Http,
        Self::Content,
        Self::DnsMismatch,
        Self::DnsFailed,
        Self::DnsTimeout,
        Self::NoRecords,
        Self::Ping,
        Self::Plugin,
        Self::Ipv4Failing,
        Self::Ipv6Failing,
        Self::BothFamiliesFailing,
        Self::MissingHeartbeat,
        Self::MissedSchedule,
        Self::NoHeartbeatYet,
        Self::Pushed,
        Self::Local,
        Self::Other,
    ];

    /// Whether this is a heartbeat that did not come (a push monitor's or a
    /// peer's), as opposed to a check that ran and failed. A miss is a
    /// verdict already: the interval or the grace was the tolerance, so it
    /// is not a sample to see repeated before believing it.
    #[must_use]
    pub fn is_missed_heartbeat(self) -> bool {
        matches!(
            self,
            Self::MissingHeartbeat | Self::MissedSchedule | Self::NoHeartbeatYet
        )
    }

    /// The code stored in the `reason` columns.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::ConnectionFailed => "connection_failed",
            Self::ConnectionTimedOut => "connection_timeout",
            Self::TooManyRedirects => "too_many_redirects",
            Self::InsecureRedirect => "insecure_redirect",
            Self::PrivateRedirect => "private_redirect",
            Self::InvalidBody => "invalid_body",
            Self::RequestError => "request_error",
            Self::Unresolvable => "unresolvable",
            Self::Http => "http",
            Self::Content => "content",
            Self::DnsMismatch => "dns_mismatch",
            Self::DnsFailed => "dns_failed",
            Self::DnsTimeout => "dns_timeout",
            Self::NoRecords => "no_records",
            Self::Ping => "ping",
            Self::Plugin => "plugin",
            Self::Ipv4Failing => "ipv4_failing",
            Self::Ipv6Failing => "ipv6_failing",
            Self::BothFamiliesFailing => "both_families_failing",
            Self::MissingHeartbeat => "missing_heartbeat",
            Self::MissedSchedule => "missed_schedule",
            Self::NoHeartbeatYet => "no_heartbeat_yet",
            Self::Pushed => "pushed",
            Self::Local => "local",
            Self::Other => "other",
        }
    }

    /// The kind a stored code names; `None` for a code this version does not
    /// know (the reason then goes through the wording fallback).
    #[must_use]
    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.code() == code)
    }
}

// Stored as its code. Decoding is lenient: a code this version does not know
// (written by a newer one, before a downgrade) reads as `Other`, whose public
// form is the safe "check failed" - never an error that breaks a page.
impl sqlx::Type<sqlx::Sqlite> for FailureKind {
    fn type_info() -> sqlx::sqlite::SqliteTypeInfo {
        <str as sqlx::Type<sqlx::Sqlite>>::type_info()
    }

    fn compatible(ty: &sqlx::sqlite::SqliteTypeInfo) -> bool {
        <str as sqlx::Type<sqlx::Sqlite>>::compatible(ty)
    }
}

impl<'r> sqlx::Decode<'r, sqlx::Sqlite> for FailureKind {
    fn decode(value: sqlx::sqlite::SqliteValueRef<'r>) -> Result<Self, sqlx::error::BoxDynError> {
        let code = <&str as sqlx::Decode<sqlx::Sqlite>>::decode(value)?;
        Ok(Self::from_code(code).unwrap_or(Self::Other))
    }
}

/// The public form of a stored failure reason. Stored reasons keep full
/// operator detail - response-body snippets, DNS answers, asserted keywords,
/// raw socket errors, the job's own push message - which anonymous viewers of
/// a public monitor must not see: snippets and DNS answers are
/// remote-controlled, keywords and JSON queries reveal operator config.
///
/// With the stored [`FailureKind`] this is a lookup: each kind has its public
/// words, and only the two kinds whose detail Hora writes itself from safe
/// parts (`HTTP 503`, `no AAAA records found`) show part of it. Rows written
/// before the kind was stored (or with a code this version does not know) go
/// through the old wording classifier, which fails safe to "check failed".
#[must_use]
pub fn public_reason(kind: Option<FailureKind>, reason: &str) -> &str {
    let Some(kind) = kind else {
        return legacy_public_reason(reason);
    };
    match kind {
        FailureKind::Timeout => "request timed out",
        FailureKind::ConnectionFailed => "connection failed",
        FailureKind::ConnectionTimedOut => "connection timed out",
        FailureKind::TooManyRedirects => "too many redirects",
        // The detail names the plain-http origin; the category is enough.
        FailureKind::InsecureRedirect => "redirected to plain http",
        FailureKind::PrivateRedirect => "redirected to a private address",
        FailureKind::InvalidBody => "invalid response body",
        FailureKind::RequestError => "request error",
        FailureKind::Unresolvable => "could not resolve host",
        FailureKind::DnsTimeout => "DNS lookup timed out",
        // "HTTP 500: <body snippet>": the status stays, the snippet goes.
        FailureKind::Http => http_status_prefix(reason).unwrap_or("check failed"),
        FailureKind::NoRecords => no_records(reason).unwrap_or("DNS lookup failed"),
        FailureKind::Content => "content check failed",
        FailureKind::DnsMismatch => "unexpected DNS answer",
        FailureKind::DnsFailed => "DNS lookup failed",
        FailureKind::Ping => "ping failed",
        FailureKind::Plugin => "plugin check failed",
        FailureKind::Ipv4Failing => "IPv4 failing (IPv6 ok)",
        FailureKind::Ipv6Failing => "IPv6 failing (IPv4 ok)",
        FailureKind::BothFamiliesFailing => "IPv4 and IPv6 failing",
        FailureKind::MissingHeartbeat => "missing heartbeat",
        // "(was due 03:00 UTC + 30 min grace)" reveals the job's schedule.
        FailureKind::MissedSchedule => "missed scheduled heartbeat",
        FailureKind::NoHeartbeatYet => "no heartbeat received yet",
        // The job's message is free text from whoever holds the push token.
        FailureKind::Local => "check could not run",
        FailureKind::Pushed | FailureKind::Other => "check failed",
    }
}

/// `HTTP 503` out of `HTTP 503: <snippet>`, anchored to a numeric status.
fn http_status_prefix(reason: &str) -> Option<&str> {
    let rest = reason.strip_prefix("HTTP ")?;
    let code = rest.find(':').map_or(rest, |colon| &rest[..colon]);
    code.parse::<u16>()
        .is_ok()
        .then(|| &reason[..5 + code.len()])
}

/// `no AAAA records found`, only for the record types the DNS probe queries.
fn no_records(reason: &str) -> Option<&str> {
    let record = reason.strip_prefix("no ")?.strip_suffix(" records found")?;
    DnsRecord::ALL
        .iter()
        .any(|known| known.as_str() == record)
        .then_some(reason)
}

/// The wording classifier for reasons stored without a kind: known-safe
/// reasons pass through verbatim, detailed ones collapse to their category,
/// anything unrecognized (raw TCP errors, free-form push messages) falls back
/// to "check failed". Frozen: new reasons carry a kind instead.
fn legacy_public_reason(reason: &str) -> &str {
    // Static reasons carry no detail; they are their own category.
    const VERBATIM: &[&str] = &[
        "request timed out",
        "connection failed",
        "connection timed out",
        "too many redirects",
        "invalid response body",
        "request error",
        "could not resolve host",
        "DNS lookup timed out",
        "missing heartbeat",
    ];
    // "HTTP 500: <body snippet>" keeps the status, drops the snippet. Anchored
    // to a numeric status: `checks.error` also stores free-form push messages,
    // so a bare "HTTP <anything>" prefix must not become a passthrough that
    // lets a push-token holder place arbitrary text on the public page.
    if let Some(status) = http_status_prefix(reason) {
        return status;
    }
    if VERBATIM.contains(&reason) {
        return reason;
    }
    // Same anchoring: only the record types the DNS probe actually queries.
    if let Some(reason) = no_records(reason) {
        return reason;
    }
    // "missed scheduled heartbeat (was due 03:00 UTC + 30 min grace)" reveals the
    // job's schedule; keep the category only.
    if reason.starts_with("missed scheduled heartbeat") {
        return "missed scheduled heartbeat";
    }
    // Keyword, JSON and number assertions embed the configured keyword/query/
    // regex - and the number reasons additionally carry the extracted value.
    if reason.starts_with("keyword ")
        || reason.starts_with("JSON query")
        || reason.starts_with("invalid JSON query")
        || reason.starts_with("number")
        || reason.starts_with("invalid number_regex")
        || reason == "response is not valid JSON"
    {
        return "content check failed";
    }
    // The DNS pin mismatch embeds the remote-controlled answer.
    if reason.starts_with("expected ") {
        return "unexpected DNS answer";
    }
    if reason.starts_with("DNS lookup failed") || reason.starts_with("resolver setup failed") {
        return "DNS lookup failed";
    }
    if reason.starts_with("icmp") {
        return "ping failed";
    }
    // Exec plugins: their output is operator infrastructure detail (paths,
    // device names, container ids); anonymous viewers get the category only.
    if reason.starts_with("plugin ") || reason.starts_with("exec ") {
        return "plugin check failed";
    }
    // Dual-stack failures embed the failing family's detail (which may be a raw
    // socket error); keep only which family broke. Checked longest-prefix first.
    if reason.starts_with("IPv4 and IPv6 failing") {
        return "IPv4 and IPv6 failing";
    }
    if reason.starts_with("IPv4 failing") {
        return "IPv4 failing (IPv6 ok)";
    }
    if reason.starts_with("IPv6 failing") {
        return "IPv6 failing (IPv4 ok)";
    }
    "check failed"
}
