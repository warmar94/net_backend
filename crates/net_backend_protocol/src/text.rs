//! Character rules shared by the `validate()` methods: which characters a name, an email address
//! or a chat message may not contain.
//!
//! - **Control characters** (Unicode category Cc: C0 incl. NUL, DEL, C1): never in names, keys,
//!   emails or passwords (PostgreSQL rejects NUL in text; terminals and logs misbehave). Chat text
//!   allows `\n` and `\t` only (multi-line messages); `\r` is not allowed (send `\n`).
//! - **Invisible and direction-changing format characters** (the list in [`is_invisible`]: bidi
//!   controls, zero-width space / joiners, word joiner, BOM, soft hyphen, …): never in display
//!   names or emails (they allow impersonation: `\u{202E}` reverses the rest of a name). Chat
//!   text rejects them too, except the zero-width joiner / non-joiner (U+200D, U+200C), which
//!   emoji sequences and several scripts need.

/// Whether `c` is a bidirectional control (U+061C, U+200E, U+200F, U+202A–U+202E, U+2066–U+2069).
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Whether `c` is the zero-width non-joiner or joiner (U+200C, U+200D).
pub fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200C}' | '\u{200D}')
}

/// Whether `c` is an invisible or direction-changing format character: soft hyphen (U+00AD),
/// Mongolian vowel separator (U+180E), zero-width space (U+200B), the joiners (U+200C, U+200D),
/// word joiner and invisible operators (U+2060–U+2064), deprecated format controls
/// (U+206A–U+206F), BOM / zero-width no-break space (U+FEFF), interlinear annotation controls
/// (U+FFF9–U+FFFB), and every [bidi control](is_bidi_control).
pub fn is_invisible(c: char) -> bool {
    is_bidi_control(c)
        || is_joiner(c)
        || matches!(c, '\u{00AD}' | '\u{180E}' | '\u{200B}' | '\u{2060}'..='\u{2064}' | '\u{206A}'..='\u{206F}' | '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}')
}

/// What is wrong with a name-like text (display name, email): `None` if fine, otherwise the reason.
/// No control characters, no [invisible](is_invisible) characters.
pub fn name_problem(text: &str) -> Option<&'static str> {
    if text.chars().any(char::is_control) {
        Some("contains control characters")
    } else if text.chars().any(is_invisible) {
        Some("contains invisible or direction-changing characters")
    } else {
        None
    }
}

/// What is wrong with a chat message text: `None` if fine, otherwise the reason. Control characters
/// other than `\n` and `\t` are refused, and [invisible](is_invisible) characters other than the
/// joiners.
pub fn message_problem(text: &str) -> Option<&'static str> {
    if text.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        Some("contains control characters (only line breaks and tabs are allowed)")
    } else if text.chars().any(|c| is_invisible(c) && !is_joiner(c)) {
        Some("contains invisible or direction-changing characters")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert_eq!(name_problem("Ada Lovelace"), None);
        assert_eq!(name_problem("Zoë 🙂"), None);
        for bad in ["a\0b", "a\u{7}", "a\u{7F}", "a\u{85}", "\u{200B}", "\u{202E}evil", "\u{FEFF}x", "a\u{200D}b", "a\u{00AD}b", "a\u{2066}b"] {
            assert!(name_problem(bad).is_some(), "{bad:?}");
        }
        assert_eq!(message_problem("line 1\nline 2\tend"), None);
        assert_eq!(message_problem("family: 👨\u{200D}👩\u{200D}👧"), None);
        for bad in ["a\0", "bell\u{7}", "cr\r\n", "\u{202E}gnp.exe", "zw\u{200B}sp", "\u{FEFF}"] {
            assert!(message_problem(bad).is_some(), "{bad:?}");
        }
    }
}
