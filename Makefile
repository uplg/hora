# Local quality gate — mirrors .github/workflows/ci.yml so you can run the exact
# same checks before pushing. `make gate` must be green for CI to pass.
.PHONY: gate fmt clippy deny test fix

# The full gate: formatting, lints, license/advisory/ban checks, and tests.
gate: fmt clippy deny test

fmt:
	cargo fmt --all -- --check

clippy:
	cargo clippy --workspace --all-targets --locked -- -D warnings

deny:
	cargo deny check

test:
	cargo test --workspace --locked

# Auto-fix what can be fixed (formatting + machine-applicable clippy suggestions).
fix:
	cargo fmt --all
	cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged
