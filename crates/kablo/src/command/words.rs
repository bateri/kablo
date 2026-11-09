//! The words of a `bateri` command and the rules they are held to, the same in every verb: what
//! is a pane, an id, a name, a machine, a label. The window reads exactly these rules where it takes
//! the words (`kablo-bateri`), so a word that passes here is one it takes.

use kablo_bateri::is_machine;
use kablo_bateri::message::LABEL_MAX;

use super::Exit;

/// The longest an id or a pane's number may be written, letter included.
const ID_MAX: usize = 19;

/// Whether `word` names an agent's pane: `p` and a number written plainly.
pub fn is_terminal(word: &str) -> bool {
    word == "p0" || is_id('p', word)
}

/// Whether `word` is the name of a window (`w`), a group (`g`), a tab (`t`) or a pane (`p`),
/// `letter` being that letter: the letter and a number written plainly (`t3`).
pub fn is_id(letter: char, word: &str) -> bool {
    let Some(digits) = word.strip_prefix(letter) else {
        return false;
    };
    word.len() <= ID_MAX
        && digits
            .bytes()
            .next()
            .is_some_and(|d| (b'1'..=b'9').contains(&d))
        && digits.bytes().all(|d| d.is_ascii_digit())
}

/// Whether `word` is a name kablo gives a terminal: 1 to 64 of `A-Za-z0-9._-`.
pub fn is_name(word: &str) -> bool {
    !word.is_empty()
        && word.len() <= 64
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// A pane written as `p9`, or `p9@prod` for one of another machine: its id and its machine.
pub fn address(word: &str) -> Option<(&str, Option<&str>)> {
    match word.split_once('@') {
        None => is_terminal(word).then_some((word, None)),
        Some((id, machine)) => {
            (is_machine(machine) && is_terminal(id)).then_some((id, Some(machine)))
        }
    }
}

/// Whether `word` is an id of the kind whose letter is `letter`: a pane may be of another machine.
pub fn is_kind(letter: char, word: &str) -> bool {
    match letter {
        'p' => address(word).is_some(),
        _ => is_id(letter, word),
    }
}

/// Whether `word` reads as the id of a pane, a tab, a group or a window.
pub fn reads_as_id(word: &str) -> bool {
    ['p', 't', 'g', 'w']
        .into_iter()
        .any(|letter| is_kind(letter, word))
}

/// Text from outside, safe to print on a terminal: no control character but the line break and
/// the tab.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(u32::from(*c), 0..=8 | 11..=31 | 127..=159))
        .collect()
}

/// A label the window and kablo take: 1 to 64 bytes of one line of plain text, not only spaces,
/// and no id (`tab --new-group g1` means the group `g1` or is a slip for it: a name is never taken
/// for an id).
pub fn check_label(what: &str, text: &str) -> Result<(), Exit> {
    if text.is_empty() || text.len() > LABEL_MAX {
        return Err(Exit::mistake(format!("{what} is 1 to {LABEL_MAX} bytes")));
    }
    if text.chars().any(|c| matches!(u32::from(c), 0..=31 | 127)) {
        return Err(Exit::mistake(format!("{what} is one line of plain text")));
    }
    if text.chars().all(|c| c == ' ') {
        return Err(Exit::mistake(format!("{what} is not only spaces")));
    }
    if reads_as_id(text) {
        return Err(Exit::mistake(format!(
            "{what} '{text}' reads as an id: give it a name that does not"
        )));
    }
    Ok(())
}

/// The kinds a verb that says the kind first takes, with their letters.
pub const KINDS: [(&str, char); 4] = [("pane", 'p'), ("tab", 't'), ("group", 'g'), ("window", 'w')];

/// `verb`, which says the kind first, got `word`. An id of one of `kinds` gets told what to write;
/// `takes` says what the verb takes.
pub fn wrong_kind(verb: &str, kinds: &[(&str, char)], takes: &str, word: &str) -> Exit {
    for (kind, letter) in kinds {
        if is_kind(*letter, word) {
            return Exit::mistake(format!(
                "{verb} takes the kind before the id. Did you mean: bateri {verb} {kind} {word}"
            ));
        }
    }
    Exit::mistake(format!("{verb} takes {takes}, not '{}'", clean(word)))
}

/// Whether `id` is the id of a `kind` (`pane`, `tab`, `group`, `window`). One of another kind is a
/// mistake that says so and, for the verbs that take every kind, what to write (`t3 is a tab, not a
/// group. Did you mean: bateri focus tab t3`). `not_a_pane` is how a word that is no pane is told.
pub fn check_id(
    verb: &str,
    kind: &str,
    id: &str,
    not_a_pane: impl FnOnce(&str) -> Exit,
) -> Result<(), Exit> {
    let letter = KINDS
        .iter()
        .find(|(word, _)| *word == kind)
        .map_or('p', |(_, letter)| *letter);
    if is_kind(letter, id) {
        return Ok(());
    }
    for (word, other) in KINDS {
        if other != letter && is_kind(other, id) {
            return Err(Exit::mistake(match verb {
                "focus" | "close" | "rename" | "move" => {
                    format!(
                        "{id} is a {word}, not a {kind}. Did you mean: bateri {verb} {word} {id}"
                    )
                }
                _ => format!("{id} is a {word}, not a {kind}: {verb} takes a {kind}"),
            }));
        }
    }
    if kind == "pane" {
        return Err(not_a_pane(id));
    }
    Err(Exit::mistake(format!(
        "'{}' is not a {kind}: name one as {letter} and its number ({letter}1); 'bateri ls' lists them",
        clean(id)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pane_an_id_and_a_name_are_written_plainly() {
        for good in ["p0", "p1", "p9", "p123", "p999999999999999999"] {
            assert!(is_terminal(good), "{good}");
        }
        for bad in [
            "",
            "p",
            "P9",
            "p09",
            "p+9",
            "p9a",
            "t9",
            "p9999999999999999999",
            "9",
            " p9",
        ] {
            assert!(!is_terminal(bad), "{bad:?}");
        }
        assert!(is_id('t', "t3") && is_id('w', "w12") && is_id('g', "g1"));
        assert!(!is_id('t', "t0") && !is_id('t', "t03") && !is_id('t', "g3") && !is_id('t', "t"));
        assert!(is_name("b-0123456789abcdef0123456789abcdef") && is_name("a.b_c-d"));
        assert!(!is_name("") && !is_name("a b") && !is_name("a/b") && !is_name(&"x".repeat(65)));
    }

    #[test]
    fn a_machine_starts_with_a_letter_or_a_digit_and_an_address_names_one_after_an_at() {
        assert_eq!(address("p9"), Some(("p9", None)));
        assert_eq!(address("p9@prod"), Some(("p9", Some("prod"))));
        assert_eq!(address("p9@"), None);
        assert_eq!(address("x@prod"), None);
        assert_eq!(address("p9@pr@od"), None);
        assert!(is_kind('p', "p9@local") && !is_kind('t', "t3@prod"));
        assert!(reads_as_id("t3") && reads_as_id("p4@x") && !reads_as_id("web"));
    }

    #[test]
    fn a_label_is_one_line_of_plain_text_that_does_not_read_as_an_id() {
        assert!(check_label("--label", "web server").is_ok());
        assert!(check_label("--label", &"x".repeat(64)).is_ok());
        for bad in ["", "   ", "a\nb", "a\tb", "t3", "p9@prod", &"x".repeat(65)] {
            assert!(check_label("--label", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn what_comes_from_outside_is_cleaned_of_all_but_the_line_break_and_the_tab() {
        assert_eq!(clean("a\u{1b}[31mb\tc\nd\u{7f}e\u{0}"), "a[31mb\tc\nde");
    }
}
