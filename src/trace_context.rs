//! Trace identifiers, and the `traceparent` string that carries them between processes.
//!
//! These helpers work with the `otel` feature off. A default build still derives the
//! trace id, still parses an incoming `traceparent`, and still prints the id on its log
//! lines, so a correlation id survives in a build that exports nothing.

use std::fmt;

/// The number of bytes in a trace id. The same width as a uuid, which is what makes
/// [`trace_id_from_uuid`] a reinterpretation rather than a hash.
pub const TRACE_ID_BYTES: usize = 16;

/// The number of bytes in a span id.
pub const SPAN_ID_BYTES: usize = 8;

/// A W3C trace id: 16 bytes, never all zero.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId([u8; TRACE_ID_BYTES]);

/// A W3C span id: 8 bytes, never all zero.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanId([u8; SPAN_ID_BYTES]);

/// A parsed `traceparent`: the trace to join, the span to hang from, and whether the
/// originator sampled it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TraceParent {
    trace_id: TraceId,
    span_id: SpanId,
    sampled: bool,
}

/// Where a turn's trace id came from.
///
/// Why this is an enum and not just an id: a continued trace also carries the caller's
/// span id, which a child span needs as its parent. A minted trace has no parent.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TraceOrigin {
    /// An incoming `traceparent` was present and valid, so this process joins that trace.
    Continued(TraceParent),
    /// No usable `traceparent` arrived, so the request id became the trace id.
    Minted(TraceId),
}

/// Why a trace id or a `traceparent` could not be used.
#[derive(Clone, PartialEq, Eq, Debug, thiserror::Error)]
pub enum TraceContextError {
    /// A trace id of all zero bytes is the "invalid" sentinel and can never be used.
    #[error("trace id is all zero, which the W3C trace context spec reserves as invalid")]
    ZeroTraceId,
    /// A span id of all zero bytes is the "invalid" sentinel and can never be used.
    #[error("span id is all zero, which the W3C trace context spec reserves as invalid")]
    ZeroSpanId,
    /// The header had the wrong number of hyphen-separated fields.
    #[error("traceparent needs at least 4 hyphen-separated fields, found {found}")]
    FieldCount {
        /// How many fields the header actually had.
        found: usize,
    },
    /// A field was not the hexadecimal the spec requires, or was the wrong length.
    #[error("traceparent field '{field}' is not {expected} hexadecimal characters")]
    Malformed {
        /// The name of the field that failed: `version`, `trace-id`, `span-id` or `flags`.
        field: &'static str,
        /// How many characters the field should have had.
        expected: usize,
    },
    /// Version `ff` is reserved and must be rejected rather than guessed at.
    #[error("traceparent version 'ff' is reserved")]
    ReservedVersion,
}

impl TraceId {
    /// The trace id these bytes spell, or an error if they are all zero.
    pub fn from_bytes(bytes: [u8; TRACE_ID_BYTES]) -> Result<Self, TraceContextError> {
        if bytes == [0; TRACE_ID_BYTES] {
            return Err(TraceContextError::ZeroTraceId);
        }
        Ok(Self(bytes))
    }

    /// The bytes behind this id.
    pub fn to_bytes(self) -> [u8; TRACE_ID_BYTES] {
        self.0
    }

    /// This id as the 32 lowercase hexadecimal characters a `traceparent` carries.
    pub fn to_hex(self) -> String {
        to_hex(&self.0)
    }

    /// The trace id these 32 hexadecimal characters spell.
    pub fn from_hex(hex: &str) -> Result<Self, TraceContextError> {
        let bytes = from_hex::<TRACE_ID_BYTES>(hex).ok_or(TraceContextError::Malformed {
            field: "trace-id",
            expected: TRACE_ID_BYTES * 2,
        })?;
        Self::from_bytes(bytes)
    }
}

impl SpanId {
    /// The span id these bytes spell, or an error if they are all zero.
    pub fn from_bytes(bytes: [u8; SPAN_ID_BYTES]) -> Result<Self, TraceContextError> {
        if bytes == [0; SPAN_ID_BYTES] {
            return Err(TraceContextError::ZeroSpanId);
        }
        Ok(Self(bytes))
    }

    /// The bytes behind this id.
    pub fn to_bytes(self) -> [u8; SPAN_ID_BYTES] {
        self.0
    }

    /// This id as the 16 lowercase hexadecimal characters a `traceparent` carries.
    pub fn to_hex(self) -> String {
        to_hex(&self.0)
    }

    /// The span id these 16 hexadecimal characters spell.
    pub fn from_hex(hex: &str) -> Result<Self, TraceContextError> {
        let bytes = from_hex::<SPAN_ID_BYTES>(hex).ok_or(TraceContextError::Malformed {
            field: "span-id",
            expected: SPAN_ID_BYTES * 2,
        })?;
        Self::from_bytes(bytes)
    }
}

impl TraceParent {
    /// A `traceparent` built from its parts.
    pub fn new(trace_id: TraceId, span_id: SpanId, sampled: bool) -> Self {
        Self {
            trace_id,
            span_id,
            sampled,
        }
    }

    /// A `traceparent` for a process that has no spans of its own.
    ///
    /// A `traceparent` must name a parent span, but a client built with the `otel`
    /// feature off has no span machinery to name one from. This derives a stable span id
    /// from the trace id, so such a client can still start a trace that the daemon
    /// continues. The id is deterministic, so the same turn always produces the same
    /// header.
    pub fn root_for(trace_id: TraceId, sampled: bool) -> Self {
        let bytes = trace_id.to_bytes();
        let mut span_bytes = [0u8; SPAN_ID_BYTES];
        span_bytes.copy_from_slice(&bytes[..SPAN_ID_BYTES]);
        // A trace id is never all zero, but its first eight bytes can be. Fall back to
        // the last eight, which cannot then also be zero.
        if span_bytes == [0; SPAN_ID_BYTES] {
            span_bytes.copy_from_slice(&bytes[SPAN_ID_BYTES..]);
        }
        Self {
            trace_id,
            span_id: SpanId(span_bytes),
            sampled,
        }
    }

    /// The trace this context belongs to.
    pub fn trace_id(self) -> TraceId {
        self.trace_id
    }

    /// The span that should become the parent of anything this process starts.
    pub fn span_id(self) -> SpanId {
        self.span_id
    }

    /// Whether the originator sampled this trace.
    pub fn sampled(self) -> bool {
        self.sampled
    }

    /// This context as a `traceparent` header value.
    pub fn to_header(self) -> String {
        format!(
            "{VERSION}-{}-{}-{:02x}",
            self.trace_id.to_hex(),
            self.span_id.to_hex(),
            u8::from(self.sampled)
        )
    }
}

impl TraceOrigin {
    /// The trace id, whichever way it was arrived at.
    pub fn trace_id(self) -> TraceId {
        match self {
            Self::Continued(parent) => parent.trace_id(),
            Self::Minted(trace_id) => trace_id,
        }
    }

    /// The span to hang new work from, when the trace was continued.
    pub fn parent_span_id(self) -> Option<SpanId> {
        match self {
            Self::Continued(parent) => Some(parent.span_id()),
            Self::Minted(_) => None,
        }
    }
}

/// The trace id a request id spells.
///
/// A uuid is 16 bytes and a W3C trace id is 16 bytes, so a turn's existing request id
/// becomes the trace id directly with no mapping table and no second identifier. Pass
/// `uuid.into_bytes()`.
///
/// Why this rejects the all-zero case: the spec reserves it as the "no trace" sentinel,
/// and a backend drops a span that carries it.
pub fn trace_id_from_uuid(uuid_bytes: [u8; TRACE_ID_BYTES]) -> Result<TraceId, TraceContextError> {
    TraceId::from_bytes(uuid_bytes)
}

/// The trace context a `traceparent` header value carries.
///
/// Unknown future versions are accepted and their extra fields ignored, as the W3C spec
/// requires. Version `ff` is reserved and is rejected.
pub fn extract_traceparent(header: &str) -> Result<TraceParent, TraceContextError> {
    let fields: Vec<&str> = header.trim().split('-').collect();
    if fields.len() < 4 {
        return Err(TraceContextError::FieldCount {
            found: fields.len(),
        });
    }

    let version = fields[0];
    if version.len() != 2 || !version.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TraceContextError::Malformed {
            field: "version",
            expected: 2,
        });
    }
    if version.eq_ignore_ascii_case("ff") {
        return Err(TraceContextError::ReservedVersion);
    }

    let trace_id = TraceId::from_hex(fields[1])?;
    let span_id = SpanId::from_hex(fields[2])?;

    let flags = fields[3];
    if flags.len() != 2 || !flags.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(TraceContextError::Malformed {
            field: "flags",
            expected: 2,
        });
    }
    let flags = u8::from_str_radix(flags, 16).map_err(|_| TraceContextError::Malformed {
        field: "flags",
        expected: 2,
    })?;

    Ok(TraceParent {
        trace_id,
        span_id,
        sampled: flags & FLAG_SAMPLED != 0,
    })
}

/// The `traceparent` header value for this context.
pub fn inject_traceparent(parent: TraceParent) -> String {
    parent.to_header()
}

/// The trace to use for a turn.
///
/// An incoming `traceparent` wins. Only when none arrived, or the one that arrived cannot
/// be parsed, does the request id become the trace id. Joining the caller's trace is the
/// whole point of propagating it, so a malformed header must not silently start a second
/// trace for work that is really one turn.
pub fn resolve_trace(
    incoming_traceparent: Option<&str>,
    request_id: [u8; TRACE_ID_BYTES],
) -> Result<TraceOrigin, TraceContextError> {
    match incoming_traceparent {
        Some(header) => extract_traceparent(header).map(TraceOrigin::Continued),
        None => trace_id_from_uuid(request_id).map(TraceOrigin::Minted),
    }
}

/// The `traceparent` version this crate writes.
const VERSION: &str = "00";

/// The `sampled` bit of the `traceparent` flags field.
const FLAG_SAMPLED: u8 = 0x01;

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0u8; N];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

impl fmt::Debug for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TraceId({})", self.to_hex())
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SpanId({})", self.to_hex())
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}
