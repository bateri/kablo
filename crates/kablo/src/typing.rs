//! What a `write` types: its strokes as the bytes the program reads.
//!
//! The wire carries a key as a number (the kitty keyboard protocol's, see
//! [`kablo_proto::v1::key`]); the screen knows keys as [`kablo_screen::Key`] and
//! turns one into the bytes a terminal in its present modes would send. This is
//! the join between the two, and the one place that says which numbers a relay can
//! press.

use kablo_proto::v1::{Stroke, key};
use kablo_screen::{Key, Modes, Mods};

/// The bytes that typing `strokes` sends to a program whose terminal is in
/// `modes`, in order. Text is as it is; a key is what the terminal sends for it. A
/// stroke that cannot be typed (a key this server does not press) is an error in
/// words for the sender, and nothing of the strokes is typed.
pub fn bytes(strokes: &[Stroke], modes: &Modes) -> Result<Vec<u8>, String> {
    let mut typed = Vec::new();
    for stroke in strokes {
        match stroke {
            Stroke::Text(text) => typed.extend_from_slice(text),
            Stroke::Key(pressed) => {
                let code = pressed.code;
                let sent = key_of(code)
                    .and_then(|key| key.encode(Mods::from_bits(pressed.mods), modes))
                    .ok_or_else(|| format!("this server cannot press the key numbered {code}"))?;
                typed.extend_from_slice(&sent);
            }
            _ => return Err("this server does not know that way of typing".to_owned()),
        }
    }
    Ok(typed)
}

/// The key a number names, if this server presses it.
fn key_of(code: u32) -> Option<Key> {
    Some(match code {
        key::TAB => Key::Tab,
        key::ENTER => Key::Enter,
        key::ESCAPE => Key::Escape,
        key::BACKSPACE => Key::Backspace,
        key::INSERT => Key::Insert,
        key::DELETE => Key::Delete,
        key::LEFT => Key::Left,
        key::RIGHT => Key::Right,
        key::UP => Key::Up,
        key::DOWN => Key::Down,
        key::PAGE_UP => Key::PageUp,
        key::PAGE_DOWN => Key::PageDown,
        key::HOME => Key::Home,
        key::END => Key::End,
        _ => return function_or_character(code),
    })
}

/// A function key (F1 to F12: the wire numbers up to F35, a terminal has no
/// sequence for the rest) or a key that types a character, which is that
/// character's number and not a control character.
fn function_or_character(code: u32) -> Option<Key> {
    if let Some(n) = (1..=12u8).find(|n| key::function(*n) == Some(code)) {
        return Some(Key::F(n));
    }
    // The private use area holds kitty's numbers for keys that type no
    // character: one that no key above has is not a character either.
    if (0xe000..=0xf8ff).contains(&code) {
        return None;
    }
    char::from_u32(code)
        .filter(|c| !c.is_control())
        .map(Key::Char)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kablo_proto::v1::key::mods;

    fn plain() -> Modes {
        Modes::default()
    }

    #[test]
    fn text_goes_as_it_is_and_keys_as_the_terminal_sends_them() {
        let strokes = [
            Stroke::text("ls -l"),
            Stroke::key(key::ENTER, 0),
            Stroke::text("é".as_bytes()),
        ];
        assert_eq!(bytes(&strokes, &plain()).unwrap(), "ls -l\ré".as_bytes());
        assert_eq!(bytes(&[], &plain()).unwrap(), b"");
    }

    #[test]
    fn a_key_is_what_the_modes_make_it() {
        let up = [Stroke::key(key::UP, 0)];
        assert_eq!(bytes(&up, &plain()).unwrap(), b"\x1b[A");
        let mut application = plain();
        application.application_cursor = true;
        assert_eq!(bytes(&up, &application).unwrap(), b"\x1bOA");
        let ctrl_c = [Stroke::key(u32::from('c'), mods::CTRL)];
        assert_eq!(bytes(&ctrl_c, &plain()).unwrap(), b"\x03");
        let mut kitty = plain();
        kitty.kitty = Modes::KITTY_DISAMBIGUATE;
        assert_eq!(bytes(&ctrl_c, &kitty).unwrap(), b"\x1b[99;5u");
    }

    #[test]
    fn the_function_keys_are_the_numbers_kitty_gives_them() {
        for n in 1..=12u8 {
            let f = [Stroke::key(key::function(n).unwrap(), 0)];
            let typed = bytes(&f, &plain()).unwrap();
            assert_eq!(typed.first(), Some(&0x1b), "F{n}");
        }
        assert_eq!(
            bytes(&[Stroke::key(key::function(1).unwrap(), 0)], &plain()).unwrap(),
            b"\x1bOP"
        );
    }

    #[test]
    fn a_number_that_is_no_key_types_nothing_of_the_write() {
        for code in [
            0,
            1,
            10,
            0x80,
            0x9f,
            key::function(13).unwrap(),
            key::function(35).unwrap(),
            0xe000,
            0xd800,
            0x11_0000,
            u32::MAX,
        ] {
            let strokes = [Stroke::text("before"), Stroke::key(code, 0)];
            let why = bytes(&strokes, &plain()).unwrap_err();
            assert!(why.contains(&code.to_string()), "{code}: {why}");
        }
    }

    #[test]
    fn a_character_is_a_key_whatever_it_is_if_it_types() {
        assert_eq!(
            bytes(&[Stroke::key(u32::from(' '), 0)], &plain()).unwrap(),
            b" "
        );
        assert_eq!(
            bytes(&[Stroke::key(0x1f600, 0)], &plain()).unwrap(),
            "\u{1f600}".as_bytes()
        );
        // Shift on a letter is the upper case of it.
        assert_eq!(
            bytes(&[Stroke::key(u32::from('a'), mods::SHIFT)], &plain()).unwrap(),
            b"A"
        );
    }
}
