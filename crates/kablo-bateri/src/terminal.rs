//! The names of terminals: an agent's number (`p9`), the name kablo has for any terminal, and the
//! address of one on another machine (`p9@prod`).

use std::fmt;

/// A terminal's number on its server: the `p9` an agent is told when it opens one and names when it
/// acts on one. Numbers are per server, count up and are never given twice.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Terminal(pub u64);

impl Terminal {
    /// The terminal a name stands for: `p` and the number written plainly (`p9`, never `p09` or
    /// `P9`). A terminal that has another kind of name (one a person made with kablo itself, or
    /// one numbered `t9` before the unit was the pane) is no terminal for an agent to name; it
    /// goes on running under that name.
    pub fn parse(name: &str) -> Option<Terminal> {
        let digits = name.strip_prefix('p')?;
        let number: u64 = digits.parse().ok()?;
        // `u64::from_str` takes a `+` and leading zeros; the one spelling is the plain one.
        (number.to_string() == digits).then_some(Terminal(number))
    }

    /// The number a terminal's name carries, `None` for a name that is not an agent's number.
    pub fn number_of(name: &str) -> Option<u64> {
        Terminal::parse(name).map(|terminal| terminal.0)
    }
}

impl fmt::Display for Terminal {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "p{}", self.0)
    }
}

/// The terminal a pane is placed beside: kablo's name for it. An agent *writes* a terminal as its
/// number (`p4`, [`Terminal`]); the terminal it runs in is the caller's whatever it is called, and
/// a request that defaults to it names it so — a terminal the Mac opened before the numbers
/// existed is `b-` and 32 digits, one a person made with `kablo new` is what they typed. Reading
/// takes every name kablo allows ([`kablo_proto::SessionId`]'s alphabet and length, so a name is
/// never a path), which includes every `pN`.
///
/// A terminal of another machine is written with the machine after an `@` — `p9@prod`, this Mac
/// `p9@local`, as `bateri ls` says them ([`Anchor::address`]). The `@` is no part of kablo's
/// alphabet, so a name and an address never read as one another.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Anchor {
    name: String,
    machine: Option<String>,
}

/// The longest a machine's name may be, in bytes.
const MACHINE_MAX: usize = 253;

/// The machine an address may name: what `bateri ls` prints after the `@` — a host name as the
/// user typed it to `ssh` without its `user@`, or `local`. Letters, digits and `. _ - :`, starting
/// with a letter or a digit (so it is never an option), at most 253 bytes.
pub fn is_machine(name: &str) -> bool {
    name.len() <= MACHINE_MAX
        && name
            .bytes()
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
}

/// The machine of this Mac in an address.
pub const LOCAL_MACHINE: &str = "local";

/// This Mac as `-S` and a `server` line name it: the word `@local`, which is no host `ssh` could be
/// given (a host starts with a letter or a digit), so it cannot be a server of that name.
pub const LOCAL_SERVER: &str = "@local";

impl Anchor {
    /// The terminal a name stands for, `None` for a name kablo does not allow (an address is
    /// not a name: [`Anchor::address`]).
    pub fn parse(name: &str) -> Option<Anchor> {
        kablo_proto::SessionId::new(name).ok().map(|name| Anchor {
            name: name.as_str().to_owned(),
            machine: None,
        })
    }

    /// A terminal as a name or as an address — `p9` or `p9@prod` — `None` for what is neither.
    pub fn address(text: &str) -> Option<Anchor> {
        let Some((name, machine)) = text.split_once('@') else {
            return Anchor::parse(text);
        };
        if !is_machine(machine) {
            return None;
        }
        Anchor::parse(name).map(|found| Anchor {
            machine: Some(machine.to_owned()),
            ..found
        })
    }

    /// An agent's terminal as an anchor.
    pub fn of(terminal: Terminal) -> Anchor {
        // `p` and at most twenty digits: a name kablo allows.
        Anchor {
            name: terminal.to_string(),
            machine: None,
        }
    }

    /// The name, as kablo has it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The machine the address names, `None` for a terminal of the machine that asks.
    pub fn machine(&self) -> Option<&str> {
        self.machine.as_deref()
    }

    /// The agent's number, when the name is one.
    pub fn terminal(&self) -> Option<Terminal> {
        Terminal::parse(self.name())
    }
}

impl fmt::Display for Anchor {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.name())?;
        match &self.machine {
            Some(machine) => write!(out, "@{machine}"),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_machine_starts_with_a_letter_or_a_digit_and_is_at_most_253_bytes() {
        assert!(is_machine("prod") && is_machine("10.0.0.2") && is_machine("local"));
        assert!(is_machine("host:22") && is_machine(&"a".repeat(253)));
        assert!(!is_machine("") && !is_machine("-x") && !is_machine("a b") && !is_machine("a@b"));
        assert!(!is_machine(&"a".repeat(254)) && !is_machine("é"));
    }

    #[test]
    fn a_terminal_is_named_by_its_plain_number() {
        assert_eq!(Terminal::parse("p9"), Some(Terminal(9)));
        assert_eq!(Terminal::parse("p0"), Some(Terminal(0)));
        assert_eq!(Terminal(14).to_string(), "p14");
        for name in [
            "",
            "p",
            "9",
            "P9",
            "p09",
            "p+9",
            "p-9",
            "p9 ",
            " p9",
            "pp9",
            "p9a",
            "b-0123456789",
            "p99999999999999999999",
            // The name the numbers had before the unit was the pane: a terminal that has it
            // runs on, and is no pane for an agent to name.
            "t9",
        ] {
            assert_eq!(Terminal::parse(name), None, "{name:?}");
        }
        assert_eq!(Terminal::number_of("p3"), Some(3));
        assert_eq!(Terminal::number_of("t3"), None);
        assert_eq!(Terminal::number_of("b-3"), None);
    }

    #[test]
    fn an_anchor_is_any_name_kablo_allows_and_an_agents_number_is_one() {
        let made = format!("b-{}", "0123456789abcdef".repeat(2));
        for name in [
            "p4",
            "p0",
            "p07",
            "t4",
            "mine",
            "b-0a1b",
            &made,
            "a.b_c-d",
            &"x".repeat(64),
        ] {
            let anchor = Anchor::parse(name).unwrap_or_else(|| panic!("{name:?}"));
            assert_eq!(anchor.name(), name);
            assert_eq!(anchor.to_string(), name);
        }
        for name in [
            "",
            " p4",
            "p4 ",
            "a b",
            "a/b",
            "../x",
            "a\nb",
            "é",
            &"x".repeat(65),
        ] {
            assert_eq!(Anchor::parse(name), None, "{name:?}");
        }
        // The agent's own spelling keeps its number; every other name has none.
        assert_eq!(
            Anchor::parse("p9").and_then(|a| a.terminal()),
            Some(Terminal(9))
        );
        assert_eq!(
            Anchor::of(Terminal(9)),
            Anchor::parse("p9").expect("a name")
        );
        assert_eq!(Anchor::parse("p09").and_then(|a| a.terminal()), None);
        assert_eq!(Anchor::parse("t9").and_then(|a| a.terminal()), None);
        assert_eq!(Anchor::parse(&made).and_then(|a| a.terminal()), None);
    }
}
