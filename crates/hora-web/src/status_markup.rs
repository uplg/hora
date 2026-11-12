//! The status page's markup (whole, or one group's): what `status_page`
//! worked out, written down.

use maud::{Markup, PreEscaped, html};

use crate::page::{Document, glyph, icon, raw, state};
use crate::status_page::{Callout, GroupBlock, LocalNotice, Row, StatusPage};
use crate::summary::StatusTemplate;

/// The dial behind the private page's mark: decoration, fixed.
const TRAIL: &str = concat!(
    r#"<svg class="trail" viewBox="0 0 400 400" preserveAspectRatio="xMidYMid slice" aria-hidden="true" focusable="false">"#,
    r#"<circle cx="200" cy="200" r="150" fill="none" stroke="currentColor" stroke-width="2" stroke-dasharray="2 10" stroke-linecap="round"/>"#,
    r#"<path d="M200 38v18M200 344v18M38 200h18M344 200h18M281 60l-9 15.6M128 324.4l-9 15.6M340 119l-15.6 9M75.6 272l-15.6 9M340 281l-15.6-9M75.6 128 60 119M281 340l-9-15.6M128 75.6 119 60" stroke="currentColor" stroke-width="3" stroke-linecap="round"/></svg>"#,
);

/// How a day reads in a bar: its class, where its mark starts, how tall it
/// is, and the words.
const LEGEND: [(&str, Option<&str>, &str, &str); 5] = [
    ("u", None, "22", "Up all day"),
    ("d", Some("7"), "15", "Slow at times (shorter)"),
    ("x", None, "29", "Outage (drops below the line)"),
    ("m", Some("11"), "11", "Maintenance (half)"),
    ("n", Some("18"), "4", "No data"),
];

fn services(count: usize) -> &'static str {
    if count == 1 { "service" } else { "services" }
}

/// A note above or below the headline: a shape, a title, a body, a date. An
/// announcement may have no body; a maintenance note always has its line.
fn callout(note: &Callout, class: &str, shape: &str, id: Option<&str>, body: bool) -> Markup {
    html! {
        div class={ "callout " (class) } role="note" id=[id] {
            (glyph(shape))
            p.k { (note.title) }
            @if body || !note.body.is_empty() { p { (note.body) } }
            @if let Some(when) = &note.when { p.when { (when) } }
        }
    }
}

impl StatusTemplate<'_> {
    /// Nothing configured yet: how to start.
    fn nothing(&self) -> Markup {
        html! {
            section.screen aria-labelledby="h" {
                div.first {
                    (self.page.owl)
                    h1 id="h" { "Nothing to watch yet." }
                    p.lead {
                        "Add a monitor to " code { "config.toml" }
                        ". Hora reads it again by itself; no restart."
                    }
                    pre.code {
                        span.k { "[[monitors]]" }
                        "\nid = \"website\"\nname = \"Website\"\nkind = \"http\"\ntarget = \"https://example.com\""
                    }
                    div.actions {
                        a.btn.primary href="https://uplg.github.io/hora/" { "Read the 5-minute guide" }
                    }
                }
            }
        }
    }

    /// Everything here is private: the door, and its keyhole.
    fn door(&self) -> Markup {
        let (chrome, page) = (&self.chrome, &self.page);
        let bad = page.bad_token;
        html! {
            section.screen aria-labelledby="h" {
                div.door {
                    div.side {
                        (PreEscaped(TRAIL))
                        div.id {
                            img src={ "/assets/mark.svg?v=" (chrome.v) } alt="" width="88" height="88";
                            p.name {
                                svg.wm viewBox="0 0 89 26" role="img" aria-label="Hora" { use href="#w-hora" {} }
                            }
                            p.promise-line { "Hora keeps the hours, so you can sleep." }
                        }
                    }
                    div.content { form method="get" action="" {
                        p.eyebrow { (icon("lock")) " " (page.summary.title) " · status" }
                        h1 id="h" { "This page is private." }
                        p.lead { "Ask the person who runs it for a link, or paste your viewer token." }
                        div class=(if bad { "field bad" } else { "field" }) {
                            label for="tok" { "Viewer token" }
                            input id="tok" name="token" type="password" autocomplete="off" required
                                aria-invalid=[bad.then_some("true")]
                                aria-describedby=[bad.then_some("tok-err")];
                            @if bad {
                                span.err id="tok-err" {
                                    (glyph("down"))
                                    "That token doesn't open this page. Check it has no space at the end."
                                }
                            }
                        }
                        (raw(chrome.theme_input()))
                        button.btn.primary.big type="submit" { "Open the page" }
                        p.aside {
                            "Running Hora? The viewer token is " code { "server.view_token" }
                            " (or the operator's " code { "server.auth_token" } ") in "
                            code { "config.toml" } "; a group's page also opens with its "
                            code { "[server.group_tokens]" } " entry."
                        }
                    } }
                }
            }
        }
    }

    /// The headline: the owl, the sentence, the window under way, the chips.
    fn hero(&self) -> Markup {
        let page = &self.page;
        html! {
            section class={ "hero " (page.state) } aria-labelledby="h" {
                (page.owl)
                div {
                    p.when { (page.when) }
                    h1 id="h" { (page.headline) }
                    p.sub { b { (page.sub_lead) } (page.sub_rest) }
                    @if let Some(window) = &page.window {
                        div.window {
                            div.ends {
                                span { (window.start) } span { (window.progress) } span { (window.end) }
                            }
                            div role="progressbar" aria-label="Maintenance window" aria-valuemin="0"
                                aria-valuemax=(window.length) aria-valuenow=(window.done)
                                aria-valuetext=(window.progress) { (window.meter) }
                        }
                    }
                }
                div.chips {
                    @for chip in &page.chips {
                        @let class = format!("chip {}", chip.state);
                        @let inside = html! {
                            @if !chip.state.is_empty() { (glyph(chip.state)) }
                            @if !chip.icon.is_empty() { (icon(chip.icon)) }
                            (chip.text)
                        };
                        @match &chip.href {
                            Some(href) => a class=(class) href=(href) { (inside) },
                            None => span class=(class) { (inside) },
                        }
                    }
                    " "
                    a.btn.quiet.follow href="/history.atom" { (icon("feed")) "Follow updates" }
                }
            }
        }
    }

    /// A down seen from this node only: said to everyone, explained to the
    /// operator.
    fn local(&self, local: &LocalNotice<'_>) -> Markup {
        html! {
            div.callout.info role="note" id="local" {
                (glyph("info"))
                p.k { "Not an outage" }
                p { (local.sentence) }
            }
            @if self.page.operator {
                details.why open {
                    summary {
                        svg.chev aria-hidden="true" { use href="#i-chev" {} }
                        h2 { "Why this is not an outage" }
                        span.tag { "Operators only · hidden from the public page" }
                    }
                    div.why-in {
                        @for detail in &local.monitors {
                            div {
                                (detail.owls)
                                table.vt {
                                    caption { (detail.monitor.name) }
                                    thead { tr {
                                        th scope="col" { "Checked from" }
                                        th scope="col" { "Sees it" }
                                        th scope="col" { "Answer" }
                                    } }
                                    tbody {
                                        @for place in &detail.places {
                                            tr {
                                                td { (place.name) " " span.muted { "· " (place.role) } }
                                                td { (state(place.state, place.word)) }
                                                td { (place.answer) }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        p.muted.small {
                            (local.routing) " "
                            a href={ "/watchers" (raw(self.chrome.q())) } { "See all watchers" }
                        }
                    }
                }
            }
        }
    }

    /// What is down or slow, above everything else.
    fn attention(&self) -> Markup {
        let (page, q) = (&self.page, raw(self.chrome.q()));
        html! {
            section.section.near aria-labelledby="attention" {
                div.section-head {
                    h2 id="attention" { "Needs attention" }
                    span.fact { (page.attention.len() + page.attention_more) " of " (page.total) }
                }
                ul.list.wide {
                    @for row in &page.attention {
                        li { div class={ "row " (row.state) } {
                            (glyph(row.state))
                            a.name href={ "/monitor/" (row.href_id) (q) } {
                                (row.m.name)
                                @if let Some(group) = row.group { small { (group) } }
                            }
                            span.state {
                                span class={ "word " (row.state) } { (row.word) }
                                @if !row.detail.is_empty() { " " span.muted { "· " (row.detail) } }
                            }
                            @if let Some(why) = &row.why { span.why { (why) } }
                            @if !row.seen.is_empty() {
                                span.vantages {
                                    span.muted.small { "Seen from" }
                                    @for place in &row.seen {
                                        span class={ "vchip " (place.state) } {
                                            (glyph(place.state)) (place.name) " " small { (place.word) }
                                        }
                                    }
                                    span.muted.small { (row.seen_note) }
                                }
                            }
                        } }
                    }
                }
                @if page.attention_more > 0 {
                    p.more { "And " (page.attention_more) " more, listed in their groups below." }
                }
            }
        }
    }

    /// At scale: a group a disclosure, folded when all is well.
    fn folded(&self, group: &GroupBlock<'_>) -> Markup {
        let (page, q) = (&self.page, raw(self.chrome.q()));
        html! {
            li { details open[group.open] {
                summary {
                    svg.chev aria-hidden="true" { use href="#i-chev" {} }
                    (glyph(group.state))
                    h3.gname {
                        (group.name)
                        small { (group.total) " " (services(group.total)) }
                        span.bar.mini { (group.bar) }
                        span.bar-cap aria-hidden="true" {
                            span { (page.days) " days ago" } span { "Today" }
                        }
                    }
                    span.gsum {
                        (state(group.state, &group.fact_lead))
                        @if group.state != "up" && !group.fact_rest.is_empty() {
                            " " span.muted { "· " (group.fact_rest) }
                        }
                        @if !group.uptime.is_empty() { span.pctx { (group.uptime) } }
                    }
                }
                div.inner {
                    ul.compact aria-label={ (group.name) ": " (group.total) " services" } {
                        @for row in &group.rows {
                            li {
                                (glyph(row.state))
                                a href={ "/monitor/" (row.href_id) (q) } { (row.m.name) }
                                span.pct {
                                    @if row.state == "up" { (row.detail) }
                                    @else { span class={ "word " (row.state) } { (row.word) } }
                                }
                            }
                        }
                    }
                    @if group.rows.len() < group.total {
                        div.more {
                            span {
                                "Showing " (group.rows.len()) " of " (group.total)
                                ", problems first, then A to Z."
                            }
                            @if !group.href.is_empty() {
                                a.btn href={ (group.href) (q) } { "Show all " (group.total) }
                            }
                        }
                    }
                }
            } }
        }
    }

    /// A service's full row: its state, its last day of answer times, its
    /// daily bar.
    fn full_row(&self, row: &Row<'_>) -> Markup {
        let href = html! { "/monitor/" (row.href_id) (raw(self.chrome.q())) };
        html! {
            li { div class={ "row " (row.state) } {
                (glyph(row.state))
                a.name href=(href) { (row.m.name) span.vh { ". " (row.bar_label) } }
                span.state {
                    (row.spark)
                    span class={ "word " (row.state) } { (row.word) }
                    @if !row.detail.is_empty() { " " span.muted { "· " (row.detail) } }
                }
                @if let Some(why) = &row.why { span.why { (why) } }
                " "
                a.bar href=(href) tabindex="-1" aria-hidden="true" { (row.bar) }
            } }
        }
    }

    /// Every service, by group.
    fn all_services(&self) -> Markup {
        let (page, q) = (&self.page, raw(self.chrome.q()));
        html! {
            section.section aria-labelledby="all" {
                div.svc-head {
                    h2 id="all" {
                        @if page.scale && !page.group_page { "All services" } @else { "Services" }
                    }
                    span.fact {
                        (page.total) " " (services(page.total))
                        @if page.groups.len() > 1 { " in " (page.groups.len()) " groups" }
                        @if page.scale && !page.group_page { " · groups where all is well stay folded" }
                    }
                }
                @if page.scale {
                    ul.gl { @for group in &page.groups { (self.folded(group)) } }
                } @else {
                    div.groups {
                        @for (index, group) in page.groups.iter().enumerate() {
                            @let id = format!("g{}", index + 1);
                            section aria-labelledby=(id) {
                                div.group-head {
                                    h3 id=(id) {
                                        @if !group.href.is_empty() && !page.group_page {
                                            a.ink href={ (group.href) (q) } { (group.name) }
                                        } @else {
                                            (group.name)
                                        }
                                    }
                                    span.fact { (group.fact) }
                                }
                                ul.list { @for row in &group.rows { (self.full_row(row)) } }
                                div.bar-legend aria-hidden="true" {
                                    span.dall { (page.days) " days ago" }
                                    span.d30 { "30 days ago" }
                                    span { "Today" }
                                }
                            }
                        }
                    }
                    details.under {
                        summary.btn.quiet { "How to read the bars" }
                        div.legend role="group" aria-label="How to read the bars" {
                            @for (index, (class, y, height, words)) in LEGEND.iter().enumerate() {
                                @if index > 0 { " " }
                                span {
                                    svg.cell viewBox="0 0 4 29" aria-hidden="true" {
                                        rect class=(class) y=[*y] width="4" height=(height) {}
                                    }
                                    (words)
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The latest incidents, and who keeps watch.
    fn below(&self) -> Markup {
        let (page, q) = (&self.page, raw(self.chrome.q()));
        html! {
            @if !page.recent.is_empty() {
                section.section aria-labelledby="past" {
                    div.section-head {
                        h2 id="past" { "Recent incidents" }
                        a.fact href={ "/history" (q) } { "All history" }
                    }
                    ul.past {
                        @for incident in page.recent {
                            li {
                                time datetime=(incident.date) { (incident.day) }
                                span.t {
                                    a href={ "/incident/" (incident.id) (q) } { (incident.title) }
                                    @if let Some(note) = &incident.note { small { (note) } }
                                }
                                span.r {
                                    @if incident.resolved { (state("up", "Resolved")) }
                                    @else { (state("down", "Ongoing")) }
                                }
                            }
                        }
                    }
                }
            }
            @if page.places > 1 || !page.peers.is_empty() {
                section.section aria-labelledby="watch" {
                    div.section-head {
                        h2 id="watch" { "Who keeps watch" }
                        @if page.operator { a.fact href={ "/watchers" (q) } { "All watchers" } }
                    }
                    @if page.places > 1 {
                        div.watchers { p {
                            "Watched from " b { (page.places) " places" }
                            ". Each place checks on its own, and asks the others before calling an outage, so a hiccup near one of them is not called an outage here."
                        } }
                    }
                    @if !page.peers.is_empty() {
                        ul.vlist {
                            @for peer in &page.peers {
                                li {
                                    (glyph(peer.state))
                                    span.n { (peer.name) small { (peer.note) } }
                                    span.r { (state(peer.state, peer.word)) }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// The page with something to show.
    fn status(&self) -> Markup {
        let page: &StatusPage<'_> = &self.page;
        html! {
            @if let Some(update) = &page.update {
                div.callout.info role="note" id="update" {
                    (glyph("info"))
                    p.k { "Hora " (update.version) " is out" }
                    @if !update.summary.is_empty() { p { (update.summary) } }
                    p.when {
                        "You run " (update.running) " · "
                        a href=(update.url) rel="noopener" { "Release notes" }
                        " · "
                        a href="https://github.com/uplg/hora/blob/main/UPGRADES.md" rel="noopener" { "Before upgrading" }
                        " · Operators only"
                    }
                }
            }
            @for note in &page.announcements { (callout(note, note.class, note.glyph, None, false)) }
            (self.hero())
            @for note in &page.maintenances { (callout(note, "maint", "maint", None, true)) }
            @if let Some(note) = &page.next { (callout(note, "info", "info", Some("next"), true)) }
            @if let Some(local) = &page.local { (self.local(local)) }
            @if !page.attention.is_empty() { (self.attention()) }
            (self.all_services())
            (self.below())
        }
    }

    pub(crate) fn render(&self) -> String {
        let (chrome, page) = (&self.chrome, &self.page);
        let site = &page.summary.title;
        let (title, description) = if page.empty.is_empty() {
            (
                format!("{} · {site}", page.headline),
                format!("{site}: {}", page.headline),
            )
        } else {
            (site.clone(), site.clone())
        };
        let main = html! {
            main id="main" class="page status-page" {
                @match page.empty {
                    "nothing" => (self.nothing()),
                    "private" => (self.door()),
                    _ => (self.status()),
                }
            }
        };
        Document::new(chrome, title, description, main)
            .head(html! {
                link rel="alternate" type="application/atom+xml" title="Incident history" href="/history.atom";
            })
            .render()
    }
}
