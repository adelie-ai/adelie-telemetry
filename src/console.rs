//! The console layer, and the filter that governs it.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::registry::LookupSpan;

use crate::config::Config;

/// The filter for every layer this crate installs.
///
/// `RUST_LOG` wins. When it is unset or unparseable the config's default filter is used.
/// One filter governs the console layer and the OTLP log exporter together: an operator
/// who turns the verbosity up expects to see the same lines wherever they are reading,
/// and a second filter would mean two answers to "why is this line missing".
pub(crate) fn env_filter(config: &Config) -> EnvFilter {
    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(config.default_filter()))
}

/// The console layer.
///
/// The writer is a parameter so a test can capture what would have been printed. Callers
/// in production pass [`std::io::stderr`].
pub(crate) fn console_layer<S, W>(
    config: &Config,
    writer: W,
) -> tracing_subscriber::fmt::Layer<
    S,
    tracing_subscriber::fmt::format::DefaultFields,
    tracing_subscriber::fmt::format::Format,
    W,
>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + 'static,
{
    let span_events = if config.span_close_events() {
        FmtSpan::CLOSE
    } else {
        FmtSpan::NONE
    };
    tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_span_events(span_events)
        .with_ansi(false)
}
