//! What an agent asks of the browsers the bateri app shows (`bateri browser`), and the answer that
//! is a file: the same text as the rest of [`crate::message`], in the same version 2, written here
//! so that the part that is about browsers is read in one place.
//!
//! A browser is an element of a pane's page, and the helper that draws it runs on the computer the
//! bateri app runs on, so the command is never relayed to another machine ([`crate::message::RELAYED`]
//! has none of these): the terminal asks the app it belongs to, and the app asks the helper.
//!
//! ```text
//! bateri 2                      bateri 2
//! browser text                  browser type
//! pane p9@prod                  pane p9
//! element x6d61696e             text x68692074686572650a
//! part 1a2b-3c-0                into x653132
//! ```
//!
//! * The verb line is `browser` and a word: `ls` (the browsers of the pane: each one's id, address
//!   and title), `text` (what the page says), `snapshot` (what the page is made of, as the
//!   accessibility tree, each element named in a way `click` takes), `shot` (a picture of the room
//!   of the browser), `click`, `type`, `open`, `back`, `reload`, `clear` (empties what the browser
//!   keeps: its `what` is `cache` or `site`) and `console` (the last lines of the page's console).
//! * `pane` is the pane that holds the browser, as the other verbs write one (`p9`, or `p9@prod` for
//!   one of another machine). `element` is the id of the browser in that pane's page (hex, below);
//!   with none, the pane's only browser, or the one that has the keyboard. `ls` takes none.
//! * `target` is what `snapshot` names an element by, or `X,Y`, a point of the page in pixels;
//!   `click` clicks it. `text` is what `type` types, and `into` is a target it clicks first, so that
//!   the keys go there. `url` is where `open` goes: an `http://` or `https://` address, a `data:`
//!   one or `about:blank`, and nothing else (`file:`, `javascript:` and the rest are a usage
//!   error). What a word means is the app's to read: a target it does not know is its
//!   `error usage`.
//! * Free words (`element`, `target`, `text`, `into`, `url`) are `x` and the hex of their bytes, as
//!   a `relay`'s words are ([`crate::message::hex_word`]), so that a line break or a space in a
//!   page's text stays in the word.
//! * `what` is what `clear` empties: `cache` (the files the browser keeps of the sites it has been
//!   to, and the service workers of the one it is at) or `site` (what the sites of the name it is at
//!   keep: the person is signed out of them). A word the app does not know is its `error usage`.
//! * `part` is a name the asker makes (letters, digits, `-` and `_`, at most 64) for the answers
//!   that may not fit a reply: `text`, `snapshot`, `shot` and `console` carry one.
//!
//! **The answer.** `ok` for a verb that has nothing to say (`click`, `type`, `open`, `back`,
//! `reload`, `clear`); `ok text` and the text, byte for byte, for `ls`, and for `text`, `snapshot`
//! and `console` when the text fits a reply ([`TEXT_MAX`]); and, when it does not, and always for `shot`, the answer
//! is a file on the machine of the one who asked, since a program on a server cannot open a path
//! of the computer the app runs on:
//!
//! ```text
//! bateri 2
//! ok filed
//! kind png
//! bytes 48213
//! ```
//!
//! `kind` is `png` (a `shot`) or `txt` (a `text`, a `snapshot` or a `console`), and `bytes` how long the file
//! is. The app writes it as `<part>.<kind>` in [`crate::paths::BROWSER`] below the asker's
//! [`crate::paths::DIR`], on the asker's machine, whose `HOME` is not the app's to know; the
//! command, which made the folder (mode 0700: a page may hold what a person signed in to), checks
//! that the file is there, whole, and prints its path. A bateri that does not know `browser` says
//! `error usage` with "is no verb" ([`crate::package::is_old`]).

use crate::message::{Failure, Fields, MAX_BYTES, hex_word, push, word_of_hex};
use crate::package::check_part;
use crate::terminal::Anchor;

/// The most text a `text`, a `snapshot` or an `ls` may say in the reply itself: what is over it
/// is a file ([`Filed`]). The reply is one message of the app channel, which is [`MAX_BYTES`] at
/// most (the larger bound is a `relay`'s alone), less the lines before the text.
pub const TEXT_MAX: usize = MAX_BYTES - 32;

/// The most an element's id may be, in bytes.
pub const ELEMENT_MAX: usize = 128;

/// The most a target may be, in bytes.
pub const TARGET_MAX: usize = 128;

/// The most an address may be, in bytes (the same as a source in an `app.json`).
pub const URL_MAX: usize = 2048;

/// The most `type` may type at once, in bytes. As hex it is twice that, within a request.
pub const TYPED_MAX: usize = 2048;

/// What an agent asks of a browser, and of which.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Browser {
    /// The pane that holds the browser: `p9`, or `p9@prod` for a pane of another machine.
    pub pane: Anchor,
    /// The id of the browser in the pane's page; none is the pane's only browser, or the one that
    /// has the keyboard. `ls` has none.
    pub element: Option<String>,
    pub action: Action,
}

/// What is asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// The browsers of the pane.
    Ls,
    /// What the page says. `part` names the file the text goes to when it is too long for a reply.
    Text { part: String },
    /// What the page is made of, as the accessibility tree.
    Snapshot { part: String },
    /// A picture of the browser's room, always a file.
    Shot { part: String },
    /// Click an element the snapshot named, or a point `X,Y`.
    Click { target: String },
    /// Type `text`, after a click on `into` when it is given.
    Type { text: String, into: Option<String> },
    /// Go to an address.
    Open { url: String },
    /// Go back in the history.
    Back,
    /// Load the page again.
    Reload,
    /// Empty what the browser keeps: its cache or the data of its site.
    Clear { what: Clearing },
    /// The last lines of the page's console (log, warning, error, uncaught error), oldest first.
    /// `part` names the file the text goes to when it is too long for a reply.
    Console { part: String },
}

/// What `clear` empties.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clearing {
    /// The files the browser keeps to load pages faster (of every site: the engine keeps them
    /// together) and the service workers and cache storage of the site it is at. Sessions and what
    /// a page saved stay.
    Cache,
    /// What the sites of the name the browser is at keep, at every port: cookies, local storage,
    /// databases, service workers. The person is signed out of them.
    Site,
}

impl Clearing {
    /// The word the request says.
    pub fn word(self) -> &'static str {
        match self {
            Clearing::Cache => "cache",
            Clearing::Site => "site",
        }
    }

    fn from_word(word: &str) -> Option<Clearing> {
        [Clearing::Cache, Clearing::Site]
            .into_iter()
            .find(|what| what.word() == word)
    }
}

impl Action {
    /// The words a sub-verb is written as, in the order the command lists them.
    pub const WORDS: [&'static str; 11] = [
        "ls", "text", "snapshot", "shot", "click", "type", "open", "back", "reload", "clear",
        "console",
    ];

    /// The sub-verb's word.
    pub fn word(&self) -> &'static str {
        match self {
            Action::Ls => "ls",
            Action::Text { .. } => "text",
            Action::Snapshot { .. } => "snapshot",
            Action::Shot { .. } => "shot",
            Action::Click { .. } => "click",
            Action::Type { .. } => "type",
            Action::Open { .. } => "open",
            Action::Back => "back",
            Action::Reload => "reload",
            Action::Clear { .. } => "clear",
            Action::Console { .. } => "console",
        }
    }

    /// The kind of file this action's answer is, when it is one; `None` for an action whose
    /// answer is never a file.
    pub fn file(&self) -> Option<FileKind> {
        match self {
            Action::Text { .. } | Action::Snapshot { .. } | Action::Console { .. } => {
                Some(FileKind::Text)
            }
            Action::Shot { .. } => Some(FileKind::Png),
            _ => None,
        }
    }
}

impl Browser {
    pub(crate) fn encode_into(&self, out: &mut String) -> Result<(), Failure> {
        out.push_str("browser ");
        out.push_str(self.action.word());
        out.push('\n');
        push(out, "pane", &self.pane.to_string())?;
        if let Some(element) = &self.element {
            if self.action == Action::Ls {
                return Err(Failure::usage("`browser ls` takes no `element`"));
            }
            check_element(element)?;
            push(out, "element", &hex_word(element))?;
        }
        match &self.action {
            Action::Ls | Action::Back | Action::Reload => {}
            Action::Text { part }
            | Action::Snapshot { part }
            | Action::Shot { part }
            | Action::Console { part } => {
                check_part(part)?;
                push(out, "part", part)?;
            }
            Action::Clear { what } => push(out, "what", what.word())?,
            Action::Click { target } => {
                check_target(target)?;
                push(out, "target", &hex_word(target))?;
            }
            Action::Type { text, into } => {
                check_typed(text)?;
                push(out, "text", &hex_word(text))?;
                if let Some(into) = into {
                    check_target(into)?;
                    push(out, "into", &hex_word(into))?;
                }
            }
            Action::Open { url } => {
                check_url(url)?;
                push(out, "url", &hex_word(url))?;
            }
        }
        Ok(())
    }
}

/// The request of a `browser` verb.
pub(crate) fn read<'a>(
    verb: &'a str,
    lines: impl Iterator<Item = &'a str>,
) -> Result<Browser, Failure> {
    let word = verb.strip_prefix("browser ").unwrap_or_default();
    let allowed: &[&str] = match word {
        "ls" => &["pane"],
        "text" | "snapshot" | "shot" | "console" => &["pane", "element", "part"],
        "clear" => &["pane", "element", "what"],
        "click" => &["pane", "element", "target"],
        "type" => &["pane", "element", "text", "into"],
        "open" => &["pane", "element", "url"],
        "back" | "reload" => &["pane", "element"],
        other => {
            return Err(Failure::usage(format!(
                "`browser {other}` is no verb: browser ls, text, snapshot, shot, click, type, open, back, reload, clear or console"
            )));
        }
    };
    let fields = Fields::read(verb, lines, allowed)?;
    let pane = fields.need("pane")?;
    let pane = Anchor::address(pane).ok_or_else(|| {
        Failure::usage(format!(
            "`pane` names a terminal as kablo does, and a machine after an `@` for one of another (`p9@prod`), not `{pane}`"
        ))
    })?;
    let element = fields
        .get("element")
        .map(|value| {
            let element = word_of_hex(value)?;
            check_element(&element)?;
            Ok::<String, Failure>(element)
        })
        .transpose()?;
    let part = |fields: &Fields<'_>| -> Result<String, Failure> {
        let part = fields.need("part")?;
        check_part(part)?;
        Ok(part.to_owned())
    };
    let action = match word {
        "ls" => Action::Ls,
        "text" => Action::Text {
            part: part(&fields)?,
        },
        "snapshot" => Action::Snapshot {
            part: part(&fields)?,
        },
        "shot" => Action::Shot {
            part: part(&fields)?,
        },
        "console" => Action::Console {
            part: part(&fields)?,
        },
        "clear" => {
            let what = fields.need("what")?;
            let what = Clearing::from_word(what)
                .ok_or_else(|| Failure::usage(format!("`what` is cache or site, not `{what}`")))?;
            Action::Clear { what }
        }
        "click" => {
            let target = word_of_hex(fields.need("target")?)?;
            check_target(&target)?;
            Action::Click { target }
        }
        "type" => {
            let text = word_of_hex(fields.need("text")?)?;
            check_typed(&text)?;
            let into = fields
                .get("into")
                .map(|value| {
                    let into = word_of_hex(value)?;
                    check_target(&into)?;
                    Ok::<String, Failure>(into)
                })
                .transpose()?;
            Action::Type { text, into }
        }
        "open" => {
            let url = word_of_hex(fields.need("url")?)?;
            check_url(&url)?;
            Action::Open { url }
        }
        "back" => Action::Back,
        _ => Action::Reload,
    };
    Ok(Browser {
        pane,
        element,
        action,
    })
}

fn check_element(element: &str) -> Result<(), Failure> {
    if element.is_empty() || element.len() > ELEMENT_MAX || element.chars().any(char::is_control) {
        return Err(Failure::usage(format!(
            "an `element` is 1 to {ELEMENT_MAX} bytes with no control character"
        )));
    }
    Ok(())
}

fn check_target(target: &str) -> Result<(), Failure> {
    if target.is_empty()
        || target.len() > TARGET_MAX
        || target.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(Failure::usage(format!(
            "a target is one word of 1 to {TARGET_MAX} bytes, as a snapshot names an element or `X,Y`"
        )));
    }
    Ok(())
}

fn check_url(url: &str) -> Result<(), Failure> {
    if url.is_empty() || url.len() > URL_MAX || url.chars().any(char::is_control) {
        return Err(Failure::usage(format!(
            "an address is 1 to {URL_MAX} bytes with no control character"
        )));
    }
    if !is_openable(url) {
        return Err(Failure::usage(
            "`open` goes to an http:// or https:// address, a data: address or about:blank"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Whether `url` is of the kinds `open` takes: `http` and `https` (with a host after the `://`),
/// `data:` and `about:blank`, the scheme in any case. Every reader of a request applies this, the
/// command to fail early and plainly, the app and the helper to hold what they are told to nothing
/// more than the command lets through.
fn is_openable(url: &str) -> bool {
    let starts = |prefix: &str| {
        url.get(..prefix.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
    };
    let to_a_host = |prefix: &str| starts(prefix) && url.len() > prefix.len();
    to_a_host("http://")
        || to_a_host("https://")
        || starts("data:")
        || url.eq_ignore_ascii_case("about:blank")
}

/// The address `open` is given, whole, as an address bar reads what is typed in it: an address
/// with its scheme (or `about:blank`, `data:`) as it is; a port alone (`3000`, `:3000`, and a path
/// after it) on this machine's `localhost`; a host a developer is at over `http` (`localhost`,
/// `*.localhost`, `[::1]`, an IPv4 address, or a name with no dot and a port, `myhost:8080`); and
/// any other name with a dot over `https` (`pencere.dev`). `None` for words that are no address
/// (a space in them, a name with no dot and no port): `open` does not search.
#[must_use]
pub fn complete_address(words: &str) -> Option<String> {
    let words = words.trim();
    if words.is_empty() || words.chars().any(char::is_whitespace) {
        return None;
    }
    let lower = words.to_ascii_lowercase();
    let has_scheme = lower.split_once("://").is_some_and(|(scheme, _)| {
        scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
    });
    if has_scheme || lower == "about:blank" || lower.starts_with("data:") {
        return Some(words.to_owned());
    }
    let (authority, rest) = match words.find(['/', '?', '#']) {
        Some(at) => words.split_at(at),
        None => (words, ""),
    };
    let port_ok = |port: &str| {
        !port.is_empty()
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u32>().is_ok_and(|n| (1..=65_535).contains(&n))
    };
    let bare = authority.strip_prefix(':').unwrap_or(authority);
    if port_ok(bare) {
        return Some(format!(
            "http://localhost:{}{rest}",
            bare.parse::<u32>().ok()?
        ));
    }
    let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
        let (inner, after) = inner.split_once(']')?;
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (format!("[{inner}]"), port)
    } else {
        match authority.split_once(':') {
            Some((host, port)) => (host.to_owned(), Some(port)),
            None => (authority.to_owned(), None),
        }
    };
    if host.is_empty() || port.is_some_and(|port| !port_ok(port)) {
        return None;
    }
    let host_lower = host.to_ascii_lowercase();
    let ipv4 = host_lower.split('.').count() == 4
        && host_lower
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()));
    let local = matches!(host_lower.as_str(), "localhost" | "[::1]" | "0.0.0.0")
        || host_lower.ends_with(".localhost")
        || ipv4;
    if local || (port.is_some() && !host.contains('.')) {
        return Some(format!("http://{authority}{rest}"));
    }
    if host.contains('.') && !host.starts_with('.') && !host.ends_with('.') {
        return Some(format!("https://{authority}{rest}"));
    }
    None
}

fn check_typed(text: &str) -> Result<(), Failure> {
    if text.is_empty() || text.len() > TYPED_MAX || text.contains('\0') {
        return Err(Failure::usage(format!(
            "what `type` types is 1 to {TYPED_MAX} bytes of text with no NUL"
        )));
    }
    Ok(())
}

/// What kind of file an answer is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    /// A picture (`shot`).
    Png,
    /// Text that did not fit a reply (`text`, `snapshot`).
    Text,
}

impl FileKind {
    /// The word the answer says and the extension of the file's name.
    pub fn extension(self) -> &'static str {
        match self {
            FileKind::Png => "png",
            FileKind::Text => "txt",
        }
    }

    fn from_word(word: &str) -> Option<FileKind> {
        [FileKind::Png, FileKind::Text]
            .into_iter()
            .find(|kind| kind.extension() == word)
    }
}

/// The answer that is a file: written by the app on the machine of the one who asked, as
/// `<part>.<extension>` in [`crate::paths::BROWSER`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filed {
    pub kind: FileKind,
    /// How long the file is.
    pub bytes: u64,
}

impl Filed {
    /// The name of the file an asker that named `part` finds.
    pub fn name(&self, part: &str) -> String {
        format!("{part}.{}", self.kind.extension())
    }

    pub(crate) fn encode_into(&self, out: &mut String) {
        out.push_str("ok filed\n");
        out.push_str(&format!("kind {}\n", self.kind.extension()));
        out.push_str(&format!("bytes {}\n", self.bytes));
    }

    /// The answer the lines after `ok filed` say. A line it does not know is passed over, so that
    /// the app may say more later.
    pub(crate) fn read(body: &str) -> Result<Filed, Failure> {
        let lines = body.strip_suffix('\n').unwrap_or(body).split('\n');
        let fields = Fields::read_ignoring_unknown("filed", lines, &["kind", "bytes"])?;
        let kind = fields.need("kind")?;
        let kind = FileKind::from_word(kind)
            .ok_or_else(|| Failure::usage(format!("`kind` is png or txt, not `{kind}`")))?;
        let bytes = fields.need("bytes")?;
        let bytes = bytes
            .parse::<u64>()
            .ok()
            .filter(|n| n.to_string() == bytes)
            .ok_or_else(|| Failure::usage("`bytes` is a number written plainly"))?;
        Ok(Filed { kind, bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{RELAYED, Reply, Request};

    const V2_TEXT: &str =
        "bateri 2\nbrowser text\npane p9@prod\nelement x6d61696e\npart 1a2b-3c-0\n";
    const V2_TYPE: &str =
        "bateri 2\nbrowser type\npane p9\ntext x68692074686572650a\ninto x653132\n";
    const V2_OPEN: &str = "bateri 2\nbrowser open\npane p9\nurl x68747470733a2f2f6c6f63616c686f73743a333030302f613f623d31\n";
    const V2_CLICK: &str = "bateri 2\nbrowser click\npane p4\ntarget x3431322c3838\n";
    const V2_LS: &str = "bateri 2\nbrowser ls\npane p9@local\n";
    const V2_CLEAR: &str = "bateri 2\nbrowser clear\npane p9\nwhat site\n";
    const V2_CONSOLE: &str = "bateri 2\nbrowser console\npane p9\npart 1a2b-3c-1\n";
    const V2_FILED: &str = "bateri 2\nok filed\nkind png\nbytes 48213\n";

    fn pane(text: &str) -> Anchor {
        Anchor::address(text).expect("a pane")
    }

    #[test]
    fn a_bare_address_is_completed_as_an_address_bar_reads_it() {
        for (typed, whole) in [
            ("3000", "http://localhost:3000"),
            (":5173/about", "http://localhost:5173/about"),
            ("localhost:3000/a?b=1", "http://localhost:3000/a?b=1"),
            ("app.localhost", "http://app.localhost"),
            ("192.168.1.5:8080", "http://192.168.1.5:8080"),
            ("[::1]:4000", "http://[::1]:4000"),
            ("myhost:8080", "http://myhost:8080"),
            ("pencere.dev", "https://pencere.dev"),
            ("Pencere.dev/docs#x", "https://Pencere.dev/docs#x"),
            ("https://pencere.dev", "https://pencere.dev"),
            ("HTTP://localhost:1", "HTTP://localhost:1"),
            ("about:blank", "about:blank"),
            ("data:text/plain,hi", "data:text/plain,hi"),
            ("  example.com  ", "https://example.com"),
        ] {
            assert_eq!(complete_address(typed).as_deref(), Some(whole), "{typed}");
        }
        for typed in [
            "",
            "rust lifetimes",
            "myhost",
            ".dev",
            "example.",
            "70000",
            "host:99999",
            "[::1",
        ] {
            assert_eq!(complete_address(typed), None, "{typed}");
        }
        // What is completed is what `open` takes, and what it does not take stays refused: a scheme
        // other than the four is passed through whole and refused by the rule every reader applies.
        assert!(is_openable(
            &complete_address("pencere.dev").expect("an address")
        ));
        assert!(!is_openable(
            &complete_address("file:///etc/passwd").expect("as it is")
        ));
    }

    /// Every request the format has, with the text it is frozen as.
    fn sample() -> Vec<(Request, &'static str)> {
        let browser = |pane_at: &str, element: Option<&str>, action| {
            Request::Browser(Browser {
                pane: pane(pane_at),
                element: element.map(str::to_owned),
                action,
            })
        };
        vec![
            (
                browser(
                    "p9@prod",
                    Some("main"),
                    Action::Text {
                        part: "1a2b-3c-0".to_owned(),
                    },
                ),
                V2_TEXT,
            ),
            (
                browser(
                    "p9",
                    None,
                    Action::Type {
                        text: "hi there\n".to_owned(),
                        into: Some("e12".to_owned()),
                    },
                ),
                V2_TYPE,
            ),
            (
                browser(
                    "p9",
                    None,
                    Action::Open {
                        url: "https://localhost:3000/a?b=1".to_owned(),
                    },
                ),
                V2_OPEN,
            ),
            (
                browser(
                    "p4",
                    None,
                    Action::Click {
                        target: "412,88".to_owned(),
                    },
                ),
                V2_CLICK,
            ),
            (browser("p9@local", None, Action::Ls), V2_LS),
            (
                browser(
                    "p9",
                    None,
                    Action::Clear {
                        what: Clearing::Site,
                    },
                ),
                V2_CLEAR,
            ),
            (
                browser(
                    "p9",
                    None,
                    Action::Console {
                        part: "1a2b-3c-1".to_owned(),
                    },
                ),
                V2_CONSOLE,
            ),
        ]
    }

    #[test]
    fn the_requests_are_written_and_read_byte_for_byte() {
        for (request, text) in sample() {
            assert_eq!(request.encode().as_deref(), Ok(text), "{text}");
            assert_eq!(Request::parse(text), Ok(request), "{text}");
        }
    }

    #[test]
    fn every_sub_verb_goes_round_with_and_without_an_element() {
        let part = "p-1".to_owned();
        let part2 = part.clone();
        let actions = [
            Action::Ls,
            Action::Text { part: part.clone() },
            Action::Snapshot { part: part.clone() },
            Action::Shot { part },
            Action::Click {
                target: "e1".to_owned(),
            },
            Action::Type {
                text: "x".to_owned(),
                into: None,
            },
            Action::Open {
                url: "http://localhost:80/".to_owned(),
            },
            Action::Back,
            Action::Reload,
            Action::Clear {
                what: Clearing::Cache,
            },
            Action::Console { part: part2 },
        ];
        assert_eq!(
            actions.iter().map(Action::word).collect::<Vec<_>>(),
            Action::WORDS
        );
        for action in actions {
            for element in [None, Some("tab-2".to_owned())] {
                if action == Action::Ls && element.is_some() {
                    continue;
                }
                let request = Request::Browser(Browser {
                    pane: pane("p3"),
                    element,
                    action: action.clone(),
                });
                let text = request.encode().expect("written");
                assert_eq!(Request::parse(&text), Ok(request), "{text}");
            }
        }
    }

    #[test]
    fn a_word_with_a_line_break_a_space_or_a_multi_byte_letter_stays_one_word() {
        let odd = Request::Browser(Browser {
            pane: pane("p9"),
            element: Some("tab ü\u{1F600}".to_owned()),
            action: Action::Type {
                text: "satır 1\nsatır 2 \t ığş".to_owned(),
                into: None,
            },
        });
        let text = odd.encode().expect("written");
        assert_eq!(text.lines().count(), 5, "{text}");
        assert_eq!(Request::parse(&text), Ok(odd));
    }

    #[test]
    fn browser_is_no_relayed_command() {
        assert!(!RELAYED.contains(&"browser"));
    }

    #[test]
    fn a_sub_verb_that_is_none_is_a_usage_error_that_says_which_there_are() {
        for text in [
            "bateri 2\nbrowser\npane p9\n",
            "bateri 2\nbrowser dance\npane p9\n",
            "bateri 2\nbrowser forward\npane p9\n",
        ] {
            let failed = Request::parse(text).expect_err(text);
            assert_eq!(failed.kind, crate::message::Kind::Usage, "{text}");
            assert!(failed.text.contains("is no verb"), "{}", failed.text);
        }
        let failed = Request::parse("bateri 2\nbrowser dance\npane p9\n").expect_err("dance");
        assert!(failed.text.contains("reload"), "{}", failed.text);
    }

    #[test]
    fn a_request_that_is_not_right_is_a_usage_error_that_says_which() {
        for (text, said) in [
            // The pane is needed, and is a terminal.
            ("bateri 2\nbrowser back\n", "needs a `pane`"),
            (
                "bateri 2\nbrowser back\npane a b\n",
                "`pane` names a terminal",
            ),
            (
                "bateri 2\nbrowser back\npane p9@\n",
                "`pane` names a terminal",
            ),
            // Every sub-verb takes its own keys and no other.
            (
                "bateri 2\nbrowser ls\npane p9\nelement x61\n",
                "takes no `element`",
            ),
            (
                "bateri 2\nbrowser back\npane p9\npart a\n",
                "takes no `part`",
            ),
            ("bateri 2\nbrowser text\npane p9\n", "needs a `part`"),
            ("bateri 2\nbrowser console\npane p9\n", "needs a `part`"),
            // What `clear` empties is one of two words.
            ("bateri 2\nbrowser clear\npane p9\n", "needs a `what`"),
            (
                "bateri 2\nbrowser clear\npane p9\nwhat all\n",
                "`what` is cache or site, not `all`",
            ),
            (
                "bateri 2\nbrowser clear\npane p9\nwhat cache\npart a\n",
                "takes no `part`",
            ),
            (
                "bateri 2\nbrowser text\npane p9\ntarget x61\npart a\n",
                "takes no `target`",
            ),
            ("bateri 2\nbrowser click\npane p9\n", "needs a `target`"),
            ("bateri 2\nbrowser type\npane p9\n", "needs a `text`"),
            ("bateri 2\nbrowser open\npane p9\n", "needs a `url`"),
            (
                "bateri 2\nbrowser open\npane p9\nurl x61\nurl x61\n",
                "given twice",
            ),
            // A part is a name that is no path.
            (
                "bateri 2\nbrowser shot\npane p9\npart ../x\n",
                "not the name of a part",
            ),
            (
                "bateri 2\nbrowser shot\npane p9\npart a.png\n",
                "not the name of a part",
            ),
            // The free words are hex, whole and text.
            (
                "bateri 2\nbrowser open\npane p9\nurl http://a\n",
                "`x` and the hex",
            ),
            ("bateri 2\nbrowser open\npane p9\nurl x6\n", "even number"),
            ("bateri 2\nbrowser open\npane p9\nurl xff\n", "not text"),
            // An address is of the kinds `open` takes: `example.com`, `ftp://a`, `file:///a`,
            // `javascript:1` and `http://` alone are not.
            (
                "bateri 2\nbrowser open\npane p9\nurl x6578616d706c652e636f6d\n",
                "http:// or https://",
            ),
            (
                "bateri 2\nbrowser open\npane p9\nurl x6674703a2f2f61\n",
                "http:// or https://",
            ),
            (
                "bateri 2\nbrowser open\npane p9\nurl x66696c653a2f2f2f61\n",
                "http:// or https://",
            ),
            (
                "bateri 2\nbrowser open\npane p9\nurl x6a6176617363726970743a31\n",
                "http:// or https://",
            ),
            (
                "bateri 2\nbrowser open\npane p9\nurl x687474703a2f2f\n",
                "http:// or https://",
            ),
            (
                "bateri 2\nbrowser open\npane p9\nurl x\n",
                "1 to 2048 bytes",
            ),
            (
                "bateri 2\nbrowser click\npane p9\ntarget x6120\n",
                "one word",
            ),
            (
                "bateri 2\nbrowser back\npane p9\nelement x\n",
                "1 to 128 bytes",
            ),
            (
                "bateri 2\nbrowser back\npane p9\nelement x0a\n",
                "no control character",
            ),
            ("bateri 2\nbrowser type\npane p9\ntext x00\n", "no NUL"),
        ] {
            let failed = Request::parse(text).expect_err(text);
            assert_eq!(failed.kind, crate::message::Kind::Usage, "{text}");
            assert!(failed.text.contains(said), "{text:?}: {}", failed.text);
        }
    }

    #[test]
    fn a_request_that_cannot_be_read_back_is_not_written() {
        let write = |element: Option<&str>, action| {
            Request::Browser(Browser {
                pane: pane("p9"),
                element: element.map(str::to_owned),
                action,
            })
            .encode()
        };
        assert!(write(Some("a"), Action::Ls).is_err());
        assert!(write(Some(""), Action::Back).is_err());
        assert!(write(Some(&"x".repeat(ELEMENT_MAX + 1)), Action::Back).is_err());
        assert!(
            write(
                None,
                Action::Text {
                    part: String::new()
                }
            )
            .is_err()
        );
        assert!(
            write(
                None,
                Action::Shot {
                    part: "a/b".to_owned()
                }
            )
            .is_err()
        );
        assert!(
            write(
                None,
                Action::Click {
                    target: "two words".to_owned()
                }
            )
            .is_err()
        );
        assert!(
            write(
                None,
                Action::Type {
                    text: String::new(),
                    into: None
                }
            )
            .is_err()
        );
        assert!(
            write(
                None,
                Action::Open {
                    url: "http://a\nb".to_owned()
                }
            )
            .is_err()
        );
    }

    #[test]
    fn open_goes_to_http_https_data_and_about_blank_only() {
        let write = |url: &str| {
            Request::Browser(Browser {
                pane: pane("p9"),
                element: None,
                action: Action::Open {
                    url: url.to_owned(),
                },
            })
            .encode()
        };
        for url in [
            "http://localhost:3000/",
            "https://example.com/a?b=1#c",
            "HTTPS://Example.com",
            "data:text/html,<p>hi</p>",
            "about:blank",
            "ABOUT:blank",
        ] {
            let text = write(url).unwrap_or_else(|failed| panic!("{url}: {}", failed.text));
            assert_eq!(
                Request::parse(&text),
                Ok(Request::Browser(Browser {
                    pane: pane("p9"),
                    element: None,
                    action: Action::Open {
                        url: url.to_owned()
                    },
                })),
                "{url}"
            );
        }
        for url in [
            "example.com",
            "//example.com",
            "ftp://example.com",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
            "about:config",
            "about:blank#x",
            "http://",
            "https:/a",
            "xhttp://a",
            " http://a",
        ] {
            let failed = write(url).expect_err(url);
            assert_eq!(failed.kind, crate::message::Kind::Usage, "{url}");
            assert!(failed.text.contains("http:// or https://"), "{url}");
        }
    }

    #[test]
    fn the_largest_words_fit_a_request() {
        let most = Request::Browser(Browser {
            pane: pane(&format!("p9@{}", "h".repeat(253))),
            element: Some("e".repeat(ELEMENT_MAX)),
            action: Action::Type {
                text: "t".repeat(TYPED_MAX),
                into: Some("i".repeat(TARGET_MAX)),
            },
        });
        let text = most.encode().expect("fits");
        assert!(text.len() <= MAX_BYTES, "{}", text.len());
        assert_eq!(Request::parse(&text), Ok(most));
        let url = Request::Browser(Browser {
            pane: pane("p9"),
            element: Some("e".repeat(ELEMENT_MAX)),
            action: Action::Open {
                url: format!("https://{}", "u".repeat(URL_MAX - 8)),
            },
        });
        assert!(url.encode().expect("fits").len() <= MAX_BYTES);
        let over = Request::Browser(Browser {
            pane: pane("p9"),
            element: None,
            action: Action::Open {
                url: format!("https://{}", "u".repeat(URL_MAX - 7)),
            },
        });
        assert!(over.encode().is_err());
    }

    #[test]
    fn a_reply_that_is_a_file_is_written_and_read_byte_for_byte() {
        let filed = Filed {
            kind: FileKind::Png,
            bytes: 48213,
        };
        assert_eq!(Reply::Filed(filed).encode(), V2_FILED);
        assert_eq!(Reply::parse(V2_FILED), Ok(Reply::Filed(filed)));
        assert_eq!(filed.name("1a2b-3c-0"), "1a2b-3c-0.png");
        let text = Filed {
            kind: FileKind::Text,
            bytes: 9000,
        };
        assert_eq!(text.name("p"), "p.txt");
        assert_eq!(
            Reply::parse(&Reply::Filed(text).encode()),
            Ok(Reply::Filed(text))
        );
        // A newer app may say more; this reader skips what it does not know.
        let more = "bateri 2\nok filed\nkind txt\nlines 12\nbytes 9000\n";
        assert_eq!(Reply::parse(more), Ok(Reply::Filed(text)));
    }

    #[test]
    fn a_reply_that_is_a_file_and_is_not_right_is_not_read() {
        for text in [
            "bateri 2\nok filed\n",
            "bateri 2\nok filed\nkind png\n",
            "bateri 2\nok filed\nbytes 3\n",
            "bateri 2\nok filed\nkind gif\nbytes 3\n",
            "bateri 2\nok filed\nkind png\nbytes 03\n",
            "bateri 2\nok filed\nkind png\nbytes -3\n",
            "bateri 2\nok filed\nkind png\nbytes 3\nbytes 3\n",
        ] {
            assert!(Reply::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn a_text_of_the_most_a_reply_holds_fits_the_size_of_a_message() {
        let reply = Reply::Text("x".repeat(TEXT_MAX)).encode();
        assert!(reply.len() <= MAX_BYTES, "{}", reply.len());
    }

    #[test]
    fn a_bateri_that_does_not_know_browser_is_told_by_its_own_words() {
        // What an older build says to the verb is what `package::is_old` reads.
        let failed = Failure::usage("`browser text` is no verb: place, ls, close");
        assert!(crate::package::is_old(&failed));
    }
}
