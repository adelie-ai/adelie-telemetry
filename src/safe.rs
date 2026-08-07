//! Rendering a value a caller can influence into a log field.
//!
//! Every binary in the fleet puts caller-influenced text on a log line: a tool name, a
//! model name, an error message quoting the input back. This is the one way to do it, and
//! it lives here rather than in any one consumer because the clients need it as much as
//! the servers do, and not all of them depend on `mcp-core`.
//!
//! One predicate, in one place, is the whole point. It has already been widened twice, and
//! the cost of a second copy is that the third widening lands in one of them.

use std::fmt;

/// The most bytes of a caller-chosen name a log field keeps.
///
/// The same limit the metrics facade puts on a label value, so one name reads the same way
/// whichever signal an operator looks at.
pub const MAX_NAME_BYTES: usize = 128;

/// The most bytes of a diagnostic message a log field keeps.
///
/// Wider than a name, because the text is mostly what the binary wrote itself and is worth
/// keeping whole.
pub const MAX_MESSAGE_BYTES: usize = 1024;

/// What replaces a character that could change what a reader sees.
///
/// Replaced rather than dropped, so the field still shows that something was there.
pub const REPLACEMENT: char = '\u{fffd}';

/// What marks a value the cap cut short.
pub const TRUNCATED: &str = "...";

/// A value a caller can influence, rendered safely into a log field.
///
/// ```
/// use adelie_telemetry::Safe;
///
/// # let tool_name = "search";
/// # let detail = "not found";
/// tracing::info!(tool = %Safe::name(tool_name), "tool call finished");
/// tracing::debug!(reason = %Safe::message(detail), "tool returned an error");
/// ```
///
/// # What makes a raw value unsafe
///
/// The console layer writes a field value straight into a line, so a newline in one
/// produces what reads as a second genuine line, with a real timestamp column, level and
/// target. An ANSI escape survives, because turning the formatter's own colour off does
/// not strip an escape carried inside a value. A bidi control reverses what a terminal
/// shows without touching a byte, so a name renders as something it is not.
///
/// Length is the second problem. Nothing bounds a tool name or a message short of the
/// transport's frame cap, which is measured in megabytes, so one request could otherwise
/// ship as much as it liked into the log.
///
/// # It renders lazily
///
/// Wrapping a value does no work. The sanitising happens inside [`fmt::Display`], so a
/// field at a level nobody enabled costs nothing beyond constructing the wrapper, and a
/// field that is rendered is written straight into the formatter with no intermediate
/// [`String`].
///
/// # It wraps anything that can be displayed
///
/// `T` is any [`fmt::Display`], not just a string, which is what stops a second wrapper
/// appearing for every value type. A JSON value implements `Display`, so it goes through
/// this one like anything else and this crate needs no JSON dependency to handle it.
///
/// # What it is not for
///
/// Values the operator supplied: a socket path, a listen address, a configuration file
/// name. Those are written once at startup and are not a caller's to choose.
pub struct Safe<T> {
    value: T,
    cap: usize,
}

impl<T: fmt::Display> Safe<T> {
    /// A name the caller chose: a tool, a method, a model, a request id.
    ///
    /// Short by nature, so the tight cap costs nothing real.
    pub fn name(value: T) -> Self {
        Self {
            value,
            cap: MAX_NAME_BYTES,
        }
    }

    /// A diagnostic message.
    ///
    /// Mostly text the binary wrote itself, but it routinely quotes the caller's own input
    /// back inside it, which is why it is sanitised at all.
    pub fn message(value: T) -> Self {
        Self {
            value,
            cap: MAX_MESSAGE_BYTES,
        }
    }

    /// A value with a cap of its own.
    ///
    /// [`name`](Self::name) and [`message`](Self::message) cover the two shapes that
    /// actually occur, and naming the shape rather than passing a number is what stops the
    /// caps drifting apart across the fleet. Reach for this only where a value genuinely
    /// fits neither, and say why at the call site.
    pub fn with_cap(value: T, cap: usize) -> Self {
        Self { value, cap }
    }
}

impl<T: fmt::Display> fmt::Display for Safe<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use fmt::Write;

        let mut sink = Sanitizing {
            out: f,
            written: 0,
            cap: self.cap,
            finished: false,
        };
        write!(sink, "{}", self.value)
    }
}

/// Writes through to a formatter, replacing what would deceive and stopping at the cap.
struct Sanitizing<'a, 'b> {
    out: &'a mut fmt::Formatter<'b>,
    written: usize,
    cap: usize,
    finished: bool,
}

impl fmt::Write for Sanitizing<'_, '_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.finished {
            return Ok(());
        }
        for character in text.chars() {
            let safe = if is_deceptive(character) {
                REPLACEMENT
            } else {
                character
            };
            // Measured whole, so the cap can never cut a character in half and invent a
            // replacement that was not in the input.
            let length = safe.len_utf8();
            if self.written + length > self.cap {
                self.finished = true;
                return self.out.write_str(TRUNCATED);
            }
            self.out.write_char(safe)?;
            self.written += length;
        }
        Ok(())
    }
}

/// Whether this character could change what a person reads, rather than what was written.
///
/// Three groups, and they fail in different ways:
///
/// - [`char::is_control`] covers category Cc: C0, C1 and DEL. A newline ends the log line
///   early and starts one that reads as genuine; an escape drives the terminal.
/// - U+2028 LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR are categories Zl and Zp.
///   `is_control` does not cover them, and some log viewers and every JSON consumer treat
///   them as a line break.
/// - The bidi controls are category Cf. They leave the bytes honest and the line structure
///   intact, and reverse what a terminal shows: a tool named with U+202E renders with
///   everything after it backwards, so the name an operator reads in `kubectl logs` is not
///   the name that was called. Deception rather than forgery, and the Trojan-source class.
///
/// # Why a list of bidi controls and not all of Cf
///
/// Cf also holds U+200D ZERO WIDTH JOINER, which carries the emoji sequences a person
/// legitimately wants to read. Hiding text is a weaker problem than reversing it, so the
/// set stops at the characters that change reading order.
///
/// The authority for the list is what `mcp-core` strips, so one answer holds across the
/// fleet. That set is a **superset** of rustc's `text_direction_codepoint_in_literal`
/// lint: it adds U+061C, U+200E and U+200F, the marks, to the lint's overrides, embeddings
/// and isolates. The marks are included because a mark still changes reading order in a
/// log line, which is the property this predicate is about. Do not narrow the list to the
/// rustc lint on the grounds that the three marks are absent from it - they are absent
/// deliberately.
///
/// # Reason from the categories, not from an attack
///
/// This predicate has been widened twice, and both times because it had been written
/// against the attack in mind - line forgery - rather than against the character categories
/// that make an attack possible. Cc breaks the line, Zl and Zp break the line in a JSON
/// consumer, Cf reorders what is displayed. Check a new case against the categories, and
/// the upper boundary against `a_zero_width_joiner_survives_the_sanitiser`, which fails if
/// this widens to all of Cf.
pub(crate) fn is_deceptive(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{2028}'      // LINE SEPARATOR
            | '\u{2029}'    // PARAGRAPH SEPARATOR
            | '\u{061c}'    // ARABIC LETTER MARK
            | '\u{200e}'    // LEFT-TO-RIGHT MARK
            | '\u{200f}'    // RIGHT-TO-LEFT MARK
            | '\u{202a}'..='\u{202e}'  // the embeddings, the pop, and the overrides
            | '\u{2066}'..='\u{2069}'  // the isolates and the pop
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_within_the_cap_is_unchanged() {
        assert_eq!(Safe::name("search").to_string(), "search");
    }

    #[test]
    fn an_empty_value_renders_empty() {
        assert_eq!(Safe::name("").to_string(), "");
    }

    /// The cap counts bytes, not characters, because that is what bounds memory.
    #[test]
    fn the_cap_counts_bytes() {
        let four_byte = "\u{1f600}";
        assert_eq!(Safe::with_cap(four_byte, 4).to_string(), four_byte);
        assert_eq!(Safe::with_cap(four_byte, 3).to_string(), TRUNCATED);
    }

    /// A replacement is three bytes where the character it replaced may be one, so the
    /// cap has to be measured after substitution or a hostile value could overrun it.
    #[test]
    fn a_replacement_counts_against_the_cap_at_its_own_width() {
        let rendered = Safe::with_cap("\n\n\n\n", 9).to_string();
        assert_eq!(rendered, "\u{fffd}\u{fffd}\u{fffd}...");
    }
}
