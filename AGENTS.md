# Agent instructions - adelie-telemetry

Shared standards live in [AGENTS.base.md](AGENTS.base.md), which is generated. This file holds the rules specific to this repo.

`adelie-telemetry` holds the telemetry setup that every Adelie Rust binary shares: one
`tracing_subscriber` stack, the three OTLP pipelines (traces, metrics, log records), an
in-process metrics registry, a shutdown guard, and trace-context helpers. It depends on no
other Adelie crate, and it must stay that way: any binary has to be able to take it without
taking anything else. It knows nothing about turns, tools, models or providers - that
vocabulary belongs to the binaries that emit it.

The crate never installs itself. No constructor runs on load, and no library calls `init`.
A binary calls `init` or nothing happens.

Warnings are denied mechanically - `[lints] rust.warnings = "deny"` and `clippy.all = "deny"`
in `Cargo.toml` - so `cargo build` / `test` / `clippy` hard-fail on any warning.

## The three configurations

The crate ships in three shapes, and all of them are load-bearing.

- **Default features.** No opentelemetry crate is resolved at all. Console logging works,
  the metrics registry accumulates and dumps, and the trace-context helpers return real
  ids. This is what a desktop install from `cargo install` gets.
- **`--features otel`.** The OTLP layers are added *beside* the console layer, never in
  place of it, so an exporting build still prints locally. TLS comes with it, from the
  default features.
- **`--no-default-features --features otel`.** Export without a TLS backend, and so without
  a crate that compiles native code. This is for a build that cannot host a C toolchain.
  It loses `https` and nothing else.

Every change is verified in all three. A new optional dependency stays behind the `otel`
feature; a dependency that a default build resolves is a change to what the whole fleet
compiles, and needs to be justified as such. A dependency that compiles native code must
stay out of the first and third, which `scripts/no-c-deps.sh` enforces.

Each transport needs a TLS provider *and* trust anchors, and they are separate Cargo
features that do not imply each other. Trust anchors alone compile cleanly and then refuse
every `https` endpoint at run time, so a change to the TLS features is checked by running
`grpc_over_tls_reaches_a_tls_handshake`, not by reading the manifest.

## Rust conventions

The fleet conventions in [AGENTS.base.md](AGENTS.base.md) apply. The points that bite
hardest here:

- Never write a diagnostic to stdout, and never let a layer default to it. The MCP stdio
  transport frames JSON-RPC on stdout, and one stray line corrupts the protocol stream.
- `?` for error propagation. Reserve `unwrap` / `expect` for tests and proven invariants,
  and make the message explain the invariant.
- Model data with types, not loose maps. A metrics summary is a struct.
- Inject the clock. Nothing in this crate reads wall time directly, so every interval and
  every duration is deterministic under test.
- Doc comments (`///`) on every public item, with a `Why:` line where the choice is not
  obvious.

## Overrides and additions to the shared base

Everything in [AGENTS.base.md](AGENTS.base.md) applies. This section records only the
points where this repo deliberately differs, or adds a rule the base does not have.

### 3.1 The gate for this repo (addition)

The `adelie-ai` repos have no CI. The gate is local and the author runs it:

- `just check` - format, clippy, build and test with default features, plus a scripted
  check that no opentelemetry crate is resolved.
- `just check-otel` - clippy, build and test with the `otel` feature and TLS.
- `just check-otel-no-tls` - the same without the TLS backend, plus a scripted check that
  no crate compiling native code is resolved.
- `just check-all` - all three. This is what the pre-push hook runs; wire it with
  `just install-hooks`.

The `[lints]` table denies warnings mechanically as well, so a plain `cargo build` or
`cargo test` also hard-fails on one.

The base gate also names a coverage floor and a secret scan. No repo in this fleet runs
either, and adding them here alone would put this repo's gate out of step with the other
seventeen crates for no gain that a reviewer sees. This repo matches the fleet gate
instead. Restoring the floor and the scan is a fleet-wide change, made in every repo at
once or not at all.

### 4.3 Branch and pull request - merge when green (override, weaker than the base)

The base opens a pull request and waits for the user. In these repos the merge is delegated:
merge your own pull request as soon as it is green and independently shippable. Green here
means more than a clean build. The gate above passed in both configurations, the tests cover
the new behavior and not only the absence of a panic, the security pass is done, and the
change stands on its own. Assign `dspadea` with `gh pr edit --add-assignee` and verify it;
a review request from the same account no-ops without an error, so never report a pull
request as review-requested. When in doubt, hold.

### 4.4 Worktrees - the group convention (addition)

Put the worktree at `.worktrees/adelie-telemetry/issue-N-slug/` under the group directory,
on a branch that mirrors the slug. Use absolute paths when creating it; a relative path
nests one worktree inside another.

### 6.1 Dependencies - the group's scan workflow (addition)

Base rule 6.1 sets the policy, including that a high or critical advisory blocks the change.
This group runs it with its own tooling:

1. Add the dependency (`cargo add <crate>`). This writes the lockfile but does not build.
2. Scan the updated lockfile with the `cve-mcp` server's `scan_packages` tool, or with
   `cargo audit`. Pass every (name, version, ecosystem) tuple.
3. Build only after the scan is clean, or after you have accepted the findings in writing.

The opentelemetry `0.x` crates version-lock as a set. Bump them together, never one at a
time, and re-run the gate in both configurations afterwards.

### 9.1 Tracker for this project

GitHub Issues on `github.com/adelie-ai/adelie-telemetry`, together with the shared
`adelie-ai` project board `Adelie AI Roadmap` (project number 1). Manage entries with the
`gh` CLI (`gh issue create`, `gh issue list`, `gh issue edit`, `gh pr create`). Put a new
issue on the board with `gh project item-add 1 --owner adelie-ai --url <issue-url>`, which
lands it in Todo. The board states are Todo, In Progress, and Done.

Work that changes how the fleet consumes this crate is tracked where the consumers are, so
check the consuming repo's tracker before opening a duplicate here.

### This repo is public (addition)

Nothing machine-specific or personal goes into anything committed: no internal hostnames,
IP addresses, absolute home paths, usernames or email addresses, in code, docs, comments,
commit messages or pull request bodies. Use `example.com`, `192.0.2.0/24` and `$HOME` style
placeholders. Collector endpoints in documentation are `localhost` or `example.com`, never
a real one.
