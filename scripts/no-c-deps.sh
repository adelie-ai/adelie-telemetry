#!/usr/bin/env bash
# A build that opts out of the TLS backend must have no crate that compiles native
# code. `aws-lc-rs` needs a C compiler and an assembler, and a consumer takes
# `default-features = false` precisely to avoid that, so a crate sneaking back in
# would silently break the reason the option exists.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

for config in "" "--no-default-features --features otel"; do
    # shellcheck disable=SC2086
    tree="$(cargo tree --edges normal --prefix none $config)"
    native="$(awk 'NF{print $1}' <<<"$tree" | sort -u \
        | grep -E -- '-sys$|^ring$|^aws-lc-rs$|^openssl$' || true)"
    if [ -n "$native" ]; then
        echo "FAIL: '${config:-default features}' resolves crates that compile native code:" >&2
        echo "$native" >&2
        exit 1
    fi
done

echo "ok: no native-code dependency in a default build or a no-TLS otel build"
