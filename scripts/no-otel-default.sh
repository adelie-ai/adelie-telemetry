#!/usr/bin/env bash
# A default-feature build must pull in no opentelemetry crate. The `otel` feature
# is the only thing that adds them, and consumers that never turn it on must not
# pay for it.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

tree="$(cargo tree --edges normal --prefix none)"

if grep -qi '^opentelemetry' <<<"$tree"; then
    echo "FAIL: a default-feature build resolves opentelemetry crates:" >&2
    grep -i '^opentelemetry' <<<"$tree" | sort -u >&2
    exit 1
fi

echo "ok: no opentelemetry crate in a default-feature build"
