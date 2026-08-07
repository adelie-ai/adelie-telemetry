# adelie-telemetry

One telemetry setup for every Adelie Rust binary: traces, metrics and logs, configured the
same way everywhere.

## Purpose

Every Adelie binary must produce the same diagnostics, with the same knobs, so an operator
can take one identifier from a user report and follow that turn through every process that
touched it. This crate holds that setup once. It depends on no other Adelie crate, so any
binary can take it without taking anything else.

Console output is the default and needs no collector. Export to an OpenTelemetry collector
is additional, not a replacement, and is available behind an off-by-default Cargo feature.

### What it owns

- Subscriber construction. One `tracing_subscriber` stack, built the same way for every
  binary.
- The three OTLP pipelines: traces, metrics and log records.
- The metrics facade and the in-process registry behind it.
- The shutdown guard that flushes the pipelines before the process exits.
- Trace-context helpers: a trace id derived from a request id, and `traceparent`
  inject and extract.

### What it refuses

- Deciding what to instrument. The call sites choose their spans, their events and their
  instruments. This crate names none of them.
- Owning any domain vocabulary. It knows nothing about turns, tools, models or providers.
  Those names live in the binaries that emit them.
- Installing itself. No constructor runs on load and no library calls `init`. A binary
  calls `init` or nothing happens.

Anything outside that list belongs to the binary that needs it.

## Use

```toml
[dependencies]
adelie-telemetry = { git = "https://github.com/adelie-ai/adelie-telemetry" }
```

### Passing the feature through

A crate that is itself a dependency must re-export the feature, or the binary at the top
has no way to turn export on. Without this line the build still succeeds and the process
exports nothing, which is the failure that is hardest to notice.

```toml
[features]
otel = ["adelie-telemetry/otel"]
```

Every crate on the path needs it, so an MCP server reaches the crate through two hops:

```toml
# some-mcp/Cargo.toml
[features]
otel = ["mcp-core/otel"]

# mcp-core/Cargo.toml
[features]
otel = ["adelie-telemetry/otel"]
```

Then `cargo build --features otel` on the server turns all three on. A leaf binary that
depends on this crate directly writes `features = ["otel"]` on the dependency instead.

### Replacing an existing subscriber

The usual starting point:

```rust
tracing_subscriber::fmt()
    .with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
    )
    .init();
```

becomes:

```rust
let _guard = adelie_telemetry::init(adelie_telemetry::Config::new("adele-daemon"))?;
```

Four things to check while replacing one:

1. **Bind the guard.** `let _guard = ...`, not `let _ = ...`. A `_` binding drops it
   immediately and the process exits without flushing. Keep it alive for all of `main`.
2. **Keep the old default filter.** A binary that was quiet unless asked passes its old
   fallback to `Config::with_default_filter`, or it starts logging at `info` where it used
   to say nothing.
3. **Drop any `with_writer` pointing at stdout.** This crate always writes to stderr.
4. **Remove `.init()` calls in libraries.** Only the binary installs a subscriber. A second
   call is a no-op, so a stale one hides itself rather than failing.

```rust
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = adelie_telemetry::init(adelie_telemetry::Config::new("adele-daemon"))?;
    tracing::info!("started");
    Ok(())
}
```

Hold the guard for as long as the process should report. Dropping it flushes.

A library must never call `init`. The binary owns the subscriber. A second call in one
process is a no-op that returns an inert guard, so a library hosted in another binary
cannot break it by trying.

## Logging

### Where it goes

**stderr, always.** Never stdout. The MCP stdio transport frames JSON-RPC on stdout, so one
log line there corrupts the protocol stream, and `adele-tui` writes the model reply there
in `--prompt` mode.

Console output is plain text, not JSON. With a collector doing the real collection, stderr
is for a person reading `kubectl logs` or `journalctl`, and plain text is easier to read.
Colour is off, so escape codes do not end up in a log file.

### How much of it

`RUST_LOG` sets the filter, through `EnvFilter`. When it is unset or unparseable, the
config's default filter applies, which is `info` unless the binary chose otherwise.

```sh
RUST_LOG=debug ./adele-daemon
RUST_LOG=info,adelie_telemetry=warn ./adele-daemon
```

One filter governs the console and the OTLP log exporter together. An operator who turns
the verbosity up expects the same lines wherever they read them, and a second filter would
mean two answers to "why is this line missing".

### What may appear at each level

This contract is a rule, not a preference.

| Level | Carries |
|---|---|
| INFO | ids, counts, durations, model names, token counts. **Never content.** |
| DEBUG | prompts, the assembled context, tool arguments. |

`RUST_LOG=debug` therefore means conversation content reaches the collector. That is
deliberate, and it is the reason the default is `info`.

### Span timing

`Config::with_span_close_events(true)` writes a line when a span closes, carrying how long
it was open:

```text
INFO turn{turn_id=4bf92f35...}: close time.busy=208µs time.idle=14.9ms
```

This is what makes turn timing visible to somebody reading a running container's log, where
there is no trace backend to open. It is noisy under a debug filter, so it is off by
default and each binary chooses.

## Metrics

Call sites use the facade and nothing else:

```rust
use adelie_telemetry::metrics::{self, Label};
use std::time::Duration;

metrics::increment("llm.requests", &[Label::new("provider", "example")]);
metrics::add("llm.tokens.input", 1_234, &[Label::new("model", "example-model")]);
metrics::record_duration("llm.latency", Duration::from_millis(320), &[]);
```

They never reach for an opentelemetry meter directly. That would make every crate that
records a metric depend on opentelemetry whether or not the feature is on.

### The registry runs with or without a collector

With the `otel` feature off the facade does not no-op. It keeps counters and fixed-bucket
histograms in process and writes a summary periodically, so metrics behave the way logs and
traces already do: local by default, exported additionally. A desktop install running a
default-feature build from `cargo install` gets real numbers in its journal.

The summary keeps running when a collector *is* configured, and the two paths report over
the same bucket boundaries, so the local dump cross-checks the exported one.

`Config::with_metrics_dump_interval` sets how often. The default is 10 minutes.
`Duration::ZERO` turns the summary off; the registry still accumulates.

Each summary reports the window that just closed beside a running total:

```text
INFO metrics summary window_seconds=600 uptime_seconds=8400 counters=4 histograms=2
INFO counter metric="llm.requests" labels=provider=example window=41 total=612
INFO duration metric="llm.latency" labels= window_count=41 window_p95_ms=2500 total_count=612 total_p95_ms=5000
```

On a pod that has run for a month, a cumulative number is dominated by history and stops
moving, so a fault that started an hour ago is invisible in it. The window shows what is
happening now.

### Buckets, not means

Durations go into fixed-bucket histograms, from 1 ms to 5 minutes. A mean hides the tail:
"the average turn took 3 seconds" and "one turn in twenty took four minutes" are the same
mean, and only the second one is the report a user files. The same boundaries feed the OTLP
view, so both paths agree about which bucket a measurement fell in.

### Cardinality

One metric may have 64 distinct label sets by default. Past that, further label sets fold
into one series labelled `cardinality=other`, and the registry stops growing. Measurements
are still counted; only the labels are lost. A label taken from a model, tool or provider
name is config-controlled in practice, and an unbounded label set is an unbounded memory
leak in a process that runs for weeks.

`Config::with_cardinality_cap` changes the limit.

**Label values are names, not content.** A prompt or a tool argument used as a label would
be both a data leak and a memory leak. The cap limits the damage; it is not permission.

## Trace context

The helpers are free functions. They need no `Config`, no `init` and no `Guard`, and they
work with the `otel` feature off, so a desktop client that exports nothing can still mint
the id a daemon adopts.

```rust
use adelie_telemetry::trace_context::{self, TraceOrigin};

// A uuid is 16 bytes and a W3C trace id is 16 bytes, so a request id becomes the trace
// id directly. Pass `uuid.into_bytes()`.
let request_id = [7u8; 16];

match trace_context::resolve_trace(incoming_traceparent, request_id)? {
    TraceOrigin::Continued(parent) => { /* join the caller's trace */ }
    TraceOrigin::Minted(trace_id)  => { /* this process is the root */ }
}
# Ok::<(), adelie_telemetry::TraceContextError>(())
```

An incoming `traceparent` always wins. A malformed one is an error rather than a silent
fall back to minting, because minting would split one turn across two traces without
saying so.

`TraceParent::root_for` builds a header for a process that has no spans of its own, which
is what a client built without the `otel` feature needs in order to start a trace at all.

The all-zero trace id and span id are the spec's "invalid" sentinels and are rejected.

## OpenTelemetry export

Off by default:

```toml
adelie-telemetry = { git = "...", features = ["otel"] }
```

With the feature off, no opentelemetry crate is resolved at all. With it on, the OTLP
layers are added beside the console layer rather than in place of it, so an exporting build
still prints locally.

### Configuration

Everything comes from the standard `OTEL_*` environment variables. There are no CLI flags
and no Adelie-specific variables. This crate passes nothing to the exporter builders, so
every variable below reaches them.

| Variable | Effect |
|---|---|
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Endpoint for all three signals. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | Endpoint for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_ENDPOINT` | Endpoint for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | Endpoint for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_PROTOCOL` | `grpc`, `http/protobuf` or `http/json`, for all three. |
| `OTEL_EXPORTER_OTLP_TRACES_PROTOCOL` | Protocol for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_PROTOCOL` | Protocol for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_PROTOCOL` | Protocol for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_HEADERS` | Headers for all three, as `key=value,key=value`. |
| `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | Headers for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_HEADERS` | Headers for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_HEADERS` | Headers for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | Export timeout in milliseconds, for all three. |
| `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` | Timeout for traces. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_METRICS_TIMEOUT` | Timeout for metrics. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | Timeout for log records. Overrides the generic one. |
| `OTEL_EXPORTER_OTLP_COMPRESSION` | `gzip` or `zstd`, for all three. Per-signal forms exist too. |
| `OTEL_EXPORTER_OTLP_METRICS_TEMPORALITY_PREFERENCE` | Metric temporality. |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, as `key=value,key=value`. |

A per-signal variable beats the generic one. The generic endpoint has the signal's path
appended to it (`/v1/traces` and so on); a per-signal endpoint is used exactly as written,
so it must include the path.

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.example.com:4318 \
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
  ./adele-daemon
```

### Choosing a transport

Both transports are compiled in, and `OTEL_EXPORTER_OTLP_PROTOCOL` selects one at run time.

**`http/protobuf` (port 4318) is the safer default.** It uses a blocking HTTP client on the
exporter's own thread and needs nothing from the process.

**`grpc` (port 4317) needs a Tokio runtime.** The exporter's transport calls into a reactor,
so `init` must be reached from inside a running runtime. Every Adelie daemon is a Tokio
binary, so this holds in practice, but a small tool that calls `init` before starting a
runtime must use the HTTP transport.

Both transports support `https`. TLS is on by default because telemetry carries log lines,
and a log line leaving the cluster in plaintext is a disclosure. Turning it off is
deliberate, not accidental.

### The two transports trust different certificate stores

They do not share a TLS stack, and the difference decides what a container image needs.

| transport | trust anchors | consequence |
|---|---|---|
| `grpc` | compiled-in webpki roots | independent of the image; works in a `FROM scratch` container |
| `http/protobuf` | the OS trust store, through `rustls-platform-verifier` | the image needs `ca-certificates`, or every HTTPS export fails |

Two things follow, and neither is obvious:

- **A container with no `ca-certificates` package has an empty OS trust store.** HTTPS over
  `http/protobuf` fails there while the same endpoint over `grpc` succeeds. Install
  `ca-certificates` in any image that exports over HTTPS, or use gRPC.
- **A private certificate authority already installed on the host works over
  `http/protobuf`**, because that path reads the OS store. It does not work over `grpc`,
  which would need the `tls-roots` feature to read the system roots instead of the
  compiled-in ones.

### The C toolchain, and opting out of it

The TLS backend builds `aws-lc-rs`, which compiles native code. That is the one part of
this crate with a build prerequisite beyond `cargo`.

What it needs is a **C compiler and an assembler** - `build-essential` on Debian, or
`gcc` plus `binutils` elsewhere. It does **not** need `cmake`: `aws-lc-sys` falls back to a
cc-only build path when cmake is absent, and still produces its static library. Verified by
putting a `cmake` that exits 127 first on `PATH` and building `--features otel` from clean,
which succeeded. Asking for `cmake` in a builder image makes the requirement look heavier
than it is.

A build that cannot have a C dependency opts out:

```toml
[dependencies]
adelie-telemetry = { git = "...", default-features = false }
```

What each configuration costs, counted with `cargo tree --edges normal`:

| configuration | crates | native code |
|---|---|---|
| default features | 25 | none |
| `--features otel` | 134 | `aws-lc-rs` |
| `--no-default-features --features otel` | 119 | none |

A release build pays in size and time too. The `otlp_probe` example, built with
`--release` from clean on a 36-thread machine:

| configuration | binary | build |
|---|---|---|
| default features | 1.3 MiB | 12s |
| `--no-default-features --features otel` | 4.7 MiB | 34s |
| `--features otel` | 8.5 MiB | 51s |

Most of that is OTLP itself, not TLS: turning export on costs 1.3 -> 4.7 MiB, and TLS adds
4.7 -> 8.5 MiB on top. A CI builder with 2 to 4 cores will be several times slower than
these times.

A default build is unaffected either way: with `otel` off there is no OTLP crate for the
TLS feature to apply to, so it does nothing. A desktop install from `cargo install` needs
no C toolchain.

Opting out costs exactly one thing: an `https` endpoint stops working. It is refused at
`init` by name, so it fails as a configuration error rather than as a network fault:

```text
ERROR telemetry export is off for this process; console logging and the metrics summary
are unaffected error=could not build the OTLP traces pipeline: OTEL_EXPORTER_OTLP_ENDPOINT
uses https, and this build has no TLS backend compiled in. Something took
`default-features = false` on adelie-telemetry, and the TLS backend is one of those
defaults. Use an http endpoint, or restore the default features
```

Plaintext export is unaffected, so an in-cluster collector on the node-local network works
in either configuration.

### Which pipeline owns an event

`tracing-opentelemetry` turns an event inside a span into a span event, and
`opentelemetry-appender-tracing` exports the same event as a log record. Left alone, both
happen and every event is counted twice.

**The log pipeline wins.** The trace layer is filtered down to spans, so an event reaches
the backend exactly once, as a log record. The log record is the complete one:
`tracing-opentelemetry` silently drops any event with no span open around it, so a pipeline
built on span events would lose every event emitted outside a span without saying so.

The one exception is `ERROR`. The trace layer sets a span's status to failed when it sees an
error event, and that status is what stops a failed turn looking green in a trace view.
Error events therefore reach both, and are the only events that appear twice.

### When a pipeline cannot be built

A wrong value in the environment costs the process its export and nothing else. The console
layer is installed either way, the metrics summary keeps running, and the reason is written
at ERROR together with the `OTEL_*` variables that were set. Header values are never
printed, because they routinely carry an API key.

A binary that would rather not start at all can read the same condition from its own log
and exit; `init` itself returns `Ok` so that a typo cannot silence a process.

### Shutdown

`init` returns a `Guard`. Dropping it flushes and shuts down traces, metrics and logs, in
that order, and writes one final metrics summary. The batch exporters buffer, and a process
that exits without a flush loses whatever was still in the buffer, which is usually the part
worth having, because a crash is what was being investigated.

Shutdown is bounded. Each provider can block for about five seconds against an unreachable
collector, and there are six calls, so an unbounded drop can run for thirty seconds.
`Config::with_shutdown_budget` caps the total, and the default is five seconds.

**Kubernetes:** set `terminationGracePeriodSeconds` to at least 30 in any deployment that
runs with `otel` on. The default is 30, and the pod needs room to flush telemetry *and*
finish whatever else it was doing before SIGKILL arrives.

## Development

```sh
just check       # format, clippy, build, test, and the no-opentelemetry check
just check-otel  # the same build and tests with the otel feature on
just check-all   # both. This is what the pre-push hook runs.
just install-hooks
```

The crate ships in three configurations and the gate covers all three:

- `check` - default features. No opentelemetry crate is resolved at all.
- `check-otel` - export on, TLS on. Built with `otel-testing`, which adds the SDK's
  in-memory exporter so a test can read back what the OTLP path really produced. It is a
  superset of `otel`, so the shipped configuration is covered by the same run.
- `check-otel-no-tls` - export on, TLS off. This is what a consumer that took
  `default-features = false` gets, and `scripts/no-c-deps.sh` holds the line that it
  resolves no crate which compiles native code.

Both configurations are part of the gate. A change that compiles with default features can
still fail with `otel` on.

### Checking against a real collector

```sh
podman run -d --name otelcol -p 4317:4317 -p 4318:4318 \
  -v ./collector.yaml:/etc/otelcol/config.yaml:z \
  docker.io/otel/opentelemetry-collector-contrib:0.144.0 --config /etc/otelcol/config.yaml

OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318 \
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
  cargo run --features otel --example otlp_probe

podman logs otelcol
```

The probe emits two nested spans, three metrics and four log records. Set
`PROBE_RUNTIME=tokio` to run it inside a Tokio runtime, which the gRPC transport needs.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
