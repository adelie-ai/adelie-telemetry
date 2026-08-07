//! The shared log-field sanitiser.
//!
//! Its reason for existing is that there must be exactly one of it. Every server, client
//! and daemon that puts a caller-influenced value on a log line reaches for this, so the
//! tests that matter most are the ones that would catch a second copy drifting from it.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};

use adelie_telemetry::metrics::Label;
use adelie_telemetry::{MAX_MESSAGE_BYTES, MAX_NAME_BYTES, Safe};

/// Everything the predicate must catch, with the name a reader would look it up by.
///
/// Built as Rust escapes, never pasted as literals: these are invisible and tooling eats
/// them silently, so a fixture carrying them as text can assert against something other
/// than what it appears to.
const DECEPTIVE: &[(char, &str)] = &[
    ('\n', "LINE FEED"),
    ('\r', "CARRIAGE RETURN"),
    ('\u{1b}', "ESCAPE"),
    ('\u{7f}', "DELETE"),
    ('\u{85}', "NEXT LINE"),
    ('\u{2028}', "LINE SEPARATOR"),
    ('\u{2029}', "PARAGRAPH SEPARATOR"),
    ('\u{061c}', "ARABIC LETTER MARK"),
    ('\u{200e}', "LEFT-TO-RIGHT MARK"),
    ('\u{200f}', "RIGHT-TO-LEFT MARK"),
    ('\u{202a}', "LEFT-TO-RIGHT EMBEDDING"),
    ('\u{202b}', "RIGHT-TO-LEFT EMBEDDING"),
    ('\u{202c}', "POP DIRECTIONAL FORMATTING"),
    ('\u{202d}', "LEFT-TO-RIGHT OVERRIDE"),
    ('\u{202e}', "RIGHT-TO-LEFT OVERRIDE"),
    ('\u{2066}', "LEFT-TO-RIGHT ISOLATE"),
    ('\u{2067}', "RIGHT-TO-LEFT ISOLATE"),
    ('\u{2068}', "FIRST STRONG ISOLATE"),
    ('\u{2069}', "POP DIRECTIONAL ISOLATE"),
];

/// Characters that must pass through untouched, so the predicate cannot simply widen.
const INNOCENT: &[(char, &str)] = &[
    ('\u{200d}', "ZERO WIDTH JOINER"),
    ('a', "LATIN SMALL LETTER A"),
    (' ', "SPACE"),
    ('\u{00e9}', "LATIN SMALL LETTER E WITH ACUTE"),
    ('\u{4e2d}', "CJK IDEOGRAPH"),
    ('\u{1f600}', "GRINNING FACE"),
];

/// No character that changes what a reader sees survives.
#[test]
fn safe_strips_every_deceptive_character() {
    for (character, name) in DECEPTIVE {
        let rendered = Safe::name(format!("before{character}after")).to_string();
        assert!(
            !rendered.contains(*character),
            "U+{:04X} {name} survived: {rendered:?}",
            *character as u32
        );
        assert!(
            rendered.starts_with("before"),
            "the readable part must survive"
        );
        assert!(rendered.ends_with("after"), "and so must what follows it");
    }
}

/// Text a person meant to read is left alone.
#[test]
fn safe_leaves_innocent_characters_alone() {
    for (character, name) in INNOCENT {
        let value = format!("before{character}after");
        assert_eq!(
            Safe::name(&value).to_string(),
            value,
            "U+{:04X} {name} must pass through untouched",
            *character as u32
        );
    }
}

/// The exported helper and the metrics label agree, character for character.
///
/// This is the test the module exists for. Two sanitisers that disagree are how a value
/// reads one way in a log line and another in a metrics summary, and how the next
/// widening lands in one place and not the other.
#[test]
fn safe_and_label_agree_character_for_character() {
    for (character, name) in DECEPTIVE.iter().chain(INNOCENT) {
        let input = format!("before{character}after");

        let through_safe = Safe::name(&input).to_string();
        let through_label = Label::new("field", input.clone()).value().to_owned();

        assert_eq!(
            through_safe, through_label,
            "U+{:04X} {name} is treated differently by Safe and by Label::new",
            *character as u32
        );
    }
}

/// A name is capped tighter than a message, and the difference is deliberate.
#[test]
fn a_name_is_capped_tighter_than_a_message() {
    // Checked when the test compiles, so reversing the two caps breaks the build rather
    // than waiting for a run.
    const { assert!(MAX_NAME_BYTES < MAX_MESSAGE_BYTES) };

    let long = "x".repeat(MAX_MESSAGE_BYTES * 2);
    assert!(Safe::name(&long).to_string().len() <= MAX_NAME_BYTES + "...".len());
    assert!(Safe::message(&long).to_string().len() <= MAX_MESSAGE_BYTES + "...".len());
    assert!(
        Safe::message(&long).to_string().len() > MAX_NAME_BYTES,
        "a message must actually keep more than a name, not just claim to"
    );
}

/// A value the cap cut short says so, rather than looking complete.
#[test]
fn truncation_marks_the_cut() {
    let long = "x".repeat(MAX_NAME_BYTES * 2);
    assert!(Safe::name(&long).to_string().ends_with("..."));

    let short = "x".repeat(8);
    assert!(
        !Safe::name(&short).to_string().ends_with("..."),
        "a value that fits must not be marked as cut"
    );
}

/// Truncation must not split a character in half.
#[test]
fn truncation_respects_character_boundaries() {
    let wide = "\u{1f600}".repeat(MAX_NAME_BYTES);
    let rendered = Safe::name(&wide).to_string();
    assert!(
        rendered
            .trim_end_matches('.')
            .chars()
            .all(|c| c == '\u{1f600}'),
        "a cut mid-character would produce a replacement that was never in the input"
    );
}

/// An explicit cap is available for a caller whose value fits neither shape.
#[test]
fn an_explicit_cap_is_available() {
    let long = "x".repeat(100);
    let rendered = Safe::with_cap(&long, 10).to_string();
    assert_eq!(rendered, "xxxxxxxxxx...");
}

/// Nothing is rendered until a subscriber asks for it.
///
/// A disabled log level must cost nothing. If wrapping a value did the work eagerly,
/// every server would pay to sanitise values that are then thrown away.
#[test]
fn nothing_renders_until_it_is_asked_for() {
    struct Counting<'a>(&'a AtomicUsize);
    impl fmt::Display for Counting<'_> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.0.fetch_add(1, Ordering::SeqCst);
            f.write_str("value")
        }
    }

    let renders = AtomicUsize::new(0);
    let safe = Safe::name(Counting(&renders));
    assert_eq!(
        renders.load(Ordering::SeqCst),
        0,
        "constructing the wrapper must not render the value"
    );

    assert_eq!(safe.to_string(), "value");
    assert_eq!(renders.load(Ordering::SeqCst), 1);
}

/// Anything that can be displayed can be wrapped.
///
/// This is what removes the need for a second wrapper per value type. A JSON value
/// implements `Display`, so it goes through this one like any other value, and this crate
/// needs no JSON dependency to handle it.
#[test]
fn any_displayable_value_can_be_wrapped() {
    assert_eq!(Safe::name(42).to_string(), "42");
    assert_eq!(
        Safe::message(std::path::Path::new("/tmp/x").display()).to_string(),
        "/tmp/x"
    );

    struct JsonLike;
    impl fmt::Display for JsonLike {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "{{\"tool\":\"a\u{202e}b\"}}")
        }
    }
    let rendered = Safe::message(JsonLike).to_string();
    assert!(
        !rendered.contains('\u{202e}'),
        "a rendered value is sanitised too"
    );
    assert!(rendered.starts_with("{\"tool\""));
}
