#!/usr/bin/env python3
"""Hora's design tokens, written from their one source (brand/tokens.json) into every copy.

    python3 brand/src/tokens.py           rewrite the consumers in place
    python3 brand/src/tokens.py --check   exit 1 and print a diff if one is out of date

Each consumer keeps a region between a `tokens:begin` and a `tokens:end` marker comment
(one region per file); only what sits between the two lines is rewritten. Standard library
only. To change a token: edit brand/tokens.json, run this, commit both.
"""
import difflib
import json
import os
import re
import sys
import textwrap

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..', '..'))
SOURCE = 'brand/tokens.json'
BEGIN, END = 'tokens:begin', 'tokens:end'
REF = re.compile(r'\{([a-z0-9-]+)\}')
DENSE_WIDTH = 110   # a dense group packs tokens up to this column
NOTE_WIDTH = 100    # a group's note wraps at this column


class Tokens:
    """The parsed source: groups in order, and every token by name."""

    def __init__(self, path=os.path.join(ROOT, SOURCE)):
        with open(path, encoding='utf-8') as f:
            data = json.load(f)
        self.groups = data['groups']
        self.responsive = data.get('responsive', [])
        self.by_name = {}
        for group, body in self.groups.items():
            for name, token in body['tokens'].items():
                if name in self.by_name:
                    raise SystemExit(f'{SOURCE}: token {name!r} is declared twice')
                self.by_name[name] = token
        for name in self.by_name:      # every reference must lead somewhere, in both themes
            self.resolve(name, 'light'), self.resolve(name, 'dark')
        for rule in self.responsive:
            for name in rule['tokens']:
                if name not in self.by_name:
                    raise SystemExit(f'{SOURCE}: responsive override of unknown token {name!r}')

    @staticmethod
    def sides(token):
        """A token's (light, dark) values; the same twice when it has one value."""
        if isinstance(token, str):
            return token, token
        if 'value' in token:
            return token['value'], token['value']
        return token['light'], token['dark']

    @staticmethod
    def note(token):
        return None if isinstance(token, str) else token.get('note')

    def resolve(self, name, theme, keep=None, seen=()):
        """The value of `name` in `theme`, references followed down to literals.
        `keep` maps the references to leave as a `var()` to the variable's name."""
        if name not in self.by_name:
            raise SystemExit(f'{SOURCE}: reference to unknown token {name!r}')
        if name in seen:
            raise SystemExit(f'{SOURCE}: token {name!r} refers to itself')
        light, dark = self.sides(self.by_name[name])

        def follow(match):
            ref = match.group(1)
            if keep and ref in keep:
                return f'var({keep[ref]})'
            return self.resolve(ref, theme, keep, seen + (name,))
        return REF.sub(follow, light if theme == 'light' else dark)

    def css_value(self, token):
        """A token as the stylesheets declare it: references stay `var()`, two sides `light-dark()`."""
        light, dark = (REF.sub(r'var(--\1)', side) for side in self.sides(token))
        return light if light == dark else f'light-dark({light}, {dark})'

    def for_consumer(self, consumer):
        for group, body in self.groups.items():
            only = body.get('only')
            if consumer in only if only else consumer in ('product', 'brand'):
                yield group, body


# --- the `:root` block of a stylesheet (product and brand gallery) ------------------

def root_block(tokens, consumer):
    out = [':root {']
    first = True
    for _, body in tokens.for_consumer(consumer):
        if not first:
            out.append('')
        first = False
        if body.get('note'):
            lines = textwrap.wrap(body['note'], NOTE_WIDTH - 5, break_long_words=False, break_on_hyphens=False)
            lines = ['  /* ' + lines[0]] + ['     ' + line for line in lines[1:]]
            lines[-1] += ' */'
            out += lines
        row = ''
        for name, token in body['tokens'].items():
            decl = f'--{name}: {tokens.css_value(token)};'
            note = tokens.note(token)
            if note:
                decl += f' /* {note} */'
            packs = body.get('dense') and not note and row and len(row) + 1 + len(decl) <= DENSE_WIDTH
            if packs and not row.endswith('*/'):
                row += ' ' + decl
                continue
            if row:
                out.append(row)
            row = '  ' + decl
        if row:
            out.append(row)
    out.append('}')
    for rule in tokens.responsive:
        decls = ' '.join(f'--{name}: {value};' for name, value in rule['tokens'].items())
        out.append(f'@media {rule["media"]} {{ :root {{ {decls} }} }}')
    return out


# --- the Starlight theme (docs/src/styles/custom.css) -------------------------------
# Starlight's variable names, each fed from a token: (variable, dark token, light token,
# dark comment, light comment). `None` for a token leaves the variable out of that side
# (`:root` is the dark side; the light block only overrides). `---` is a blank line and
# `#` a comment (dark text, light text). Values are written resolved: Starlight's own
# rules read these names, not ours.

DOCS = [
    ('#', 'dark: night ground, paper ink, bright petrol',
          'light: cloud ground, ink, petrol (the ramp is inverted: white = darkest)'),
    ('--sl-color-white', 'raw-cloud', 'ink', None, None),
    ('--sl-color-gray-1', 'ink', 'ink', 'ink', 'ink, 14.05:1'),
    ('--sl-color-gray-2', 'docs-text', 'docs-text', 'body text, 12.04:1', 'body text, 10.84:1'),
    ('--sl-color-gray-3', 'ink-muted', 'ink-muted', 'ink-muted, 7.83:1', 'ink-muted, 6.12:1'),
    ('--sl-color-gray-4', 'docs-icon', 'docs-icon', 'icons, borders', 'icons, borders'),
    ('--sl-color-gray-5', 'line', 'line', 'line', 'line'),
    ('--sl-color-gray-6', 'surface', 'ground-raised', 'surface', 'raised'),
    ('--sl-color-gray-7', None, 'surface', None, 'surface'),
    ('--sl-color-black', 'ground', 'ground', 'ground', 'ground'),
    ('---',),
    ('--sl-color-accent-low', 'accent-wash', 'accent-wash', 'petrol wash', None),
    ('--sl-color-accent', 'accent', 'accent', 'petrol bright', None),
    ('--sl-color-accent-high', 'docs-link', 'docs-link', 'links, 11.77:1', None),
    ('---',),
    ('#', 'asides on the state system: note and tip = petrol (info, up), caution = amber,\n'
          'danger = brick. Card icons and badges use these too.', None),
    ('--sl-color-blue-low', 'accent-wash', 'accent-wash', None, None),
    ('--sl-color-blue', 'accent', 'accent-soft', None, None),
    ('--sl-color-blue-high', 'docs-link', 'accent', None, None),
    ('--sl-color-purple-low', 'accent-wash', 'accent-wash', None, None),
    ('--sl-color-purple', 'accent', 'accent-soft', None, None),
    ('--sl-color-purple-high', 'docs-link', 'accent', None, None),
    ('--sl-color-green-low', 'accent-wash', 'accent-wash', None, None),
    ('--sl-color-green', 'accent', 'accent-soft', None, None),
    ('--sl-color-green-high', 'docs-link', 'accent', None, None),
    ('--sl-color-orange-low', 'wash-degraded', 'wash-degraded', None, None),
    ('--sl-color-orange', 'st-degraded', 'st-degraded', None, None),
    ('--sl-color-orange-high', 'docs-caution-high', 'docs-caution-high', None, None),
    ('--sl-color-red-low', 'wash-down', 'wash-down', None, None),
    ('--sl-color-red', 'st-down', 'st-down', None, None),
    ('--sl-color-red-high', 'docs-danger-high', 'docs-danger-high', None, None),
    ('---',),
    ('--sl-color-bg-nav', 'ground', 'ground', None, None),
    ('--sl-color-bg-sidebar', 'ground', 'ground', None, None),
    ('--sl-color-bg-inline-code', 'ground-raised', 'ground-raised', None, None),
    ('--sl-color-hairline-light', 'line', 'line', None, None),
    ('--sl-color-hairline', 'line', 'line', None, None),
    ('--sl-color-hairline-shade', 'docs-hairline-shade', 'docs-hairline-shade', None, None),
    ('--sl-color-backdrop-overlay', 'docs-backdrop', 'docs-backdrop', None, None),
    ('---',),
    ('--sl-font', 'font-text', None, None, None),
    ('--sl-font-mono', 'font-mono', None, None, None),
    ('--sl-line-height', 'docs-line-height', None, None, None),
    ('--sl-line-height-headings', 'docs-line-height-headings', None, None, None),
    ('--sl-content-width', 'docs-content-width', None, None, None),
    ('---',),
    ('#', "Hora's own, for the rules below and the splash", None),
    ('--hora-display', 'font-display', None, None, None),
    ('--hora-brand', 'font-brand', None, None, None),
    ('--hora-radius', 'radius-l', None, None, None),
    ('--hora-surface', 'surface', 'surface', None, None),
    ('--hora-raised', 'ground-raised', 'ground-raised', None, None),
    ('--hora-accent-soft', 'accent-soft', 'accent-soft', None, None),
    ('--hora-on-accent', 'on-accent', 'on-accent', None, None),
    ('--hora-owl-fill', 'owl-fill', 'owl-fill', None, None),
    ('--hora-owl-line', 'owl-line', 'owl-line', None, None),
    ('--hora-blush', 'blush', 'blush', None, None),
]
# Starlight raises its headings at 50em; these replace that scale.
DOCS_HEADINGS = [('--sl-text-h1', 'docs-h1'), ('--sl-text-h2', 'docs-h2'),
                 ('--sl-text-h3', 'docs-h3'), ('--sl-text-h4', 'docs-h4')]
# The one reference the docs keep as a variable: the name's font falls back on the display one.
DOCS_KEEP = {'font-display': '--hora-display'}


def docs_side(tokens, theme):
    side = 1 if theme == 'dark' else 2
    out = []
    for row in DOCS:
        if row[0] == '---':
            if out and out[-1] != '':
                out.append('')
        elif row[0] == '#':
            if row[side]:
                lines = row[side].split('\n')
                out += ['\t/* ' + lines[0]] + ['\t   ' + line for line in lines[1:]]
                out[-1] += ' */'
        elif row[side]:
            line = f'\t{row[0]}: {tokens.resolve(row[side], theme, DOCS_KEEP)};'
            if row[side + 2]:
                line += f' /* {row[side + 2]} */'
            out.append(line)
    while out and out[-1] == '':
        out.pop()
    return out


def docs_css(tokens):
    out = [':root {'] + docs_side(tokens, 'dark') + ['}', '']
    out += ["/* Headings: Starlight raises them at 50em; keep the brand's calmer scale. */",
            '@media (min-width: 50em) {', '\t:root {']
    out += [f'\t\t{var}: {tokens.resolve(name, "dark")};' for var, name in DOCS_HEADINGS]
    out += ['\t}', '}', '', ":root[data-theme='light'] {"] + docs_side(tokens, 'light') + ['}']
    return out


def docs_table(tokens):
    """brand.md section 12: the same mapping, as a table a person reads."""
    out = ['| Starlight variable | Dark (`:root`) | Light (`[data-theme=\'light\']`) | Token |', '|---|---|---|---|']
    for row in DOCS:
        if row[0] in ('---', '#'):
            continue
        var, dark, light = row[:3]
        cells = [f'`{tokens.resolve(name, theme, DOCS_KEEP)}`' if name else None
                 for name, theme in ((dark, 'dark'), (light, 'light'))]
        names = ' / '.join(f'`{name}`' for name in dict.fromkeys(n for n in (dark, light) if n))
        out.append(f'| `{var}` | {cells[0] or "(unset)"} | {cells[1] or "(same)"} | {names} |')
    for var, name in DOCS_HEADINGS:
        out.append(f'| `{var}` | `{tokens.resolve(name, "dark")}` from 50em | (same) | `{name}` |')
    return out


# --- Expressive Code's syntax themes (docs/src/code-themes.mjs) ---------------------

CODE_THEME = [('bg', 'surface'), ('fg', 'docs-text'), ('muted', 'ink-muted'), ('key', 'accent'),
              ('call', 'docs-link'), ('value', 'warn-text'), ('number', 'docs-caution-high'),
              ('punct', 'ink-muted'), ('down', 'st-down')]


def code_themes(tokens):
    out = []
    for theme in ('dark', 'light'):
        if out:
            out.append('')
        out.append(f"export const hora{theme.title()} = theme('hora-{theme}', '{theme}', {{")
        out += [f"\t{key}: '{tokens.resolve(name, theme)}'," for key, name in CODE_THEME]
        out.append('});')
    return out


# --- Rust constants -----------------------------------------------------------------

def heatmap_rs(tokens):
    def tiers(theme):
        return ', '.join(f'"{tokens.resolve(f"heat-{step}", theme)}"' for step in range(5))
    out = [
        "/// Each tier's colour, from the brand tokens (`--heat-0` to `--heat-4`, light",
        '/// theme): the fill attribute every viewer can read, even one that drops the',
        '/// stylesheet. On the monitor page the stylesheet repaints the tiers from the',
        '/// live tokens (and so follows the theme); the standalone image restyles',
        '/// itself for dark mode with its own `<style>`.',
        f'const TIER_LIGHT: [&str; 5] = [{tiers("light")}];',
        f'const TIER_DARK: [&str; 5] = [{tiers("dark")}];',
        "/// The standalone image's card (`--surface`) and its labels (`--ink-muted`).",
    ]
    for const, name in (('SURFACE', 'surface'), ('INK_MUTED', 'ink-muted')):
        for theme in ('light', 'dark'):
            out.append(f'const {const}_{theme.upper()}: &str = "{tokens.resolve(name, theme)}";')
    return out


def layout_rs(tokens):
    # Upper case, as the metas have always carried it (and the tests read it).
    return ["/// The page's ground in each theme (`--ground`), for `theme-color`."] + [
        f'const GROUND_{theme.upper()}: &str = "{tokens.resolve("ground", theme).upper()}";'
        for theme in ('light', 'dark')]


# --- consumers ----------------------------------------------------------------------

CONSUMERS = [
    ('crates/hora-web/assets/hora.css', lambda t: root_block(t, 'product')),
    ('brand/src/common.py', lambda t: root_block(t, 'brand')),
    ('docs/src/styles/custom.css', docs_css),
    ('docs/src/code-themes.mjs', code_themes),
    ('brand/brand.md', docs_table),
    ('crates/hora-web/src/heatmap.rs', heatmap_rs),
    ('crates/hora-web/src/layout.rs', layout_rs),
]


def rewrite(path, text, body):
    """`text` with the lines between its two markers replaced by `body`."""
    lines = text.split('\n')
    begin = [i for i, line in enumerate(lines) if BEGIN in line]
    end = [i for i, line in enumerate(lines) if END in line]
    if len(begin) != 1 or len(end) != 1 or end[0] < begin[0]:
        raise SystemExit(f'{path}: expected one `{BEGIN}` line followed by one `{END}` line')
    return '\n'.join(lines[:begin[0] + 1] + body + lines[end[0]:])


def main(argv):
    if argv not in ([], ['--check']):
        raise SystemExit(__doc__.strip())
    check = bool(argv)
    tokens = Tokens()
    stale = 0
    for path, render in CONSUMERS:
        full = os.path.join(ROOT, path)
        with open(full, encoding='utf-8', newline='') as f:
            before = f.read()
        after = rewrite(path, before, render(tokens))
        if after == before:
            continue
        stale += 1
        if check:
            sys.stdout.writelines(difflib.unified_diff(
                before.splitlines(keepends=True), after.splitlines(keepends=True),
                path, f'{path} (from {SOURCE})'))
        else:
            with open(full, 'w', encoding='utf-8', newline='') as f:
                f.write(after)
            print('wrote', path)
    if check and stale:
        print(f'\n{stale} file(s) out of date with {SOURCE}: run `python3 brand/src/tokens.py` (or `just tokens`)',
              file=sys.stderr)
        return 1
    if not stale:
        print(f'tokens: {len(CONSUMERS)} consumers up to date with {SOURCE}')
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv[1:]))
