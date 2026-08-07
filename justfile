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
# `otel-testing` adds the SDK in-memory exporter the histogram-bucket test reads back
# from. It is a superset of `otel`, so this covers the shipped configuration too.
test-otel:
    cargo test --features otel-testing

# The otel feature with the TLS backend left out, which is what a build that cannot
# have a C dependency gets. `aws-lc-rs` compiles native code, so this configuration is
# the one that has to keep working with no C compiler or assembler present.
check-otel-no-tls:
    cargo clippy --all-targets --no-default-features --features otel -- -D warnings
    cargo build --no-default-features --features otel
    cargo test --no-default-features --features otel
    ./scripts/no-c-deps.sh

# Every configuration the crate ships in. This is what the pre-push hook runs.
check-all: check check-otel check-otel-no-tls

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
