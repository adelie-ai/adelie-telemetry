set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# --- Local verification ("local CI") ---
# Run locally instead of GitHub Actions. `install-hooks` wires `check-all` into a
# git pre-push hook so it runs automatically before every push.

# The gate for the default feature set.
check: fmt-check lint build test no-otel-default
fmt-check:
    cargo fmt --check
fmt:
    cargo fmt
lint:
    cargo clippy --all-targets -- -D warnings
build:
    cargo build
test:
    cargo test

# The gate for the `otel` feature set. The crate ships two configurations, so
# both must pass before a push.
check-otel: lint-otel build-otel test-otel
lint-otel:
    cargo clippy --all-targets --features otel -- -D warnings
build-otel:
    cargo build --features otel
test-otel:
    cargo test --features otel

# Both configurations. This is what the pre-push hook runs.
check-all: check check-otel

# A default-feature build must pull in no opentelemetry crate.
no-otel-default:
    ./scripts/no-otel-default.sh

premerge:
    git fetch origin
    git rebase origin/main
    just check-all

install-hooks:
    git config core.hooksPath .githooks
    @echo "pre-push hook active - bypass once with: git push --no-verify"
