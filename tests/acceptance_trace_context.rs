//! Trace-context acceptance criteria.
//!
//! Nothing in this file calls `init`, builds a `Config` or holds a `Guard`. That is the
//! point: a desktop client shipping a default-feature build must be able to mint the id
//! the daemon adopts without installing telemetry at all.

use adelie_telemetry::trace_context::{
    self, SpanId, TraceContextError, TraceId, TraceOrigin, TraceParent,
};

const REQUEST_ID: [u8; 16] = [
    0x4b, 0xf9, 0x2f, 0x35, 0x77, 0xb3, 0x4d, 0xa6, 0xa3, 0xce, 0x92, 0x9d, 0x0e, 0x0e, 0x47, 0x36,
];

/// The uuid's bytes are recoverable from the trace id, and it works with default features.
#[test]
fn trace_id_from_uuid_round_trips() {
    let trace_id = trace_context::trace_id_from_uuid(REQUEST_ID).expect("a non-zero uuid is valid");

    assert_eq!(
        trace_id.to_bytes(),
        REQUEST_ID,
        "the request id must survive unchanged, so one value identifies the turn everywhere"
    );
    assert_eq!(trace_id.to_hex(), "4bf92f3577b34da6a3ce929d0e0e4736");
    assert_eq!(
        TraceId::from_hex(&trace_id.to_hex()).expect("our own hex must parse"),
        trace_id
    );
}

/// Minting needs no `Config`, no `init` and no `Guard`.
#[test]
fn trace_id_mints_without_installing_telemetry() {
    let trace_id = trace_context::trace_id_from_uuid(REQUEST_ID).expect("a non-zero uuid is valid");
    let header = TraceParent::root_for(trace_id, true).to_header();

    let parsed = trace_context::extract_traceparent(&header).expect("our own header must parse");
    assert_eq!(
        parsed.trace_id(),
        trace_id,
        "a client that installs nothing must still produce a header the daemon can adopt"
    );
    assert!(
        parsed.span_id().to_bytes() != [0; 8],
        "a synthetic root still needs a usable parent span id"
    );
}

/// The all-zero uuid is the spec's "no trace" sentinel and can never become a trace id.
#[test]
fn trace_id_from_uuid_rejects_all_zero() {
    assert_eq!(
        trace_context::trace_id_from_uuid([0; 16]),
        Err(TraceContextError::ZeroTraceId)
    );
}

/// An incoming `traceparent` wins; the request id is only used when none arrived.
#[test]
fn traceparent_extract_takes_precedence_over_mint() {
    let incoming = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    let origin = trace_context::resolve_trace(Some(incoming), REQUEST_ID)
        .expect("a valid header must be continued");

    let TraceOrigin::Continued(parent) = origin else {
        panic!("a valid incoming traceparent must continue that trace, not mint a new one");
    };
    assert_eq!(parent.trace_id().to_hex(), "0af7651916cd43dd8448eb211c80319c");
    assert_eq!(parent.span_id().to_hex(), "b7ad6b7169203331");
    assert!(parent.sampled());
    assert_eq!(
        origin.trace_id().to_hex(),
        "0af7651916cd43dd8448eb211c80319c",
        "the caller's trace id must win over the local request id"
    );
    assert_eq!(origin.parent_span_id(), Some(parent.span_id()));
}

/// With no incoming header, the request id becomes the trace id.
#[test]
fn resolve_trace_mints_from_request_id_when_no_header_arrives() {
    let origin =
        trace_context::resolve_trace(None, REQUEST_ID).expect("a non-zero request id is valid");

    assert_eq!(origin, TraceOrigin::Minted(TraceId::from_bytes(REQUEST_ID).expect("non-zero")));
    assert_eq!(origin.trace_id().to_bytes(), REQUEST_ID);
    assert_eq!(
        origin.parent_span_id(),
        None,
        "a minted trace is a root and has no parent span"
    );
}

/// A malformed header must not silently start a second trace for work that is one turn.
#[test]
fn resolve_trace_rejects_a_malformed_header_rather_than_minting() {
    let result = trace_context::resolve_trace(Some("not-a-traceparent"), REQUEST_ID);
    assert!(
        result.is_err(),
        "falling back to a mint would split one turn across two traces without saying so"
    );
}

/// The header round-trips through inject and extract unchanged.
#[test]
fn traceparent_round_trips_through_inject_and_extract() {
    let trace_id = TraceId::from_bytes(REQUEST_ID).expect("non-zero");
    let span_id = SpanId::from_bytes([1, 2, 3, 4, 5, 6, 7, 8]).expect("non-zero");
    let parent = TraceParent::new(trace_id, span_id, false);

    let header = trace_context::inject_traceparent(parent);
    assert_eq!(header, "00-4bf92f3577b34da6a3ce929d0e0e4736-0102030405060708-00");
    assert_eq!(
        trace_context::extract_traceparent(&header).expect("our own header must parse"),
        parent
    );
}

/// An unknown future version is accepted and its extra fields ignored, as W3C requires.
#[test]
fn traceparent_accepts_an_unknown_future_version() {
    let header = "01-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01-extrafield";
    let parent = trace_context::extract_traceparent(header)
        .expect("a newer version must be read, not rejected");
    assert_eq!(parent.trace_id().to_hex(), "0af7651916cd43dd8448eb211c80319c");
}

/// Version `ff` is reserved.
#[test]
fn traceparent_rejects_the_reserved_version() {
    let header = "ff-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    assert_eq!(
        trace_context::extract_traceparent(header),
        Err(TraceContextError::ReservedVersion)
    );
}

/// The spec's invalid-id sentinels are rejected wherever they appear.
#[test]
fn traceparent_rejects_the_zero_sentinels() {
    let zero_trace = "00-00000000000000000000000000000000-b7ad6b7169203331-01";
    assert_eq!(
        trace_context::extract_traceparent(zero_trace),
        Err(TraceContextError::ZeroTraceId)
    );

    let zero_span = "00-0af7651916cd43dd8448eb211c80319c-0000000000000000-01";
    assert_eq!(
        trace_context::extract_traceparent(zero_span),
        Err(TraceContextError::ZeroSpanId)
    );
}

/// Fields of the wrong length or the wrong alphabet are rejected by name.
#[test]
fn traceparent_rejects_malformed_fields() {
    let short_trace = "00-0af765-b7ad6b7169203331-01";
    assert_eq!(
        trace_context::extract_traceparent(short_trace),
        Err(TraceContextError::Malformed {
            field: "trace-id",
            expected: 32
        })
    );

    let not_hex = "00-0af7651916cd43dd8448eb211c80319g-b7ad6b7169203331-01";
    assert_eq!(
        trace_context::extract_traceparent(not_hex),
        Err(TraceContextError::Malformed {
            field: "trace-id",
            expected: 32
        })
    );

    assert_eq!(
        trace_context::extract_traceparent("00-0af7651916cd43dd8448eb211c80319c"),
        Err(TraceContextError::FieldCount { found: 2 })
    );
}
