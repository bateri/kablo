//! Keys as the bytes a terminal sends for them, in the terminal's current modes.
//!
//! A program that is not attached to anything sees a key the way it would see it from a
//! terminal: arrows are `ESC [ A` or `ESC O A` by the cursor-key mode, and a program that
//! turned the kitty keyboard protocol on gets `CSI u`. The modes are the screen's
//! ([`Screen::modes`](crate::Screen::modes)), which follows what the program wrote; there
//! is no way to set one from outside.

use std::ops::BitOr;

use alacritty_terminal::term::TermMode;

/// A key. Text keys are named by the character they type without shift (`a`, `1`, `;`,
/// space); an upper-case letter is that key with shift. Enter, Tab, Escape and Backspace
/// are keys of their own: the protocol numbers them 13, 9, 27 and 127.
///
/// Not here: keypad keys, the function keys past F12, the lock and media keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A key that types a character. A control character is no key.
    Char(char),
    Enter,
    Tab,
    Escape,
    Backspace,
    Insert,
    Delete,
    Up,
    Down,
    Right,
    Left,
    PageUp,
    PageDown,
    Home,
    End,
    /// F1 through F12.
    F(u8),
}

/// The modifiers held with a key, in the kitty protocol's bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Mods = Mods(0);
    pub const SHIFT: Mods = Mods(1);
    pub const ALT: Mods = Mods(2);
    pub const CTRL: Mods = Mods(4);
    pub const SUPER: Mods = Mods(8);
    pub const HYPER: Mods = Mods(16);
    pub const META: Mods = Mods(32);

    /// The modifiers of kitty's bits; the lock bits and bits that are no modifier are
    /// dropped.
    pub const fn from_bits(bits: u8) -> Mods {
        Mods(bits & 0x3f)
    }

    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Whether every modifier of `other` is held.
    pub const fn contains(self, other: Mods) -> bool {
        self.0 & other.0 == other.0
    }

    const fn is_empty(self) -> bool {
        self.0 == 0
    }

    const fn without(self, other: Mods) -> Mods {
        Mods(self.0 & !other.0)
    }

    /// The modifiers as a CSI parameter: one more than the bits.
    fn param(self) -> u32 {
        1 + u32::from(self.0)
    }
}

impl BitOr for Mods {
    type Output = Mods;

    fn bitor(self, other: Mods) -> Mods {
        Mods(self.0 | other.0)
    }
}

/// The modes of a terminal that decide which bytes a key is. Read from a screen
/// ([`Screen::modes`](crate::Screen::modes)), never written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Modes {
    /// DECCKM (`CSI ? 1 h`): the cursor keys send `ESC O` where they send `CSI`.
    pub application_cursor: bool,
    /// DECKPAM (`ESC =`): the keypad sends application sequences.
    pub application_keypad: bool,
    /// The kitty keyboard flags in force, as that protocol numbers them (the
    /// `KITTY_*` constants); `0` is a terminal the program did not ask anything of.
    pub kitty: u8,
}

impl Modes {
    pub const KITTY_DISAMBIGUATE: u8 = 1;
    pub const KITTY_EVENT_TYPES: u8 = 2;
    pub const KITTY_ALTERNATE_KEYS: u8 = 4;
    pub const KITTY_ALL_KEYS: u8 = 8;
    pub const KITTY_ASSOCIATED_TEXT: u8 = 16;

    pub(crate) fn of(mode: TermMode) -> Modes {
        let kitty = [
            (TermMode::DISAMBIGUATE_ESC_CODES, Self::KITTY_DISAMBIGUATE),
            (TermMode::REPORT_EVENT_TYPES, Self::KITTY_EVENT_TYPES),
            (TermMode::REPORT_ALTERNATE_KEYS, Self::KITTY_ALTERNATE_KEYS),
            (TermMode::REPORT_ALL_KEYS_AS_ESC, Self::KITTY_ALL_KEYS),
            (
                TermMode::REPORT_ASSOCIATED_TEXT,
                Self::KITTY_ASSOCIATED_TEXT,
            ),
        ]
        .into_iter()
        .filter(|(bit, _)| mode.contains(*bit))
        .fold(0, |all, (_, flag)| all | flag);
        Modes {
            application_cursor: mode.contains(TermMode::APP_CURSOR),
            application_keypad: mode.contains(TermMode::APP_KEYPAD),
            kitty,
        }
    }

    fn has(self, flag: u8) -> bool {
        self.kitty & flag != 0
    }
}

const ESC: u8 = 0x1b;

impl Key {
    /// The bytes a terminal in `modes` sends for this key pressed with `mods`; `None`
    /// for a key that has none (a control character, an F key past F12).
    ///
    /// Without a kitty flag the bytes are the traditional ones (alt is an `ESC` before,
    /// ctrl on a letter the control character, shift on Tab `CSI Z`; a modifier that has
    /// no traditional form is dropped). With the disambiguate flag the keys that would
    /// collide with other sequences (Escape, anything with ctrl, alt or super) are
    /// `CSI u`; Enter, Tab and Backspace stay what they were unless a modifier is held,
    /// so that a shell can be typed to after a program that set the flag died. With
    /// all-keys every key is. Only presses are written; the flags for key releases and
    /// repeats, and for alternate keys, ask for more than a key sent once can say.
    pub fn encode(self, mods: Mods, modes: &Modes) -> Option<Vec<u8>> {
        match self {
            Key::Char(c) => text_key(c, mods, modes),
            Key::Enter | Key::Tab | Key::Escape | Key::Backspace => {
                Some(editing_key(self, mods, modes))
            }
            _ => functional_key(self, mods, modes),
        }
    }
}

/// A key that types a character.
fn text_key(c: char, mods: Mods, modes: &Modes) -> Option<Vec<u8>> {
    if c.is_control() {
        return None;
    }
    let typed = Typed::of(c, mods);
    if modes.has(Modes::KITTY_ALL_KEYS) {
        let text = if modes.has(Modes::KITTY_ASSOCIATED_TEXT) {
            typed.text()
        } else {
            None
        };
        return Some(csi_u(u32::from(typed.key), typed.mods, text));
    }
    if modes.has(Modes::KITTY_DISAMBIGUATE) {
        return Some(match typed.text() {
            Some(text) => utf8(text),
            None => csi_u(u32::from(typed.key), typed.mods, None),
        });
    }
    let mut out = Vec::new();
    if typed.mods.contains(Mods::ALT) {
        out.push(ESC);
    }
    let control = if typed.mods.contains(Mods::CTRL) {
        control_byte(typed.key)
    } else {
        None
    };
    match control {
        Some(byte) => out.push(byte),
        None => out.extend(utf8(typed.shifted.unwrap_or(typed.key))),
    }
    Some(out)
}

/// A character key as the protocol sees it: the key (the character without shift), the
/// modifiers with the shift an upper-case letter carries, and the upper-case letter shift
/// makes of a lower-case one.
struct Typed {
    key: char,
    mods: Mods,
    shifted: Option<char>,
}

impl Typed {
    fn of(c: char, mods: Mods) -> Self {
        let lower = single(c.to_lowercase()).filter(|lower| *lower != c);
        let upper = single(c.to_uppercase()).filter(|upper| *upper != c);
        match (lower, upper) {
            (Some(lower), _) => Self {
                key: lower,
                mods: mods | Mods::SHIFT,
                shifted: Some(c),
            },
            (None, Some(upper)) if mods.contains(Mods::SHIFT) => Self {
                key: c,
                mods,
                shifted: Some(upper),
            },
            (None, _) => Self {
                key: c,
                mods,
                shifted: None,
            },
        }
    }

    /// The text the key types with these modifiers, when that is known: not with a
    /// modifier besides shift, and not with shift on a key that is no letter (what that
    /// types is the layout's to say).
    fn text(&self) -> Option<char> {
        if !self.mods.without(Mods::SHIFT).is_empty() {
            return None;
        }
        match (self.mods.contains(Mods::SHIFT), self.shifted) {
            (false, _) => Some(self.key),
            (true, shifted) => shifted,
        }
    }
}

/// The one character an iterator of case mappings yields, if it yields one.
fn single(mut chars: impl Iterator<Item = char>) -> Option<char> {
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// The control character a key makes with ctrl held, in the traditional encoding.
fn control_byte(key: char) -> Option<u8> {
    match key {
        'a'..='z' => u8::try_from(key).ok().map(|byte| byte & 0x1f),
        ' ' | '@' => Some(0),
        '[' => Some(27),
        '\\' => Some(28),
        ']' => Some(29),
        '^' => Some(30),
        '_' => Some(31),
        '?' => Some(127),
        _ => None,
    }
}

/// Enter, Tab, Escape and Backspace.
fn editing_key(key: Key, mods: Mods, modes: &Modes) -> Vec<u8> {
    let code = match key {
        Key::Enter => 13,
        Key::Tab => 9,
        Key::Escape => 27,
        _ => 127,
    };
    let stays_legacy = key != Key::Escape && mods.is_empty();
    if modes.has(Modes::KITTY_ALL_KEYS) || (modes.has(Modes::KITTY_DISAMBIGUATE) && !stays_legacy) {
        return csi_u(code, mods, None);
    }
    let mut out = Vec::new();
    if mods.contains(Mods::ALT) {
        out.push(ESC);
    }
    match key {
        Key::Enter => out.push(b'\r'),
        Key::Tab if mods.contains(Mods::SHIFT) => out.extend_from_slice(b"\x1b[Z"),
        Key::Tab => out.push(b'\t'),
        Key::Escape => out.push(ESC),
        _ if mods.contains(Mods::CTRL) => out.push(0x08),
        _ => out.push(0x7f),
    }
    out
}

/// How a key that is not text is written: `CSI 1 ; mods letter` (or `ESC O letter`
/// without modifiers where the mode says) or `CSI number ; mods ~`.
enum Form {
    Letter(u8),
    Tilde(u8),
}

fn functional_key(key: Key, mods: Mods, modes: &Modes) -> Option<Vec<u8>> {
    // The second field: whether the unmodified key is `ESC O` rather than `CSI`.
    let cursor = modes.application_cursor;
    let (form, ss3) = match key {
        Key::Up => (Form::Letter(b'A'), cursor),
        Key::Down => (Form::Letter(b'B'), cursor),
        Key::Right => (Form::Letter(b'C'), cursor),
        Key::Left => (Form::Letter(b'D'), cursor),
        Key::Home => (Form::Letter(b'H'), cursor),
        Key::End => (Form::Letter(b'F'), cursor),
        Key::Insert => (Form::Tilde(2), false),
        Key::Delete => (Form::Tilde(3), false),
        Key::PageUp => (Form::Tilde(5), false),
        Key::PageDown => (Form::Tilde(6), false),
        Key::F(1) => (Form::Letter(b'P'), true),
        Key::F(2) => (Form::Letter(b'Q'), true),
        Key::F(3) => (Form::Letter(b'R'), true),
        Key::F(4) => (Form::Letter(b'S'), true),
        Key::F(5) => (Form::Tilde(15), false),
        Key::F(6) => (Form::Tilde(17), false),
        Key::F(7) => (Form::Tilde(18), false),
        Key::F(8) => (Form::Tilde(19), false),
        Key::F(9) => (Form::Tilde(20), false),
        Key::F(10) => (Form::Tilde(21), false),
        Key::F(11) => (Form::Tilde(23), false),
        Key::F(12) => (Form::Tilde(24), false),
        _ => return None,
    };
    let sequence = match (form, mods.is_empty()) {
        (Form::Letter(letter), true) => {
            let intro = if ss3 { 'O' } else { '[' };
            format!("\x1b{intro}{}", char::from(letter))
        }
        // `CSI 1 ; mods R` is also what the cursor position report looks like, so with
        // the keyboard protocol on F3 is the other of its two forms.
        (Form::Letter(b'R'), false) if modes.has(Modes::KITTY_DISAMBIGUATE) => {
            format!("\x1b[13;{}~", mods.param())
        }
        (Form::Letter(letter), false) => {
            format!("\x1b[1;{}{}", mods.param(), char::from(letter))
        }
        (Form::Tilde(number), true) => format!("\x1b[{number}~"),
        (Form::Tilde(number), false) => format!("\x1b[{number};{}~", mods.param()),
    };
    Some(sequence.into_bytes())
}

/// `CSI code ; mods ; text u`, the parts a key does not have left out.
fn csi_u(code: u32, mods: Mods, text: Option<char>) -> Vec<u8> {
    let sequence = match (mods.is_empty(), text) {
        (true, None) => format!("\x1b[{code}u"),
        (false, None) => format!("\x1b[{code};{}u", mods.param()),
        (true, Some(text)) => format!("\x1b[{code};;{}u", u32::from(text)),
        (false, Some(text)) => format!("\x1b[{code};{};{}u", mods.param(), u32::from(text)),
    };
    sequence.into_bytes()
}

fn utf8(c: char) -> Vec<u8> {
    c.encode_utf8(&mut [0; 4]).as_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Screen;

    /// The modes a screen has after a program wrote `bytes`: the only way to set one.
    fn after(bytes: &str) -> Modes {
        let mut screen = Screen::new(10, 3, 0);
        screen.feed(bytes.as_bytes());
        screen.modes()
    }

    fn sent(key: Key, mods: Mods, modes: &Modes) -> String {
        let bytes = key.encode(mods, modes).unwrap_or_default();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn plain(key: Key, modes: &Modes) -> String {
        sent(key, Mods::NONE, modes)
    }

    const CTRL: Mods = Mods::CTRL;
    const ALT: Mods = Mods::ALT;
    const SHIFT: Mods = Mods::SHIFT;

    #[test]
    fn a_fresh_screen_has_no_mode_set() {
        assert_eq!(after(""), Modes::default());
    }

    #[test]
    fn modes_follow_what_the_program_wrote() {
        let cursor = after("\x1b[?1h");
        assert!(cursor.application_cursor && !cursor.application_keypad);
        assert!(!after("\x1b[?1h\x1b[?1l").application_cursor);
        assert!(after("\x1b=").application_keypad);
        assert!(!after("\x1b=\x1b>").application_keypad);

        assert_eq!(after("\x1b[>1u").kitty, Modes::KITTY_DISAMBIGUATE);
        assert_eq!(after("\x1b[=9;1u").kitty, 9);
        assert_eq!(after("\x1b[=31;1u").kitty, 31);
        assert_eq!(after("\x1b[>1u\x1b[>8u").kitty, Modes::KITTY_ALL_KEYS);
        assert_eq!(after("\x1b[>1u\x1b[>8u\x1b[<u").kitty, 1);
        assert_eq!(after("\x1b[>1u\x1b[<u").kitty, 0);
    }

    #[test]
    fn arrows_follow_the_cursor_key_mode() {
        let normal = Modes::default();
        let application = after("\x1b[?1h");
        let cases = [
            (Key::Up, "A"),
            (Key::Down, "B"),
            (Key::Right, "C"),
            (Key::Left, "D"),
            (Key::Home, "H"),
            (Key::End, "F"),
        ];
        for (key, letter) in cases {
            assert_eq!(plain(key, &normal), format!("\x1b[{letter}"));
            assert_eq!(plain(key, &application), format!("\x1bO{letter}"));
            // A modifier is a parameter, in either mode.
            assert_eq!(sent(key, CTRL, &normal), format!("\x1b[1;5{letter}"));
            assert_eq!(sent(key, CTRL, &application), format!("\x1b[1;5{letter}"));
        }
        assert_eq!(sent(Key::Up, SHIFT | ALT, &normal), "\x1b[1;4A");
    }

    #[test]
    fn the_tilde_keys_and_function_keys() {
        let modes = Modes::default();
        let tilde = [
            (Key::Insert, 2),
            (Key::Delete, 3),
            (Key::PageUp, 5),
            (Key::PageDown, 6),
            (Key::F(5), 15),
            (Key::F(6), 17),
            (Key::F(7), 18),
            (Key::F(8), 19),
            (Key::F(9), 20),
            (Key::F(10), 21),
            (Key::F(11), 23),
            (Key::F(12), 24),
        ];
        for (key, number) in tilde {
            assert_eq!(plain(key, &modes), format!("\x1b[{number}~"));
            assert_eq!(sent(key, SHIFT, &modes), format!("\x1b[{number};2~"));
        }
        let ss3 = [(1, "P"), (2, "Q"), (3, "R"), (4, "S")];
        for (n, letter) in ss3 {
            // The cursor-key mode is not theirs.
            for modes in [Modes::default(), after("\x1b[?1h")] {
                assert_eq!(plain(Key::F(n), &modes), format!("\x1bO{letter}"));
            }
            assert_eq!(
                sent(Key::F(n), CTRL, &Modes::default()),
                format!("\x1b[1;5{letter}")
            );
        }
    }

    #[test]
    fn keys_that_have_no_bytes_have_none() {
        let modes = Modes::default();
        for key in [Key::F(0), Key::F(13), Key::Char('\n'), Key::Char('\x1b')] {
            assert_eq!(key.encode(Mods::NONE, &modes), None);
        }
        let kitty = after("\x1b[=31;1u");
        assert_eq!(Key::F(13).encode(Mods::NONE, &kitty), None);
        assert_eq!(Key::Char('\t').encode(CTRL, &kitty), None);
    }

    #[test]
    fn without_a_kitty_flag_keys_are_the_traditional_bytes() {
        let m = Modes::default();
        assert_eq!(plain(Key::Enter, &m), "\r");
        assert_eq!(plain(Key::Tab, &m), "\t");
        assert_eq!(plain(Key::Escape, &m), "\x1b");
        assert_eq!(plain(Key::Backspace, &m), "\x7f");
        assert_eq!(sent(Key::Backspace, CTRL, &m), "\x08");
        assert_eq!(sent(Key::Tab, SHIFT, &m), "\x1b[Z");
        assert_eq!(sent(Key::Enter, ALT, &m), "\x1b\r");
        assert_eq!(sent(Key::Escape, ALT, &m), "\x1b\x1b");
        assert_eq!(plain(Key::Char('a'), &m), "a");
        assert_eq!(plain(Key::Char(' '), &m), " ");
        assert_eq!(plain(Key::Char('\u{e9}'), &m), "\u{e9}");
        assert_eq!(sent(Key::Char('a'), SHIFT, &m), "A");
        assert_eq!(plain(Key::Char('A'), &m), "A");
        assert_eq!(sent(Key::Char('1'), SHIFT, &m), "1");
        assert_eq!(sent(Key::Char('c'), CTRL, &m), "\x03");
        assert_eq!(sent(Key::Char('C'), CTRL, &m), "\x03");
        assert_eq!(sent(Key::Char('z'), CTRL, &m), "\x1a");
        assert_eq!(sent(Key::Char('['), CTRL, &m), "\x1b");
        assert_eq!(sent(Key::Char('\\'), CTRL, &m), "\x1c");
        assert_eq!(sent(Key::Char(' '), CTRL, &m), "\0");
        assert_eq!(sent(Key::Char('x'), ALT, &m), "\x1bx");
        assert_eq!(sent(Key::Char('x'), ALT | CTRL, &m), "\x1b\x18");
        assert_eq!(sent(Key::Char('x'), ALT | SHIFT, &m), "\x1bX");
        // A modifier with no traditional form is dropped, and so is ctrl on a key that
        // makes no control character.
        assert_eq!(sent(Key::Char('x'), Mods::SUPER, &m), "x");
        assert_eq!(sent(Key::Char('1'), CTRL, &m), "1");
    }

    #[test]
    fn the_mode_bits_other_than_the_kitty_ones_change_no_text_key() {
        // Nothing about the keypad or the cursor keys is a text key's business.
        let m = after("\x1b[?1h\x1b=");
        assert_eq!(plain(Key::Char('a'), &m), "a");
        assert_eq!(plain(Key::Enter, &m), "\r");
        assert_eq!(sent(Key::Char('c'), CTRL, &m), "\x03");
    }

    #[test]
    fn with_the_disambiguate_flag_ctrl_alt_and_escape_are_csi_u() {
        let m = after("\x1b[>1u");
        assert_eq!(sent(Key::Char('c'), CTRL, &m), "\x1b[99;5u");
        assert_eq!(sent(Key::Char('C'), CTRL, &m), "\x1b[99;6u");
        assert_eq!(sent(Key::Char('x'), ALT, &m), "\x1b[120;3u");
        assert_eq!(sent(Key::Char('x'), CTRL | ALT, &m), "\x1b[120;7u");
        assert_eq!(sent(Key::Char(' '), CTRL, &m), "\x1b[32;5u");
        assert_eq!(sent(Key::Char('x'), Mods::SUPER, &m), "\x1b[120;9u");
        assert_eq!(plain(Key::Escape, &m), "\x1b[27u");
        assert_eq!(sent(Key::Escape, ALT, &m), "\x1b[27;3u");
        // What types text is text.
        assert_eq!(plain(Key::Char('a'), &m), "a");
        assert_eq!(sent(Key::Char('a'), SHIFT, &m), "A");
        assert_eq!(plain(Key::Char('\u{e9}'), &m), "\u{e9}");
        // Shift on a key whose shifted character the layout decides is said exactly.
        assert_eq!(sent(Key::Char('1'), SHIFT, &m), "\x1b[49;2u");
    }

    #[test]
    fn with_the_disambiguate_flag_enter_tab_and_backspace_stay_what_a_shell_reads() {
        let m = after("\x1b[>1u");
        assert_eq!(plain(Key::Enter, &m), "\r");
        assert_eq!(plain(Key::Tab, &m), "\t");
        assert_eq!(plain(Key::Backspace, &m), "\x7f");
        // With a modifier they are the escape codes the program asked to tell apart.
        assert_eq!(sent(Key::Enter, SHIFT, &m), "\x1b[13;2u");
        assert_eq!(sent(Key::Enter, CTRL, &m), "\x1b[13;5u");
        assert_eq!(sent(Key::Tab, SHIFT, &m), "\x1b[9;2u");
        assert_eq!(sent(Key::Backspace, CTRL, &m), "\x1b[127;5u");
    }

    #[test]
    fn with_the_disambiguate_flag_the_other_keys_keep_their_forms() {
        let m = after("\x1b[>1u");
        assert_eq!(plain(Key::Up, &m), "\x1b[A");
        assert_eq!(plain(Key::Up, &after("\x1b[>1u\x1b[?1h")), "\x1bOA");
        assert_eq!(sent(Key::Left, CTRL, &m), "\x1b[1;5D");
        assert_eq!(sent(Key::Delete, CTRL, &m), "\x1b[3;5~");
        assert_eq!(plain(Key::F(1), &m), "\x1bOP");
        assert_eq!(plain(Key::F(3), &m), "\x1bOR");
        // `CSI 1 ; 5 R` would read as a cursor position report.
        assert_eq!(sent(Key::F(3), CTRL, &m), "\x1b[13;5~");
        assert_eq!(sent(Key::F(4), CTRL, &m), "\x1b[1;5S");
    }

    #[test]
    fn with_the_all_keys_flag_every_key_is_csi_u() {
        let m = after("\x1b[>8u");
        assert_eq!(plain(Key::Enter, &m), "\x1b[13u");
        assert_eq!(plain(Key::Tab, &m), "\x1b[9u");
        assert_eq!(plain(Key::Escape, &m), "\x1b[27u");
        assert_eq!(plain(Key::Backspace, &m), "\x1b[127u");
        assert_eq!(plain(Key::Char('a'), &m), "\x1b[97u");
        assert_eq!(sent(Key::Char('a'), SHIFT, &m), "\x1b[97;2u");
        assert_eq!(plain(Key::Char('A'), &m), "\x1b[97;2u");
        assert_eq!(sent(Key::Char('c'), CTRL, &m), "\x1b[99;5u");
        assert_eq!(plain(Key::Char('\u{e9}'), &m), "\x1b[233u");
        assert_eq!(sent(Key::Tab, SHIFT, &m), "\x1b[9;2u");
        // The keys that were never text keep their forms.
        assert_eq!(plain(Key::Up, &m), "\x1b[A");
        assert_eq!(plain(Key::F(5), &m), "\x1b[15~");
    }

    #[test]
    fn the_associated_text_flag_adds_the_text_a_key_types() {
        let m = after("\x1b[=24;1u");
        assert_eq!(plain(Key::Char('a'), &m), "\x1b[97;;97u");
        assert_eq!(sent(Key::Char('a'), SHIFT, &m), "\x1b[97;2;65u");
        assert_eq!(plain(Key::Char('A'), &m), "\x1b[97;2;65u");
        // Not for a key that types nothing, or whose text is not known.
        assert_eq!(sent(Key::Char('c'), CTRL, &m), "\x1b[99;5u");
        assert_eq!(sent(Key::Char('1'), SHIFT, &m), "\x1b[49;2u");
        assert_eq!(plain(Key::Enter, &m), "\x1b[13u");
        // The flag alone, without all-keys, asks for nothing.
        assert_eq!(plain(Key::Char('a'), &after("\x1b[=16;1u")), "a");
    }

    #[test]
    fn the_flags_that_are_about_releases_and_alternates_change_no_press() {
        let m = after("\x1b[=7;1u");
        assert_eq!(m.kitty, 7);
        assert_eq!(sent(Key::Char('c'), CTRL, &m), "\x1b[99;5u");
        assert_eq!(plain(Key::Enter, &m), "\r");
        let only_events = after("\x1b[>2u");
        assert_eq!(sent(Key::Char('c'), CTRL, &only_events), "\x03");
    }

    #[test]
    fn a_program_that_popped_its_flags_gets_the_traditional_bytes_again() {
        let m = after("\x1b[>1u\x1b[<u");
        assert_eq!(sent(Key::Char('c'), CTRL, &m), "\x03");
        assert_eq!(plain(Key::Escape, &m), "\x1b");
    }

    #[test]
    fn mods_are_kitty_bits() {
        assert_eq!(Mods::from_bits(0xff).bits(), 0x3f);
        assert_eq!((CTRL | ALT).bits(), 6);
        assert!((CTRL | ALT).contains(ALT));
        assert!(!ALT.contains(CTRL | ALT));
        assert_eq!(Mods::default(), Mods::NONE);
    }
}
