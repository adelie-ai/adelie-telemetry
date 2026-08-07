#!/usr/bin/env bash
# Both transports must verify against the operating system trust store, and neither
# may bundle a root set into the binary.
#
# Why it is enforced rather than documented: a bundled root set cannot see a CA that
# an administrator installed on the host, which is what a private CA in a lab or an
# enterprise is, and it goes stale against public CA rotation with no fix but an
# upstream release plus a rebuild. Reintroducing one is a silent change in which
# certificates a process accepts.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

tree="$(cargo tree --edges normal --prefix none --features otel)"
bundled="$(awk 'NF{print $1}' <<<"$tree" | sort -u | grep -E '^webpki-roots$|^webpki-root-certs$' || true)"

if [ -n "$bundled" ]; then
    echo "FAIL: --features otel resolves a bundled root set:" >&2
    echo "$bundled" >&2
    echo >&2
    echo "Both transports are meant to read the OS trust store. Check that otel-tls uses" >&2
    echo "opentelemetry-otlp's 'tls-roots' and not 'tls-webpki-roots'." >&2
    exit 1
fi

echo "ok: no bundled certificate root set; both transports read the OS trust store"
