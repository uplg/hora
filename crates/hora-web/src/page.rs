//! The document every page is set in, and the small pieces they share,
//! written with maud: markup as Rust, escaped by default, a component a
//! function.
//!
//! What a request alone decides (its theme, its token in the links, a kiosk's
//! refresh) reaches the markup through [`Chrome`]'s methods as ready-made
//! HTML - or as a marker to be filled per request when the page is cached
//! (see `layout::Spliced`) - and is written as it is with [`raw`].

use maud::{DOCTYPE, Markup, PreEscaped, html};

use crate::layout::Chrome;

/// The shapes of the states and the icons, drawn once per page and
/// referenced with `<use>`.
const SPRITE: &str = include_str!("../assets/sprite.html");

/// HTML that is already HTML: one of [`Chrome`]'s per-request fragments, or
/// a drawing built by `render`. Never a string that came from outside.
pub(crate) fn raw(html: impl AsRef<str>) -> PreEscaped<String> {
    PreEscaped(html.as_ref().to_owned())
}

/// A state's shape: `up`, `degraded`, `down`, `maint`, `none`.
pub(crate) fn glyph(state: &str) -> Markup {
    html! {
        svg class={ "g " (state) } aria-hidden="true" { use href={ "#g-" (state) } {} }
    }
}

/// A state's shape and its word.
pub(crate) fn state(state: &str, text: &str) -> Markup {
    html! {
        (glyph(state))
        span class={ "word " (state) } { (text) }
    }
}

/// An icon of the sprite, at text size.
pub(crate) fn icon(name: &str) -> Markup {
    html! {
        svg aria-hidden="true" width="16" height="16" { use href={ "#i-" (name) } {} }
    }
}

/// A page: its head, and what goes in its body.
pub(crate) struct Document<'a> {
    chrome: &'a Chrome,
    title: String,
    description: String,
    head: Markup,
    body_class: Option<&'static str>,
    header: Option<Markup>,
    footer: Option<Markup>,
    main: Markup,
}

impl<'a> Document<'a> {
    /// A page with the usual header and footer around `main`.
    pub(crate) fn new(
        chrome: &'a Chrome,
        title: String,
        description: String,
        main: Markup,
    ) -> Self {
        Self {
            chrome,
            title,
            description,
            head: html! {},
            body_class: None,
            header: None,
            footer: None,
            main,
        }
    }

    /// More for the `<head>` (a feed link).
    pub(crate) fn head(mut self, head: Markup) -> Self {
        self.head = head;
        self
    }

    /// A class on `<body>`, with a header and a footer of the page's own
    /// (the report, which is also a printed document).
    pub(crate) fn framed(
        mut self,
        body_class: &'static str,
        header: Markup,
        footer: Markup,
    ) -> Self {
        self.body_class = Some(body_class);
        self.header = Some(header);
        self.footer = Some(footer);
        self
    }

    pub(crate) fn render(self) -> String {
        let chrome = self.chrome;
        let versioned = |path: &str| format!("{path}?v={}", chrome.v);
        html! {
            (DOCTYPE)
            // The theme attribute is a per-request fragment *inside* the
            // tag, which the macro has no place for.
            (raw(format!("<html lang=\"en\"{}>", chrome.theme_attr())))
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                (raw(chrome.refresh_meta()))
                title { (self.title) }
                meta name="description" content=(self.description);
                meta name="color-scheme" content=(raw(chrome.scheme()));
                (raw(chrome.theme_color()))
                link rel="icon" href="/favicon.svg" type="image/svg+xml";
                link rel="apple-touch-icon" href=(versioned("/assets/apple-touch-icon.png"));
                link rel="manifest" href="/manifest.webmanifest";
                meta name="apple-mobile-web-app-title" content=(chrome.site);
                link rel="preload" href=(versioned("/assets/fonts/CalSansVF.woff2")) as="font" type="font/woff2" crossorigin;
                link rel="preload" href=(versioned("/assets/fonts/Fraunces-VF.woff2")) as="font" type="font/woff2" crossorigin;
                link rel="stylesheet" href=(versioned("/assets/hora.css"));
                (self.head)
            }
            body class=[self.body_class] {
                a.skip href="#main" { "Skip to content" }
                (PreEscaped(SPRITE))
                @match self.header {
                    Some(header) => (header),
                    None => (site_header(chrome)),
                }
                (self.main)
                @match self.footer {
                    Some(footer) => (footer),
                    None => (site_footer(chrome)),
                }
            }
            (PreEscaped("</html>"))
        }
        .into_string()
    }
}

/// The header of every page but the report: the operator's sign, the
/// navigation, and what this viewer may know about itself.
fn site_header(chrome: &Chrome) -> Markup {
    let q = raw(chrome.q());
    let link = |href: String, name: &str, label: &str| {
        html! {
            a.navlink href={ (href) (q) } aria-current=[(chrome.current == name).then_some("page")] { (label) }
        }
    };
    html! {
        header.top { div.page {
            a.sign href={ "/" (q) } {
                @match (&chrome.logo, &chrome.logo_dark) {
                    (Some(logo), Some(dark)) => {
                        img.op-logo.for-light src=(logo) alt="";
                        img.op-logo.for-dark src=(dark) alt="";
                    }
                    (Some(logo), None) => { img.op-logo src=(logo) alt=""; }
                    (None, _) => { span.op-logo aria-hidden="true" { (chrome.initial) } }
                }
                b { (chrome.site) }
                span { "Status" }
            }
            nav aria-label="Pages" {
                (link("/".to_owned(), "status", "Status")) " "
                (link("/history".to_owned(), "history", "History")) " "
                (link("/timeline".to_owned(), "timeline", "Timeline")) " "
                (link(format!("/report/{}", chrome.month), "report", "Report"))
                @if chrome.operator {
                    " " (link("/watchers".to_owned(), "watchers", "Watchers"))
                }
            }
            (raw(chrome.live()))
            @if chrome.operator {
                " " span.signed {
                    svg aria-hidden="true" width="14" height="14" { use href="#i-lock" {} }
                    "Operator"
                }
            }
            " "
            a.btn.theme-btn href=(raw(chrome.theme_href()))
                aria-label={ "Theme: " (raw(chrome.theme_label())) ". Change theme" } {
                svg aria-hidden="true" width="16" height="16" { use href="#i-sun" {} }
                span.lbl { (raw(chrome.theme_label())) }
            }
        } }
    }
}

fn site_footer(chrome: &Chrome) -> Markup {
    let q = raw(chrome.q());
    html! {
        footer.foot { div.page {
            nav aria-label="More" {
                a href={ "/history" (q) } { "Incident history" } " "
                a href={ "/timeline" (q) } { "Timeline" } " "
                a href={ "/report/" (chrome.month) (q) } { "Monthly report" } " "
                a href="/history.atom" { "Atom feed" } " "
                a href="/api/summary" { "JSON" }
            }
            a.watched href="https://github.com/uplg/hora" rel="noopener" {
                img src={ "/assets/mark.svg?v=" (chrome.v) } alt="" width="22" height="22";
                span { "Watched over by" }
                svg.wm viewBox="0 0 89 26" role="img" aria-label="Hora" { use href="#w-hora" {} }
            }
        } }
    }
}
