//! Character rules shared by the `validate()` methods: which characters a name, an email address
//! or a chat message may not contain.
//!
//! - **Control characters** (Unicode category Cc: C0 incl. NUL, DEL, C1): never in names, keys,
//!   emails or passwords (PostgreSQL rejects NUL in text; terminals and logs misbehave). Chat text
//!   allows `\n` and `\t` only (multi-line messages); `\r` is not allowed (send `\n`).
//! - **Invisible and direction-changing format characters** (the list in [`is_invisible`]: bidi
//!   controls, zero-width space / joiners, word joiner, BOM, soft hyphen, blank fillers, …): never
//!   in display names or emails (they allow impersonation: `\u{202E}` reverses the rest of a
//!   name). Chat text rejects them too, except the zero-width joiner / non-joiner (U+200D,
//!   U+200C), which emoji sequences and several scripts need.
//! - **Tag characters** (U+E0000–U+E007F): never in names; in chat text only as part of an emoji
//!   tag sequence (a subdivision flag).
//! - **Chat text must show something:** at least one character that is not white space, a joiner,
//!   a tag, a variation selector or a combining mark; and at most [`MAX_COMBINING_RUN`] combining
//!   marks in a row (no "zalgo" stacks that draw over the lines around them).

/// Whether `c` is a bidirectional control (U+061C, U+200E, U+200F, U+202A–U+202E, U+2066–U+2069).
pub fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Whether `c` is the zero-width non-joiner or joiner (U+200C, U+200D).
pub fn is_joiner(c: char) -> bool {
    matches!(c, '\u{200C}' | '\u{200D}')
}

/// Whether `c` is an invisible or direction-changing format character: soft hyphen (U+00AD),
/// combining grapheme joiner (U+034F), the Hangul fillers (U+115F, U+1160, U+3164, U+FFA0),
/// Mongolian vowel separator (U+180E), zero-width space (U+200B), the joiners (U+200C, U+200D),
/// word joiner and invisible operators (U+2060–U+2064), deprecated format controls
/// (U+206A–U+206F), the braille blank (U+2800), BOM / zero-width no-break space (U+FEFF),
/// interlinear annotation controls (U+FFF9–U+FFFB), and every [bidi control](is_bidi_control).
pub fn is_invisible(c: char) -> bool {
    is_bidi_control(c)
        || is_joiner(c)
        || matches!(
            c,
            '\u{00AD}'
                | '\u{034F}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{180E}'
                | '\u{200B}'
                | '\u{2060}'..='\u{2064}'
                | '\u{206A}'..='\u{206F}'
                | '\u{2800}'
                | '\u{3164}'
                | '\u{FEFF}'
                | '\u{FFA0}'
                | '\u{FFF9}'..='\u{FFFB}'
        )
}

/// Whether `c` is a tag character (U+E0000–U+E007F; emoji subdivision flags use them).
pub fn is_tag(c: char) -> bool {
    matches!(c, '\u{E0000}'..='\u{E007F}')
}

/// Whether `c` is a variation selector (U+FE00–U+FE0F, U+E0100–U+E01EF).
pub fn is_variation_selector(c: char) -> bool {
    matches!(c, '\u{FE00}'..='\u{FE0F}' | '\u{E0100}'..='\u{E01EF}')
}

/// Whether `c` is in one of the blocks of combining diacritical marks (U+0300–U+036F,
/// U+1AB0–U+1AFF, U+1DC0–U+1DFF, U+20D0–U+20FF, U+FE20–U+FE2F): the marks that stack.
pub fn is_combining_mark(c: char) -> bool {
    matches!(c, '\u{0300}'..='\u{036F}' | '\u{1AB0}'..='\u{1AFF}' | '\u{1DC0}'..='\u{1DFF}' | '\u{20D0}'..='\u{20FF}' | '\u{FE20}'..='\u{FE2F}')
}

/// The most [combining marks](is_combining_mark) in a row a chat message may hold.
pub const MAX_COMBINING_RUN: usize = 8;

/// Whether `c` shows nothing on its own: white space, a joiner, a tag, a variation selector, a
/// combining mark, or an [invisible](is_invisible) character.
fn shows_nothing(c: char) -> bool {
    c.is_whitespace() || is_invisible(c) || is_tag(c) || is_variation_selector(c) || is_combining_mark(c)
}

/// What is wrong with a name-like text (display name, email): `None` if fine, otherwise the reason.
/// No control characters, no [invisible](is_invisible) characters, no [tags](is_tag).
pub fn name_problem(text: &str) -> Option<&'static str> {
    if text.chars().any(char::is_control) {
        Some("contains control characters")
    } else if text.chars().any(|c| is_invisible(c) || is_tag(c)) {
        Some("contains invisible or direction-changing characters")
    } else {
        None
    }
}

/// What is wrong with a chat message text: `None` if fine, otherwise the reason. Control characters
/// other than `\n` and `\t` are refused, [invisible](is_invisible) characters other than the
/// joiners, a text that shows nothing (only white space, joiners, tags, variation selectors or
/// combining marks), and more than [`MAX_COMBINING_RUN`] combining marks in a row.
pub fn message_problem(text: &str) -> Option<&'static str> {
    if text.chars().any(|c| c.is_control() && c != '\n' && c != '\t') {
        return Some("contains control characters (only line breaks and tabs are allowed)");
    }
    if text.chars().any(|c| is_invisible(c) && !is_joiner(c)) {
        return Some("contains invisible or direction-changing characters");
    }
    if text.chars().all(shows_nothing) {
        return Some("shows nothing (only spaces, joiners or marks)");
    }
    let mut run = 0usize;
    for c in text.chars() {
        run = if is_combining_mark(c) { run + 1 } else { 0 };
        if run > MAX_COMBINING_RUN {
            return Some("stacks too many combining marks");
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert_eq!(name_problem("Ada Lovelace"), None);
        assert_eq!(name_problem("Zoë 🙂"), None);
        for bad in [
            "a\0b",
            "a\u{7}",
            "a\u{7F}",
            "a\u{85}",
            "\u{200B}",
            "\u{202E}evil",
            "\u{FEFF}x",
            "a\u{200D}b",
            "a\u{00AD}b",
            "a\u{2066}b",
            "\u{3164}",
            "\u{115F}",
            "\u{2800}",
            "a\u{E0041}",
        ] {
            assert!(name_problem(bad).is_some(), "{bad:?}");
        }
        assert_eq!(message_problem("line 1\nline 2\tend"), None);
        assert_eq!(message_problem("family: 👨\u{200D}👩\u{200D}👧"), None);
        // A subdivision flag (tags), a keycap (variation selector + combining mark), accents.
        for good in ["\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}", "1\u{FE0F}\u{20E3}", "cafe\u{301} na\u{303}o"] {
            assert_eq!(message_problem(good), None, "{good:?}");
        }
        let zalgo = format!("z{}", "\u{0301}".repeat(MAX_COMBINING_RUN + 1));
        let fine = format!("z{}", "\u{0301}".repeat(MAX_COMBINING_RUN));
        assert_eq!(message_problem(&fine), None);
        for bad in [
            "a\0",
            "bell\u{7}",
            "cr\r\n",
            "\u{202E}gnp.exe",
            "zw\u{200B}sp",
            "\u{FEFF}",
            "\u{200D}",
            "\u{200D}\u{200C}\u{200D}",
            "\u{3164}",
            "\u{115F}",
            "\u{2800}",
            "\u{E0041}\u{E0042}",
            "\u{0301}\u{0302}",
            "\u{FE0F}",
            zalgo.as_str(),
        ] {
            assert!(message_problem(bad).is_some(), "{bad:?}");
        }
    }
}
