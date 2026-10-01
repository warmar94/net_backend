//! The shared `validate()` rules, especially the character rules (control, invisible and
//! direction-changing characters) and the limits fixed in review round 1.

use net_backend_protocol::auth::{ChangePasswordRequest, LoginRequest, RegisterRequest, SteamLoginRequest, UpdateAccountRequest, STEAM_TICKET_MAX_HEX};
use net_backend_protocol::chat::{JoinRoom, SendMessage, DEFAULT_MAX_TEXT_CHARS};
use net_backend_protocol::storage::{is_valid_name, BatchGet, ObjectRef};
use net_backend_protocol::{codes, Cursor, PageRequest, RoomId, ValidationDetails};

const GOOD_PASSWORD: &str = "correct horse battery";

/// Characters no name, key or email may contain.
const NAME_POISON: &[&str] = &[
    "\0", "\u{1}", "\u{7}", "\u{1B}", "\u{7F}", "\u{85}", "\u{9F}", // C0, DEL, C1
    "\u{200B}", "\u{200C}", "\u{200D}", "\u{2060}", "\u{FEFF}", "\u{00AD}", // zero-width, BOM, soft hyphen
    "\u{202A}", "\u{202B}", "\u{202D}", "\u{202E}", "\u{2066}", "\u{2067}", "\u{2068}", "\u{2069}", "\u{200E}", "\u{200F}", "\u{061C}", // bidi
];

fn fields(result: Result<(), net_backend_protocol::ApiError>) -> Vec<String> {
    match result {
        Ok(()) => Vec::new(),
        Err(error) => {
            assert!(error.is(codes::VALIDATION_FAILED));
            error.details_as::<ValidationDetails>().map(|d| d.fields.into_keys().collect()).unwrap_or_default()
        }
    }
}

#[test]
fn emails_refuse_control_and_invisible_characters() {
    assert!(fields(RegisterRequest::new("ada@example.com", GOOD_PASSWORD).validate()).is_empty());
    for poison in NAME_POISON {
        for email in [format!("a{poison}@example.com"), format!("ada@exam{poison}ple.com")] {
            assert_eq!(fields(RegisterRequest::new(email.clone(), GOOD_PASSWORD).validate()), ["email"], "{email:?}");
        }
    }
}

#[test]
fn display_names_refuse_control_invisible_and_untrimmed() {
    let name = |n: &str| fields(UpdateAccountRequest::new().with_display_name(n).validate());
    for good in ["Ada", "Zoë", "李小龍", "Ada Lovelace", "🙂 smile"] {
        assert!(name(good).is_empty(), "{good:?}");
    }
    for poison in NAME_POISON {
        assert_eq!(name(&format!("Ad{poison}a")), ["display_name"], "{poison:?}");
        assert_eq!(name(poison), ["display_name"], "{poison:?}");
    }
    for bad in ["  Ada  ", " Ada", "Ada\u{3000}", "\u{202E}evil", ""] {
        assert_eq!(name(bad), ["display_name"], "{bad:?}");
    }
    assert_eq!(fields(RegisterRequest::new("a@example.com", GOOD_PASSWORD).with_display_name("\u{FEFF}").validate()), ["display_name"]);
}

#[test]
fn chat_text_allows_line_breaks_and_joiners_only() {
    let text = |t: &str| fields(SendMessage::new(RoomId(1), t).validate(DEFAULT_MAX_TEXT_CHARS));
    for good in ["hello", "line 1\nline 2", "a\tb", "👨\u{200D}👩\u{200D}👧", "क्\u{200D}ष", "Zoë ✨"] {
        assert!(text(good).is_empty(), "{good:?}");
    }
    for bad in ["a\0b", "bell\u{7}", "a\rb", "esc\u{1B}[31m", "c1\u{85}", "\u{202E}txt.exe", "zw\u{200B}sp", "bom\u{FEFF}", "\u{2066}iso"] {
        assert_eq!(text(bad), ["text"], "{bad:?}");
    }
    let flood = "\u{1}".repeat(DEFAULT_MAX_TEXT_CHARS);
    assert_eq!(text(&flood), ["text"]);
}

#[test]
fn room_keys_and_storage_names_are_plain_ascii() {
    for bad in ["\0", "a b/c", "World", "wörld", "w\u{200B}", "-x", ""] {
        assert!(JoinRoom::new(bad).validate().is_err(), "{bad:?}");
    }
    assert!(JoinRoom::new("trade-eu.2").validate().is_ok());
    for bad in ["a\0", "a\u{202E}", "slot 1", "slot\n1"] {
        assert!(!is_valid_name(bad), "{bad:?}");
        assert!(BatchGet::new(vec![ObjectRef::new("saves", bad)]).validate().is_err(), "{bad:?}");
    }
}

#[test]
fn passwords_refuse_control_and_whitespace_only() {
    for bad in [" ".repeat(12), "\0".repeat(12), format!("{GOOD_PASSWORD}\0"), "\t".repeat(12)] {
        assert_eq!(fields(ChangePasswordRequest::new("old", bad.clone()).validate()), ["new_password"], "{bad:?}");
    }
    assert!(fields(ChangePasswordRequest::new("old", "  spaced  out  pass  ").validate()).is_empty());
}

#[test]
fn steam_tickets_up_to_steams_buffer_pass() {
    // GetTicketForWebApiResponse_t::m_rgubTicket holds 2560 bytes = 5120 hex characters.
    let full = "0a".repeat(2560);
    assert!(SteamLoginRequest::new(full, "my-game").validate().is_ok());
    assert!(SteamLoginRequest::new("0".repeat(STEAM_TICKET_MAX_HEX + 2), "my-game").validate().is_err());
}

#[test]
fn cursors_are_bounded() {
    assert!(PageRequest::after(Cursor::new("x".repeat(513))).validate().is_err());
    assert!(PageRequest::first().validate().is_ok());
}

#[test]
fn secret_decode_errors_never_quote_the_value() {
    for json in [
        r#"{"email":"a@example.com","password":98765432109}"#,
        r#"{"email":"a@example.com","password":true}"#,
        r#"{"email":"a@example.com","password":["s3cr3t-in-array"]}"#,
        r#"{"email":"a@example.com","password":{"x":"s3cr3t-in-map"}}"#,
        r#"{"email":"a@example.com","password":-12.5}"#,
    ] {
        let message = serde_json::from_str::<LoginRequest>(json).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(message.contains("a secret must be a JSON string"), "{message}");
        for leaked in ["98765432109", "s3cr3t", "12.5"] {
            assert!(!message.contains(leaked), "{message}");
        }
    }
    assert!(serde_json::from_str::<LoginRequest>(r#"{"email":"a@example.com","password":"fine"}"#).is_ok());
}
