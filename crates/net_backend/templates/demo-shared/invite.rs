//! Lobby join codes through Steam. A lobby's 8-character code travels as its number (below 2^40)
//! in the connect string `+nb_lobby <number>`: Steam's "Join Game" (rich presence `connect`), game
//! invites and the command line of a game Steam starts. The demo's `steam` feature uses it; each
//! demo uses a part of it.
#![allow(dead_code)]

use {{protocol}}::lobbies::LobbyCode;

/// The first word of the connect string.
pub const CONNECT_PREFIX: &str = "+nb_lobby";

/// The connect string of a code: `+nb_lobby 590122524587` for `K7M2-Q9XD`.
pub fn connect_string(code: &LobbyCode) -> String {
    format!("{CONNECT_PREFIX} {}", code.to_u64())
}

/// The code of a number (a join request's lobby number); `None` for 0 and for 2^40 and above.
pub fn code_of(number: u64) -> Option<LobbyCode> {
    if number == 0 {
        return None;
    }
    LobbyCode::from_u64(number)
}

/// The code a connect string, a command line or the program's arguments (joined with spaces)
/// carry: `+nb_lobby <number>` or `+nb_lobby=<number>` anywhere (the rule of bevy_steam_kit's
/// parser). `None` when it is missing or the number is not a code.
pub fn code_in(text: &str) -> Option<LobbyCode> {
    let mut words = text.split_whitespace();
    while let Some(word) = words.next() {
        let word = word.trim_matches('"');
        let number = if word == CONNECT_PREFIX {
            words.next()?
        } else if let Some(rest) = word
            .strip_prefix(CONNECT_PREFIX)
            .and_then(|r| r.strip_prefix('='))
        {
            rest
        } else {
            continue;
        };
        return number
            .trim_matches('"')
            .parse::<u64>()
            .ok()
            .and_then(code_of);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_travel_as_numbers() {
        let code = LobbyCode::parse("K7M2-Q9XD").unwrap();
        assert_eq!(connect_string(&code), "+nb_lobby 590122524587");
        assert_eq!(code_of(590122524587), Some(code.clone()));
        assert_eq!(code_in("+nb_lobby 590122524587"), Some(code.clone()));
        assert_eq!(code_in("+nb_lobby=590122524587"), Some(code.clone()));
        assert_eq!(
            code_in("demo.exe -x \"+nb_lobby\" \"590122524587\" --y"),
            Some(code.clone())
        );
        assert_eq!(code_in(&connect_string(&code)), Some(code));
        // Every code survives the trip.
        for number in [1u64, 31, 32, 1 << 20, (1 << 40) - 1] {
            let code = code_of(number).unwrap();
            assert_eq!(code.to_u64(), number);
            assert_eq!(code_in(&connect_string(&code)), Some(code));
        }
    }

    #[test]
    fn what_is_not_a_code() {
        assert_eq!(code_of(0), None);
        assert_eq!(code_of(1 << 40), None);
        for text in [
            "",
            "+nb_lobby",
            "+nb_lobby 0",
            "+nb_lobby K7M2Q9XD",
            "+nb_lobby -5",
            "+nb_lobby 1099511627776",
            "+connect_lobby 590122524587",
            "+nb_lobbyx 590122524587",
            "nb_lobby 590122524587",
        ] {
            assert_eq!(code_in(text), None, "{text}");
        }
    }
}
