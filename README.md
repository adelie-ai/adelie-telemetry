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

### Testing against it

Two pieces of state in this crate are process-global, and a test binary runs its tests in
parallel threads of one process. Both bite the same way.

**The registry.** `metrics::global()` is shared by every test in a binary, so two tests
that record the same instrument interfere and an assertion on an exact count fails about
half the time. Hold a `TestScope`. It gives the thread a registry of its own, and the
ordinary facade functions record into that one instead:

```rust
use adelie_telemetry::metrics::{self, Label, TestScope};

#[test]
fn a_failed_call_is_counted() {
    let scope = TestScope::new();

    my_crate::call_the_model();            // records through metrics::increment

    let summary = scope.snapshot();
    assert_eq!(summary.counters[0].total, 1);
}
```

Bind it - `let _ = TestScope::new()` drops it at once and records nothing. Every test doing
this runs in parallel with every other, and no test needs a mutex or a binary to itself.

`TestScope::with_settings` takes a `Settings` and a clock, so a test can drive a window
with a `ManualClock` instead of waiting ten minutes, and can exercise the cardinality cap
at a lower limit.

Two limits worth knowing. A scope covers **the thread that holds it**: code under test
that records from a thread it spawned itself, or from a multi-threaded async runtime,
still records into the process registry. A plain `#[test]` and a current-thread
`#[tokio::test]` both stay on one thread. And a scope does not change the OTLP bridge; a
measurement taken inside one still reaches whatever meter provider is installed, which in
a test is the no-op one.

**The environment.** The `OTEL_*` variables are worse. `std::env::set_var` is `unsafe` in
edition 2024 because `setenv` rewrites a shared array while any other thread may be reading
it - so two tests that set *different* variables still race, and a comment claiming a test
owns its variable is not a sound basis for the `unsafe` block. Do not test by mutating the
environment. Inject the lookup instead, the same way this crate injects the clock, and read
the variables once at the edge:

```rust
fn build(lookup: impl Fn(&str) -> Option<String>) -> Config { /* ... */ }

// production
build(|name| std::env::var(name).ok());

// test - no unsafe, no lock, and every test runs in parallel
build(|name| (name == "OTEL_EXPORTER_OTLP_PROTOCOL").then(|| "grpc".to_owned()));
```

Where a test genuinely must set a real variable - driving a whole process, say - put it in
a child process instead, as `tests/acceptance_resilience.rs` does. A separate process has
its own environment and cannot race this one.

### The registry runs with or without a collector

With the `otel` feature off the facade does not no-op. It keeps counters and fixed-bucket
histograms in process and writes a summary periodically, so metrics behave the way logs and
traces already do: local by default, exported additionally. A desktop install running a
default-feature build from `cargo install` gets real numbers in its journal.

**The summary is off while the OTLP metrics pipeline is running.** Those series are
already exported as metrics, so writing them to the log as well is a second copy of one
set of numbers in a different signal - from every binary in the fleet, every ten minutes,
into the same backend. The summary exists for somebody reading a container log with no
backend attached, which is exactly the case where nothing is exporting metrics.

Startup says which way it resolved, so an operator who expected a summary and does not see
one can read the reason rather than guess:

```text
INFO the metrics summary interval interval_ms=0 reason="the OTLP metrics pipeline exports the same series"
INFO the metrics summary interval interval_ms=600000 reason="no metrics exporter is configured"
```

Highest first, the interval comes from:

1. `Config::with_metrics_dump_interval`, for a binary that must have a particular one.
2. `ADELIE_TELEMETRY_METRICS_SUMMARY_INTERVAL_MS`, in whole milliseconds. `0` turns the
   summary off whatever the pipeline is doing. A value that is not a whole number of
   milliseconds is named at startup and ignored, which leaves the choice where it would
   have been rather than enabling or disabling anything by accident.
3. Whether the OTLP metrics pipeline was built: off if it was, ten minutes if it was not.

The registry keeps accumulating either way. Turning the summary off stops the lines, not
the counting, and the final summary at shutdown goes with them.

Both paths report over the same bucket boundaries, so where both are on the local dump
cross-checks the exported one.

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

### Value histograms - a measurement that is not a duration

`record_duration` always measures a `Duration`, in milliseconds, over the shared duration
buckets above. A different measurement - a per-request token count, a queue depth - needs
its own unit and its own bucket boundaries. `record_value` is the same fixed-bucket shape,
generic over both:

```rust
use adelie_telemetry::metrics::{self, Label};

const TOKEN_BUCKETS: &[f64] = &[0.0, 1_024.0, 8_192.0, 25_000.0, 100_000.0, 1_048_576.0];

metrics::record_value(
    "gen_ai.client.token.usage",
    input_tokens as f64,
    "{token}",
    TOKEN_BUCKETS,
    &[Label::new("gen_ai.token.type", "input")],
);
```

`Registry::snapshot()` reports these in `value_histograms`, alongside the existing counters
and duration histograms, each carrying the unit it was recorded under.

The boundaries above shape the in-process registry on their own - nothing further is
needed for the local dump to use them. The OTLP export is a separate step, because a
bucket boundary is a property of the `SdkMeterProvider`'s `View`s, fixed once at `init`:
register the same boundaries there, keyed by the same unit, or the exported histogram
falls back to the SDK's defaults while the local dump uses yours.

```rust
let config = adelie_telemetry::Config::new("my-binary")
    .with_histogram_view("{token}", TOKEN_BUCKETS);
```

One call per unit a binary records `record_value` under. This crate still owns no domain
vocabulary (see "What it refuses" above): it ships no unit and no boundaries of its own
for anything but the shared duration histogram, so a binary that wants its OTLP export to
agree with its local dump has to say so once, at `init`.

### Cardinality

One metric may have 64 distinct label sets by default. Past that, further label sets fold
into one series labelled `cardinality=other`, and the registry stops growing. Measurements
are still counted; only the labels are lost. A label taken from a model, tool or provider
name is config-controlled in practice, and an unbounded label set is an unbounded memory
leak in a process that runs for weeks.

`Config::with_cardinality_cap` changes the limit.

**Label values are names, not content.** A prompt or a tool argument used as a label would
be both a data leak and a memory leak. The cap limits the damage; it is not permission.

## Putting a caller's value on a log line

A tool name, a model name, an error quoting the input back: anything a caller can influence
goes through `Safe` before it reaches a field.

```rust
use adelie_telemetry::Safe;

tracing::info!(tool = %Safe::name(tool_name), "tool call finished");
tracing::debug!(reason = %Safe::message(detail), "tool returned an error");
```

Without it, three things go wrong, and only the first is obvious:

- A newline ends the log line and starts one that reads as a genuine record, with a real
  timestamp column, level and target.
- An ANSI escape survives. Turning the formatter's own colour off does not strip an escape
  carried inside a value.
- A bidi control reverses what the terminal shows without changing a byte, so the name in
  `kubectl logs` is not the name that was called.

And nothing bounds the length of a caller's value short of the transport's frame cap, which
is measured in megabytes.

### Which constructor

| constructor | cap | for |
|---|---|---|
| `Safe::name` | 128 bytes | a tool, method, model or request id - short by nature |
| `Safe::message` | 1024 bytes | a diagnostic, mostly your own text quoting the caller's |
| `Safe::with_cap` | yours | a value that genuinely fits neither; say why at the call site |

The shape is named rather than the number passed, because that is what stops the caps
drifting apart across eighteen crates. `Safe::name`'s cap is the same limit the metrics
facade puts on a label value, so one name reads the same way whichever signal you look at.

### It costs nothing when nobody is looking

Wrapping a value does no work. Sanitising happens inside `Display`, so a field at a level
nobody enabled costs only the wrapper, and a field that is rendered goes straight into the
formatter with no intermediate `String`.

### It wraps anything, not just strings

`Safe<T>` takes any `Display`. A JSON value implements `Display`, so it needs no second
wrapper and this crate needs no JSON dependency:

```rust
tracing::debug!(arguments = %Safe::message(&json_value), "tool call arguments");
```

### One predicate, one place

`Safe` and `metrics::Label::new` share the predicate. They have to: a value that read one
way on a log line and another in a metrics summary would send an operator looking for a
difference that is not there. `safe_and_label_agree_character_for_character` fails if they
ever diverge, and it is the reason this lives in one crate rather than being copied per
server.

What is stripped: category Cc (C0, C1, DEL), U+2028 and U+2029, and the bidi controls
U+061C, U+200E, U+200F, U+202A-U+202E and U+2066-U+2069. What is not: the rest of Cf,
including the zero-width joiner that carries emoji sequences a person wants to read.

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

Everything comes from the standard `OTEL_*` environment variables. There are no CLI flags.
This crate passes no endpoint, protocol, header or timeout to the exporter builders, so
every variable below reaches them. One variable is this crate's own, and it is named as
such below the table.

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
| `OTEL_SERVICE_NAME` | The service name. **Beats the name the binary passed to `Config::new`.** |
| `OTEL_RESOURCE_ATTRIBUTES` | Extra resource attributes, as `key=value,key=value`. A `service.name` entry in it does **not** beat `Config::new`. |
| `OTEL_SDK_DISABLED` | `true` builds no pipeline at all. Any other value leaves export on. |
| `OTEL_TRACES_EXPORTER` | `none` switches traces off. `otlp` is the default and the only other value. |
| `OTEL_METRICS_EXPORTER` | `none` switches metrics off. `otlp` is the default and the only other value. |
| `OTEL_LOGS_EXPORTER` | `none` switches log records off. `otlp` is the default and the only other value. |

Two more variables are **not** `OTEL_*` ones, and are deliberately outside that namespace:

| Variable | Effect |
|---|---|
| `ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS` | How long the guard may spend flushing, in whole milliseconds. `0` means do not wait at all. |
| `ADELIE_TELEMETRY_METRICS_SUMMARY_INTERVAL_MS` | How often the in-process metrics summary is written, in whole milliseconds. `0` turns it off. |

Neither is exporter configuration. `OTEL_EXPORTER_OTLP_TIMEOUT` is read by the SDK and
means the per-export timeout, which is a different thing; the first of these is how long
the process is willing to wait before it stops. The second governs a report this crate
writes to its own log, which no exporter is involved in at all.

A per-signal variable beats the generic one. The generic endpoint has the signal's path
appended to it (`/v1/traces` and so on); a per-signal endpoint is used exactly as written,
so it must include the path.

```sh
OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.example.com:4318 \
OTEL_EXPORTER_OTLP_PROTOCOL=http/protobuf \
  ./adele-daemon
```

### What startup says

One line at INFO, once the console exists to write it on:

```text
INFO telemetry export is on service_name=adele-daemon service_name_from=OTEL_SERVICE_NAME
resource_attributes=k8s.namespace.name,k8s.node.name,k8s.pod.name,service.name,...
traces=on metrics=on logs=off shutdown_budget_ms=5000
```

It answers the questions an operator has after setting a variable: which name reached the
backend and which source supplied it, whether the pairs in `OTEL_RESOURCE_ATTRIBUTES`
arrived, which signals export, and how long a stop may take. The service name and the key
list are read back off the resource the SDK built, not from the decision that went into
it.

A variable that was set and could not be honoured is named at WARN, and export continues:

```text
WARN a telemetry variable could not be honoured detail=OTEL_TRACES_EXPORTER=zipkin names
an exporter this build does not have. Only `otlp` and `none` are implemented, so this
signal keeps exporting over OTLP
```

### Which value names the service

Highest first:

1. `OTEL_SERVICE_NAME`.
2. The name the binary passed to `Config::new`.
3. `service.name` inside `OTEL_RESOURCE_ATTRIBUTES`, which never wins.

The two variables are treated differently on purpose, and the difference is propagation.
`OTEL_SERVICE_NAME` is per process: the daemon builds the environment of every MCP server
it spawns from an allowlist, and that variable is not on it, so honouring it renames one
process. `OTEL_RESOURCE_ATTRIBUTES` **is** passed down to every server, so that a server
span carries the pod, the namespace and the node. If a `service.name` entry in it won,
every server and the daemon would report as one service and no trace would be readable.
The specification agrees: user-supplied resource information outranks that variable.

Every other pair in `OTEL_RESOURCE_ATTRIBUTES` reaches the resource untouched. To tell two
deployments of one binary apart, either name is fine; to add deployment context to both,
use `OTEL_RESOURCE_ATTRIBUTES`.

### Turning export off

Three ways, none of which needs a rebuild:

```sh
OTEL_SDK_DISABLED=true ./adele-daemon          # no pipeline at all
OTEL_LOGS_EXPORTER=none ./adele-daemon         # one signal off, the other two on
```

Only the exact value `true` disables the SDK, which the specification requires; `1`, `yes`
and `on` leave export running and are reported at WARN. An exporter name this build does
not have - `zipkin`, `prometheus`, `console` - is reported and ignored, and the signal
keeps exporting over OTLP. Reading an unrecognised name as `none` would silently stop a
signal an operator asked for.

The console layer and the metrics summary are unaffected by all of this. They are what an
operator falls back on when there is no backend, so nothing about export can remove them.

**With no endpoint variable set at all**, the OTLP default `http://localhost:4318` applies.
That is the specification's default and this crate keeps it, because a collector on the
default port is the ordinary development case. Startup names the default and the three
ways above, so a process exporting into a socket nobody is listening on says so rather
than retrying in silence.

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

### Both transports read the OS trust store

Neither bundles a root set. `grpc` uses tonic with the system roots; `http/protobuf` goes
through reqwest and `rustls-platform-verifier`, which reads the same place.

Two consequences:

- **A container needs a CA bundle.** Install `ca-certificates`, or every HTTPS export fails
  on both transports. A `FROM scratch` or distroless image with no bundle cannot export
  over HTTPS at all.
- **A private certificate authority installed on the host just works**, on both transports,
  with no code change and no rebuild.

#### gRPC needs its roots passed in, and that is not obvious

`opentelemetry-otlp` builds its tonic channel with a bare `ClientTlsConfig::new()` for an
`https` endpoint, and tonic's root sets are opt-in booleans on that config that default to
false. A gRPC exporter left to itself therefore verifies against **no roots at all** and
rejects every certificate as `UnknownIssuer` - whichever `tls-*-roots` feature is compiled
in. Enabling the Cargo feature is a no-op by itself; `with_enabled_roots()` is what turns it
on, and this crate calls it.

That failure is worth recognising, because it reads like a certificate problem and is not:

```text
TonicTracesClient.ExportFailed grpc_code="Unavailable"
  grpc_message="invalid peer certificate: UnknownIssuer"
```

If you see it against a collector whose certificate is otherwise fine, the roots were never
loaded.

#### Why the OS store rather than a bundled one

A bundled root set - `webpki-roots` - was tried first and replaced. It can verify a public
certificate perfectly well, and the reason for moving is not that it failed to:

- It cannot see a certificate authority an administrator installed on the host, so a private
  CA can never work with it.
- It is a snapshot taken when that crate version was published, so it goes stale against CA
  rotation and needs an upstream release plus a rebuild to catch up. `webpki-roots 1.0.9`,
  the current release, carries `ISRG Root X1` and `X2` and no entry for the newer
  `ISRG Root YR` that Let's Encrypt has begun issuing from.
- Having one transport on bundled roots and the other on the OS store meant two answers to
  every trust question.

`scripts/no-bundled-roots.sh` fails the gate if a bundled root set comes back.

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
collector, and there are six calls, so an unbounded drop can run for thirty seconds. The
budget caps the total, and the default is five seconds.

`ADELIE_TELEMETRY_SHUTDOWN_BUDGET_MS` sets it at run time, so a rollout can follow a
shorter `terminationGracePeriodSeconds` without rebuilding every binary in the fleet.
`Config::with_shutdown_budget` outranks the variable, for a binary that must have a
particular budget. A value that is not a whole number of milliseconds, or is negative, is
named at startup and ignored.

`0` means do not wait at all: nothing is flushed and the process stops. Whatever the
exporters had buffered is dropped, which is what a budget of zero asks for, so it is
stated once at INFO and is not a warning.

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
