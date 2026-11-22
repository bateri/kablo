//! The `view` verb: an agent reads the view a pane shows and drives it, through the bateri app.
//!
//! ```text
//! bateri 2
//! view
//! terminal p9@prod
//! format json
//! action fill
//! ref k3f9qa
//! text x68656c6c6f
//! ```
//!
//! `terminal` is the pane, as for `close` (`p9`, `p9@prod`); with none it is the caller's own. With
//! no `action` the request is a reading; otherwise `action` is one of [`Action::word`] and the lines
//! it takes follow. `command` takes the title of a command of the page, and with a `ref` the title of a
//! command of that element (what the reading lists under it); `reveal` takes a `ref` and, for a list,
//! a table or an outline, the `index` of the row (from 1) to bring into view. A bateri that is older
//! than these says so ([`is_older`]). What a person typed is free text and travels as `x` and the hex of its bytes (a
//! line break survives the line that frames it); a name (`ref`) is as the bateri app wrote it,
//! lower case. `format` is how the reading that every answer ends with is written (`text` is the
//! default). A `view` may be as long as a `relay` ([`crate::message::RELAY_MAX_BYTES`]): what is
//! filled into a field is more than a notification.
//!
//! The answer is `ok text` (the reading, or for `text` the text asked for) or, for `shot`,
//! `ok picture` and the hex of a PNG ([`crate::message::Reply::Picture`]).

use crate::message::{Failure, Fields, Format, Kind, hex_word, push, word_of_hex};
use crate::terminal::Anchor;

/// The longest a `wait` waits for, in seconds, and how long it waits where it is told no more.
pub const WAIT_MOST: u32 = 120;
pub const WAIT_DEFAULT: u32 = 10;
/// The most keys a `press` takes.
pub const KEYS_MOST: usize = 16;
/// The most a `scroll` is told to go by.
pub const SCROLL_MOST: u32 = 1000;

/// Which way a `scroll` goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Way {
    Up,
    Down,
    Top,
    Bottom,
}

impl Way {
    /// The word the request carries.
    pub fn word(self) -> &'static str {
        match self {
            Way::Up => "up",
            Way::Down => "down",
            Way::Top => "top",
            Way::Bottom => "bottom",
        }
    }

    fn from_word(word: &str) -> Option<Way> {
        [Way::Up, Way::Down, Way::Top, Way::Bottom]
            .into_iter()
            .find(|way| way.word() == word)
    }
}

/// What an agent does to a view, or the reading alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Read the view.
    Read,
    /// Do the command the page lists under this title.
    Command(String),
    /// Do the command that an element lists (`actions` in its reading) under this title: its
    /// name (a `ref`, or a whole id) and the title.
    CommandOn {
        target: String,
        title: String,
    },
    /// Bring an element into view, or the `index`th row (from 1) of the list, the table or the
    /// outline it names: the page scrolls, loads what it has not built, and moves its cursor there.
    Reveal {
        target: String,
        index: Option<u32>,
    },
    /// Click an element: its name (a `ref`, or a whole id).
    Click(String),
    DoubleClick(String),
    /// Replace the whole text of a field or an editor.
    Fill {
        target: String,
        text: String,
    },
    /// Type into the field or editor that has the keyboard.
    Type(String),
    /// Press keys, as `cmd+enter` writes them.
    Press(Vec<String>),
    /// Scroll a box that scrolls, by `count` screens (a screen where none).
    Scroll {
        target: String,
        way: Way,
        count: Option<u32>,
    },
    /// Ask for the context menu of an element, and read its lines.
    Menu(String),
    /// Pick a line of the menu asked for: its title, or its number.
    Pick(String),
    /// The whole text of a field or an editor.
    Text(String),
    /// Wait for words to show (or, with `gone`, to go), at most `seconds`.
    Wait {
        text: String,
        gone: bool,
        seconds: Option<u32>,
    },
    /// A picture of the view, in full size where `full`.
    Shot {
        full: bool,
    },
}

impl Action {
    /// The word of the `action` line; none for a reading.
    pub fn word(&self) -> Option<&'static str> {
        Some(match self {
            Action::Read => return None,
            Action::Command(_) | Action::CommandOn { .. } => "command",
            Action::Reveal { .. } => "reveal",
            Action::Click(_) => "click",
            Action::DoubleClick(_) => "double-click",
            Action::Fill { .. } => "fill",
            Action::Type(_) => "type",
            Action::Press(_) => "press",
            Action::Scroll { .. } => "scroll",
            Action::Menu(_) => "menu",
            Action::Pick(_) => "pick",
            Action::Text(_) => "text",
            Action::Wait { .. } => "wait",
            Action::Shot { .. } => "shot",
        })
    }

    /// The seconds the bateri app may take to answer: a `wait` takes the time it is told, the rest
    /// the app's own few.
    pub fn waits(&self) -> u32 {
        match self {
            Action::Wait { seconds, .. } => seconds.unwrap_or(WAIT_DEFAULT),
            _ => 0,
        }
    }
}

/// One `view` request: the pane, how the reading is written, and what is done.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// The pane; the caller's own where none.
    pub terminal: Option<Anchor>,
    pub format: Format,
    pub action: Action,
}

/// The words a view's action is, in the order the lines of a request carry them.
const KEYS: [&str; 12] = [
    "terminal", "format", "action", "ref", "text", "keys", "way", "count", "gone", "for", "full",
    "index",
];

impl View {
    /// The lines of the request after the verb.
    pub(crate) fn encode_into(&self, out: &mut String) -> Result<(), Failure> {
        out.push_str("view\n");
        if let Some(terminal) = &self.terminal {
            push(out, "terminal", &terminal.to_string())?;
        }
        if self.format != Format::default() {
            push(out, "format", self.format.word())?;
        }
        let Some(word) = self.action.word() else {
            return Ok(());
        };
        push(out, "action", word)?;
        match &self.action {
            Action::Read => {}
            Action::Command(title) => push(out, "text", &hex_word(title))?,
            Action::CommandOn { target, title } => {
                check_ref(target)?;
                push(out, "ref", target)?;
                push(out, "text", &hex_word(title))?;
            }
            Action::Reveal { target, index } => {
                check_ref(target)?;
                push(out, "ref", target)?;
                if let Some(index) = index {
                    check_index(*index)?;
                    push(out, "index", &index.to_string())?;
                }
            }
            Action::Click(target)
            | Action::DoubleClick(target)
            | Action::Menu(target)
            | Action::Text(target) => {
                check_ref(target)?;
                push(out, "ref", target)?;
            }
            Action::Fill { target, text } => {
                check_ref(target)?;
                push(out, "ref", target)?;
                push(out, "text", &hex_word(text))?;
            }
            Action::Type(text) | Action::Pick(text) => push(out, "text", &hex_word(text))?,
            Action::Press(keys) => {
                if keys.is_empty() || keys.len() > KEYS_MOST {
                    return Err(Failure::usage(format!(
                        "`press` takes from one to {KEYS_MOST} keys"
                    )));
                }
                let words: Vec<String> = keys.iter().map(|key| hex_word(key)).collect();
                push(out, "keys", &words.join(" "))?;
            }
            Action::Scroll { target, way, count } => {
                check_ref(target)?;
                push(out, "ref", target)?;
                push(out, "way", way.word())?;
                if let Some(count) = count {
                    check_count(*count)?;
                    push(out, "count", &count.to_string())?;
                }
            }
            Action::Wait {
                text,
                gone,
                seconds,
            } => {
                push(out, "text", &hex_word(text))?;
                if *gone {
                    push(out, "gone", "yes")?;
                }
                if let Some(seconds) = seconds {
                    check_seconds(*seconds)?;
                    push(out, "for", &seconds.to_string())?;
                }
            }
            Action::Shot { full } => {
                if *full {
                    push(out, "full", "yes")?;
                }
            }
        }
        Ok(())
    }
}

/// Whether a refusal is of a bateri that does not know the `view` verb: it says there is no such
/// verb, or, for a request over what it takes of any verb, that the message is too long (it reads
/// the limit before the verb, and a `view` may be longer than it allows).
pub fn is_old(failure: &Failure) -> bool {
    failure.kind == Kind::Usage
        && (failure.text.contains("is no verb")
            || failure.text.contains(&format!(
                "the message is over {} bytes",
                crate::message::MAX_BYTES
            )))
}

/// Whether a refusal is of a bateri that does not know what a newer `view` asks: the `reveal`
/// action, or a `command` or a `reveal` with the lines they take now. An older reader refuses a
/// word it does not know as it refuses an action, and a line it does not take by its key.
pub fn is_older(failure: &Failure) -> bool {
    failure.kind == Kind::Usage
        && (failure.text.contains("is no action")
            || failure.text == "`command` takes no `ref`"
            || failure.text == "`view` takes no `index`")
}

/// A name of an element as the bateri app writes it: six letters and digits, or a whole id of 16
/// hexadecimal digits; lower case.
fn check_ref(name: &str) -> Result<(), Failure> {
    let plain = match name.len() {
        6 => name
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase()),
        16 => name
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')),
        _ => false,
    };
    if plain {
        Ok(())
    } else {
        Err(Failure::usage(format!(
            "a `ref` is six letters and digits, or the 16 digits of a whole id, not `{name}`"
        )))
    }
}

fn check_count(count: u32) -> Result<(), Failure> {
    if (1..=SCROLL_MOST).contains(&count) {
        Ok(())
    } else {
        Err(Failure::usage(format!(
            "`count` is from 1 to {SCROLL_MOST}, not {count}"
        )))
    }
}

fn check_index(index: u32) -> Result<(), Failure> {
    if index >= 1 {
        Ok(())
    } else {
        Err(Failure::usage("`index` is a row from 1, not 0"))
    }
}

fn check_seconds(seconds: u32) -> Result<(), Failure> {
    if (1..=WAIT_MOST).contains(&seconds) {
        Ok(())
    } else {
        Err(Failure::usage(format!(
            "`for` is from 1 to {WAIT_MOST} seconds, not {seconds}"
        )))
    }
}

/// A whole number as a request writes it: digits, no sign, no leading zero.
fn plain(key: &str, value: &str) -> Result<u32, Failure> {
    value
        .parse::<u32>()
        .ok()
        .filter(|number| number.to_string() == value)
        .ok_or_else(|| Failure::usage(format!("`{key}` is a whole number, not `{value}`")))
}

fn free(fields: &Fields<'_>, key: &str) -> Result<String, Failure> {
    word_of_hex(fields.need(key)?)
}

fn named(fields: &Fields<'_>) -> Result<String, Failure> {
    let name = fields.need("ref")?;
    check_ref(name)?;
    Ok(name.to_owned())
}

/// The request of a `view` verb: the lines after it.
pub(crate) fn read<'a>(
    verb: &'a str,
    lines: impl Iterator<Item = &'a str>,
) -> Result<View, Failure> {
    let fields = Fields::read(verb, lines, &KEYS)?;
    let terminal = fields
        .get("terminal")
        .map(|value| {
            Anchor::address(value).ok_or_else(|| {
                Failure::usage(format!(
                    "`terminal` names a pane as kablo does, and a machine after an `@` for one of another (`p9@prod`), not `{value}`"
                ))
            })
        })
        .transpose()?;
    let format = match fields.get("format") {
        None | Some("text") => Format::Text,
        Some("json") => Format::Json,
        Some(other) => {
            return Err(Failure::usage(format!(
                "`format` is text or json, not `{other}`"
            )));
        }
    };
    let word = fields.get("action");
    // The lines each action takes: any other is a mistake, as a key twice is.
    let takes: &[&str] = match word {
        None => &[],
        Some("command") => &["text", "ref"],
        Some("type" | "pick") => &["text"],
        Some("reveal") => &["ref", "index"],
        Some("click" | "double-click" | "menu" | "text") => &["ref"],
        Some("fill") => &["ref", "text"],
        Some("press") => &["keys"],
        Some("scroll") => &["ref", "way", "count"],
        Some("wait") => &["text", "gone", "for"],
        Some("shot") => &["full"],
        Some(other) => {
            return Err(Failure::usage(format!(
                "`{other}` is no action: command, reveal, click, double-click, fill, type, press, scroll, menu, pick, text, wait or shot"
            )));
        }
    };
    for key in &KEYS[3..] {
        if fields.get(key).is_some() && !takes.contains(key) {
            return Err(Failure::usage(format!(
                "`{}` takes no `{key}`",
                word.unwrap_or("view")
            )));
        }
    }
    let action = match word {
        None => Action::Read,
        Some("command") => match fields.get("ref") {
            None => Action::Command(free(&fields, "text")?),
            Some(_) => Action::CommandOn {
                target: named(&fields)?,
                title: free(&fields, "text")?,
            },
        },
        Some("reveal") => Action::Reveal {
            target: named(&fields)?,
            index: fields
                .get("index")
                .map(|value| {
                    let index = plain("index", value)?;
                    check_index(index).map(|()| index)
                })
                .transpose()?,
        },
        Some("click") => Action::Click(named(&fields)?),
        Some("double-click") => Action::DoubleClick(named(&fields)?),
        Some("menu") => Action::Menu(named(&fields)?),
        Some("text") => Action::Text(named(&fields)?),
        Some("fill") => Action::Fill {
            target: named(&fields)?,
            text: free(&fields, "text")?,
        },
        Some("type") => Action::Type(free(&fields, "text")?),
        Some("pick") => Action::Pick(free(&fields, "text")?),
        Some("press") => {
            let keys = fields
                .need("keys")?
                .split(' ')
                .map(word_of_hex)
                .collect::<Result<Vec<String>, Failure>>()?;
            if keys.len() > KEYS_MOST {
                return Err(Failure::usage(format!(
                    "`press` takes from one to {KEYS_MOST} keys"
                )));
            }
            Action::Press(keys)
        }
        Some("scroll") => {
            let way = fields.need("way")?;
            Action::Scroll {
                target: named(&fields)?,
                way: Way::from_word(way).ok_or_else(|| {
                    Failure::usage(format!("`way` is up, down, top or bottom, not `{way}`"))
                })?,
                count: fields
                    .get("count")
                    .map(|value| {
                        let count = plain("count", value)?;
                        check_count(count).map(|()| count)
                    })
                    .transpose()?,
            }
        }
        Some("wait") => Action::Wait {
            text: free(&fields, "text")?,
            gone: fields.flag("gone")?,
            seconds: fields
                .get("for")
                .map(|value| {
                    let seconds = plain("for", value)?;
                    check_seconds(seconds).map(|()| seconds)
                })
                .transpose()?,
        },
        Some("shot") => Action::Shot {
            full: fields.flag("full")?,
        },
        Some(_) => return Err(Failure::usage("no such action")),
    };
    Ok(View {
        terminal,
        format,
        action,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{Kind, Request};
    use crate::terminal::Terminal;

    // The frozen text of each request of version 2's `view`. Changing the format adds a version
    // and its fixtures; it does not rewrite these.
    const V2_VIEW: &str = "bateri 2\nview\n";
    const V2_VIEW_PANE_JSON: &str = "bateri 2\nview\nterminal p9@prod\nformat json\n";
    const V2_VIEW_COMMAND: &str =
        "bateri 2\nview\naction command\ntext x526573746172742061706921\n";
    const V2_VIEW_COMMAND_ON: &str =
        "bateri 2\nview\naction command\nref k3f9qa\ntext x52657374617274\n";
    const V2_VIEW_REVEAL: &str = "bateri 2\nview\naction reveal\nref k3f9qa\n";
    const V2_VIEW_REVEAL_ROW: &str = "bateri 2\nview\naction reveal\nref k3f9qa\nindex 248\n";
    const V2_VIEW_CLICK: &str = "bateri 2\nview\nterminal p9\naction click\nref k3f9qa\n";
    const V2_VIEW_DOUBLE_CLICK: &str = "bateri 2\nview\naction double-click\nref k3f9qa\n";
    const V2_VIEW_FILL: &str =
        "bateri 2\nview\nformat json\naction fill\nref k3f9qa\ntext x73656c6563742031\n";
    const V2_VIEW_FILL_EMPTY: &str = "bateri 2\nview\naction fill\nref k3f9qa\ntext x\n";
    const V2_VIEW_TYPE: &str = "bateri 2\nview\naction type\ntext x6162\n";
    const V2_VIEW_PRESS: &str = "bateri 2\nview\naction press\nkeys x636d642b656e746572 x746162\n";
    const V2_VIEW_SCROLL: &str = "bateri 2\nview\naction scroll\nref k3f9qa\nway down\ncount 3\n";
    const V2_VIEW_SCROLL_TOP: &str = "bateri 2\nview\naction scroll\nref k3f9qa\nway top\n";
    const V2_VIEW_MENU: &str = "bateri 2\nview\naction menu\nref k3f9qa\n";
    const V2_VIEW_PICK: &str = "bateri 2\nview\naction pick\ntext x436f7079\n";
    const V2_VIEW_TEXT: &str = "bateri 2\nview\naction text\nref 0123456789abcdef\n";
    const V2_VIEW_WAIT: &str = "bateri 2\nview\naction wait\ntext x446f6e65\ngone yes\nfor 30\n";
    const V2_VIEW_WAIT_BARE: &str = "bateri 2\nview\naction wait\ntext x446f6e65\n";
    const V2_VIEW_SHOT: &str = "bateri 2\nview\naction shot\n";
    const V2_VIEW_SHOT_FULL: &str = "bateri 2\nview\nterminal p9\naction shot\nfull yes\n";

    fn pane(number: u64) -> Option<Anchor> {
        Some(Anchor::of(Terminal(number)))
    }

    fn view(terminal: Option<Anchor>, format: Format, action: Action) -> Request {
        Request::View(View {
            terminal,
            format,
            action,
        })
    }

    fn json(action: Action) -> Request {
        view(None, Format::Json, action)
    }

    fn plain_view(action: Action) -> Request {
        view(None, Format::Text, action)
    }

    #[test]
    fn each_request_is_written_as_its_frozen_text_and_read_back() {
        let on_prod = Anchor::address("p9@prod");
        let cases: Vec<(&str, Request)> = vec![
            (V2_VIEW, plain_view(Action::Read)),
            (V2_VIEW_PANE_JSON, view(on_prod, Format::Json, Action::Read)),
            (
                V2_VIEW_COMMAND,
                plain_view(Action::Command("Restart api!".into())),
            ),
            (
                V2_VIEW_COMMAND_ON,
                plain_view(Action::CommandOn {
                    target: "k3f9qa".into(),
                    title: "Restart".into(),
                }),
            ),
            (
                V2_VIEW_REVEAL,
                plain_view(Action::Reveal {
                    target: "k3f9qa".into(),
                    index: None,
                }),
            ),
            (
                V2_VIEW_REVEAL_ROW,
                plain_view(Action::Reveal {
                    target: "k3f9qa".into(),
                    index: Some(248),
                }),
            ),
            (
                V2_VIEW_CLICK,
                view(pane(9), Format::Text, Action::Click("k3f9qa".into())),
            ),
            (
                V2_VIEW_DOUBLE_CLICK,
                plain_view(Action::DoubleClick("k3f9qa".into())),
            ),
            (
                V2_VIEW_FILL,
                json(Action::Fill {
                    target: "k3f9qa".into(),
                    text: "select 1".into(),
                }),
            ),
            (
                V2_VIEW_FILL_EMPTY,
                plain_view(Action::Fill {
                    target: "k3f9qa".into(),
                    text: String::new(),
                }),
            ),
            (V2_VIEW_TYPE, plain_view(Action::Type("ab".into()))),
            (
                V2_VIEW_PRESS,
                plain_view(Action::Press(vec!["cmd+enter".into(), "tab".into()])),
            ),
            (
                V2_VIEW_SCROLL,
                plain_view(Action::Scroll {
                    target: "k3f9qa".into(),
                    way: Way::Down,
                    count: Some(3),
                }),
            ),
            (
                V2_VIEW_SCROLL_TOP,
                plain_view(Action::Scroll {
                    target: "k3f9qa".into(),
                    way: Way::Top,
                    count: None,
                }),
            ),
            (V2_VIEW_MENU, plain_view(Action::Menu("k3f9qa".into()))),
            (V2_VIEW_PICK, plain_view(Action::Pick("Copy".into()))),
            (
                V2_VIEW_TEXT,
                plain_view(Action::Text("0123456789abcdef".into())),
            ),
            (
                V2_VIEW_WAIT,
                plain_view(Action::Wait {
                    text: "Done".into(),
                    gone: true,
                    seconds: Some(30),
                }),
            ),
            (
                V2_VIEW_WAIT_BARE,
                plain_view(Action::Wait {
                    text: "Done".into(),
                    gone: false,
                    seconds: None,
                }),
            ),
            (V2_VIEW_SHOT, plain_view(Action::Shot { full: false })),
            (
                V2_VIEW_SHOT_FULL,
                view(pane(9), Format::Text, Action::Shot { full: true }),
            ),
        ];
        for (text, request) in cases {
            assert_eq!(Request::parse(text).as_ref(), Ok(&request), "{text}");
            assert_eq!(request.encode().as_deref(), Ok(text), "{request:?}");
        }
    }

    #[test]
    fn free_text_survives_a_line_break_and_any_character() {
        let text = "line one\nline two\t\u{1b}[31m ünï 日本 \u{0}";
        let request = plain_view(Action::Fill {
            target: "k3f9qa".into(),
            text: text.into(),
        });
        let written = request.encode().expect("encoded");
        assert_eq!(written.lines().count(), 5, "{written}");
        assert!(!written.contains('\u{1b}'));
        assert_eq!(Request::parse(&written), Ok(request));
    }

    #[test]
    fn a_view_may_be_as_long_as_a_relay_and_no_longer() {
        use crate::message::{MAX_BYTES, RELAY_MAX_BYTES};
        let long = "x".repeat(MAX_BYTES);
        let request = plain_view(Action::Type(long));
        let written = request.encode().expect("a typed text of a page's size");
        assert!(written.len() > MAX_BYTES);
        assert_eq!(Request::parse(&written), Ok(request));
        let too_long = plain_view(Action::Type("x".repeat(RELAY_MAX_BYTES)));
        assert!(too_long.encode().is_err());
        let text = format!(
            "bateri 2\nview\naction type\ntext x{}\n",
            "61".repeat(RELAY_MAX_BYTES)
        );
        assert_eq!(Request::parse(&text).expect_err("long").kind, Kind::Usage);
    }

    #[test]
    fn what_is_wrong_is_a_mistake_that_says_which() {
        for (text, says) in [
            ("bateri 2\nview\naction dance\n", "is no action"),
            ("bateri 2\nview\naction click\n", "needs a `ref`"),
            ("bateri 2\nview\naction click\nref K3F9QA\n", "a `ref` is"),
            ("bateri 2\nview\naction click\nref abc\n", "a `ref` is"),
            (
                "bateri 2\nview\naction click\nref ghijklmnopqrstuv\n",
                "a `ref` is",
            ),
            (
                "bateri 2\nview\naction click\nref k3f9qa\ntext x61\n",
                "takes no `text`",
            ),
            ("bateri 2\nview\nref k3f9qa\n", "takes no `ref`"),
            (
                "bateri 2\nview\naction shot\nref k3f9qa\n",
                "takes no `ref`",
            ),
            (
                "bateri 2\nview\naction fill\nref k3f9qa\n",
                "needs a `text`",
            ),
            (
                "bateri 2\nview\naction fill\nref k3f9qa\ntext hello\n",
                "`x` and",
            ),
            (
                "bateri 2\nview\naction fill\nref k3f9qa\ntext x6\n",
                "even number",
            ),
            ("bateri 2\nview\naction press\nkeys\n", "has no value"),
            (
                "bateri 2\nview\naction scroll\nref k3f9qa\nway sideways\n",
                "up, down, top or bottom",
            ),
            (
                "bateri 2\nview\naction scroll\nref k3f9qa\nway up\ncount 0\n",
                "from 1 to",
            ),
            (
                "bateri 2\nview\naction scroll\nref k3f9qa\nway up\ncount 01\n",
                "whole number",
            ),
            (
                "bateri 2\nview\naction wait\ntext x61\nfor 121\n",
                "from 1 to 120",
            ),
            (
                "bateri 2\nview\naction wait\ntext x61\nfor 0\n",
                "from 1 to 120",
            ),
            (
                "bateri 2\nview\naction wait\ntext x61\ngone maybe\n",
                "yes or no",
            ),
            ("bateri 2\nview\nformat yaml\n", "text or json"),
            ("bateri 2\nview\nterminal\n", "has no value"),
            ("bateri 2\nview\nterminal p9@\n", "names a pane"),
            ("bateri 2\nview\nformat json\nformat json\n", "given twice"),
            ("bateri 2\nview\nbogus x\n", "takes no `bogus`"),
            (
                "bateri 2\nview\naction command\nref K3F9QA\ntext x61\n",
                "a `ref` is",
            ),
            (
                "bateri 2\nview\naction command\nref k3f9qa\n",
                "needs a `text`",
            ),
            ("bateri 2\nview\naction reveal\n", "needs a `ref`"),
            (
                "bateri 2\nview\naction reveal\nref k3f9qa\nindex 0\n",
                "a row from 1",
            ),
            (
                "bateri 2\nview\naction reveal\nref k3f9qa\nindex 07\n",
                "whole number",
            ),
            (
                "bateri 2\nview\naction reveal\nref k3f9qa\ntext x61\n",
                "takes no `text`",
            ),
            (
                "bateri 2\nview\naction click\nref k3f9qa\nindex 2\n",
                "takes no `index`",
            ),
        ] {
            let failure = Request::parse(text).expect_err(text);
            assert_eq!(failure.kind, Kind::Usage, "{text}");
            assert!(failure.text.contains(says), "{text}: {}", failure.text);
        }
        let sixteen: Vec<String> = (0..=KEYS_MOST).map(|n| format!("k{n}")).collect();
        assert!(plain_view(Action::Press(sixteen)).encode().is_err());
        assert!(plain_view(Action::Press(vec![])).encode().is_err());
        assert!(plain_view(Action::Click("K3F9QA".into())).encode().is_err());
        assert!(
            plain_view(Action::Wait {
                text: "a".into(),
                gone: false,
                seconds: Some(121)
            })
            .encode()
            .is_err()
        );
    }

    #[test]
    fn a_bateri_that_is_older_is_told_from_what_it_refuses() {
        // What the readers of before `command` with a `ref` and `reveal` say to them: the texts are
        // the ones those readers wrote (this module before the change), and `is_older` reads them.
        for says in [
            "`reveal` is no action: command, click, double-click, fill, type, press, scroll, menu, pick, text, wait or shot",
            "`command` takes no `ref`",
            "`view` takes no `index`",
        ] {
            assert!(is_older(&Failure::usage(says)), "{says}");
        }
        assert!(!is_older(&Failure::usage("`format` is text or json")));
        assert!(!is_older(&Failure::usage("a `ref` is six letters")));
        assert!(!is_older(&Failure::new(Kind::Failed, "it is no action")));
        // A reader that is as new as this one refuses neither.
        for text in [V2_VIEW_COMMAND_ON, V2_VIEW_REVEAL, V2_VIEW_REVEAL_ROW] {
            assert!(Request::parse(text).is_ok(), "{text}");
        }
    }

    const V2_REPLY_PICTURE: &str = "bateri 2\nok picture\n89504e470d0a1a0a00ff\n";

    #[test]
    fn a_picture_reply_is_the_hex_of_the_bytes_and_is_read_back_whole() {
        use crate::message::Reply;
        let png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff];
        let reply = Reply::Picture(png);
        assert_eq!(reply.encode(), V2_REPLY_PICTURE);
        assert_eq!(Reply::parse(V2_REPLY_PICTURE), Ok(reply));
        // Four megabytes of every byte: the text is twice that, and comes back as it was.
        let big: Vec<u8> = (0..4 * 1024 * 1024)
            .map(|n: usize| (n % 251) as u8)
            .collect();
        let reply = Reply::Picture(big);
        let written = reply.encode();
        assert_eq!(
            written.len(),
            "bateri 2\nok picture\n".len() + 8 * 1024 * 1024 + 1
        );
        assert_eq!(Reply::parse(&written), Ok(reply));
        for bad in [
            "bateri 2\nok picture\n",
            "bateri 2\nok picture\n\n",
            "bateri 2\nok picture\nabc\n",
            "bateri 2\nok picture\nzz\n",
            "bateri 2\nok picture\n89 50\n",
        ] {
            let failure = Reply::parse(bad).expect_err(bad);
            assert_eq!(failure.kind, Kind::Usage, "{bad}");
        }
    }

    #[test]
    fn the_seconds_a_wait_takes_are_its_own_or_the_default() {
        assert_eq!(Action::Read.waits(), 0);
        assert_eq!(Action::Click("k3f9qa".into()).waits(), 0);
        let wait = |seconds| Action::Wait {
            text: "x".into(),
            gone: false,
            seconds,
        };
        assert_eq!(wait(None).waits(), WAIT_DEFAULT);
        assert_eq!(wait(Some(30)).waits(), 30);
    }

    #[test]
    fn a_bateri_that_does_not_know_the_verb_is_told_from_one_that_found_a_mistake() {
        // What a bateri from before `view` says: it does not know the verb, or the request is
        // longer than any request of its own is allowed to be (it reads the length first).
        for old in [
            Failure::usage("`view` is no verb"),
            Failure::usage("the message is over 8192 bytes"),
        ] {
            assert!(is_old(&old), "{}", old.text);
        }
        // What one that knows it says: a mistake in the request, or the length of its own limit.
        for new in [
            Failure::usage("`ref` is six letters and digits"),
            Failure::usage("the message is over 131072 bytes"),
            Failure::new(Kind::Failed, "the message is over 8192 bytes"),
        ] {
            assert!(!is_old(&new), "{}", new.text);
        }
    }
}
