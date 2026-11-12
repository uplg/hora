# Hora brand

The night watch: a little owl that dozes while everything is up, and a parliament
of owls (one per vantage point) that agrees before it wakes you. Third sibling of
the synthe.se family, after Ariane and Maison.

- **Start here:** open [`index.html`](index.html) in a browser - the gallery of
  the brand, the owl states and every mockup (light/dark, desktop/phone).
- [`brand.md`](brand.md) - concept, mascot rules, tokens with contrast ratios,
  state system, type, voice (EN/FR), do and don't, and the Starlight theme
  (section 12) for `docs/`.
- [`tokens.json`](tokens.json) - the design tokens, once: colours (light and
  dark), states, the heatmap ramp, fonts, type scale, spacing, radii.
- `assets/` - mark, favicon, wordmark, owl states, OG card; `fonts/` - Fraunces,
  Cal Sans, Borel, Mona Sans Mono (OFL, licences alongside).

The mockups are generated: `python3 src/build.py` (status pages and the rest via
`src/*.py`), thumbnails with `node src/thumbs.mjs <page>...`, which borrows the
Playwright install of `../maison/e2e` (or set `PLAYWRIGHT_PKG` to another
`package.json` that depends on `playwright`).

## Changing a token

Edit [`tokens.json`](tokens.json), nothing else, then run `python3 brand/src/tokens.py`
(or `just tokens`) from the repository root. It rewrites the region between the
`tokens:begin` and `tokens:end` markers in every copy: the product stylesheet
(`crates/hora-web/assets/hora.css`), this gallery's CSS (`src/common.py`), the
docs theme and its syntax themes (`docs/src/styles/custom.css`,
`docs/src/code-themes.mjs`), the Rust constants (`heatmap.rs`, `layout.rs`) and
the table in `brand.md` section 12. A token is a value, or a `light` and a
`dark` one; `{name}` refers to another token and stays a `var(--name)` in the
stylesheets. A group marked `only` goes to the gallery (`brand`) or the docs
(`docs`) alone; which Starlight variable takes which token is a table at the
top of `src/tokens.py`. `just gate` and CI run `tokens.py --check`, which fails
with a diff when a copy was edited by hand or left behind. Then rebuild the
mockups (`python3 src/build.py`) and re-check the contrast table in `brand.md`
section 4, which is prose and not generated.
