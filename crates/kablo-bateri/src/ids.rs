//! The names a window, a group and a tab are known by from outside: `w1`, `g1`, `t3`.
//!
//! A number is the app's to give and each kind counts on its own, across every window; this
//! module only spells and reads the names.

use std::fmt;

/// What a number names. The prefix says which kind a name is, so a command that means a tab and
/// is given a group's name can say so instead of closing the wrong thing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Kind {
    Window,
    Group,
    Tab,
}

impl Kind {
    /// All kinds, in the order the file lists them.
    pub const ALL: [Kind; 3] = [Kind::Window, Kind::Group, Kind::Tab];

    /// The letter a name starts with.
    pub const fn prefix(self) -> char {
        match self {
            Kind::Window => 'w',
            Kind::Group => 'g',
            Kind::Tab => 't',
        }
    }

    /// The word for it in a message.
    pub const fn word(self) -> &'static str {
        match self {
            Kind::Window => "window",
            Kind::Group => "group",
            Kind::Tab => "tab",
        }
    }

    /// The kind a letter starts the name of.
    pub fn of_prefix(prefix: char) -> Option<Kind> {
        Kind::ALL.into_iter().find(|kind| kind.prefix() == prefix)
    }

    /// Its place among [`Kind::ALL`], for a table kept one per kind.
    pub const fn slot(self) -> usize {
        match self {
            Kind::Window => 0,
            Kind::Group => 1,
            Kind::Tab => 2,
        }
    }
}

/// A window's, a group's or a tab's name: its kind's letter and its number, from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Id {
    kind: Kind,
    number: u64,
}

impl Id {
    /// The name of number `number` of `kind`; there is no number 0.
    pub fn new(kind: Kind, number: u64) -> Option<Id> {
        (number > 0).then_some(Id { kind, number })
    }

    /// The thing a name stands for: a letter and the number written plainly (`t3`, never `t03`,
    /// `T3` or `t+3`), and not `t0`.
    pub fn parse(name: &str) -> Option<Id> {
        let mut chars = name.chars();
        let kind = Kind::of_prefix(chars.next()?)?;
        let digits = chars.as_str();
        let number: u64 = digits.parse().ok()?;
        // `u64::from_str` takes a `+` and leading zeros; the one spelling is the plain one.
        if number.to_string() != digits {
            return None;
        }
        Id::new(kind, number)
    }

    pub fn kind(self) -> Kind {
        self.kind
    }

    pub fn number(self) -> u64 {
        self.number
    }
}

impl fmt::Display for Id {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "{}{}", self.kind.prefix(), self.number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_a_letter_and_the_number_written_plainly() {
        let tab = Id::new(Kind::Tab, 3).expect("an id");
        assert_eq!(tab.to_string(), "t3");
        assert_eq!(Id::parse("t3"), Some(tab));
        assert_eq!(Id::parse("w12").map(|id| id.kind()), Some(Kind::Window));
        assert_eq!(Id::parse("g1").map(|id| id.number()), Some(1));
        for name in [
            "",
            "t",
            "3",
            "T3",
            "t03",
            "t+3",
            "t-3",
            "t0",
            "t3 ",
            " t3",
            "tt3",
            "t3a",
            "p3",
            "t3.1",
            "t99999999999999999999999",
        ] {
            assert_eq!(Id::parse(name), None, "{name:?}");
        }
        assert_eq!(Id::new(Kind::Group, 0), None);
    }
}
