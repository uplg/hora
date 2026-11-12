# Local quality gate — mirrors .github/workflows/ci.yml so you can run the exact
# same checks before pushing. `just gate` must be green for CI to pass.

# The full gate: formatting, lints, license/advisory/ban checks, vuln audit, tests,
# and the design tokens in step with their source.
gate: fmt clippy deny audit test tokens-check

fmt:
    cargo fmt --all -- --check

clippy:
    cargo clippy --workspace --all-targets --locked -- -D warnings

deny:
    cargo deny check

audit:
    cargo audit

test:
    cargo test --workspace --locked

# Rewrite every copy of the design tokens (stylesheet, brand gallery, docs theme,
# Rust constants) from brand/tokens.json, the one place to edit them.
tokens:
    python3 brand/src/tokens.py

# Fail, with a diff, if a copy has drifted from brand/tokens.json.
tokens-check:
    python3 brand/src/tokens.py --check

# Auto-fix what can be fixed (formatting + machine-applicable clippy suggestions).
fix:
    cargo fmt --all
    cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged

# What is behind, on every surface: crates, docs packages, pinned Actions and
# Docker base images. Reporting only - a human decides when to move. Needs
# cargo-edit, bun and an authenticated gh (for the Actions releases).
deps:
    #!/usr/bin/env bash
    set -uo pipefail
    echo "== Crates"
    cargo upgrade --dry-run --incompatible allow --recursive false 2>&1 | grep -v "aborting upgrade due to dry run"
    echo; echo "== Docs (bun)"
    (cd docs && bun outdated)
    echo; echo "== Actions (pinned SHA, version in the trailing comment)"
    grep -ho 'uses: [^ ]*@[0-9a-f]\{40\} # .*' .github/workflows/*.yml | sort -u \
    | while read -r _ ref _ ours; do
        repo=${ref%@*}
        theirs=$(gh api "repos/$repo/releases/latest" --jq .tag_name 2>/dev/null || echo "?")
        [ "$ours" = stable ] && continue
        [ "$ours" = "$theirs" ] && state=ok || state="behind -> $theirs"
        printf '  %-36s %-10s %s\n' "$repo" "$ours" "$state"
    done
    echo; echo "== Bun (docs workflow)"
    ours=$(sed -n 's/^ *bun-version: *\([0-9.]*\).*/\1/p' .github/workflows/docs.yml)
    theirs=$(gh api repos/oven-sh/bun/releases/latest --jq .tag_name 2>/dev/null || echo "?")
    theirs=${theirs#bun-v}
    printf '  %-36s %-10s %s\n' bun "$ours" "$([ "$ours" = "$theirs" ] && echo ok || echo "behind -> $theirs")"
    echo; echo "== Docker base images"
    ours=$(sed -n 's/^FROM alpine:\([0-9.]*\).*/\1/p' Dockerfile)
    theirs=$(curl -fsS "https://registry.hub.docker.com/v2/repositories/library/alpine/tags?page_size=50" \
        | python3 -c 'import json,sys,re; print(max((t["name"] for t in json.load(sys.stdin)["results"] if re.fullmatch(r"\d+\.\d+",t["name"])), key=lambda v: tuple(map(int,v.split(".")))))')
    printf '  %-36s %-10s %s\n' alpine "$ours" "$([ "$ours" = "$theirs" ] && echo ok || echo "behind -> $theirs")"
    echo "  blackdex/rust-musl                   stable     (floating tag, follows Rust stable)"

# Build the image for this machine and smoke-test it, as CI does before a release
image-smoke:
    docker build -t hora:smoke .
    .github/scripts/image-smoke.sh hora:smoke
