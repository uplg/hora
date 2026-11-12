# Hora, brand book

Proposal, 3 October 2026. Hora is the third sibling of Ariane and Maison: same synthe.se tokens, same type, same mark recipe. Open `index.html` first; this file is the written reference behind it.

## 1. Concept: the night watch

**Hora** means "hours". The Horai, Greek goddesses of the hours, also kept the gates of Olympus. In old towns the **night watchman** walked the streets and called the hours: *"Ten o'clock, and all is well."* A status page does the same job: say the time, say the state, in one sentence.

Hora's watcher is a **little owl** (*Athene noctua*, Athena's owl, Greek like the Horai), the animal of the night watch.

- **It dozes while all is well.** The promise is *"flapping never wakes you up"*. Retries, several failures in a row, peers that must agree and root-cause grouping are what let the owl keep its eyes shut. Calm is the normal state.
- **It opens one eye** when something is slow, and **wakes** only for a real outage.
- **It is never alone.** Hora nodes watch from several places and from each other (the mesh). A group of owls is a *parliament*: this one votes before it wakes you. One owl awake and two asleep is a local problem; three awake is an outage.

One line: **Hora keeps the hours, so you can sleep.** French: **Hora veille, vous dormez.**

## 2. The owl

### Drawing
- Built on the family grid: 48-unit box, cream body, petrol details, the family's eye arcs (stroke 1.7 in the mark, 1.8 in the UI face), blush `#E6A49A` at 0.85, **no mouth**. A small petrol beak is the only feature between the eyes.
- Two leaf-shaped tufts (Maison's ears, as an owl's), a round body (Ariane's ball of yarn), two little feet.
- Faint texture at 0.28 opacity in the mark (0.2 in the outlined UI face): the facial disc (two rings) and the folded wings.
- In the UI (`HoraFace`, see `src/common.py`, `OWL_PARTS`) it is outlined like `ArianeFace`: surface fill, ink line, so it follows the theme.

### States (the pose is the page's overall state)

| State | Pose | Badge (the state's shape) | Motion |
|---|---|---|---|
| All up | both eyes shut (asleep), tufts at rest | none | none |
| Degraded | one eye shut, one open; one tuft up | half disc | none |
| Down | both eyes open, tufts up | diamond with ! | one perk of the tufts on waking (0.6 s), then still |
| Maintenance | eyes shut, tufts folded flat (a planned nap) | clock | none |
| No data | two dashes, no blush | dashed ring | none |

Files: `assets/owl-up.svg`, `owl-degraded.svg`, `owl-down.svg`, `owl-maint.svg`, `owl-none.svg` (each switches to dark colours with `prefers-color-scheme`).

### Rules
1. **Never sad, never scared.** No mouth, no tears, no frown, no sweat drop. "Down" is awake and attentive. It shows what Hora is doing, never how it feels about you (Ariane's rule).
2. **Moves only in answer to something.** No idle loop, no breathing. It moves once when the state changes, then stays still (the Clippy lesson).
3. **Never the only carrier of the state.** Always beside the sentence that says the state, and its badge repeats the state's shape. For assistive tech the owl is `aria-hidden`, or labelled with the same sentence.
4. **Reduced motion:** poses stay, transitions and the perk are dropped (`prefers-reduced-motion: reduce`).
5. **One per page**, at the hero, the door or an empty state. Never in rows, tables or charts. The mesh view (`watchers.html`) and the concept pictures are the only places several owls perch together, one per place.
6. **Can be turned off** by operators who want a neutral page (proposal: `[page] mascot = false`). Nothing is lost: sentence and shapes carry everything.

## 3. Mark, wordmark, favicon

- `assets/mark.svg`: petrol tile `#0E5E68`, 48 × 48, `rx 11`, the cream owl asleep. Ariane's and Maison's exact recipe.
- `assets/favicon.svg`: redrawn on a 16 grid. Tile, silhouette, tufts, two shut eyes; texture, beak and blush dropped. Checked at 16 and 32 px.
- `assets/mark-maskable.svg`: full-bleed petrol, owl scaled to 80 % (PWA, Android).
- `assets/wordmark.svg`: mark, then **Hora** in Borel Display, set as outlines (no font needed). Gap = mark width / 6. Ink, switches to paper in dark mode.
- `assets/wordmark-on-petrol.svg`: the sign on the brand's petrol, tile ringed in cloud at 35 % (Maison's door).
- `assets/og-card.svg`: 1200 × 630. The owl asleep inside twelve hour ticks, name in Borel, tagline in Fraunces. Export to PNG for social networks (most do not read SVG).
- On a status page the operator's name leads (their initial tile + name in Fraunces); Hora signs the footer: mark + "Watched over by **Hora**".

### Alternatives explored (not chosen)
- **B, the alarm clock that does not ring** (`assets/variant-b-clock.svg`): literal "hours" and "doesn't wake you". Weaker as a character; the bells read as cat ears.
- **C, the watchman's lantern** (`assets/variant-c-lantern.svg`): the night watch's tool, a face in the glass. Elegant, less cute, harder to pose.

## 4. Palette

The synthe.se set, unchanged, declared once with `light-dark()` as in Maison's `tokens.css`. One hue, petrol. **Up is petrol, not green**: Hora looks like no other status page, and the brand colour means "all is well".

| Token | Light | Dark | Role |
|---|---|---|---|
| `--ground` | `#F2F0EA` | `#151A21` | page |
| `--surface` | `#FAF9F5` | `#1A2029` | cards, rows |
| `--ground-raised` | `#E7E4DB` | `#1E2530` | chips, code, tracks |
| `--line` | `#D6D2C6` | `#2E3744` | hairlines (never a state) |
| `--ink` | `#1B222C` | `#EAE7E0` | text |
| `--ink-muted` | `#525A66` | `#A8AEB8` | secondary text |
| `--accent` | `#0E5E68` | `#63C0CB` | links, up as text |
| `--accent-soft` | `#4E8A92` | `#3E7A84` | up as graphic |
| `--accent-wash` | `#D8E4E2` | `#1D2A31` | up wash, callouts |
| `--st-degraded` | `#B4633F` | `#D89372` | slow, graphic |
| `--st-degraded-text` | `#8F4B22` | `#D89372` | slow, text |
| `--st-down` | `#A5372A` | `#E07A63` | down, text and graphic |
| `--st-maint` | `#525A66` | `#A8AEB8` | maintenance (= ink-muted) |
| `--st-none` | `#87837A` | `#687180` | no data (new for Hora) |
| `--wash-degraded` | `#F5E7DA` | `#30261E` | slow wash (new) |
| `--wash-down` | `#F3E2DC` | `#33211F` | down wash (= blocked-wash) |
| `--blush` | `#E6A49A` | `#B8776E` | the owl's cheeks only |

### Contrast (WCAG 2.x, computed with `src/contrast.py`)

Text pairs, on ground / surface / raised:

| Pair | Light | Dark |
|---|---|---|
| ink | 14.05 / 15.20 / 12.59 | 14.15 / 13.26 / 12.48 |
| ink-muted | 6.12 / 6.62 / 5.48 | 7.83 / 7.34 / 6.91 |
| accent (up text, links) | 6.54 / 7.07 / 5.86 | 8.27 / 7.74 / 7.29 |
| slow text | 5.76 / 6.23 / 5.17 | 6.94 / 6.50 / 6.12 |
| down | 5.80 / 6.27 / 5.19 | 5.94 / 5.56 / 5.24 |
| on-accent (button text) | 6.54 | 8.27 |
| cloud on petrol (door, OG) | 6.54 | 6.54 |

State graphics (WCAG 1.4.11 asks 3:1), on ground / surface / raised:

| State colour | Light | Dark |
|---|---|---|
| up `#4E8A92` / `#3E7A84` | 3.43 / 3.71 / 3.07 | 3.60 / 3.37 / 3.17 |
| slow `#B4633F` / `#D89372` | 3.84 / 4.16 / 3.44 | 6.94 / 6.50 / 6.12 |
| down `#A5372A` / `#E07A63` | 5.80 / 6.27 / 5.19 | 5.94 / 5.56 / 5.24 |
| maintenance | 6.12 / 6.62 / 5.48 | 7.83 / 7.34 / 6.91 |
| no data `#87837A` / `#687180` | 3.32 / 3.59 / **2.97** | 3.55 / 3.32 / 3.13 |

On their own washes (light / dark): ink 12.29 to 13.21 / 11.91 to 12.48; the state word 5.72 (up), 5.42 (slow), 5.26 (down), 5.48 (maintenance) / 6.96, 5.86, 5.18, 6.91. The up glyph on the up wash would be 3.00 / 3.03, too close: on that wash it takes the text-grade petrol (5.72 / 6.96). No-data on raised is 2.97 in light: no-data cells only sit on surface.

## 5. The state system: colour + shape + word

| State | Shape | Day in the 90-day bar | EN | FR |
|---|---|---|---|---|
| Up | filled disc | full height | Up | Opérationnel |
| Degraded | half disc | shorter (top cut) | Slow | Lent |
| Down | diamond with ! | full, and drops below the line | Down | En panne |
| Maintenance | clock | half height | Maintenance | Maintenance |
| No data | dashed ring | a stub | No data | Pas de données |
| Info (announcements, events) | disc with i | n/a | Note | Note |

Rules: the word is always present (in the row, the chip, the hero sentence); the shape survives greyscale and colour blindness; the colour helps. Announcements are callouts (info / warning / critical) with an icon and a word, a wash, never a coloured border alone.

**Vantage points.** A place is written as its name ("Paris", "Frankfurt", "Montréal") with a pin, and its own state glyph and word. Public visitors see "checked from 3 places" and, when places disagree, one calm sentence ("Our Paris checkpoint can't reach it; the others can. Not an outage."). Node ids, heartbeats, verdict strings and per-place tables stay in the operator view.

## 6. Typography, spacing, corners (the family's)

- **Fraunces Variable** 500: page title, hero sentence (clamp 1.75 to 2.6 rem), group names, big numbers.
- **Cal Sans 2 VF** 400 to 600: everything else. Body 16/24, secondary 14/20, label 14/20 500, meta 13/16, tiny 12/16; never under 12 px. `tabular-nums` everywhere.
- **Borel Display**: the name only, beside the mark, nudged down 0.16 em.
- **Mona Sans Mono**: config, CLI, captured answers.
- Spacing on 4: 2, 4, 8, 12, 16, 24, 32, 48. Radius: 2, 4, 6, 8, 10, 12, 16, pill. Controls 44 / 36 / 32 px (44 on touch). Focus: 3 px petrol ring, 2 px offset.
- Layout: Ariane's `.page` (max 2400 px, margins 16 / 24 / 32); on a big screen, more columns, never longer lines.

## 7. Voice and tone

Calm, plain, factual, a little warm. The time, the state, what you will notice. No exclamation marks, no blame, no jargon on public pages, no em-dashes (Léonard's rule).

| Moment | Not this | EN | FR |
|---|---|---|---|
| All up | All Systems Operational | Everything is running. | Tout fonctionne. |
| Hero eyebrow | Last updated | Saturday 3 October, 14:32 UTC | Samedi 3 octobre, 14 h 32 UTC |
| Outage | Major outage on payments-api | Card payments are down since 14:02. | Les paiements par carte sont en panne depuis 14 h 02. |
| Impact | Degraded performance | What you'll notice: invoice emails arrive about 4 minutes late. | Ce que vous verrez : les e-mails de facture arrivent avec 4 minutes de retard. |
| Local only | Vantage disagreement 1/3 | Our Paris checkpoint can't reach it; the others can. Not an outage. | Notre point de contrôle de Paris ne le joint plus ; les autres oui. Pas une panne. |
| Confirmed | Confirmed 3/3 vantages | Seen down from Paris, Frankfurt and Montréal. | Vu en panne depuis Paris, Francfort et Montréal. |
| Budget | Error budget 31m (72 %) | 31 min of allowed downtime left this month. | Encore 31 min d'interruption possible ce mois-ci. |
| Recovered | Resolved | Up again at 19:53. Down for 41 min. | De retour à 19 h 53, après 41 min de panne. |
| Empty | No monitors configured | Nothing to watch yet. | Rien à surveiller pour l'instant. |
| Private | 401 Unauthorized | This page is private. | Cette page est privée. |

Taglines: **Hora keeps the hours, so you can sleep.** · **Hora veille, vous dormez.** · **It only wakes you for real.** / **Elle ne vous réveille que pour de vrai.**

Operator words (p50, p95, p99, SLO, burn rate, vantage, quorum, heartbeat) live behind "For operators" or in the authenticated views.

## 8. Do and don't

Do
- Lead with one sentence that answers "is it working?", with the time.
- Put only what is wrong first; fold groups that are all well, with a count.
- Give every state a word and a shape; give every number a unit and a period.
- Say where it was checked from, and say it calmly when places disagree.
- Keep the owl small, still and beside its sentence.

Don't
- Use green. Up is petrol.
- Use colour alone, a coloured left border alone, or a tooltip alone (touch and keyboard never reach `title=`).
- Draw 90 nodes per monitor on the public page, or a chart per card at scale.
- Animate the owl in a loop, make it sad, or let it speak.
- Put operator jargon on the public page.

## 9. Scale (700+ monitors)

- Hero: overall sentence plus counts ("709 of 712 up").
- **Needs attention**: only the monitors that are not up, with their group and one line of why.
- **Groups folded** when all is well: one summary row (name, count, state in words, 90-day % and one group bar).
- One group open at a time, as **compact rows** (glyph, name, % or the state word; no bar, no chart), 24 per page, problems first, then A to Z; "Show all" is a plain link to `/status/{group}`.
- The 90-day bar is **one `<svg>` with a few run-length rects** and the day gaps drawn by a CSS mask (about 150 bytes per monitor instead of 90 elements). On phones the same SVG is shown three times wider, right-aligned: the last 30 days.
- Per-day detail (focusable, touch-friendly tooltips) only on the monitor's own page.
- The scale mockup is 47 KB in all, 16 KB of it content for 712 monitors (the styles become one cached file in the app); today's page is 6.6 MB.

## 10. Borrowed and new

From **Ariane**: the mark grammar (tile, cream character, sleepy face, blush, texture at 0.28), `ArianeFace`'s rules (no mouth, never sad, moves in answer, never the only carrier, reduced motion keeps poses), `Brand.svelte`'s sign (mark + Borel), the page container and breakpoints, the half-disc for "in progress".

From **Maison**: `tokens.css` as written (`light-dark()`, contrast in the header), the ears that became tufts, groups as framed lists in columns, the summary chips, state in words ("Injoignable depuis 12 min"), colour + shape + name for Tempo days (here for states), the door (`AuthShell`) for private pages, the README shape.

New for Hora: the owl and its five poses with a state badge; the multi-owl picture of the mesh; the five-state shape set (disc, half disc, diamond, clock, dashed ring) and the bar heights; `--st-none`, `--wash-degraded`, `--blush` as tokens; the watchman's call as the hero; "Needs attention" and folded groups for scale; the public / operator split for vantage points.

## 11. Niche research (from knowledge, not re-checked online)

Kept: the headline sentence (Statuspage, incident.io), 90-day bars with 30 days on phones (Statuspage), dated newest-first updates (Statuspage, Instatus), subscribe near the headline, "what you'll notice" impact wording (incident.io, Better Stack), uptime % next to the bar (OpenStatus, Hyperping).

Avoided: colour-only dots and bars (most), operator jargon on public pages (Uptime Kuma, Gatus), uniform card grids that do not scale (Uptime Kuma, Cachet), hover-only day details, full-page meta refresh.

Hora's difference: only what is wrong up front, shapes in the bar, several watchers and a calm sentence when they disagree, the hour in the hero, a face with rules.

## 12. Docs theme (Starlight)

The docs carry the same brand on every page (see `docs-page.html`). The theme is `docs/src/styles/custom.css`, with the four woff2 files in `docs/public/fonts/`, `logo` pointed at the wordmark (`logo: { src: './src/assets/wordmark.svg', replacesTitle: true }`) and `favicon` at `favicon.svg`.

**No value is written here by hand.** Every token lives once, in [`tokens.json`](tokens.json); `python3 brand/src/tokens.py` writes it into the product stylesheet, the gallery, the docs theme and its syntax themes, the Rust constants, and the table below. To change a colour or a size, change it there (see the README).

The mapping. Variable names are Starlight's own (`@astrojs/starlight/dist/style/props.css`): `:root` is the dark side, `[data-theme='light']` the light one, which only overrides; in light, Starlight's grey ramp runs the other way (`white` is the darkest). The `docs-*` tokens are the few the docs need beyond the product's set (a body grey between ink and ink-muted, the links, the aside titles).

<!-- tokens:begin - generated from brand/tokens.json by brand/src/tokens.py, do not edit -->
| Starlight variable | Dark (`:root`) | Light (`[data-theme='light']`) | Token |
|---|---|---|---|
| `--sl-color-white` | `#f2f0ea` | `#1b222c` | `raw-cloud` / `ink` |
| `--sl-color-gray-1` | `#eae7e0` | `#1b222c` | `ink` |
| `--sl-color-gray-2` | `#d9d6cf` | `#2e3540` | `docs-text` |
| `--sl-color-gray-3` | `#a8aeb8` | `#525a66` | `ink-muted` |
| `--sl-color-gray-4` | `#6b7380` | `#87837a` | `docs-icon` |
| `--sl-color-gray-5` | `#2e3744` | `#d6d2c6` | `line` |
| `--sl-color-gray-6` | `#1a2029` | `#e7e4db` | `surface` / `ground-raised` |
| `--sl-color-gray-7` | (unset) | `#faf9f5` | `surface` |
| `--sl-color-black` | `#151a21` | `#f2f0ea` | `ground` |
| `--sl-color-accent-low` | `#1d2a31` | `#d8e4e2` | `accent-wash` |
| `--sl-color-accent` | `#63c0cb` | `#0e5e68` | `accent` |
| `--sl-color-accent-high` | `#a9dde3` | `#0a4a52` | `docs-link` |
| `--sl-color-blue-low` | `#1d2a31` | `#d8e4e2` | `accent-wash` |
| `--sl-color-blue` | `#63c0cb` | `#4e8a92` | `accent` / `accent-soft` |
| `--sl-color-blue-high` | `#a9dde3` | `#0e5e68` | `docs-link` / `accent` |
| `--sl-color-purple-low` | `#1d2a31` | `#d8e4e2` | `accent-wash` |
| `--sl-color-purple` | `#63c0cb` | `#4e8a92` | `accent` / `accent-soft` |
| `--sl-color-purple-high` | `#a9dde3` | `#0e5e68` | `docs-link` / `accent` |
| `--sl-color-green-low` | `#1d2a31` | `#d8e4e2` | `accent-wash` |
| `--sl-color-green` | `#63c0cb` | `#4e8a92` | `accent` / `accent-soft` |
| `--sl-color-green-high` | `#a9dde3` | `#0e5e68` | `docs-link` / `accent` |
| `--sl-color-orange-low` | `#30261e` | `#f5e7da` | `wash-degraded` |
| `--sl-color-orange` | `#d89372` | `#b4633f` | `st-degraded` |
| `--sl-color-orange-high` | `#ebc2ae` | `#8f4b22` | `docs-caution-high` |
| `--sl-color-red-low` | `#33211f` | `#f3e2dc` | `wash-down` |
| `--sl-color-red` | `#e07a63` | `#a5372a` | `st-down` |
| `--sl-color-red-high` | `#f0b3a6` | `#a5372a` | `docs-danger-high` |
| `--sl-color-bg-nav` | `#151a21` | `#f2f0ea` | `ground` |
| `--sl-color-bg-sidebar` | `#151a21` | `#f2f0ea` | `ground` |
| `--sl-color-bg-inline-code` | `#1e2530` | `#e7e4db` | `ground-raised` |
| `--sl-color-hairline-light` | `#2e3744` | `#d6d2c6` | `line` |
| `--sl-color-hairline` | `#2e3744` | `#d6d2c6` | `line` |
| `--sl-color-hairline-shade` | `#151a21` | `#c9c4b6` | `docs-hairline-shade` |
| `--sl-color-backdrop-overlay` | `rgb(21 26 33 / 66%)` | `rgb(27 34 44 / 45%)` | `docs-backdrop` |
| `--sl-font` | `'Cal Sans', system-ui, sans-serif` | (same) | `font-text` |
| `--sl-font-mono` | `'Mona Sans Mono', ui-monospace, SFMono-Regular, Menlo, monospace` | (same) | `font-mono` |
| `--sl-line-height` | `1.75` | (same) | `docs-line-height` |
| `--sl-line-height-headings` | `1.15` | (same) | `docs-line-height-headings` |
| `--sl-content-width` | `46rem` | (same) | `docs-content-width` |
| `--hora-display` | `'Fraunces Variable', Georgia, serif` | (same) | `font-display` |
| `--hora-brand` | `'Borel Display', var(--hora-display)` | (same) | `font-brand` |
| `--hora-radius` | `10px` | (same) | `radius-l` |
| `--hora-surface` | `#1a2029` | `#faf9f5` | `surface` |
| `--hora-raised` | `#1e2530` | `#e7e4db` | `ground-raised` |
| `--hora-accent-soft` | `#3e7a84` | `#4e8a92` | `accent-soft` |
| `--hora-on-accent` | `#151a21` | `#f2f0ea` | `on-accent` |
| `--hora-owl-fill` | `#1a2029` | `#faf9f5` | `owl-fill` |
| `--hora-owl-line` | `#eae7e0` | `#1b222c` | `owl-line` |
| `--hora-blush` | `#b8776e` | `#e6a49a` | `blush` |
| `--sl-text-h1` | `2.75rem` from 50em | (same) | `docs-h1` |
| `--sl-text-h2` | `1.75rem` from 50em | (same) | `docs-h2` |
| `--sl-text-h3` | `1.3125rem` from 50em | (same) | `docs-h3` |
| `--sl-text-h4` | `1.125rem` from 50em | (same) | `docs-h4` |
<!-- tokens:end -->

Contrast of the docs' own pairs: body text (gray-2) 12.04:1 dark, 10.84:1 light; links (accent-high dark) 11.77:1; aside titles 5.4:1 or more on their washes; gray-4 (borders, icons only) 3.3 to 3.7:1.

The rules that are not values (they live in `custom.css`, below the generated block):

- **Type.** Fraunces 500, tracked -0.01em, for what you read first: the page title, the headings in the content, card titles. Cal Sans for everything else, `tabular-nums`. The name beside the mark is Borel, nudged down 0.16 em. Starlight raises its headings at 50em; the theme holds them to the calmer scale in the table.
- **Sidebar.** The current page is a wash, petrol text and a 3 px inset rule, never a solid accent block.
- **Asides** sit on the state system: note and tip are petrol (info, up), caution is amber, danger is brick. A wash and a rounded corner, never a coloured side border alone.
- **Code.** Expressive Code frames take the family's surfaces and the 10 px radius, with no shadow; the syntax themes (`docs/src/code-themes.mjs`) use one hue for structure (petrol), one warm hue for values (the slow amber) and muted ink for comments. Inline code has a hairline and a 4 px radius.
- **Focus.** One ring everywhere (WCAG 2.4.7): 3 px accent, 2 px offset.

## 13. Open questions

1. Owl (chosen), alarm clock (B) or lantern (C)?
2. Facial-disc rings: keep (owl reads instantly) or drop (some may read them as glasses)?
3. Ship the owl on operators' public pages by default, with `mascot = false` to turn it off?
4. "Notice, not a page" for disputed downs: today Hora softens the wording only. Add a `notify_unconfirmed` channel list?
5. French public pages: `[page] lang = "fr"`, with the voice table above as the first strings.
