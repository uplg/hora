//! Monitor fields parsed once at load (regexes, JSON paths, addresses, DNS
//! record types) rather than on every probe.

use serde::Deserialize;

use super::split_host_port;

/// A config string parsed once, at load, into the typed value the probe uses
/// on every tick (a compiled regex, a `JSONPath`, a socket address) instead of
/// re-parsing it per probe.
///
/// A malformed value does *not* fail deserialization: the error is kept and
/// reported by validation, which can name the monitor - serde's error path
/// only says `monitors.number_regex`, not which `[[monitors]]` entry is wrong.
/// Equality (hot-reload change detection) and `Debug` go by the raw text.
#[derive(Clone)]
pub struct Parsed<T> {
    raw: String,
    value: Result<T, String>,
}

/// How a [`Parsed`] field turns its raw text into a value.
pub trait ParseField: Sized {
    /// Parse the raw config text.
    ///
    /// # Errors
    ///
    /// A human-readable reason (validation prefixes the monitor and field)
    /// when `raw` is not a valid value.
    fn parse_field(raw: &str) -> Result<Self, String>;
}

impl<T: ParseField> Parsed<T> {
    /// Parse `raw` now, keeping the error (if any) for validation.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let value = T::parse_field(&raw);
        Self { raw, value }
    }
}

impl<T> Parsed<T> {
    /// The text as written in the config.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// The parsed value; `None` only for a malformed value, which validation
    /// rejects before any probe runs.
    #[must_use]
    pub fn get(&self) -> Option<&T> {
        self.value.as_ref().ok()
    }

    /// Why the raw text did not parse, if it did not.
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.value.as_ref().err().map(String::as_str)
    }
}

impl<T: Clone> Parsed<T> {
    /// The parsed value with its text, once validation has made sure it
    /// parsed; `None` for a malformed value.
    #[must_use]
    pub fn compiled(&self) -> Option<Compiled<T>> {
        self.get().map(|value| Compiled {
            raw: self.raw.clone(),
            value: value.clone(),
        })
    }
}

/// A config string and the value it parsed into, after validation: unlike a
/// [`Parsed`], it cannot hold an error, so the probes use the value directly.
/// Equality (hot-reload change detection) and `Debug` go by the raw text.
#[derive(Clone)]
pub struct Compiled<T> {
    raw: String,
    value: T,
}

impl<T> Compiled<T> {
    /// The text as written in the config.
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl<T: ParseField> Compiled<T> {
    /// Parse `raw`, for values built outside a config file (tests, the CLI).
    ///
    /// # Errors
    ///
    /// The parse error, as for a config value.
    pub fn parse(raw: &str) -> Result<Self, String> {
        T::parse_field(raw).map(|value| Self {
            raw: raw.to_owned(),
            value,
        })
    }
}

impl<T> std::ops::Deref for Compiled<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> PartialEq for Compiled<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Compiled<T> {}

impl<T> std::fmt::Debug for Compiled<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.raw, f)
    }
}

impl<T> PartialEq for Parsed<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T> Eq for Parsed<T> {}

impl<T> std::fmt::Debug for Parsed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.raw, f)
    }
}

impl<'de, T: ParseField> Deserialize<'de> for Parsed<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

impl ParseField for regex::Regex {
    fn parse_field(raw: &str) -> Result<Self, String> {
        Self::new(raw).map_err(|err| err.to_string())
    }
}

impl ParseField for croner::Cron {
    fn parse_field(raw: &str) -> Result<Self, String> {
        super::parse_cron(raw).map_err(|err| err.to_string())
    }
}

impl ParseField for serde_json_path::JsonPath {
    fn parse_field(raw: &str) -> Result<Self, String> {
        Self::parse(raw).map_err(|err| err.to_string())
    }
}

/// A DNS resolver address: an IP and a port, IPv6 bracketed. A hostname is
/// refused rather than resolved at load: it would be looked up through some
/// *other* resolver, and silently go stale when its address changes.
impl ParseField for std::net::SocketAddr {
    fn parse_field(raw: &str) -> Result<Self, String> {
        if let Ok(addr) = raw.parse() {
            return Ok(addr);
        }
        Err(if split_host_port(raw).is_some() {
            "must be an IP address and port such as 9.9.9.9:53 or [2620:fe::fe]:53, not a hostname"
        } else {
            "must be host:port, e.g. 9.9.9.9:53 or [2620:fe::fe]:53"
        }
        .to_owned())
    }
}

/// The DNS record types a dns monitor can query: the one list shared by the
/// config, the probe and the public failure reasons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsRecord {
    A,
    Aaaa,
    Cname,
    Mx,
    Ns,
    Txt,
    Srv,
    Soa,
    Ptr,
}

impl DnsRecord {
    /// Every supported type.
    pub const ALL: [Self; 9] = [
        Self::A,
        Self::Aaaa,
        Self::Cname,
        Self::Mx,
        Self::Ns,
        Self::Txt,
        Self::Srv,
        Self::Soa,
        Self::Ptr,
    ];

    /// Uppercase name, as DNS tools (and the probe's failure reasons) spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::Aaaa => "AAAA",
            Self::Cname => "CNAME",
            Self::Mx => "MX",
            Self::Ns => "NS",
            Self::Txt => "TXT",
            Self::Srv => "SRV",
            Self::Soa => "SOA",
            Self::Ptr => "PTR",
        }
    }
}

impl ParseField for DnsRecord {
    fn parse_field(raw: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|record| record.as_str().eq_ignore_ascii_case(raw.trim()))
            .ok_or_else(|| format!("unsupported dns_record type {raw:?}"))
    }
}

/// `expected_status` as the file spells it: one entry or a list of them.
///
/// One key rather than a second `expected_statuses` beside it: two keys
/// would need a rule for a file that sets both, and a list of one would mean
/// the same as the scalar under another name. Widening the existing key keeps
/// every file that loads today loading, and `expected_status = 200` means
/// what it always did.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawStatuses {
    One(RawStatus),
    Many(Vec<RawStatus>),
}

/// One `expected_status` entry: a status (`401`) or a class (`"2xx"`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub(crate) enum RawStatus {
    Code(i64),
    Class(String),
}

/// One accepted status or class of statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusMatch {
    /// A whole class, by its hundreds digit (`2` for `2xx`).
    Class(u16),
    Code(u16),
}

impl StatusMatch {
    fn first(self) -> u16 {
        match self {
            Self::Class(class) => class * 100,
            Self::Code(code) => code,
        }
    }

    fn matches(self, status: u16) -> bool {
        match self {
            Self::Class(class) => status / 100 == class,
            Self::Code(code) => status == code,
        }
    }
}

/// The statuses an HTTP monitor accepts as up, when it says so
/// (`expected_status`): statuses and classes, in one canonical form.
///
/// Canonical means sorted, without repeats, and without a status its own
/// class already covers, so `[401, 200]`, `[200, 401, 200]` and - for a
/// single one - `200` and `[200]` are the same value: the same check, the
/// same fingerprint between peers, no monitor restart on a reload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedStatus(Vec<StatusMatch>);

impl ExpectedStatus {
    /// Exactly this status, for values built outside a config file (tests).
    #[must_use]
    pub fn code(status: u16) -> Self {
        Self(vec![StatusMatch::Code(status)])
    }

    /// From the file's value.
    ///
    /// # Errors
    ///
    /// A reason (validation prefixes the monitor) for an entry that is
    /// neither an HTTP status nor a class, or for an empty list - which no
    /// answer could satisfy.
    pub(crate) fn from_raw(raw: &RawStatuses) -> Result<Self, String> {
        let entries = match raw {
            RawStatuses::One(entry) => std::slice::from_ref(entry),
            RawStatuses::Many(entries) => entries.as_slice(),
        };
        if entries.is_empty() {
            return Err("expected_status lists no status: no answer could be up".to_owned());
        }
        let parsed = entries
            .iter()
            .map(|entry| match entry {
                RawStatus::Code(code) => u16::try_from(*code)
                    .ok()
                    .filter(|code| (100..=599).contains(code))
                    .map(StatusMatch::Code)
                    // No server answers outside 100-599, so such an entry
                    // could never match.
                    .ok_or_else(|| {
                        format!("expected_status {code} is not an HTTP status (100-599)")
                    }),
                RawStatus::Class(text) => Self::class(text).ok_or_else(|| {
                    format!(
                        "expected_status {text:?} is not a status class (\"1xx\" to \"5xx\"); \
                         a single status is a number, without quotes"
                    )
                }),
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self::canonical(parsed))
    }

    /// From a command-line value: entries separated by commas (`200,401`,
    /// `2xx,401`).
    ///
    /// # Errors
    ///
    /// As for a config value.
    pub fn parse_list(text: &str) -> Result<Self, String> {
        let entries = text
            .split(',')
            .map(str::trim)
            .map(|entry| {
                entry
                    .parse()
                    .map_or_else(|_| RawStatus::Class(entry.to_owned()), RawStatus::Code)
            })
            .collect();
        Self::from_raw(&RawStatuses::Many(entries))
    }

    /// `"2xx"` (in either case) as a class.
    fn class(text: &str) -> Option<StatusMatch> {
        let class = match text.to_ascii_lowercase().as_str() {
            "1xx" => 1,
            "2xx" => 2,
            "3xx" => 3,
            "4xx" => 4,
            "5xx" => 5,
            _ => return None,
        };
        Some(StatusMatch::Class(class))
    }

    fn canonical(mut entries: Vec<StatusMatch>) -> Self {
        let classes: Vec<StatusMatch> = entries
            .iter()
            .copied()
            .filter(|entry| matches!(entry, StatusMatch::Class(_)))
            .collect();
        entries.retain(|entry| match *entry {
            StatusMatch::Class(_) => true,
            StatusMatch::Code(code) => !classes.iter().any(|class| class.matches(code)),
        });
        // By the first status each accepts. Two entries now share one only
        // when they are the same entry twice, which the sort puts side by
        // side.
        entries.sort_unstable_by_key(|entry| entry.first());
        entries.dedup();
        Self(entries)
    }

    /// Whether an answer with this status is up.
    #[must_use]
    pub fn matches(&self, status: u16) -> bool {
        self.0.iter().any(|entry| entry.matches(status))
    }

    /// Whether a redirect (any 3xx) is among the accepted answers.
    #[must_use]
    pub fn accepts_redirect(&self) -> bool {
        self.0.iter().any(|entry| entry.first() / 100 == 3)
    }
}

/// The canonical form, entries separated by commas (`200,401`, `2xx,401`):
/// what peers compare, and what a failure reason may quote.
impl std::fmt::Display for ExpectedStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, entry) in self.0.iter().enumerate() {
            if index > 0 {
                f.write_str(",")?;
            }
            match entry {
                StatusMatch::Class(class) => write!(f, "{class}xx")?,
                StatusMatch::Code(code) => write!(f, "{code}")?,
            }
        }
        Ok(())
    }
}

/// The request an HTTP monitor sends (`method`): `GET` unless it says `HEAD`.
///
/// Only the two methods that carry no request body: Hora has no setting for
/// one, and a `POST` or `PUT` without a body is a request few services are
/// meant to answer. It would also raise a question redirects do not have to
/// answer today (a 301, 302 or 303 turns a `POST` into a `GET`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HttpMethod {
    #[default]
    Get,
    /// Headers only: the cheapest check of a large page or a download, and
    /// the one for an endpoint that counts its `GET`s.
    Head,
}

impl HttpMethod {
    /// Uppercase, as HTTP spells it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
        }
    }
}

impl ParseField for HttpMethod {
    fn parse_field(raw: &str) -> Result<Self, String> {
        [Self::Get, Self::Head]
            .into_iter()
            .find(|method| method.as_str().eq_ignore_ascii_case(raw.trim()))
            .ok_or_else(|| {
                format!(
                    "method {raw:?} is not supported: \"GET\" or \"HEAD\" (Hora sends no \
                     request body)"
                )
            })
    }
}

impl From<HttpMethod> for reqwest::Method {
    fn from(method: HttpMethod) -> Self {
        match method {
            HttpMethod::Get => Self::GET,
            HttpMethod::Head => Self::HEAD,
        }
    }
}
