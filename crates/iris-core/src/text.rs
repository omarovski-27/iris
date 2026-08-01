//! Turning a transcript into keystrokes.
//!
//! Split out from [`crate::inject`] so the fiddly part — UTF-16, surrogate
//! pairs, control characters — is plain data and can be tested anywhere.

/// One synthetic keystroke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyUnit {
    /// A UTF-16 code unit delivered with `KEYEVENTF_UNICODE`. This bypasses the
    /// keyboard layout entirely, so `ü` and `→` arrive intact on a US layout.
    Unicode(u16),
    /// A virtual-key press. Needed for the few characters that `KEYEVENTF_UNICODE`
    /// does not reliably turn into an action.
    Virtual(u16),
}

impl KeyUnit {
    /// Whether a `SendInput` batch may end after this unit.
    ///
    /// A high surrogate is the first half of an astral character, and Windows
    /// only reassembles the pair when both halves arrive in one batch (see
    /// [`plan`]), so a batch must never be cut between them.
    pub fn may_end_batch(&self) -> bool {
        !matches!(self, KeyUnit::Unicode(u) if (0xD800..0xDC00).contains(u))
    }
}

/// `VK_RETURN`. `KEYEVENTF_UNICODE` with U+000D inserts a control character in
/// some controls instead of committing a line, so newlines go through the
/// virtual key.
pub const VK_RETURN: u16 = 0x0D;
/// `VK_TAB`, for the same reason.
pub const VK_TAB: u16 = 0x09;

/// Tidy a transcript for insertion.
///
/// Engines return leading/trailing whitespace inconsistently; the caller wants
/// to control spacing, so strip it here and let `trailing_space` decide.
pub fn prepare(text: &str, trailing_space: bool) -> String {
    let mut s = text.trim().to_string();
    if trailing_space && !s.is_empty() {
        s.push(' ');
    }
    s
}

/// Expand text into the keystrokes that reproduce it.
///
/// Characters outside the Basic Multilingual Plane (emoji, for instance) are two
/// UTF-16 code units and must be sent as two `Unicode` events — Windows
/// reassembles the surrogate pair as long as they arrive in one `SendInput`
/// batch.
pub fn plan(text: &str) -> Vec<KeyUnit> {
    let mut units = Vec::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '\n' => units.push(KeyUnit::Virtual(VK_RETURN)),
            '\t' => units.push(KeyUnit::Virtual(VK_TAB)),
            // Swallow CR so CRLF does not produce two line breaks.
            '\r' => {}
            _ => {
                let mut buf = [0u16; 2];
                for unit in ch.encode_utf16(&mut buf) {
                    units.push(KeyUnit::Unicode(*unit));
                }
            }
        }
    }
    units
}

/// How many units [`plan`] would produce, without producing them.
///
/// [`crate::inject::inject`] needs the length before it knows whether it will
/// send keystrokes at all, and the transcript is planned again inside the
/// keystroke path itself; counting here keeps that decision off the
/// allocator on a path with a sub-second latency budget. Must agree with
/// `plan(text).len()` exactly — the test below pins that.
pub fn plan_len(text: &str) -> usize {
    text.chars()
        .map(|ch| match ch {
            '\r' => 0,
            '\n' | '\t' => 1,
            _ => ch.len_utf16(),
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_len_agrees_with_plan_on_everything_plan_special_cases() {
        for text in [
            "",
            "hello",
            "ü→日本",
            "😀 emoji 😀",
            "line\nbreak\ttab",
            "crlf\r\nkept as one",
            "\r\r\r",
            &"a".repeat(300),
        ] {
            assert_eq!(plan_len(text), plan(text).len(), "{text:?}");
        }
    }

    #[test]
    fn ascii_becomes_one_unicode_event_per_character() {
        assert_eq!(
            plan("hi"),
            vec![KeyUnit::Unicode(b'h' as u16), KeyUnit::Unicode(b'i' as u16)]
        );
    }

    #[test]
    fn the_reported_regression_string_plans_without_substitution() {
        // "This is a full sentence." injected as 21 literal 0x20 bytes after
        // the first word. Every unit here must be its own character's exact
        // UTF-16 code point, one per character, none repeated or dropped —
        // proving `plan` cannot be where the substitution happens; see
        // `inject.rs`'s modifier-state correction for the actual fix.
        let text = prepare("This is a full sentence.", true);
        assert_eq!(text, "This is a full sentence. ");
        let units = plan(&text);
        let expected: Vec<KeyUnit> = text.chars().map(|c| KeyUnit::Unicode(c as u16)).collect();
        assert_eq!(units, expected);
        assert_eq!(units.len(), 25, "25 bytes landed in the hex dump");
    }

    #[test]
    fn accented_and_symbol_characters_survive() {
        // Single BMP code units, sent verbatim regardless of keyboard layout.
        assert_eq!(plan("ü"), vec![KeyUnit::Unicode(0x00FC)]);
        assert_eq!(plan("→"), vec![KeyUnit::Unicode(0x2192)]);
        assert_eq!(
            plan("日本"),
            vec![KeyUnit::Unicode(0x65E5), KeyUnit::Unicode(0x672C)]
        );
    }

    #[test]
    fn astral_characters_become_surrogate_pairs() {
        // U+1F600 -> D83D DE00
        assert_eq!(
            plan("😀"),
            vec![KeyUnit::Unicode(0xD83D), KeyUnit::Unicode(0xDE00)]
        );
    }

    #[test]
    fn newlines_and_tabs_use_virtual_keys_and_cr_is_dropped() {
        assert_eq!(
            plan("a\r\n\tb"),
            vec![
                KeyUnit::Unicode(b'a' as u16),
                KeyUnit::Virtual(VK_RETURN),
                KeyUnit::Virtual(VK_TAB),
                KeyUnit::Unicode(b'b' as u16),
            ]
        );
    }

    #[test]
    fn prepare_trims_and_optionally_adds_one_space() {
        assert_eq!(prepare("  hello world \n", false), "hello world");
        assert_eq!(prepare("  hello world \n", true), "hello world ");
        assert_eq!(prepare("   ", true), "", "empty stays empty");
    }

    #[test]
    fn a_batch_may_not_end_on_a_high_surrogate() {
        let pair = plan("😀");
        assert!(
            !pair[0].may_end_batch(),
            "high surrogate must keep its pair"
        );
        assert!(pair[1].may_end_batch());
        assert!(KeyUnit::Unicode(b'a' as u16).may_end_batch());
        assert!(KeyUnit::Unicode(0x65E5).may_end_batch());
        assert!(KeyUnit::Virtual(VK_RETURN).may_end_batch());
    }

    #[test]
    fn batching_on_the_predicate_never_splits_a_surrogate_pair() {
        // Mirrors `inject::send_keystrokes`: two events per unit, flush once
        // the batch is full *and* the last unit allows it. The text is laid
        // out so the limit lands exactly on the high surrogate.
        let max_events = 8;
        let mut batches: Vec<Vec<KeyUnit>> = Vec::new();
        let mut current = Vec::new();
        for unit in plan("abc😀d") {
            current.push(unit);
            if current.len() * 2 >= max_events && unit.may_end_batch() {
                batches.push(std::mem::take(&mut current));
            }
        }
        if !current.is_empty() {
            batches.push(current);
        }

        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), 5, "the pair rode along in one batch");
        for batch in &batches {
            assert!(batch.last().unwrap().may_end_batch());
        }
    }

    #[test]
    fn planning_is_proportional_to_the_text() {
        assert!(plan("").is_empty());
        assert_eq!(plan(&"x".repeat(500)).len(), 500);
    }
}
