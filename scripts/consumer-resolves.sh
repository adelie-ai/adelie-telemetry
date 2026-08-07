#!/usr/bin/env bash
# This crate must impose no version ceiling on a consumer that has not opted into
# export.
#
# That is the invariant, and it is stronger than "a consumer can resolve". Cargo does
# not fail when we ask for a dependency feature that no longer exists: it walks back
# through versions until it finds one that still has the feature, and resolves happily.
# The consumer is then capped at that old version, sees an unexplained downgrade in its
# lockfile, and hits a hard resolution failure the day anything else in its graph needs
# something newer.
#
# Two details make it worse than it sounds, and both are why this check exists:
#
#   - Our own Cargo.lock does not constrain a dependent, so it hides the cap here
#     while every consumer resolving fresh is subject to it.
#   - A feature in "default" takes part in version selection whether or not the
#     consumer enables "otel", so the cap reaches consumers that compile no
#     opentelemetry code at all.
#
# The compiled output is a separate question and is fine: a default build still
# compiles zero opentelemetry crates, which no-otel-default.sh checks. This script is
# about version *selection*, not about what ends up in the binary.
#
# So this builds a throwaway consumer outside the repo, points it at the working tree,
# does NOT enable "otel", and asks for a dependency that the cap is known to reach.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

mkdir -p "$work/src"
echo 'fn main() {}' > "$work/src/main.rs"

fail() {
    echo "FAIL: $1" >&2
    echo >&2
    echo "This crate is capping a dependency for its consumers. The usual cause is a" >&2
    echo "feature we enable on an optional dependency that upstream has removed: cargo" >&2
    echo "silently backtracks to the last version that still had it, and every consumer" >&2
    echo "inherits the cap. Our own Cargo.lock hides this, so it cannot be seen from" >&2
    echo "inside the repo." >&2
    exit 1
}

fail_probe() {
    echo "FAIL: $1" >&2
    echo >&2
    echo "The probe manifest this script generates is not what it should be, so any" >&2
    echo "result from it would be meaningless. Fix the generator before trusting a pass." >&2
    exit 1
}

# --- Generate the probe manifest -------------------------------------------------
#
# Every heredoc here is quoted, so the shell interprets nothing inside it. That is
# deliberate and it is not style: an unquoted heredoc runs anything in backticks or
# $( ) as a command while writing the file. This script shipped that way once, and the
# backticks in the comment below were executed and silently dropped from the output.
# The one value that has to vary is written with printf instead of expanded inline.

probe_errors="$work/probe.err"
{
    cat <<'PROBE_HEAD'
[package]
name = "consumer-resolution-probe"
version = "0.0.0"
edition = "2024"

# Kept out of any workspace, so the repo's own settings cannot influence the result.
[workspace]

[dependencies]
# Deliberately without `features = ["otel"]`. A consumer that never turns export on
# must still be free to take current versions.
PROBE_HEAD

    printf 'adelie-telemetry = { path = "%s" }\n' "$repo"

    cat <<'PROBE_TAIL'

# reqwest reaches this crate only through the optional OTLP HTTP exporter, and six of
# the thirteen MCP servers depend on it directly, so it is the dependency a ceiling
# here would be felt through first.
reqwest = "0.13"
PROBE_TAIL
} > "$work/Cargo.toml" 2>"$probe_errors"

# --- Check the generator before trusting the check --------------------------------
#
# A checker whose own output nobody reads is worth as little as a test that cannot
# fail. These three assertions are what would have caught the unquoted heredoc.

if [ -s "$probe_errors" ]; then
    echo "  generator wrote to stderr:" >&2
    sed 's/^/    /' "$probe_errors" >&2
    fail_probe "writing the probe manifest produced errors"
fi

if ! grep -qF 'features = ["otel"]' "$work/Cargo.toml"; then
    fail_probe "the explanatory comment was mangled, so the shell interpreted the manifest"
fi

if ! grep -qF "adelie-telemetry = { path = \"$repo\" }" "$work/Cargo.toml"; then
    fail_probe "the probe does not point at this working tree"
fi

if grep -E '^adelie-telemetry' "$work/Cargo.toml" | grep -q 'features'; then
    fail_probe "the probe enables a feature, so it no longer tests the no-otel consumer"
fi

cp "$repo/rust-toolchain.toml" "$work/" 2>/dev/null || true

cd "$work"

# 1. A consumer with no lockfile, resolving from scratch.
cargo generate-lockfile 2>&1 | tee "$work/out" | sed 's/^/  /'
if grep -q '^error' "$work/out"; then
    fail "a fresh consumer cannot resolve at all"
fi

# 2. Nothing may be held below its newest compatible version.
#
#    This is the check that matters, and the one a plain resolve does not make. When a
#    feature we enable no longer exists upstream, Cargo does not fail: it backtracks to
#    the last version that still had the feature and resolves happily, reporting the
#    hold-back only as a note. The consumer is then silently pinned, and only discovers
#    it when something else in its graph needs the newer version.
if grep -q '(available:' "$work/out"; then
    echo >&2
    grep '(available:' "$work/out" >&2
    fail "a consumer is held below the newest compatible version of a dependency"
fi

# 3. The same consumer after `cargo update`, which is what moves a project onto
#    versions our own lockfile never saw.
cargo update 2>&1 | tee "$work/out" | sed 's/^/  /'
if grep -q '^error' "$work/out"; then
    fail "a consumer cannot take newer dependency versions"
fi
if grep -q '(available:' "$work/out"; then
    echo >&2
    grep '(available:' "$work/out" >&2
    fail "a consumer cannot move to the newest compatible version of a dependency"
fi

echo "ok: a consumer that has not enabled otel is capped on nothing, and can still take"
echo "    the newest compatible version of every dependency after cargo update"
