//! Where a pane goes and how it came to be there, as the words of a message.

/// A side of the anchor an agent can ask for, or a tab of its own. The words are the ones the
/// drag's preview writes on the region it shows, lower case.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Above,
    Below,
    /// A tab of its own, in the window of the anchor's tab.
    Tab,
}

impl Side {
    /// Every side, in the order the words are listed to a person.
    pub const ALL: [Side; 5] = [Side::Left, Side::Right, Side::Above, Side::Below, Side::Tab];

    /// The word the command line and the message carry.
    pub fn word(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
            Side::Above => "above",
            Side::Below => "below",
            Side::Tab => "tab",
        }
    }

    /// The side a word names.
    pub fn from_word(word: &str) -> Option<Side> {
        Side::ALL.into_iter().find(|side| side.word() == word)
    }
}

/// How the pane came to be where it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landed {
    /// What was asked for, or — when nothing was asked — the palette's choice.
    AsAsked,
    /// Beside the group the anchor is part of: the anchor itself was too small to share.
    BesideGroup,
    /// In a tab of its own because this tab had no room (a tab asked for is [`Landed::AsAsked`]).
    NewTab,
    /// In no tab yet: the anchor is in none, and the terminal waits in the list of what runs in
    /// the background until it is. Only the Mac can say so.
    Waiting,
}

impl Landed {
    /// The word the reply and the JSON carry.
    pub fn word(self) -> &'static str {
        match self {
            Landed::AsAsked => "as_asked",
            Landed::BesideGroup => "beside_group",
            Landed::NewTab => "new_tab",
            Landed::Waiting => "waiting",
        }
    }

    /// The case a word names.
    pub fn from_word(word: &str) -> Option<Landed> {
        [
            Landed::AsAsked,
            Landed::BesideGroup,
            Landed::NewTab,
            Landed::Waiting,
        ]
        .into_iter()
        .find(|landed| landed.word() == word)
    }
}
