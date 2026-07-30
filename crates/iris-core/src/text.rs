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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_becomes_one_unicode_event_per_character() {
        assert_eq!(
            plan("hi"),
            vec![KeyUnit::Unicode(b'h' as u16), KeyUnit::Unicode(b'i' as u16)]
        );
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
    fn planning_is_proportional_to_the_text() {
        assert!(plan("").is_empty());
        assert_eq!(plan(&"x".repeat(500)).len(), 500);
    }
}
