//! What every page shares around its content: the operator's sign, the
//! navigation (carrying the viewer's `?token=` and `?theme=` from page to
//! page), the theme, the versioned stylesheet and fonts. Read from the request
//! head by the [`Chrome`] extractor; each handler then says which page it is.
//!
//! The status pages are rendered once per snapshot and audience and served
//! as shared bytes, but a few of their parts belong to the request alone: the
//! `?token=` and `?theme=` every in-site link carries, the forced theme, the
//! opt-in `?refresh=`. Those parts are [`Hole`]s: a cached render writes an
//! unforgeable marker in their place ([`Chrome::cached`]), [`Spliced`] cuts
//! the page at the markers once, and each request fills them in from its own
//! [`PerRequest`]. A request that asks for nothing (no query at all) gets the
//! pre-filled bytes as they are, so the common path stays a reference count.
//! The token is only ever spliced into the response being sent: a cached
//! render never holds one, whoever asked first.

use std::borrow::Cow;
use std::future::Future;
use std::hash::BuildHasher as _;
use std::sync::LazyLock;

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Query};
use axum::http::request::Parts;
use chrono::Utc;

use crate::AppState;
use crate::handlers::assets::ASSET_VERSION;

/// The kiosk refresh a page may ask for with `?refresh=`, in seconds:
/// anything outside is ignored (no refresh, the default).
pub(crate) const REFRESH_SECS: std::ops::RangeInclusive<u16> = 10..=3600;

/// The page chrome handed to every page (as `chrome`), used by
/// the document and its header and footer (see `crate::page`).
#[derive(Clone)]
pub(crate) struct Chrome {
    /// The operator's name (`[page].title`), the sign in the header.
    pub(crate) site: String,
    /// The sign's initial tile ("P" for Pelican).
    pub(crate) initial: String,
    /// The operator's logo (`[page].logo`), shown in place of the initial
    /// tile: its versioned URL, `/logo?v=...`.
    pub(crate) logo: Option<String>,
    /// The dark theme's logo (`[page].logo_dark`), `/logo-dark?v=...`.
    pub(crate) logo_dark: Option<String>,
    /// Which navigation entry is the current page (`status`, `history`, ...).
    pub(crate) current: &'static str,
    /// Whether the viewer holds the operator token (shows the operator pill
    /// and the watchers link).
    pub(crate) operator: bool,
    /// The asset cache buster (`?v=`).
    pub(crate) v: &'static str,
    /// This month (`2026-10`), the report link's target.
    pub(crate) month: String,
    /// What this request alone asks for.
    pub(crate) req: PerRequest,
    /// Render the per-request parts as markers, for a page cached across
    /// requests (see [`Spliced`]).
    holes: bool,
}

/// What a request alone asks of a page, beside its audience: the parts a
/// shared render leaves as [`Hole`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PerRequest {
    /// `light` / `dark` when `?theme=` forces one (embeds, kiosks), else the
    /// visitor's system preference applies.
    pub(crate) theme: Option<&'static str>,
    /// The query every in-site link carries: `?token=...&theme=...`, or empty.
    pub(crate) q: String,
    /// This page with the next theme (auto, light, dark, auto again), as a
    /// query-only relative link: the theme switch is a plain link, the pages
    /// run no script.
    pub(crate) theme_href: String,
    /// The current theme's name, the switch's label.
    pub(crate) theme_label: &'static str,
    /// `?refresh=<secs>` within [`REFRESH_SECS`]: the page reloads itself.
    pub(crate) refresh: Option<u16>,
    /// A `?token=` came with the request (whatever it opens).
    pub(crate) has_token: bool,
}

/// A part of a page that depends on the request, not on the audience.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hole {
    /// ` data-theme="dark"` on `<html>`, or nothing.
    ThemeAttr,
    /// The `color-scheme` meta's content.
    Scheme,
    /// The `<meta http-equiv="refresh">`, or nothing.
    Refresh,
    /// The "Updates every 30 s" pill, or nothing.
    Live,
    /// The query in-site links carry.
    Query,
    /// The same, to follow a link's own query: `&token=...`, or nothing.
    QueryMore,
    /// The theme switch's link.
    ThemeHref,
    /// The theme switch's label.
    ThemeLabel,
    /// The private door's hidden theme field, or nothing.
    ThemeInput,
    /// The `theme-color` metas: one per system scheme, or the forced
    /// theme's alone.
    ThemeColor,
}

impl Hole {
    const ALL: [Self; 10] = [
        Self::ThemeAttr,
        Self::Scheme,
        Self::Refresh,
        Self::Live,
        Self::Query,
        Self::QueryMore,
        Self::ThemeHref,
        Self::ThemeLabel,
        Self::ThemeInput,
        Self::ThemeColor,
    ];

    fn index(self) -> usize {
        self as usize
    }
}

/// The markers a cached render writes in place of each [`Hole`]: a
/// private-use character, a per-process random nonce, the hole's number. No
/// configured name, probe answer or announcement can forge one (it would
/// have to guess the nonce), so a page is only ever cut where the template
/// put a hole.
static MARKERS: LazyLock<(String, Vec<String>)> = LazyLock::new(|| {
    let nonce = std::collections::hash_map::RandomState::new().hash_one(0x484f_5241_u32);
    let prefix = format!("\u{E000}{nonce:016x}");
    let markers = (0..Hole::ALL.len())
        .map(|index| format!("{prefix}{index:02}\u{E001}"))
        .collect();
    (prefix, markers)
});

impl PerRequest {
    /// Read the per-request parts from the URI's query.
    pub(crate) fn from_uri(uri: &axum::http::Uri) -> Self {
        let pairs: Vec<(String, String)> =
            Query::<Vec<(String, String)>>::try_from_uri(uri).map_or_default(|Query(pairs)| pairs);
        let last = |name: &str| {
            pairs
                .iter()
                .rev()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        };
        let theme = last("theme").and_then(|value| match value {
            "light" => Some("light"),
            "dark" => Some("dark"),
            _ => None,
        });
        let token = last("token").filter(|token| !token.is_empty());
        let refresh = last("refresh")
            .and_then(|value| value.parse::<u16>().ok())
            .filter(|secs| REFRESH_SECS.contains(secs));

        let mut carried = Vec::new();
        if let Some(token) = token {
            carried.push(format!("token={}", hora_core::fmt::percent_encode(token)));
        }
        if let Some(theme) = theme {
            carried.push(format!("theme={theme}"));
        }
        let q = if carried.is_empty() {
            String::new()
        } else {
            format!("?{}", carried.join("&"))
        };

        // The switch keeps every other parameter (a report's `group`, the
        // token, a kiosk's refresh) and moves the theme one step along. It is
        // a query-only link, so the same bytes serve every path of the page.
        let (next, theme_label) = match theme {
            None => (Some("light"), "Auto"),
            Some("light") => (Some("dark"), "Light"),
            Some(_) => (None, "Dark"),
        };
        let mut kept: Vec<String> = pairs
            .iter()
            .filter(|(key, _)| key != "theme")
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    hora_core::fmt::percent_encode(key),
                    hora_core::fmt::percent_encode(value)
                )
            })
            .collect();
        if let Some(next) = next {
            kept.push(format!("theme={next}"));
        }
        Self {
            theme,
            q,
            theme_href: format!("?{}", kept.join("&")),
            theme_label,
            refresh,
            has_token: token.is_some(),
        }
    }

    /// Whether this request asks for nothing of its own: the pre-filled
    /// bytes of a [`Spliced`] page serve it as they are.
    fn is_plain(&self) -> bool {
        *self == Self::plain()
    }

    /// A request without a query.
    fn plain() -> Self {
        Self {
            theme: None,
            q: String::new(),
            theme_href: "?theme=light".to_owned(),
            theme_label: "Auto",
            refresh: None,
            has_token: false,
        }
    }

    /// A hole's HTML for this request. Every value is HTML-safe here: the
    /// query parts are percent-encoded (only `&` needs escaping), the rest
    /// come from fixed sets or are numbers.
    fn fill(&self, hole: Hole) -> String {
        match hole {
            Hole::ThemeAttr => self
                .theme
                .map_or_default(|theme| format!(" data-theme=\"{theme}\"")),
            Hole::Scheme => self.theme.unwrap_or("light dark").to_owned(),
            Hole::Refresh => self
                .refresh
                .map_or_default(|secs| format!("<meta http-equiv=\"refresh\" content=\"{secs}\">")),
            Hole::Live => self.refresh.map_or_default(|secs| {
                format!(
                    "<span class=\"pill\"><svg class=\"g up\" aria-hidden=\"true\"><use \
                         href=\"#g-up\"/></svg>Updates every {}</span>",
                    hora_core::fmt::duration(i64::from(secs))
                )
            }),
            Hole::Query => self.q.replace('&', "&amp;"),
            Hole::QueryMore => self
                .q
                .strip_prefix('?')
                .map_or_default(|rest| format!("&amp;{}", rest.replace('&', "&amp;"))),
            Hole::ThemeHref => self.theme_href.replace('&', "&amp;"),
            Hole::ThemeLabel => self.theme_label.to_owned(),
            Hole::ThemeInput => self.theme.map_or_default(|theme| {
                format!("<input type=\"hidden\" name=\"theme\" value=\"{theme}\">")
            }),
            // The browser's own chrome takes the page's ground: the system's
            // scheme picks it, unless `?theme=` forces one (then a media
            // query on the system's would paint the wrong one).
            Hole::ThemeColor => match self.theme {
                Some("dark") => format!("<meta name=\"theme-color\" content=\"{GROUND_DARK}\">"),
                Some(_) => format!("<meta name=\"theme-color\" content=\"{GROUND_LIGHT}\">"),
                None => format!(
                    "<meta name=\"theme-color\" content=\"{GROUND_LIGHT}\" \
                     media=\"(prefers-color-scheme: light)\">\n\
                     <meta name=\"theme-color\" content=\"{GROUND_DARK}\" \
                     media=\"(prefers-color-scheme: dark)\">"
                ),
            },
        }
    }
}

// tokens:begin - generated from brand/tokens.json by brand/src/tokens.py, do not edit
/// The page's ground in each theme (`--ground`), for `theme-color`.
const GROUND_LIGHT: &str = "#F2F0EA";
const GROUND_DARK: &str = "#151A21";
// tokens:end

impl Chrome {
    /// Build the chrome from the request URI, the operator's page title and
    /// the versions of their logos (light, dark), if they set them.
    pub(crate) fn from_uri(
        uri: &axum::http::Uri,
        site: &str,
        logos: (Option<&str>, Option<&str>),
    ) -> Self {
        let (logo, logo_dark) = logos;
        Self {
            site: site.to_owned(),
            initial: site
                .chars()
                .find(|c| c.is_alphanumeric())
                .map_or_else(|| "H".to_owned(), |c| c.to_uppercase().collect()),
            logo: logo.map(|version| format!("/logo?v={version}")),
            logo_dark: logo_dark.map(|version| format!("/logo-dark?v={version}")),
            current: "",
            operator: false,
            v: ASSET_VERSION.as_str(),
            month: Utc::now().format("%Y-%m").to_string(),
            req: PerRequest::from_uri(uri),
            holes: false,
        }
    }

    /// The chrome of a page rendered once for every request of an audience:
    /// the per-request parts are left as holes for [`Spliced`] to fill. The
    /// request's own values are dropped here, so they cannot reach the cache.
    #[must_use]
    pub(crate) fn cached(mut self) -> Self {
        self.req = PerRequest::plain();
        self.holes = true;
        self
    }

    /// Mark the navigation entry this page is.
    #[must_use]
    pub(crate) fn at(mut self, current: &'static str) -> Self {
        self.current = current;
        self
    }

    /// Record whether the viewer is the operator.
    #[must_use]
    pub(crate) fn operator(mut self, operator: bool) -> Self {
        self.operator = operator;
        self
    }

    /// A per-request part, as HTML: the marker in a cached render, else this
    /// request's value. The templates print it unescaped (`|safe`), so both
    /// renders write the very same characters.
    fn hole(&self, hole: Hole) -> Cow<'_, str> {
        if self.holes {
            Cow::Borrowed(MARKERS.1[hole.index()].as_str())
        } else {
            Cow::Owned(self.req.fill(hole))
        }
    }

    pub(crate) fn q(&self) -> Cow<'_, str> {
        self.hole(Hole::Query)
    }

    /// [`Self::q`] for a link that has a query of its own.
    pub(crate) fn q_more(&self) -> Cow<'_, str> {
        self.hole(Hole::QueryMore)
    }

    pub(crate) fn theme_attr(&self) -> Cow<'_, str> {
        self.hole(Hole::ThemeAttr)
    }

    pub(crate) fn scheme(&self) -> Cow<'_, str> {
        self.hole(Hole::Scheme)
    }

    pub(crate) fn refresh_meta(&self) -> Cow<'_, str> {
        self.hole(Hole::Refresh)
    }

    pub(crate) fn live(&self) -> Cow<'_, str> {
        self.hole(Hole::Live)
    }

    pub(crate) fn theme_href(&self) -> Cow<'_, str> {
        self.hole(Hole::ThemeHref)
    }

    pub(crate) fn theme_label(&self) -> Cow<'_, str> {
        self.hole(Hole::ThemeLabel)
    }

    pub(crate) fn theme_input(&self) -> Cow<'_, str> {
        self.hole(Hole::ThemeInput)
    }

    pub(crate) fn theme_color(&self) -> Cow<'_, str> {
        self.hole(Hole::ThemeColor)
    }
}

/// A page rendered with holes ([`Chrome::cached`]), cut at them once: the
/// pieces between holes are slices of one shared buffer.
pub(crate) struct Spliced {
    pieces: Vec<Bytes>,
    /// The hole after each piece but the last.
    holes: Vec<Hole>,
    /// The page as a request without a query gets it, filled in advance.
    plain: Bytes,
}

impl Spliced {
    /// Cut a cached render at its holes.
    pub(crate) fn new(rendered: String) -> Self {
        let (prefix, _) = &*MARKERS;
        let marker_len = MARKERS.1[0].len();
        let whole = Bytes::from(rendered);
        let text = std::str::from_utf8(&whole).unwrap_or_default();
        let mut pieces = Vec::new();
        let mut holes = Vec::new();
        let mut from = 0;
        while let Some(found) = text[from..].find(prefix.as_str()) {
            let at = from + found;
            let index = text
                .get(at + prefix.len()..at + prefix.len() + 2)
                .and_then(|digits| digits.parse::<usize>().ok())
                .and_then(|index| Hole::ALL.get(index));
            let Some(&hole) = index else {
                // Not one of ours after all (it cannot be forged; be safe).
                from = at + prefix.len();
                continue;
            };
            pieces.push(whole.slice(from..at));
            holes.push(hole);
            from = at + marker_len;
        }
        pieces.push(whole.slice(from..));
        let mut spliced = Self {
            pieces,
            holes,
            plain: Bytes::new(),
        };
        spliced.plain = spliced.assemble(&PerRequest::plain());
        spliced
    }

    /// The page for this request: the shared bytes when it asks for nothing
    /// of its own, else the pieces with its values spliced in.
    pub(crate) fn serve(&self, req: &PerRequest) -> Bytes {
        if req.is_plain() {
            self.plain.clone()
        } else {
            self.assemble(req)
        }
    }

    fn assemble(&self, req: &PerRequest) -> Bytes {
        if self.holes.is_empty() {
            return self.pieces.first().cloned().unwrap_or_default();
        }
        let fills: Vec<String> = Hole::ALL.iter().map(|&hole| req.fill(hole)).collect();
        let size = self.pieces.iter().map(Bytes::len).sum::<usize>()
            + self
                .holes
                .iter()
                .map(|hole| fills[hole.index()].len())
                .sum::<usize>();
        let mut out = Vec::with_capacity(size);
        for (piece, hole) in self.pieces.iter().zip(&self.holes) {
            out.extend_from_slice(piece);
            out.extend_from_slice(fills[hole.index()].as_bytes());
        }
        if let Some(last) = self.pieces.last() {
            out.extend_from_slice(last);
        }
        Bytes::from(out)
    }
}

impl FromRequestParts<AppState> for Chrome {
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> impl Future<Output = Result<Self, Self::Rejection>> + Send {
        let chrome = {
            let config = state.config.borrow();
            let page = &config.page;
            let logos = (
                page.logo_image.as_ref().map(|logo| logo.version.as_str()),
                page.logo_dark_image
                    .as_ref()
                    .map(|logo| logo.version.as_str()),
            );
            Self::from_uri(&parts.uri, &page.title, logos)
        };
        std::future::ready(Ok(chrome))
    }
}

/// Small numbers as words, as a sentence says them ("two are slow").
pub(crate) fn count_word(n: usize) -> String {
    const WORDS: [&str; 11] = [
        "no", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten",
    ];
    WORDS
        .get(n)
        .map_or_else(|| n.to_string(), |word| (*word).to_owned())
}

/// `Paris`, `Paris and Frankfurt`, `Paris, Frankfurt and Montréal`.
pub(crate) fn join_and<S: AsRef<str>>(items: &[S]) -> String {
    match items {
        [] => String::new(),
        [one] => one.as_ref().to_owned(),
        [init @ .., last] => format!(
            "{} and {}",
            init.iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(", "),
            last.as_ref()
        ),
    }
}

pub(crate) use hora_core::fmt::latency as ms;

/// Whole minutes as every page says a span: `41 min`, `3 h 7 min`,
/// `2 d 4 h` - [`hora_core::fmt::duration`], for the figures the pages hold
/// in minutes (a budget, a downtime rounded up to the minute).
pub(crate) fn minutes(total: i64) -> String {
    // A figure kept in minutes says its nothing in minutes too: "0 s" in a
    // column of "41 min" reads as another measurement.
    if total <= 0 {
        return "0 min".to_owned();
    }
    hora_core::fmt::duration(total.saturating_mul(60))
}

pub(crate) use hora_core::fmt::permille as pct;

#[cfg(test)]
mod tests {
    use super::*;

    fn chrome(uri: &str) -> Chrome {
        Chrome::from_uri(&uri.parse().unwrap(), "pelican", (None, None))
    }

    #[test]
    fn carries_token_and_theme_only() {
        let page = chrome("/status/App?token=a%20b&theme=dark&group=X");
        assert_eq!(page.req.q, "?token=a%20b&theme=dark");
        assert_eq!(page.req.theme, Some("dark"));
        assert_eq!(page.initial, "P");
        // The switch keeps the other parameters and cycles dark back to auto.
        assert_eq!(page.req.theme_href, "?token=a%20b&group=X");
        assert_eq!(page.req.theme_label, "Dark");
        assert_eq!(page.q(), "?token=a%20b&amp;theme=dark");
    }

    #[test]
    fn plain_page_has_no_query_and_cycles_to_light() {
        let page = chrome("/history");
        assert_eq!(page.req, PerRequest::plain());
        assert_eq!(page.req.theme_href, "?theme=light");
        // An unknown theme is ignored, not echoed.
        assert_eq!(chrome("/?theme=%22%3E").req.theme, None);
        assert_eq!(chrome("/?theme=%22%3E").req.theme_href, "?theme=light");
    }

    #[test]
    fn refresh_is_opt_in_and_bounded() {
        assert_eq!(chrome("/").req.refresh, None);
        assert_eq!(chrome("/?refresh=60").req.refresh, Some(60));
        assert_eq!(chrome("/?refresh=10").req.refresh, Some(10));
        assert_eq!(chrome("/?refresh=3600").req.refresh, Some(3600));
        for ignored in ["9", "3601", "0", "-5", "abc", "", "99999999"] {
            assert_eq!(chrome(&format!("/?refresh={ignored}")).req.refresh, None);
        }
        let page = chrome("/?refresh=300");
        assert_eq!(
            page.refresh_meta(),
            r#"<meta http-equiv="refresh" content="300">"#
        );
        assert!(page.live().contains("Updates every 5 min"));
        assert_eq!(chrome("/").refresh_meta(), "");
        assert_eq!(chrome("/").live(), "");
        // The refresh is the page's own: links to other pages do not carry it,
        // the theme switch (this page again) does.
        assert_eq!(page.req.q, "");
        assert_eq!(page.req.theme_href, "?refresh=300&theme=light");
    }

    /// A page rendered with holes, then spliced, is byte for byte the page
    /// rendered for that request directly.
    #[test]
    fn spliced_page_matches_a_direct_render() {
        let page = |chrome: &Chrome| {
            format!(
                "<html{}><meta content=\"{}\">{}<nav>{}<a href=\"/h{}\">h</a>\
                 <a href=\"{}\">{}</a></nav><form>{}</form><a href=\"/m/x{}\">x</a>\
                 <a href=\"/r?group=g{}\">r</a>{}</html>",
                chrome.theme_attr(),
                chrome.scheme(),
                chrome.refresh_meta(),
                chrome.live(),
                chrome.q(),
                chrome.theme_href(),
                chrome.theme_label(),
                chrome.theme_input(),
                chrome.q(),
                chrome.q_more(),
                chrome.theme_color(),
            )
        };
        let cached = Spliced::new(page(&chrome("/").cached()));
        for uri in [
            "/",
            "/?token=s3cret&theme=dark",
            "/status/A?theme=light&refresh=30&x=%3Cy%3E",
            "/?token=a%26b",
        ] {
            let direct = chrome(uri);
            assert_eq!(
                cached.serve(&direct.req),
                Bytes::from(page(&direct)),
                "{uri}"
            );
        }
        // The plain request is served the pre-filled bytes, not a copy.
        let plain = chrome("/").req;
        assert_eq!(cached.serve(&plain).as_ptr(), cached.plain.as_ptr());
    }

    /// A forced theme names its one ground; the system's scheme picks
    /// between two otherwise.
    #[test]
    fn theme_color_follows_a_forced_theme() {
        let auto = chrome("/").theme_color().into_owned();
        assert_eq!(auto.matches("<meta").count(), 2);
        assert!(auto.contains("prefers-color-scheme: dark"));
        assert_eq!(
            chrome("/?theme=dark").theme_color(),
            "<meta name=\"theme-color\" content=\"#151A21\">"
        );
        assert_eq!(
            chrome("/?theme=light").theme_color(),
            "<meta name=\"theme-color\" content=\"#F2F0EA\">"
        );
    }

    /// The cached render holds no request value: a token given to the
    /// render is dropped, not baked in.
    #[test]
    fn a_cached_render_holds_no_token() {
        let chrome = chrome("/?token=s3cret&theme=dark").cached();
        let rendered = format!(
            "{}{}{}",
            chrome.q(),
            chrome.theme_href(),
            chrome.theme_attr()
        );
        assert!(!rendered.contains("s3cret"));
        assert!(!rendered.contains("dark"));
        let spliced = Spliced::new(rendered);
        assert!(
            !spliced
                .serve(&PerRequest::plain())
                .windows(6)
                .any(|w| w == b"s3cret")
        );
    }

    /// Text that merely looks like a marker (the private-use character in a
    /// name) is left alone: only the nonce makes a marker.
    #[test]
    fn marker_lookalikes_are_kept() {
        let text = "a\u{E000}0000000000000000 01\u{E001}b".to_owned();
        let spliced = Spliced::new(text.clone());
        assert_eq!(spliced.serve(&PerRequest::plain()), Bytes::from(text));
    }

    #[test]
    fn words_and_units() {
        assert_eq!(
            join_and(&["Paris", "Frankfurt", "Montréal"]),
            "Paris, Frankfurt and Montréal"
        );
        assert_eq!(ms(64), "64 ms");
        assert_eq!(ms(1849), "1.8 s");
        assert_eq!(minutes(0), "0 min");
        assert_eq!(minutes(41), "41 min");
        assert_eq!(minutes(187), "3 h 7 min");
        assert_eq!(minutes(2 * 1440 + 4 * 60), "2 d 4 h");
        assert_eq!(pct(999), "99.9 %");
        assert_eq!(count_word(2), "two");
        assert_eq!(count_word(12), "12");
    }
}
