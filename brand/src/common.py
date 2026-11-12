"""Shared pieces for the Hora mockups: tokens, base CSS, the sprite, the owl, the 90-day bar.

Every page is self-contained (CSS inline) except the fonts, referenced from ./fonts/.
"""
import random

# ---------------------------------------------------------------------------
# Tokens. The synthe.se set as Maison writes it (one declaration per colour, light-dark()),
# plus Hora's own: the five states, their washes, the owl's blush.
# ---------------------------------------------------------------------------
TOKENS = r"""
@font-face { font-family: 'Fraunces Variable'; src: url('fonts/Fraunces-VF.woff2') format('woff2'); font-weight: 100 900; font-display: swap; }
@font-face { font-family: 'Cal Sans'; src: url('fonts/CalSansVF.woff2') format('woff2'); font-weight: 400 700; font-display: swap; }
@font-face { font-family: 'Borel Display'; src: url('fonts/BorelDisplay-Regular.woff2') format('woff2'); font-display: swap; }
@font-face { font-family: 'Mona Sans Mono'; src: url('fonts/MonaSansMono.woff2') format('woff2'); font-weight: 200 900; font-display: swap; }

:root { color-scheme: light dark; }
:root[data-theme='light'] { color-scheme: light; }
:root[data-theme='dark'] { color-scheme: dark; }

/* tokens:begin - generated from brand/tokens.json by brand/src/tokens.py, do not edit */
:root {
  /* raw palette: synthe.se (2026-08-17), unchanged */
  --raw-cloud: #f2f0ea; --raw-cloud-deep: #e7e4db; --raw-ink: #1b222c; --raw-ink-soft: #525a66;
  --raw-night: #151a21; --raw-night-raised: #1e2530; --raw-paper-dark: #eae7e0; --raw-muted-dark: #a8aeb8;
  --raw-petrol: #0e5e68; --raw-petrol-soft: #4e8a92; --raw-petrol-wash: #d8e4e2; --raw-petrol-bright: #63c0cb;
  --raw-petrol-dim: #3e7a84; --raw-petrol-wash-dark: #1d2a31; --raw-warn: #b4633f; --raw-warn-dark: #d89372;
  --raw-down: #a5372a; --raw-down-dark: #e07a63; --raw-blush: #e6a49a;

  /* semantic: what the rules use, one declaration per colour (as Maison's tokens.css) */
  --ground: light-dark(var(--raw-cloud), var(--raw-night));
  --ground-raised: light-dark(var(--raw-cloud-deep), var(--raw-night-raised));
  --surface: light-dark(#faf9f5, #1a2029);
  --line: light-dark(#d6d2c6, #2e3744);
  --ink: light-dark(var(--raw-ink), var(--raw-paper-dark));
  --ink-muted: light-dark(var(--raw-ink-soft), var(--raw-muted-dark));
  --accent: light-dark(var(--raw-petrol), var(--raw-petrol-bright));
  --accent-soft: light-dark(var(--raw-petrol-soft), var(--raw-petrol-dim));
  --accent-wash: light-dark(var(--raw-petrol-wash), var(--raw-petrol-wash-dark));
  --on-accent: light-dark(var(--raw-cloud), var(--raw-night));
  --warn-text: light-dark(#8f4b22, var(--raw-warn-dark));
  --blocked-wash: light-dark(#f3e2dc, #33211f);

  /* heatmap: one hue, each step further from the surface (1.5:1 or more between neighbours in both
     themes), so the tiers still read in greyscale and none borrows the colour that means "down" */
  --heat-0: light-dark(#d5e3e2, #27383f); --heat-1: light-dark(#9fc0c2, #2f5f69);
  --heat-2: light-dark(#5a959c, #4d8f99); --heat-3: light-dark(#0e5e68, #8cc4cb);
  --heat-4: light-dark(#1b222c, #e6f4f5);

  /* five states, each colour + shape + word (the colour is the least of the three) */
  --st-up: light-dark(var(--raw-petrol-soft), var(--raw-petrol-dim)); /* graphics, 3:1+ */
  --st-up-text: var(--accent);
  --st-degraded: light-dark(var(--raw-warn), var(--raw-warn-dark));
  --st-degraded-text: var(--warn-text);
  --st-down: light-dark(var(--raw-down), var(--raw-down-dark));
  --st-down-text: var(--st-down);
  --st-maint: var(--ink-muted);
  --st-none: light-dark(#87837a, #687180);
  --wash-up: var(--accent-wash);
  --wash-degraded: light-dark(#f5e7da, #30261e);
  --wash-down: var(--blocked-wash);
  --wash-maint: var(--ground-raised);
  --blush: light-dark(var(--raw-blush), #b8776e);
  --owl-fill: var(--surface); /* the owl's body: surface, outlined in ink */
  --owl-line: var(--ink);

  --shadow-near: light-dark(rgb(27 34 44 / 6%), rgb(0 0 0 / 30%));
  --shadow-far: light-dark(rgb(27 34 44 / 6%), rgb(0 0 0 / 25%));
  --shadow: 0 1px 2px var(--shadow-near), 0 4px 14px var(--shadow-far);

  --focus-ring: 3px solid var(--accent);
  --focus-offset: 2px;

  --font-display: 'Fraunces Variable', Georgia, serif;
  --font-text: 'Cal Sans', system-ui, sans-serif;
  --font-mono: 'Mona Sans Mono', ui-monospace, SFMono-Regular, Menlo, monospace;

  --t-hero: 500 clamp(1.75rem, 1.35rem + 1.8vw, 2.6rem)/1.15 var(--font-display);
  --t-page: 500 clamp(1.75rem, 1.4rem + 1.4vw, 2.25rem)/1.15 var(--font-display);
  --t-section: 500 1.375rem/1.625rem var(--font-display);
  --t-group: 500 1.1875rem/1.625rem var(--font-display);
  --t-body: 400 1rem/1.5rem var(--font-text);
  --t-secondary: 400 0.875rem/1.25rem var(--font-text);
  --t-label: 500 0.875rem/1.25rem var(--font-text);
  --t-meta: 500 0.8125rem/1rem var(--font-text);
  --t-tiny: 500 0.75rem/1rem var(--font-text);

  --page-max: 2400px; --page-margin: 16px; --measure: 42rem; --measure-wide: 64rem;

  --s-1: 4px; --s-2: 8px; --s-3: 12px; --s-4: 16px; --s-5: 24px; --s-6: 32px; --s-7: 48px;

  --control-h: 44px; --control-h-s: 36px; --control-h-xs: 32px; --control-h-l: 52px;

  --radius-s: 4px; --radius: 6px; --radius-m: 8px; --radius-l: 10px; --radius-xl: 12px; --radius-2xl: 16px;
  --radius-pill: 999px;

  --row-min: 48px; --header-h: 56px;
  --glyph: 16px; /* a state's shape beside its word */
  --bar-day-h: 22px; /* the 90-day bar */
  --group-col: 26rem; /* a group's column on wide screens */

  /* the gallery's own: the name in Borel beside the mark, a popover shadow, the half steps */
  --shadow-pop: 0 10px 30px rgb(27 34 44 / 18%);
  --font-brand: 'Borel Display', var(--font-display);
  --t-brand: 400 1.35rem/1 var(--font-brand);
  --s-half: 2px;
  --radius-xs: 2px;
}
@media (min-width: 600px) { :root { --page-margin: 24px; } }
@media (min-width: 1600px) { :root { --page-margin: 32px; } }
@media (max-width: 599px) { :root { --row-min: 56px; } }
@media (pointer: coarse) { :root { --control-h-s: 44px; --control-h-xs: 44px; } }
/* tokens:end */
"""

BASE = r"""
*, *::before, *::after { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body { margin: 0; background: var(--ground); color: var(--ink); font: var(--t-body); font-variant-numeric: tabular-nums;
  -webkit-font-smoothing: antialiased; text-rendering: optimizeLegibility; }
h1, h2, h3, p { margin: 0; }
a { color: var(--accent); text-underline-offset: 0.18em; text-decoration-thickness: 1px; }
a:hover { text-decoration-thickness: 2px; }
:focus-visible { outline: var(--focus-ring); outline-offset: var(--focus-offset); border-radius: var(--radius-s); }
[hidden] { display: none !important; }
code, pre, kbd { font-family: var(--font-mono); font-size: 0.875em; }
.skip { position: absolute; left: var(--s-2); top: -100px; z-index: 9; background: var(--ink); color: var(--ground);
  padding: var(--s-2) var(--s-3); border-radius: var(--radius); font: var(--t-label); }
.skip:focus { top: var(--s-2); }
.vh { position: absolute !important; width: 1px; height: 1px; overflow: hidden; clip: rect(0 0 0 0); white-space: nowrap; }

.page { width: 100%; max-width: var(--page-max); margin: 0 auto; padding: 0 var(--page-margin); }
.measure { max-width: var(--measure); }
.eyebrow { font: var(--t-meta); text-transform: uppercase; letter-spacing: 0.06em; color: var(--ink-muted); }
.muted { color: var(--ink-muted); }
.num { font-variant-numeric: tabular-nums; }

/* header: the operator's sign on the left, the live pill and the theme on the right */
.top { border-bottom: 1px solid var(--line); background: var(--ground); }
.top .page { min-height: var(--header-h); display: flex; align-items: center; gap: var(--s-3); flex-wrap: wrap; padding-block: var(--s-2); }
.sign { display: flex; align-items: center; gap: 10px; color: var(--ink); text-decoration: none; margin-right: auto; min-width: 0; }
.sign .op-logo { width: 32px; height: 32px; border-radius: var(--radius-m); display: grid; place-items: center; flex: none;
  background: var(--ink); color: var(--ground); font: 600 1rem/1 var(--font-display); }
.sign b { font: 500 1.15rem/1.2 var(--font-display); white-space: nowrap; }
.sign span { font: var(--t-secondary); color: var(--ink-muted); white-space: nowrap; }
.sign.hora span.name { font: var(--t-brand); color: var(--ink); transform: translateY(0.16em); }
.top nav { display: flex; gap: var(--s-1); }
@media (max-width: 599px) {
  .top .page { row-gap: 0; }
  .top nav { order: 3; width: 100%; margin-inline: calc(-1 * var(--s-3)); }
  .top .pill { display: none; }
  .theme-btn .lbl { display: none; }
  .theme-btn { padding: 0; width: var(--control-h-xs); }
}
.navlink { display: inline-flex; align-items: center; min-height: var(--control-h-s); padding: 0 var(--s-3); border-radius: var(--radius-pill);
  color: var(--ink-muted); text-decoration: none; font: var(--t-label); }
.navlink:hover { background: var(--ground-raised); color: var(--ink); }
.navlink[aria-current='page'] { background: var(--accent-wash); color: var(--accent); }

.pill { display: inline-flex; align-items: center; gap: 6px; min-height: var(--control-h-xs); padding: 0 var(--s-3);
  border: 1px solid var(--line); border-radius: var(--radius-pill); font: var(--t-meta); color: var(--ink-muted); background: var(--surface); white-space: nowrap; }
.pill .g { width: 10px; height: 10px; color: var(--st-up); }

.btn { display: inline-flex; align-items: center; justify-content: center; gap: var(--s-2); min-height: var(--control-h-s);
  padding: 0 var(--s-4); border-radius: var(--radius); border: 1px solid var(--line); background: var(--surface); color: var(--ink);
  font: var(--t-label); text-decoration: none; cursor: pointer; white-space: nowrap; }
.btn:hover { border-color: var(--ink-muted); }
.btn.primary { background: var(--accent); border-color: var(--accent); color: var(--on-accent); }
.btn.primary:hover { filter: brightness(1.08); }
.btn.quiet { border-color: transparent; background: transparent; color: var(--accent); padding: 0 var(--s-2); }
.btn svg { width: 16px; height: 16px; flex: none; }
.theme-btn { min-height: var(--control-h-xs); padding: 0 var(--s-3); font: var(--t-meta); border-radius: var(--radius-pill); }

/* the state glyphs: shape + colour, always followed by the word */
.g { width: var(--glyph); height: var(--glyph); flex: none; display: inline-block; vertical-align: -0.18em; }
.g.up { color: var(--st-up); } .g.degraded { color: var(--st-degraded); } .g.down { color: var(--st-down); }
.g.maint { color: var(--st-maint); } .g.none { color: var(--st-none); } .g.info { color: var(--accent); }
.word { font: var(--t-label); }
.g + .word { margin-left: 4px; }
.word.up { color: var(--st-up-text); } .word.degraded { color: var(--st-degraded-text); } .word.down { color: var(--st-down-text); }
.word.maint, .word.none { color: var(--ink-muted); }

.chips { display: flex; flex-wrap: wrap; gap: var(--s-2); }
.chip { display: inline-flex; align-items: center; gap: 6px; min-height: var(--control-h-xs); padding: 0 var(--s-3);
  border-radius: var(--radius-pill); background: var(--ground-raised); color: var(--ink); font: var(--t-label); text-decoration: none; white-space: nowrap; }
.chip.degraded { background: var(--wash-degraded); color: var(--st-degraded-text); }
.chip.down { background: var(--wash-down); color: var(--st-down-text); }
.chip.up { background: var(--wash-up); color: var(--st-up-text); }
.chip.up .g { color: var(--accent); } /* 3.0:1 is too close on the wash: the up glyph goes text-grade there */
.chip.maint { background: var(--wash-maint); color: var(--ink-muted); }
a.chip:hover { box-shadow: inset 0 0 0 1px currentColor; }

/* the hero: the owl calls the hour, the sentence says it, the line under it proves it */
.hero { display: grid; grid-template-columns: auto 1fr; gap: var(--s-4) var(--s-5); align-items: center;
  padding: var(--s-5); border-radius: var(--radius-2xl); background: var(--wash-up); margin-top: var(--s-5); }
.hero.degraded { background: var(--wash-degraded); }
.hero.down { background: var(--wash-down); }
.hero.maint { background: var(--wash-maint); }
.hero > .owl { width: clamp(64px, 12vw, 96px); height: auto; }
.hero h1 { font: var(--t-hero); letter-spacing: -0.01em; text-wrap: balance; }
.hero .sub { margin-top: var(--s-2); color: var(--ink-muted); font: var(--t-body); }
.hero .sub b { color: var(--ink); font-weight: 500; }
.hero .chips { grid-column: 1 / -1; }
@media (min-width: 840px) { .hero { padding: var(--s-6); } .hero .chips { grid-column: 2; } }
@media (max-width: 420px) { .hero { grid-template-columns: 1fr; } }

.section { margin-top: var(--s-7); }
.section-head { display: flex; align-items: baseline; gap: var(--s-3); flex-wrap: wrap; margin-bottom: var(--s-3); }
.section-head h2 { font: var(--t-group); }
.section-head .fact { margin-left: auto; font: var(--t-secondary); color: var(--ink-muted); }

/* callouts: announcements and maintenance; an icon and a word, a wash, never a coloured edge alone */
.callout { display: grid; grid-template-columns: auto 1fr; gap: var(--s-1) var(--s-3); padding: var(--s-4); border-radius: var(--radius-l);
  background: var(--accent-wash); margin-top: var(--s-4); }
.callout > .g { margin-top: 3px; }
.callout .k { font: var(--t-label); }
.callout p { color: var(--ink); max-width: var(--measure); }
.callout .when { font: var(--t-meta); color: var(--ink-muted); margin-top: var(--s-1); }
.callout.warning { background: var(--wash-degraded); } .callout.warning .k { color: var(--st-degraded-text); }
.callout.critical { background: var(--wash-down); } .callout.critical .k { color: var(--st-down-text); }
.callout.maint { background: var(--wash-maint); }
.callout.info .k { color: var(--accent); }

/* groups as framed lists of rows (Maison); columns of whole groups on wide screens */
.groups { display: grid; gap: var(--s-6) var(--s-5); grid-template-columns: repeat(auto-fill, minmax(min(100%, var(--group-col)), 1fr)); align-items: start; }
.group-head { display: flex; align-items: baseline; gap: var(--s-3); margin-bottom: var(--s-3); }
.group-head h3 { font: var(--t-group); }
.group-head .fact { margin-left: auto; font: var(--t-secondary); color: var(--ink-muted); white-space: nowrap; }
.list { list-style: none; margin: 0; padding: 0; background: var(--surface); border: 1px solid var(--line); border-radius: var(--radius-xl); overflow: hidden; }
.list > li + li { border-top: 1px solid var(--line); }
.row { display: grid; grid-template-columns: var(--glyph) 1fr auto; column-gap: var(--s-3); row-gap: var(--s-2); align-items: center;
  padding: var(--s-3) var(--s-4); min-height: var(--row-min); color: inherit; text-decoration: none; }
a.row:hover { background: color-mix(in srgb, var(--ground-raised) 55%, transparent); }
.row .name { font: var(--t-label); font-size: 0.9375rem; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.row .name small { font: var(--t-secondary); color: var(--ink-muted); margin-left: 6px; }
.row .state { font: var(--t-secondary); text-align: right; white-space: nowrap; }
.row .state .word { font: var(--t-label); }
.row .bar { grid-column: 1 / -1; }
.row .why { grid-column: 2 / -1; font: var(--t-secondary); color: var(--ink-muted); margin-top: -4px; }
.row.down .why, .row.degraded .why { color: var(--ink); }
.bar-legend { display: flex; justify-content: space-between; font: var(--t-tiny); color: var(--ink-muted); padding: var(--s-2) var(--s-4) var(--s-3); }
.bar-legend .d30 { display: none; }
@media (max-width: 599px) { .bar-legend .d90 { display: none; } .bar-legend .d30 { display: inline; } }

/* the 90-day bar: ONE svg, a handful of run-length rects, gaps drawn by a mask (not 90 nodes) */
.bar { display: block; overflow: hidden; height: calc(var(--bar-day-h) + 7px); }
.bar svg { display: block; width: 100%; height: 100%; overflow: visible;
  -webkit-mask-image: repeating-linear-gradient(90deg, #000 0 calc(100% / 90 - 1.5px), transparent calc(100% / 90 - 1.5px) calc(100% / 90));
  mask-image: repeating-linear-gradient(90deg, #000 0 calc(100% / 90 - 1.5px), transparent calc(100% / 90 - 1.5px) calc(100% / 90)); }
.bar .u { fill: var(--st-up); } .bar .d { fill: var(--st-degraded); } .bar .x { fill: var(--st-down); }
.bar .m { fill: var(--st-maint); } .bar .n { fill: var(--st-none); }
/* a phone shows the last 30 days: the same svg, three times wider, its right end in view */
@media (max-width: 599px) { .bar svg { width: 300%; margin-left: -200%; } }

/* the legend: what each shape means, once per page */
.legend { display: flex; flex-wrap: wrap; gap: var(--s-2) var(--s-4); font: var(--t-secondary); color: var(--ink-muted); }
.legend span { display: inline-flex; align-items: center; gap: 6px; }
.legend svg.cell { width: 8px; height: 24px; overflow: visible; }

/* the footer: Hora signs the page, quietly */
.foot { margin-top: var(--s-7); border-top: 1px solid var(--line); }
.foot .page { display: flex; flex-wrap: wrap; align-items: center; gap: var(--s-3) var(--s-5); padding-block: var(--s-5) var(--s-6); font: var(--t-secondary); color: var(--ink-muted); }
.foot nav { display: flex; flex-wrap: wrap; gap: var(--s-1) var(--s-4); }
.foot a { color: var(--ink-muted); }
.foot a:hover { color: var(--ink); }
.watched { display: inline-flex; align-items: center; gap: var(--s-2); margin-left: auto; color: var(--ink-muted); text-decoration: none; }
.watched img { width: 22px; height: 22px; border-radius: 5px; }
.watched .b { font: 400 1.05rem/1 var(--font-brand); color: var(--ink); transform: translateY(0.14em); }

/* the owl: pose = state; it moves only when the state changes, once */
.owl { overflow: visible; flex: none; }
.owl .ln { fill: var(--owl-fill); stroke: var(--owl-line); stroke-width: 1.5; stroke-linejoin: round; }
.owl .tx { fill: none; stroke: var(--owl-line); stroke-width: 1; stroke-linecap: round; opacity: 0.2; }
.owl .ft { fill: var(--owl-line); }
.owl .ey path, .owl .ey line { fill: none; stroke: var(--owl-line); stroke-width: 1.8; stroke-linecap: round; }
.owl .ey ellipse { fill: var(--owl-line); }
.owl .ey .glint { fill: var(--owl-fill); }
.owl .bk { fill: var(--accent); stroke: var(--accent); stroke-width: 0.9; stroke-linejoin: round; }
.owl .ck { fill: var(--blush); opacity: 0.85; transition: opacity 0.3s; }
.owl .tl, .owl .tr { transform-box: fill-box; transition: transform 0.35s cubic-bezier(.3,1.4,.6,1); }
.owl .tl { transform-origin: 100% 100%; } .owl .tr { transform-origin: 0% 100%; }
.owl .ey > g { transition: opacity 0.18s; opacity: 0; }
.owl .bd { fill: var(--ground); }
.owl .badge > g { opacity: 0; }
.owl .badge { opacity: 0; transition: opacity 0.2s; }
/* up: asleep, the tufts at rest. Nothing is wrong, so nothing moves. */
.owl[data-state='up'] .shut-l, .owl[data-state='up'] .shut-r { opacity: 1; }
.owl[data-state='up'] .tl { transform: rotate(-5deg); } .owl[data-state='up'] .tr { transform: rotate(5deg); }
/* degraded: one eye open, one tuft up: half an eye on it */
.owl[data-state='degraded'] .shut-l, .owl[data-state='degraded'] .open-r { opacity: 1; }
.owl[data-state='degraded'] .tl { transform: rotate(-5deg); } .owl[data-state='degraded'] .tr { transform: rotate(-6deg); }
/* down: awake, both eyes open, tufts up. Attentive, never alarmed, never sad. */
.owl[data-state='down'] .open-l, .owl[data-state='down'] .open-r { opacity: 1; }
.owl[data-state='down'] .tl { transform: rotate(3deg); } .owl[data-state='down'] .tr { transform: rotate(-3deg); }
/* maintenance: a planned nap, tufts folded */
.owl[data-state='maint'] .shut-l, .owl[data-state='maint'] .shut-r { opacity: 1; }
.owl[data-state='maint'] .tl { transform: rotate(-22deg); } .owl[data-state='maint'] .tr { transform: rotate(22deg); }
/* no data: it cannot see yet; two dashes, no blush */
.owl[data-state='none'] .dash { opacity: 1; }
.owl[data-state='none'] .ck { opacity: 0; }
.owl[data-state='degraded'] .badge, .owl[data-state='down'] .badge, .owl[data-state='maint'] .badge, .owl[data-state='none'] .badge { opacity: 1; }
.owl[data-state='degraded'] .b-degraded, .owl[data-state='down'] .b-down, .owl[data-state='maint'] .b-maint, .owl[data-state='none'] .b-none { opacity: 1; }
.owl .b-degraded { color: var(--st-degraded); } .owl .b-down { color: var(--st-down); } .owl .b-maint { color: var(--st-maint); } .owl .b-none { color: var(--st-none); }
/* waking: one perk of the tufts when the state turns to down, then still */
.owl.woke .tl, .owl.woke .tr { animation: perk 0.6s ease-out 1; }
@keyframes perk { 40% { translate: 0 -1.2px; } }
@media (prefers-reduced-motion: reduce) {
  .owl *, .owl { transition: none !important; animation: none !important; }
}

/* theme switch (the only script on a page, with the owl demo) */
.theme-btn .lbl::before { content: 'Auto'; }
:root[data-theme='light'] .theme-btn .lbl::before { content: 'Light'; }
:root[data-theme='dark'] .theme-btn .lbl::before { content: 'Dark'; }

.mock-note { margin: var(--s-6) 0 0; padding: var(--s-3) var(--s-4); border: 1px dashed var(--line); border-radius: var(--radius-l);
  font: var(--t-secondary); color: var(--ink-muted); max-width: var(--measure-wide); }
.mock-note b { color: var(--ink); font-weight: 500; }
"""

THEME_SCRIPT = r"""<script>
/* theme: system by default; the button cycles auto, light, dark and remembers (per viewer) */
(function () {
  var d = document.documentElement, k = 'hora-theme';
  try { var t = localStorage.getItem(k); if (t) d.dataset.theme = t; } catch (e) {}
  addEventListener('click', function (e) {
    var b = e.target.closest('.theme-btn'); if (!b) return;
    var next = { '': 'light', light: 'dark', dark: '' }[d.dataset.theme || ''];
    if (next) d.dataset.theme = next; else delete d.dataset.theme;
    b.setAttribute('aria-label', 'Theme: ' + (next || 'auto') + '. Change theme');
    try { next ? localStorage.setItem(k, next) : localStorage.removeItem(k); } catch (e) {}
  });
})();
</script>"""

# ---------------------------------------------------------------------------
# Glyph sprite: the five state shapes (+ info, icons). 16x16, currentColor.
# ---------------------------------------------------------------------------
SPRITE = r"""<svg width="0" height="0" style="position:absolute" aria-hidden="true" focusable="false">
<symbol id="g-up" viewBox="0 0 16 16"><circle cx="8" cy="8" r="6" fill="currentColor"/></symbol>
<symbol id="g-degraded" viewBox="0 0 16 16"><circle cx="8" cy="8" r="5.4" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M8 2.6a5.4 5.4 0 0 0 0 10.8z" fill="currentColor"/></symbol>
<symbol id="g-down" viewBox="0 0 16 16"><path d="M8 1.2 14.8 8 8 14.8 1.2 8Z" fill="currentColor" stroke="currentColor" stroke-width="1" stroke-linejoin="round"/><path d="M8 4.9v3.6" stroke="var(--surface, #fff)" stroke-width="1.7" stroke-linecap="round"/><circle cx="8" cy="11" r=".95" fill="var(--surface, #fff)"/></symbol>
<symbol id="g-maint" viewBox="0 0 16 16"><circle cx="8" cy="8" r="5.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="M8 4.9V8l2.2 1.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></symbol>
<symbol id="g-none" viewBox="0 0 16 16"><circle cx="8" cy="8" r="5.6" fill="none" stroke="currentColor" stroke-width="1.6" stroke-dasharray="2.4 2.0"/></symbol>
<symbol id="g-info" viewBox="0 0 16 16"><circle cx="8" cy="8" r="6.4" fill="currentColor"/><path d="M8 7.3v4" stroke="var(--surface, #fff)" stroke-width="1.7" stroke-linecap="round"/><circle cx="8" cy="4.9" r=".95" fill="var(--surface, #fff)"/></symbol>
<symbol id="i-sun" viewBox="0 0 16 16"><circle cx="8" cy="8" r="3" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M8 1.5v1.6M8 12.9v1.6M1.5 8h1.6M12.9 8h1.6M3.4 3.4l1.1 1.1M11.5 11.5l1.1 1.1M3.4 12.6l1.1-1.1M11.5 4.5l1.1-1.1" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></symbol>
<symbol id="i-feed" viewBox="0 0 16 16"><path d="M3 3.5a9.5 9.5 0 0 1 9.5 9.5M3 7.5A5.5 5.5 0 0 1 8.5 13" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/><circle cx="3.8" cy="12.2" r="1.3" fill="currentColor"/></symbol>
<symbol id="i-arrow" viewBox="0 0 16 16"><path d="M3 8h10M9 4l4 4-4 4" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></symbol>
<symbol id="i-chev" viewBox="0 0 16 16"><path d="M6 3.5 10.5 8 6 12.5" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/></symbol>
<symbol id="i-copy" viewBox="0 0 16 16"><rect x="5" y="5" width="8.5" height="8.5" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M10.5 2.8H4.4a1.6 1.6 0 0 0-1.6 1.6v6.1" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round"/></symbol>
<symbol id="i-pin" viewBox="0 0 16 16"><path d="M8 14.5s4.8-4.3 4.8-8a4.8 4.8 0 0 0-9.6 0c0 3.7 4.8 8 4.8 8Z" fill="none" stroke="currentColor" stroke-width="1.5"/><circle cx="8" cy="6.5" r="1.6" fill="currentColor"/></symbol>
<symbol id="i-lock" viewBox="0 0 16 16"><rect x="3" y="7" width="10" height="7.5" rx="1.6" fill="none" stroke="currentColor" stroke-width="1.5"/><path d="M5.2 7V5a2.8 2.8 0 0 1 5.6 0v2" fill="none" stroke="currentColor" stroke-width="1.5"/></symbol>
<symbol id="i-print" viewBox="0 0 16 16"><path d="M4.5 6V2.5h7V6M4.5 11.5H3a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h10a1 1 0 0 1 1 1v3.5a1 1 0 0 1-1 1h-1.5" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linejoin="round"/><rect x="4.5" y="9.5" width="7" height="4.5" fill="none" stroke="currentColor" stroke-width="1.5"/></symbol>
<symbol id="i-search" viewBox="0 0 16 16"><circle cx="7" cy="7" r="4.6" fill="none" stroke="currentColor" stroke-width="1.6"/><path d="m10.5 10.5 3.5 3.5" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"/></symbol>
<symbol id="i-gh" viewBox="0 0 16 16"><path fill="currentColor" d="M8 .6a7.4 7.4 0 0 0-2.34 14.42c.37.07.5-.16.5-.36v-1.3c-2.06.45-2.5-.87-2.5-.87-.33-.86-.82-1.09-.82-1.09-.67-.46.05-.45.05-.45.74.05 1.13.76 1.13.76.66 1.13 1.73.8 2.15.61.07-.48.26-.8.47-.99-1.64-.19-3.37-.82-3.37-3.65 0-.8.29-1.46.76-1.98-.08-.19-.33-.94.07-1.95 0 0 .62-.2 2.03.76a7 7 0 0 1 3.7 0c1.41-.96 2.03-.76 2.03-.76.4 1.01.15 1.76.07 1.95.47.52.76 1.18.76 1.98 0 2.84-1.73 3.46-3.38 3.64.27.23.5.68.5 1.37v2.03c0 .2.13.43.5.36A7.4 7.4 0 0 0 8 .6Z"/></symbol>
</svg>"""


def glyph(state, label=None):
    """A state's shape. Decorative unless a label is given (the word beside it carries it)."""
    aria = f'role="img" aria-label="{label}"' if label else 'aria-hidden="true"'
    return f'<svg class="g {state}" {aria}><use href="#g-{state}"/></svg>'


def icon(name, cls=''):
    return f'<svg class="{cls}" aria-hidden="true" width="16" height="16"><use href="#i-{name}"/></svg>'


WORD = {'up': 'Up', 'degraded': 'Slow', 'down': 'Down', 'maint': 'Maintenance', 'none': 'No data'}


def state(st, text=None):
    """Shape + word. The word is the truth; the shape and colour help."""
    return f'{glyph(st)}<span class="word {st}">{text or WORD[st]}</span>'


# ---------------------------------------------------------------------------
# The owl (HoraFace), theme-aware, pose by data-state. 48-unit box.
# ---------------------------------------------------------------------------
OWL_PARTS = r"""<path class="ln tl" d="M17.6 16.8C15.4 15.9 13.9 14 13.4 11.3 15.9 11.7 18.4 13 20.2 15.2Z"/>
<path class="ln tr" d="M30.4 16.8C32.6 15.9 34.1 14 34.6 11.3 32.1 11.7 29.6 13 27.8 15.2Z"/>
<ellipse class="ft" cx="20.6" cy="41.1" rx="1.9" ry="1.2"/><ellipse class="ft" cx="27.4" cy="41.1" rx="1.9" ry="1.2"/>
<path class="ln" d="M24 14C30.9 14 35.6 19.2 35.6 26.6 35.6 34.6 30.6 40.4 24 40.4S12.4 34.6 12.4 26.6C12.4 19.2 17.1 14 24 14Z"/>
<g class="tx"><circle cx="18.9" cy="24.6" r="4.9"/><circle cx="29.1" cy="24.6" r="4.9"/><path d="M14.4 30.8C15.8 33.7 18.1 36.3 21 37.8M33.6 30.8C32.2 33.7 29.9 36.3 27 37.8"/></g>
<g class="ey">
<g class="shut-l"><path d="M16.7 24.4a2.2 2.2 0 0 0 4.4 0"/></g>
<g class="shut-r"><path d="M26.9 24.4a2.2 2.2 0 0 0 4.4 0"/></g>
<g class="open-l"><ellipse cx="18.9" cy="24.6" rx="2.2" ry="2.6"/><circle class="glint" cx="19.7" cy="23.6" r=".75"/></g>
<g class="open-r"><ellipse cx="29.1" cy="24.6" rx="2.2" ry="2.6"/><circle class="glint" cx="29.9" cy="23.6" r=".75"/></g>
<g class="dash"><path d="M16.9 24.8h4M27.1 24.8h4"/></g>
</g>
<path class="bk" d="M22.9 28.3h2.2L24 30.1Z"/>
<ellipse class="ck" cx="16.4" cy="30.6" rx="2" ry="1.2"/><ellipse class="ck" cx="31.6" cy="30.6" rx="2" ry="1.2"/>
<g class="badge"><circle class="bd" cx="39.5" cy="38.5" r="7.2"/>
<g class="b-degraded"><svg x="33.5" y="32.5" width="12" height="12"><use href="#g-degraded"/></svg></g>
<g class="b-down"><svg x="33.5" y="32.5" width="12" height="12"><use href="#g-down"/></svg></g>
<g class="b-maint"><svg x="33.5" y="32.5" width="12" height="12"><use href="#g-maint"/></svg></g>
<g class="b-none"><svg x="33.5" y="32.5" width="12" height="12"><use href="#g-none"/></svg></g>
</g>"""


def owl(st='up', size=88, label=None, cls='', extra=''):
    aria = f'role="img" aria-label="{label}"' if label else 'aria-hidden="true"'
    return (f'<svg class="owl {cls}" data-state="{st}" viewBox="6 6 40 40" width="{size}" height="{size}" {aria} {extra}>'
            f'{OWL_PARTS}</svg>')


# ---------------------------------------------------------------------------
# The 90-day bar: run-length rects (a handful per monitor), heights encode the state.
# up: full; slow: shorter; down: full plus a tail below the line; maintenance: half; no data: a stub.
# ---------------------------------------------------------------------------
H = 22
GEOM = {'u': (0, H), 'd': (7, H - 7), 'x': (0, H + 7), 'm': (11, H - 11), 'n': (H - 4, 4)}


def bar_svg(days):
    runs, i = [], 0
    while i < len(days):
        j = i
        while j < len(days) and days[j] == days[i]:
            j += 1
        runs.append((days[i], i, j - i))
        i = j
    rects = []
    for s, start, n in runs:
        y, h = GEOM[s]
        rects.append(f'<rect class="{s}" x="{start}" y="{y}" width="{n}" height="{h}"/>')
    return (f'<svg viewBox="0 0 90 {H + 7}" preserveAspectRatio="none" aria-hidden="true" focusable="false">'
            + ''.join(rects) + '</svg>')


def days(seed, down=(), slow=(), maint=(), none_before=0):
    d = ['u'] * 90
    for i in range(none_before):
        d[i] = 'n'
    for i in slow:
        d[i] = 'd'
    for i in maint:
        d[i] = 'm'
    for i in down:
        d[i] = 'x'
    return d


def bar_label(d, pct):
    nd = d.count('x'); ns = d.count('d')
    bits = [f'{pct} up over 90 days']
    if nd:
        bits.append(f'{nd} day{"s" if nd > 1 else ""} with an outage')
    if ns:
        bits.append(f'{ns} slow day{"s" if ns > 1 else ""}')
    if not nd and not ns:
        bits.append('no incident')
    return ', '.join(bits)


def bar(d, pct, href='monitor.html'):
    return f'<a class="bar" href="{href}" aria-label="{bar_label(d, pct)}. Open history">{bar_svg(d)}</a>'


LEGEND = (
    '<div class="legend" aria-label="How to read the bars">'
    '<span><svg class="cell" viewBox="0 0 4 29"><rect class="u" style="fill:var(--st-up)" width="4" height="22"/></svg>Up all day</span>'
    '<span><svg class="cell" viewBox="0 0 4 29"><rect style="fill:var(--st-degraded)" y="7" width="4" height="15"/></svg>Slow at times (shorter)</span>'
    '<span><svg class="cell" viewBox="0 0 4 29"><rect style="fill:var(--st-down)" width="4" height="29"/></svg>Outage (drops below the line)</span>'
    '<span><svg class="cell" viewBox="0 0 4 29"><rect style="fill:var(--st-maint)" y="11" width="4" height="11"/></svg>Maintenance (half)</span>'
    '<span><svg class="cell" viewBox="0 0 4 29"><rect style="fill:var(--st-none)" y="18" width="4" height="4"/></svg>No data</span>'
    '</div>')


def theme_button():
    return ('<button class="btn theme-btn" type="button" aria-label="Theme: auto. Change theme">'
            + icon('sun') + '<span class="lbl" aria-hidden="true"></span></button>')


def header(op='Pelican', op_initial='P', sub='Status', current=None, live=True, nav=True):
    links = [('Status', 'status-ok.html'), ('History', 'monitor.html'), ('Report', 'report.html')]
    navh = ''
    if nav:
        navh = '<nav aria-label="Pages">' + ''.join(
            f'<a class="navlink" href="{h}"{" aria-current=\"page\"" if t == current else ""}>{t}</a>' for t, h in links) + '</nav>'
    livep = ('<span class="pill" title="This page updates itself every 30 s">'
             + glyph('up') + 'Live · 12 s ago</span>') if live else ''
    return (f'<header class="top"><div class="page">'
            f'<a class="sign" href="status-ok.html"><span class="op-logo" aria-hidden="true">{op_initial}</span>'
            f'<b>{op}</b><span>{sub}</span></a>{navh}{livep}{theme_button()}</div></header>')


def footer(extra_links=True):
    links = ('<nav aria-label="More"><a href="monitor.html">Incident history</a><a href="incident.html">Timeline</a>'
             '<a href="report.html">Monthly report</a><a href="#">Atom feed</a><a href="#">JSON</a></nav>') if extra_links else ''
    return (f'<footer class="foot"><div class="page">{links}'
            f'<a class="watched" href="docs-home.html"><img src="assets/mark.svg" alt="" width="22" height="22">'
            f'<span>Watched over by</span><span class="b">Hora</span></a></div></footer>')


def shell(title, body, desc='', css='', theme_color=True, scripts=''):
    return f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title}</title>
<meta name="description" content="{desc}">
<meta name="color-scheme" content="light dark">
<meta name="theme-color" content="#F2F0EA" media="(prefers-color-scheme: light)">
<meta name="theme-color" content="#151A21" media="(prefers-color-scheme: dark)">
<link rel="icon" href="assets/favicon.svg" type="image/svg+xml">
<link rel="preload" href="fonts/CalSansVF.woff2" as="font" type="font/woff2" crossorigin>
{THEME_SCRIPT}
<style>{TOKENS}{BASE}{css}</style>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>
{SPRITE}
{body}
{scripts}
</body>
</html>
"""


# ---------------------------------------------------------------------------
# Several watchers: a few owls on one branch, each with its own pose. The mesh, drawn.
# ---------------------------------------------------------------------------
def owls(states, names=None, label='', width=320, cls=''):
    n = len(states)
    step = 46
    W = step * n + 8
    parts = []
    for i, st in enumerate(states):
        x = 4 + i * step
        parts.append(f'<svg class="owl" data-state="{st}" x="{x}" y="0" width="46" height="46" viewBox="6 6 40 40">{OWL_PARTS}</svg>')
    branch = f'<path class="branch" d="M0 45.5C{W * 0.3:.0f} 43.5 {W * 0.7:.0f} 47.5 {W} 44.5" />'
    labels = ''
    if names:
        labels = ''.join(f'<text class="olab" x="{4 + i * step + 23}" y="58" text-anchor="middle">{nm}</text>' for i, nm in enumerate(names))
    H = 62 if names else 50
    aria = f'role="img" aria-label="{label}"' if label else 'aria-hidden="true"'
    return f'<svg class="owls {cls}" viewBox="0 0 {W} {H}" width="{width}" {aria}>{branch}{"".join(parts)}{labels}</svg>'


BASE += r"""
.owls { overflow: visible; display: block; max-width: 100%; height: auto; }
.owls .branch { fill: none; stroke: var(--ink-muted); stroke-width: 1.4; stroke-linecap: round; opacity: 0.5; }
.owls .olab { font: 500 5.2px var(--font-text); fill: var(--ink-muted); }
/* where it is watched from: places, each with its own shape and word */
.vantages { display: flex; flex-wrap: wrap; gap: var(--s-2); align-items: center; }
.vchip { display: inline-flex; align-items: center; gap: 6px; min-height: 28px; padding: 0 10px; border-radius: var(--radius-pill); background: var(--ground-raised); font: var(--t-meta); color: var(--ink); white-space: nowrap; }
.vchip .g { width: 12px; height: 12px; }
.vchip small { font: var(--t-meta); color: var(--ink-muted); }
.vchip.down { background: var(--wash-down); } .vchip.degraded { background: var(--wash-degraded); }
.watchers { display: flex; flex-wrap: wrap; align-items: center; gap: var(--s-2) var(--s-3); font: var(--t-secondary); color: var(--ink-muted); }
.watchers .owls { width: 92px; }
"""
