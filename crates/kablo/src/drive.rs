//! Driving a session from the machine it runs on, without attaching to it:
//! `kablo send`, `send-key`, `capture`, `wait` and `label`, and the waiting
//! half of `kablo new --wait`.
//!
//! These are the programs' commands (an agent, a script) and they are not
//! clients in the sense of `kablo connect`: they never attach, so the relay
//! goes on answering what the program asks its terminal and the app that shows
//! the session is not disturbed (`docs/PROTOCOL.md`, section 5, Driving a
//! session without attaching). Each one starts the same way. It scans the
//! socket directory, which tells it every session's id, label and relay
//! version in one pass, finds the one it was told to work on, and stops when
//! that session's relay is from before these messages: an older relay skips a
//! kind it does not know, so a `wait` would be waited out for ever and a `send`
//! would be dropped without a word.
//!
//! The conversation itself is a [`Link`], the same one the bridge keeps for an
//! app: when a relay is replaced (`kablo upgrade`) a `read` and a `wait` are
//! asked again of the new one, and a `write` never is, because the keys would be
//! pressed twice. Nothing here sets a timer while it goes on: the only limit is
//! the one `wait --for` was given, which is the poll's timeout, and the link's
//! own for a session that does not answer.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{
    Capture, ErrorCode, Key, MAX_HISTORY, MAX_LABEL, Message, OFFSET_END, Packet, State, Status,
    Stroke, key,
};

use crate::cli::usage_error;
use crate::link::Link;
use crate::listing::printable;
use crate::origin::{self, Candidate, Origin};
use crate::{exec, scan, sockdir, sys};

/// The status of a command that could not do what it was asked: the session is
/// not there or is ambiguous, its relay is old, the connection failed, the
/// program in it has ended. The words are on standard error.
pub const EXIT_TROUBLE: u8 = 1;

/// The status of `wait` and `new --wait` when kablo's own part fails: what they
/// end with is the program's status, and `1` is a status programs have. The same
/// number `kablo exec` uses.
pub const EXIT_WAIT_TROUBLE: u8 = exec::EXIT_TROUBLE;

/// The status of a command whose output was not read to the end because
/// whatever reads it went away (128 and the number of `SIGPIPE`), as `kablo exec`
/// and `kablo app` end.
pub const EXIT_PIPE: u8 = 141;

/// The status of `wait --for` when the time ran out first: what GNU `timeout`
/// ends with. A program may end with 124 (or 255) too, and the two cannot be
/// told apart by the status alone, as with `ssh`.
pub const EXIT_TIMEOUT: u8 = 124;

/// The first release whose relay takes `write`, `read` and `wait` and keeps a
/// label. A relay from before it skips them, and the commands refuse to be
/// sent to one.
pub const DRIVES_SINCE: [u64; 3] = [0, 2, 0];

/// The most text one `send` types: a message is one frame, and a little of it
/// is the message's own.
const MAX_TEXT: usize = kablo_proto::MAX_PAYLOAD - 64;

/// How long `new --wait` waits, once it has seen the program end, for the
/// session to go; the program's status is the answer either way.
const DELIVERY_WAIT: Duration = Duration::from_secs(5);

/// Which kind of status a command ends with when kablo itself fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
    /// Commands that end with `0` or `1`.
    Quick,
    /// Commands that end with a program's status (`wait`, `new --wait`).
    Waiting,
}

impl Family {
    fn trouble(self) -> u8 {
        match self {
            Family::Quick => EXIT_TROUBLE,
            Family::Waiting => EXIT_WAIT_TROUBLE,
        }
    }
}

// --- the arguments -----------------------------------------------------------------

/// Splits `args` into the words that are not options. `option` is given each
/// option's name and the words after it, and says whether it took it; `--`
/// ends the options.
fn positionals<'a>(
    args: &'a [OsString],
    mut option: impl FnMut(&str, &mut std::slice::Iter<'a, OsString>) -> Result<bool, String>,
) -> Result<Vec<&'a OsStr>, String> {
    let mut words = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--" {
            words.extend(it.by_ref().map(OsString::as_os_str));
            break;
        }
        let bytes = arg.as_bytes();
        // A lone `-` is a word (it means the input).
        if bytes.len() > 1 && bytes.starts_with(b"-") {
            let name = arg.to_str().unwrap_or("-?");
            if !option(name, &mut it)? {
                return Err(format!("unknown option {}", printable(bytes)));
            }
        } else {
            words.push(arg.as_os_str());
        }
    }
    Ok(words)
}

/// A session's id or label, which is text.
fn name_of(word: &OsStr) -> Result<String, String> {
    word.to_str()
        .map(str::to_owned)
        .ok_or_else(|| "a session's id or label is text".to_owned())
}

/// What `kablo send` types: the text, or the input when there is none.
#[derive(Debug, PartialEq, Eq)]
pub struct Send {
    target: Option<String>,
    /// `None`: read it from standard input.
    text: Option<Vec<u8>>,
}

/// Reads the arguments after `kablo send`: `[ID] [TEXT | -]`. One word is the
/// text (for the terminal this runs in), two are the session and the text, none
/// is the input; a lone `-` is the input too, for a session named.
pub fn parse_send(args: &[OsString]) -> Result<Send, String> {
    let words = positionals(args, |_, _| Ok(false))?;
    let text = |word: &OsStr| (word != "-").then(|| word.as_bytes().to_vec());
    match words.as_slice() {
        [] => Ok(Send {
            target: None,
            text: None,
        }),
        [word] => Ok(Send {
            target: None,
            text: text(word),
        }),
        [target, word] => Ok(Send {
            target: Some(name_of(target)?),
            text: text(word),
        }),
        _ => Err("send takes a session and one text (quote the text if it has spaces)".to_owned()),
    }
}

/// What `kablo send-key` presses.
#[derive(Debug, PartialEq, Eq)]
pub struct SendKey {
    target: Option<String>,
    keys: Vec<Key>,
}

/// Reads the arguments after `kablo send-key`: `[ID] KEY...`. The first word is
/// the session unless it is the name of a key (a session called `enter` is
/// reached by its id or by a label that is not a key's name).
pub fn parse_send_key(args: &[OsString]) -> Result<SendKey, String> {
    let words = positionals(args, |_, _| Ok(false))?;
    let key_of = |word: &OsStr| word.to_str().and_then(parse_key);
    let (target, keys) = match words.split_first() {
        None => return Err("send-key needs keys to press".to_owned()),
        Some((first, rest)) if key_of(first).is_none() => {
            if rest.is_empty() {
                return Err(format!(
                    "send-key needs a key after the session ({} is not the name of a key)",
                    printable(first.as_bytes())
                ));
            }
            (Some(name_of(first)?), rest)
        }
        Some(_) => (None, words.as_slice()),
    };
    let mut pressed = Vec::new();
    for word in keys {
        pressed.push(key_of(word).ok_or_else(|| {
            format!(
                "{} is not the name of a key (try enter, tab, escape, backspace, space, up, \
                 down, left, right, home, end, pageup, pagedown, insert, delete, f1 to f12, \
                 ctrl+X, alt+X, shift+tab)",
                printable(word.as_bytes())
            )
        })?);
    }
    Ok(SendKey {
        target,
        keys: pressed,
    })
}

/// The key a name stands for: a named key, or a character with a modifier
/// (`ctrl+c`, `alt+x`), with any modifiers (`ctrl`, `alt`, `shift`) in front of
/// either. A character alone is not a key: that is what `send` types.
pub fn parse_key(name: &str) -> Option<Key> {
    let mut mods = 0u8;
    let mut rest = name;
    while let Some((head, tail)) = rest.split_once('+') {
        if tail.is_empty() {
            break;
        }
        mods |= match head.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => key::mods::CTRL,
            "alt" | "option" => key::mods::ALT,
            "shift" => key::mods::SHIFT,
            _ => return None,
        };
        rest = tail;
    }
    let lower = rest.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" => key::ENTER,
        "tab" => key::TAB,
        "escape" => key::ESCAPE,
        "backspace" => key::BACKSPACE,
        "space" => u32::from(' '),
        "up" => key::UP,
        "down" => key::DOWN,
        "left" => key::LEFT,
        "right" => key::RIGHT,
        "home" => key::HOME,
        "end" => key::END,
        "pageup" => key::PAGE_UP,
        "pagedown" => key::PAGE_DOWN,
        "insert" => key::INSERT,
        "delete" => key::DELETE,
        other => match other.strip_prefix('f') {
            Some(digits) if digits.bytes().all(|b| b.is_ascii_digit()) && !digits.is_empty() => {
                let n: u8 = digits.parse().ok().filter(|n| (1..=12).contains(n))?;
                if n.to_string() != digits {
                    return None;
                }
                key::function(n)?
            }
            _ => {
                let mut chars = rest.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) if mods != 0 && !c.is_control() => {
                        u32::from(c.to_ascii_lowercase())
                    }
                    _ => return None,
                }
            }
        },
    };
    Some(Key::new(code, mods))
}

/// What `kablo capture` reads.
#[derive(Debug, PartialEq, Eq)]
pub struct CaptureArgs {
    target: Option<String>,
    capture: Capture,
    trim: bool,
}

/// Reads the arguments after `kablo capture`:
/// `[ID] [--history ROWS] [--unwrap] [--trim] [--vt]`.
pub fn parse_capture(args: &[OsString]) -> Result<CaptureArgs, String> {
    let mut capture = Capture::default();
    let (mut trim, mut others) = (false, false);
    let words = positionals(args, |name, it| {
        match name {
            "--history" => {
                let rows = it
                    .next()
                    .and_then(|value| value.to_str())
                    .and_then(|value| value.parse::<u32>().ok())
                    .filter(|&rows| rows <= MAX_HISTORY)
                    .ok_or_else(|| {
                        format!("--history takes a number of rows from 0 to {MAX_HISTORY}")
                    })?;
                capture.history = rows;
                others = true;
            }
            "--unwrap" => {
                capture.unwrap = true;
                others = true;
            }
            "--trim" => {
                trim = true;
                others = true;
            }
            "--vt" => capture.picture = true,
            _ => return Ok(false),
        }
        Ok(true)
    })?;
    if capture.picture && others {
        return Err(
            "--vt prints the screen as the bytes that draw it and takes no other option".to_owned(),
        );
    }
    let target = match words.as_slice() {
        [] => None,
        [word] => Some(name_of(word)?),
        _ => return Err("capture takes at most one session".to_owned()),
    };
    Ok(CaptureArgs {
        target,
        capture,
        trim,
    })
}

/// What `kablo wait` waits for.
#[derive(Debug, PartialEq, Eq)]
pub struct WaitArgs {
    target: String,
    /// How long, as it was written, and as a duration.
    within: Option<(String, Duration)>,
}

/// Reads the arguments after `kablo wait`: `ID [--for TIME]`.
pub fn parse_wait(args: &[OsString]) -> Result<WaitArgs, String> {
    let mut within = None;
    let words = positionals(args, |name, it| {
        if name != "--for" {
            return Ok(false);
        }
        let text = it
            .next()
            .and_then(|value| value.to_str())
            .ok_or("--for takes a time")?;
        within = Some((text.to_owned(), duration(text)?));
        Ok(true)
    })?;
    match words.as_slice() {
        [word] => Ok(WaitArgs {
            target: name_of(word)?,
            within,
        }),
        [] => Err("wait needs the session to wait for (it cannot wait for itself)".to_owned()),
        _ => Err("wait takes one session".to_owned()),
    }
}

/// A time: a number and a unit, `ms`, `s`, `m` or `h` (seconds when there is none).
pub fn duration(text: &str) -> Result<Duration, String> {
    let wrong = || {
        format!(
            "--for takes a time such as 500ms, 30, 30s, 5m or 2h, more than nothing (not {})",
            printable(text.as_bytes())
        )
    };
    let split = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);
    let seconds: f64 = match unit {
        "ms" => 0.001,
        "" | "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        _ => return Err(wrong()),
    };
    let amount: f64 = number.parse().map_err(|_| wrong())?;
    Duration::try_from_secs_f64(amount * seconds)
        .ok()
        .filter(|time| !time.is_zero())
        .ok_or_else(wrong)
}

/// What `kablo label` shows or sets.
#[derive(Debug, PartialEq, Eq)]
pub struct LabelArgs {
    target: Option<String>,
    /// `None`: show the label.
    text: Option<String>,
}

/// Reads the arguments after `kablo label`: `[ID] [TEXT]`. One word is the text
/// (for the terminal this runs in), two are the session and the text, none shows
/// the label of the terminal this runs in. An empty text clears it.
pub fn parse_label(args: &[OsString]) -> Result<LabelArgs, String> {
    let words = positionals(args, |_, _| Ok(false))?;
    let text = |word: &OsStr| -> Result<String, String> {
        let text = word.to_str().ok_or("a label is text")?;
        if text.len() > MAX_LABEL {
            return Err(format!("a label is at most {MAX_LABEL} bytes"));
        }
        Ok(text.to_owned())
    };
    match words.as_slice() {
        [] => Ok(LabelArgs {
            target: None,
            text: None,
        }),
        [word] => Ok(LabelArgs {
            target: None,
            text: Some(text(word)?),
        }),
        [target, word] => Ok(LabelArgs {
            target: Some(name_of(target)?),
            text: Some(text(word)?),
        }),
        _ => Err("label takes a session and one text (quote the text if it has spaces)".to_owned()),
    }
}

// --- finding the session -------------------------------------------------------------

/// Why no session was found to work on.
#[derive(Debug, PartialEq, Eq)]
enum Miss {
    /// The command was not told which session and runs in none.
    Usage(String),
    /// The name says no session or says more than one, or the session is broken.
    Trouble(String),
}

/// The session `name` says among `statuses`: the one whose id it is or whose
/// label it is (an id exactly, a label without regard to case); with no name,
/// the session `own` finds this process to be in (`usage` is how to say which,
/// for when there is none). A name that fits more than one
/// session, an id that is another's label included, names none: the command
/// would type into whichever it picked, and the wrong terminal is worse than
/// none.
fn find<'a>(
    statuses: &'a [Status],
    name: Option<&str>,
    usage: &str,
    own: impl FnOnce(&[Status]) -> Origin,
) -> Result<&'a Status, Miss> {
    let session = match name {
        None => match own(statuses) {
            Origin::Terminal(id) => statuses.iter().find(|status| status.id == id),
            Origin::None => None,
        }
        .ok_or_else(|| {
            Miss::Usage(format!(
                "this is not run in a terminal of a session, so say which one: {usage}"
            ))
        })?,
        Some(name) => {
            let lower = name.to_lowercase();
            let fits: Vec<&Status> = statuses
                .iter()
                .filter(|status| {
                    status.id.as_str() == name
                        || status
                            .label
                            .as_deref()
                            .is_some_and(|label| label.to_lowercase() == lower)
                })
                .collect();
            match fits.as_slice() {
                [] => {
                    return Err(Miss::Trouble(format!(
                        "no session has the id or the label {}",
                        printable(name.as_bytes())
                    )));
                }
                [one] => *one,
                many => {
                    let mut rows = String::new();
                    for status in many {
                        rows.push_str("  ");
                        rows.push_str(&printable(status.id.as_str().as_bytes()));
                        if let Some(label) = &status.label {
                            rows.push_str("  (label ");
                            rows.push_str(&printable(label.as_bytes()));
                            rows.push(')');
                        }
                        rows.push('\n');
                    }
                    return Err(Miss::Trouble(format!(
                        "{} is the id or the label of more than one session, so nothing was \
                         done:\n{}",
                        printable(name.as_bytes()),
                        rows.trim_end()
                    )));
                }
            }
        }
    };
    if session.state == State::BROKEN {
        return Err(Miss::Trouble(format!(
            "session {} is broken: its relay does not answer",
            session.id
        )));
    }
    Ok(session)
}

/// The session this process runs in, among the ones `statuses` say are running.
fn own_session(statuses: &[Status]) -> Origin {
    let candidates: Vec<Candidate> = statuses.iter().filter_map(Candidate::of).collect();
    origin::find(std::process::id(), &candidates)
}

/// Reads a release as the three numbers before any `-` or `+`.
pub fn release_numbers(version: &str) -> Option<[u64; 3]> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let mut numbers = [0u64; 3];
    for number in &mut numbers {
        *number = parts.next()?.parse().ok()?;
    }
    parts.next().is_none().then_some(numbers)
}

/// Whether the relay of a session with this `version` takes the commands in
/// here: `Err` is what to tell the person.
fn check_relay(status: &Status) -> Result<(), String> {
    if release_numbers(&status.version).is_some_and(|version| version >= DRIVES_SINCE) {
        return Ok(());
    }
    let which = if status.version.is_empty() {
        "a kablo that does not say which release it is".to_owned()
    } else {
        format!("kablo {}", printable(status.version.as_bytes()))
    };
    let [major, minor, patch] = DRIVES_SINCE;
    Err(format!(
        "session {id} is served by {which}, from before this command (kablo {major}.{minor}.{patch} \
         and later take it); replace its relay with `kablo upgrade {id} PATH`, PATH being a \
         newer kablo",
        id = status.id
    ))
}

/// The sessions in `dir` whose socket is there, as the files say: none when there
/// is no directory.
fn socket_ids(dir: &Path) -> io::Result<Vec<SessionId>> {
    match sockdir::check_dir(dir) {
        Ok(()) => sockdir::session_ids(dir),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The sessions whose id is `name` or whose label file says it, a label without
/// regard to case: the ones a command that names `name` may be about.
fn fitting(dir: &Path, name: &str) -> io::Result<Vec<SessionId>> {
    let lower = name.to_lowercase();
    Ok(socket_ids(dir)?
        .into_iter()
        .filter(|id| {
            id.as_str() == name
                || sockdir::read_label(dir, id).is_some_and(|label| label.to_lowercase() == lower)
        })
        .collect())
}

/// What to say when `label` is given to session `id` and another session is
/// called that already, by its id or its label: a command that names either
/// does nothing, because it cannot tell which is meant.
pub fn clash_note(dir: &Path, id: &SessionId, label: &str) -> Option<String> {
    let others: Vec<String> = fitting(dir, label)
        .ok()?
        .into_iter()
        .filter(|other| other != id)
        .map(|other| other.to_string())
        .collect();
    (!label.is_empty() && !others.is_empty()).then(|| {
        format!(
            "note: {} {} called {} too, so a command that names it will do nothing until \
             one of them is renamed",
            others.join(", "),
            if others.len() == 1 { "is" } else { "are" },
            printable(label.as_bytes())
        )
    })
}

/// What a command works on.
struct Aim {
    dir: PathBuf,
    status: Status,
}

/// Finds the session a command was pointed at (or the one it runs in) and
/// checks its relay. On failure the words are written and the status is the
/// command's to end with.
fn aim(name: Option<&str>, usage: &str, family: Family, err: &mut dyn Write) -> Result<Aim, u8> {
    let dir = match sockdir::socket_dir() {
        Ok(dir) => dir,
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            return Err(family.trouble());
        }
    };
    // A name is looked for in the files (the sockets and the labels), and only the
    // sessions it may be about are asked how they stand: a command does not wake
    // every relay on the machine, and a session that is broken does not hold up the
    // ones that are not named. With no name every running session is asked, since
    // which one this process runs in is told by their shells.
    let asked = match name {
        Some(name) => fitting(&dir, name).map(|ids| scan::Scan::of(&dir, ids, scan::WAIT)),
        None => scan::Scan::start(&dir, scan::WAIT),
    };
    let statuses = match asked {
        Ok(scan) => scan.run(),
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            return Err(family.trouble());
        }
    };
    let status = match find(&statuses, name, usage, own_session) {
        Ok(status) => status.clone(),
        Err(Miss::Usage(why)) => return Err(usage_error(err, &why)),
        Err(Miss::Trouble(why)) => {
            let _ = writeln!(err, "kablo: {why}");
            return Err(family.trouble());
        }
    };
    if let Err(why) = check_relay(&status) {
        let _ = writeln!(err, "kablo: {why}");
        return Err(family.trouble());
    }
    Ok(Aim { dir, status })
}

// --- the conversation ----------------------------------------------------------------

/// What a conversation hears next.
#[derive(Debug)]
enum Heard {
    Packet(Box<Packet>),
    /// The time given ran out.
    TimedOut,
    /// The link is over and nothing more will come.
    Over,
}

/// A blocking conversation with one session's relay.
struct Talk {
    link: Link,
    id: SessionId,
    heard: VecDeque<Packet>,
}

impl Talk {
    /// Connects to session `id`; `Err` is what to tell the person.
    fn open(dir: &Path, id: &SessionId) -> Result<Talk, String> {
        match Link::open(dir, id.clone(), Instant::now()) {
            Ok(link) => Ok(Talk {
                link,
                id: id.clone(),
                heard: VecDeque::new(),
            }),
            Err(packet) => Err(match packet.message {
                Message::Error { code, text } if code == ErrorCode::UNKNOWN_SESSION => {
                    format!("session {id} is gone ({text})")
                }
                Message::Error { text, .. } => format!("cannot reach session {id}: {text}"),
                _ => format!("cannot reach session {id}"),
            }),
        }
    }

    /// How many times the connection to the session was made again.
    fn reconnects(&self) -> u32 {
        self.link.reconnects()
    }

    /// Sends `message` to the session.
    fn say(&mut self, message: Message) {
        let mut out = Vec::new();
        self.link.send(
            Packet::about(self.id.clone(), message),
            Instant::now(),
            &mut out,
        );
        self.heard.extend(out);
    }

    /// What the session says next, waiting until `until` for it.
    fn hear(&mut self, until: Option<Instant>) -> Heard {
        loop {
            if let Some(packet) = self.heard.pop_front() {
                return Heard::Packet(Box::new(packet));
            }
            if self.link.is_gone() {
                return Heard::Over;
            }
            let now = Instant::now();
            let mut out = Vec::new();
            self.link.tick(now, &mut out);
            if !out.is_empty() {
                self.heard.extend(out);
                continue;
            }
            if self.link.is_gone() {
                return Heard::Over;
            }
            if until.is_some_and(|at| now >= at) {
                return Heard::TimedOut;
            }
            let mut set: Vec<libc::pollfd> = self.link.pollfd(true).into_iter().collect();
            let wake = [self.link.deadline(), until].into_iter().flatten().min();
            let wait = wake.map(|at| at.saturating_duration_since(now));
            // A link with no connection and no time to try again has nothing
            // left to wait for.
            if set.is_empty() && wait.is_none() {
                return Heard::Over;
            }
            if sys::poll(&mut set, wait).is_err() {
                return Heard::Over;
            }
            if let Some(fd) = set.first()
                && fd.revents != 0
            {
                self.link.ready(fd.revents, Instant::now(), &mut out);
                self.heard.extend(out);
            }
        }
    }
}

/// What to say of an error the session sent.
fn said(id: &SessionId, code: ErrorCode, text: &str) -> String {
    if code == ErrorCode::UNKNOWN_SESSION {
        format!("session {id} is gone")
    } else {
        format!("session {id}: {}", printable(text.as_bytes()))
    }
}

/// Opens a conversation or says why not.
fn open(aim: &Aim, family: Family, err: &mut dyn Write) -> Result<Talk, u8> {
    Talk::open(&aim.dir, &aim.status.id).map_err(|why| {
        let _ = writeln!(err, "kablo: {why}");
        family.trouble()
    })
}

// --- the commands ----------------------------------------------------------------------

/// `kablo send [ID] [TEXT]`.
pub fn send(args: &[OsString], err: &mut dyn Write) -> u8 {
    let parsed = match parse_send(args) {
        Ok(parsed) => parsed,
        Err(why) => return usage_error(err, &why),
    };
    // The session first: a command that is run in no session, or names none, ends
    // before it waits for any input.
    let aim = match aim(
        parsed.target.as_deref(),
        "kablo send ID TEXT",
        Family::Quick,
        err,
    ) {
        Ok(aim) => aim,
        Err(status) => return status,
    };
    let text = match parsed.text {
        Some(text) => text,
        None => {
            let mut text = Vec::new();
            let limit = MAX_TEXT as u64 + 1;
            if let Err(e) = io::stdin().lock().take(limit).read_to_end(&mut text) {
                let _ = writeln!(err, "kablo: cannot read the input: {e}");
                return EXIT_TROUBLE;
            }
            text
        }
    };
    if text.len() > MAX_TEXT {
        let _ = writeln!(
            err,
            "kablo: that is more than the {MAX_TEXT} bytes one send types"
        );
        return EXIT_TROUBLE;
    }
    if text.is_empty() {
        return 0;
    }
    type_in(&aim, vec![Stroke::text(text)], err)
}

/// `kablo send-key [ID] KEY...`.
pub fn send_key(args: &[OsString], err: &mut dyn Write) -> u8 {
    let parsed = match parse_send_key(args) {
        Ok(parsed) => parsed,
        Err(why) => return usage_error(err, &why),
    };
    let aim = match aim(
        parsed.target.as_deref(),
        "kablo send-key ID KEY...",
        Family::Quick,
        err,
    ) {
        Ok(aim) => aim,
        Err(status) => return status,
    };
    let strokes = parsed
        .keys
        .iter()
        .map(|pressed| Stroke::key(pressed.code, pressed.mods))
        .collect();
    type_in(&aim, strokes, err)
}

/// Types `strokes` and asks the session whether it did, with a `ping` after
/// them: an error comes before its `pong`. Once.
fn type_in(aim: &Aim, strokes: Vec<Stroke>, err: &mut dyn Write) -> u8 {
    let mut talk = match open(aim, Family::Quick, err) {
        Ok(talk) => talk,
        Err(status) => return status,
    };
    talk.say(Message::Write(strokes));
    talk.say(Message::Ping(b"kablo".to_vec()));
    loop {
        match talk.hear(None) {
            Heard::Packet(packet) => match packet.message {
                Message::Pong(_) => return 0,
                Message::Error { code, text } => {
                    let _ = writeln!(err, "kablo: {}", said(&aim.status.id, code, &text));
                    return EXIT_TROUBLE;
                }
                _ => {}
            },
            Heard::TimedOut | Heard::Over => {
                let _ = writeln!(
                    err,
                    "kablo: the connection to session {} was lost before it said whether \
                     anything was typed",
                    aim.status.id
                );
                return EXIT_TROUBLE;
            }
        }
    }
}

/// How many times the screen is asked for when the relay is replaced while it
/// answers.
const READ_TRIES: u32 = 3;

/// `kablo capture [ID] [--history ROWS] [--unwrap] [--trim] [--vt]`.
pub fn capture(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let parsed = match parse_capture(args) {
        Ok(parsed) => parsed,
        Err(why) => return usage_error(err, &why),
    };
    let aim = match aim(
        parsed.target.as_deref(),
        "kablo capture ID",
        Family::Quick,
        err,
    ) {
        Ok(aim) => aim,
        Err(status) => return status,
    };
    let bytes = match read_screen(&aim, parsed.capture, err) {
        Ok(bytes) => bytes,
        Err(status) => return status,
    };
    let shown = if parsed.trim { trimmed(&bytes) } else { &bytes };
    match out.write_all(shown).and_then(|()| out.flush()) {
        Ok(()) => 0,
        // Whatever reads the output went away (`kablo capture | head`).
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => EXIT_PIPE,
        Err(e) => {
            let _ = writeln!(err, "kablo: cannot write the output: {e}");
            EXIT_TROUBLE
        }
    }
}

/// Asks for the screen and puts the pieces of the answer together. A relay that
/// is replaced while it answers in pieces begins the answer again with the new
/// one, and a piece cannot say which answer it is of, so what came over a
/// connection that has been made again is thrown away and the screen asked for
/// again.
fn read_screen(aim: &Aim, capture: Capture, err: &mut dyn Write) -> Result<Vec<u8>, u8> {
    for _ in 0..READ_TRIES {
        let mut talk = open(aim, Family::Quick, err)?;
        talk.say(Message::Read(capture));
        let mut bytes = Vec::new();
        loop {
            match talk.hear(None) {
                Heard::Packet(packet) => match packet.message {
                    Message::ScreenText { more, data, .. } if !capture.picture => {
                        bytes.extend_from_slice(&data);
                        if !more {
                            break;
                        }
                    }
                    Message::Picture { more, data, .. } if capture.picture => {
                        bytes.extend_from_slice(&data);
                        if !more {
                            break;
                        }
                    }
                    Message::Error { code, text } => {
                        let _ = writeln!(err, "kablo: {}", said(&aim.status.id, code, &text));
                        return Err(EXIT_TROUBLE);
                    }
                    _ => {}
                },
                Heard::TimedOut | Heard::Over => {
                    let _ = writeln!(
                        err,
                        "kablo: the connection to session {} was lost before the screen came",
                        aim.status.id
                    );
                    return Err(EXIT_TROUBLE);
                }
            }
        }
        if talk.reconnects() == 0 {
            return Ok(bytes);
        }
    }
    let _ = writeln!(
        err,
        "kablo: the relay of session {} was replaced again and again while its screen was read",
        aim.status.id
    );
    Err(EXIT_TROUBLE)
}

/// The text without the lines at its end that show nothing.
fn trimmed(text: &[u8]) -> &[u8] {
    let Some(last) = text.iter().rposition(|b| !b.is_ascii_whitespace()) else {
        return &[];
    };
    let end = text
        .get(last..)
        .and_then(|tail| tail.iter().position(|&b| b == b'\n'))
        .map_or(text.len(), |at| last + at + 1);
    text.get(..end).unwrap_or(text)
}

/// `kablo wait ID [--for TIME]`.
pub fn wait(args: &[OsString], err: &mut dyn Write) -> u8 {
    let parsed = match parse_wait(args) {
        Ok(parsed) => parsed,
        Err(why) => return usage_error(err, &why),
    };
    let aim = match aim(Some(&parsed.target), "kablo wait ID", Family::Waiting, err) {
        Ok(aim) => aim,
        Err(status) => return status,
    };
    let mut talk = match open(&aim, Family::Waiting, err) {
        Ok(talk) => talk,
        Err(status) => return status,
    };
    // The one timer here is the person's: a program that never ends is waited
    // for as long as they said, or for ever.
    let until = parsed
        .within
        .as_ref()
        .and_then(|(_, time)| Instant::now().checked_add(*time));
    match end_of(&mut talk, until) {
        Ended::Program(exit) => exec::status_of(exit),
        Ended::Timeout => {
            let within = parsed.within.map(|(text, _)| text).unwrap_or_default();
            let _ = writeln!(
                err,
                "kablo: the program in session {} had not ended after {}",
                aim.status.id,
                printable(within.as_bytes())
            );
            EXIT_TIMEOUT
        }
        Ended::Trouble(why) => {
            let _ = writeln!(err, "kablo: {why}");
            EXIT_WAIT_TROUBLE
        }
    }
}

/// How a wait for a program's end came out.
enum Ended {
    Program(kablo_proto::v1::Exit),
    Timeout,
    Trouble(String),
}

/// Asks the session to say when its program ends and waits for it, until
/// `until` if there is one. The link asks again of a relay that replaces the
/// one asked, so `kablo upgrade` does not end the wait.
fn end_of(talk: &mut Talk, until: Option<Instant>) -> Ended {
    talk.say(Message::Wait);
    loop {
        match talk.hear(until) {
            Heard::Packet(packet) => match packet.message {
                Message::Ended(exit) => return Ended::Program(exit),
                Message::Error { code, text } => {
                    return Ended::Trouble(said(&talk.id, code, &text));
                }
                _ => {}
            },
            Heard::TimedOut => return Ended::Timeout,
            Heard::Over => {
                return Ended::Trouble(format!(
                    "the connection to session {} was lost before its program ended",
                    talk.id
                ));
            }
        }
    }
}

/// The waiting half of `kablo new --wait`: waits for the program in the session
/// just made to end, then takes the session over (the end is told to an
/// ordinary client that attaches at the end of the journal, without a picture,
/// so nothing is played again; when its queue is empty the relay tells the
/// warden it was heard and the session goes), and returns the program's status.
/// Seeing the end through a `wait` does not close the session: this is.
pub fn await_end(dir: &Path, id: &SessionId, err: &mut dyn Write) -> u8 {
    let mut talk = match Talk::open(dir, id) {
        Ok(talk) => talk,
        Err(why) => {
            let _ = writeln!(err, "kablo: {why}");
            return EXIT_WAIT_TROUBLE;
        }
    };
    match end_of(&mut talk, None) {
        Ended::Program(exit) => {
            take_over(dir, id);
            exec::status_of(exit)
        }
        Ended::Timeout => EXIT_WAIT_TROUBLE,
        Ended::Trouble(why) => {
            let _ = writeln!(err, "kablo: {why}");
            EXIT_WAIT_TROUBLE
        }
    }
}

/// Attaches to a session whose program has ended, at the end of its journal and
/// with no picture, hears the end, and stays until the session has gone (or
/// [`DELIVERY_WAIT`] has passed): the session is not kept for a client that has
/// not heard, and this one has. A session that is already gone has been taken
/// by somebody else.
fn take_over(dir: &Path, id: &SessionId) {
    let Ok(mut talk) = Talk::open(dir, id) else {
        return;
    };
    talk.say(Message::attach(OFFSET_END));
    let until = Instant::now().checked_add(DELIVERY_WAIT);
    loop {
        match talk.hear(until) {
            Heard::Packet(_) => {}
            Heard::TimedOut | Heard::Over => return,
        }
    }
}

/// `kablo label [ID] [TEXT]`.
pub fn label(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let parsed = match parse_label(args) {
        Ok(parsed) => parsed,
        Err(why) => return usage_error(err, &why),
    };
    let aim = match aim(
        parsed.target.as_deref(),
        "kablo label ID TEXT",
        Family::Quick,
        err,
    ) {
        Ok(aim) => aim,
        Err(status) => return status,
    };
    let Some(text) = parsed.text else {
        if let Some(label) = &aim.status.label {
            let _ = writeln!(out, "{}", printable(label.as_bytes()));
        }
        return 0;
    };
    // The file is the label: the relay reads it for `status` (`kablo list`).
    match sockdir::write_label(&aim.dir, &aim.status.id, &text) {
        Ok(()) => {
            if let Some(note) = clash_note(&aim.dir, &aim.status.id, &text) {
                let _ = writeln!(err, "kablo: {note}");
            }
            0
        }
        Err(e) => {
            let _ = writeln!(err, "kablo: cannot write the label: {e}");
            EXIT_TROUBLE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    fn status(id: &str, label: Option<&str>) -> Status {
        let mut status = Status::new(SessionId::new(id).unwrap());
        status.label = label.map(str::to_owned);
        status.version = "0.2.0".to_owned();
        status
    }

    fn pressed(code: u32, mods: u8) -> Option<Key> {
        Some(Key::new(code, mods))
    }

    #[test]
    fn keys_are_named_or_a_character_with_a_modifier() {
        assert_eq!(parse_key("enter"), pressed(key::ENTER, 0));
        assert_eq!(parse_key("Escape"), pressed(key::ESCAPE, 0));
        assert_eq!(parse_key("space"), pressed(0x20, 0));
        assert_eq!(parse_key("pageup"), pressed(key::PAGE_UP, 0));
        assert_eq!(parse_key("f1"), pressed(key::function(1).unwrap(), 0));
        assert_eq!(parse_key("F12"), pressed(key::function(12).unwrap(), 0));
        assert_eq!(
            parse_key("ctrl+c"),
            pressed(u32::from('c'), key::mods::CTRL)
        );
        assert_eq!(
            parse_key("Ctrl+C"),
            pressed(u32::from('c'), key::mods::CTRL)
        );
        assert_eq!(parse_key("alt+x"), pressed(u32::from('x'), key::mods::ALT));
        assert_eq!(parse_key("shift+tab"), pressed(key::TAB, key::mods::SHIFT));
        assert_eq!(
            parse_key("ctrl+alt+up"),
            pressed(key::UP, key::mods::CTRL | key::mods::ALT)
        );
        assert_eq!(
            parse_key("ctrl++"),
            pressed(u32::from('+'), key::mods::CTRL)
        );
        // A character alone is text, which `send` types; and what is no key is none.
        for name in [
            "a",
            "t",
            "f0",
            "f13",
            "f01",
            "ctrl+",
            "+",
            "ctrl+nope",
            "hyper+a",
            "",
            "ctrl+ab",
        ] {
            assert_eq!(parse_key(name), None, "{name}");
        }
    }

    #[test]
    fn send_takes_a_session_and_a_text_or_just_the_text() {
        let parsed = parse_send(&words(&["echo hi"])).unwrap();
        assert_eq!(
            (parsed.target, parsed.text),
            (None, Some(b"echo hi".to_vec()))
        );
        let parsed = parse_send(&words(&["t", "echo hi"])).unwrap();
        assert_eq!(
            (parsed.target, parsed.text),
            (Some("t".to_owned()), Some(b"echo hi".to_vec()))
        );
        // No text, or a lone `-`, is the input; `--` lets a text start with `-`.
        assert_eq!(parse_send(&[]).unwrap().text, None);
        assert_eq!(parse_send(&words(&["t", "-"])).unwrap().text, None);
        assert_eq!(
            parse_send(&words(&["--", "-n"])).unwrap().text,
            Some(b"-n".to_vec())
        );
        assert!(parse_send(&words(&["a", "b", "c"])).is_err());
        assert!(parse_send(&words(&["-n"])).is_err());
    }

    #[test]
    fn send_key_takes_the_session_unless_the_first_word_is_a_key() {
        let own = parse_send_key(&words(&["enter"])).unwrap();
        assert_eq!(own.target, None);
        assert_eq!(own.keys, [Key::new(key::ENTER, 0)]);
        let aimed = parse_send_key(&words(&["t", "ctrl+c", "enter"])).unwrap();
        assert_eq!(aimed.target.as_deref(), Some("t"));
        assert_eq!(aimed.keys.len(), 2);
        // Every word that is not the session must be a key.
        assert!(parse_send_key(&words(&["t", "enter", "nope"])).is_err());
        assert!(parse_send_key(&words(&["enter", "nope"])).is_err());
        // A session and no key, and no word at all.
        assert!(parse_send_key(&words(&["t"])).is_err());
        assert!(parse_send_key(&[]).is_err());
    }

    #[test]
    fn capture_takes_its_options_before_or_after_the_session() {
        let parsed =
            parse_capture(&words(&["t", "--history", "50", "--unwrap", "--trim"])).unwrap();
        assert_eq!(parsed.target.as_deref(), Some("t"));
        assert_eq!(parsed.capture.history, 50);
        assert!(parsed.capture.unwrap && parsed.trim && !parsed.capture.picture);
        let parsed = parse_capture(&words(&["--vt"])).unwrap();
        assert_eq!(parsed.target, None);
        assert!(parsed.capture.picture);
        // The picture is the screen's bytes and the others are about text.
        assert!(parse_capture(&words(&["--vt", "--trim"])).is_err());
        assert!(parse_capture(&words(&["--history", "x"])).is_err());
        assert!(parse_capture(&words(&["--history", "100001"])).is_err());
        assert!(parse_capture(&words(&["--history"])).is_err());
        assert!(parse_capture(&words(&["a", "b"])).is_err());
        assert!(parse_capture(&words(&["--nope"])).is_err());
    }

    #[test]
    fn wait_needs_a_session_and_takes_a_time() {
        let parsed = parse_wait(&words(&["x", "--for", "1s"])).unwrap();
        assert_eq!(parsed.target, "x");
        assert_eq!(parsed.within.unwrap().1, Duration::from_secs(1));
        assert_eq!(parse_wait(&words(&["x"])).unwrap().within, None);
        assert!(parse_wait(&[]).is_err());
        assert!(parse_wait(&words(&["x", "y"])).is_err());
        assert!(parse_wait(&words(&["x", "--for"])).is_err());
        assert!(parse_wait(&words(&["--for", "1s"])).is_err());
    }

    #[test]
    fn a_time_is_a_number_and_a_unit() {
        let ok = |text: &str| duration(text).unwrap();
        assert_eq!(ok("500ms"), Duration::from_millis(500));
        assert_eq!(ok("30"), Duration::from_secs(30));
        assert_eq!(ok("30s"), Duration::from_secs(30));
        assert_eq!(ok("1.5s"), Duration::from_millis(1500));
        assert_eq!(ok("5m"), Duration::from_secs(300));
        assert_eq!(ok("2h"), Duration::from_secs(7200));
        for text in [
            "", "s", "0", "0s", "-1s", "1d", "1..2s", "1 s", "nan", "1e3",
        ] {
            assert!(duration(text).is_err(), "{text}");
        }
    }

    #[test]
    fn a_label_is_one_text_of_at_most_64_bytes() {
        let own = parse_label(&words(&["build"])).unwrap();
        assert_eq!((own.target, own.text.as_deref()), (None, Some("build")));
        let aimed = parse_label(&words(&["t", ""])).unwrap();
        assert_eq!(
            (aimed.target.as_deref(), aimed.text.as_deref()),
            (Some("t"), Some(""))
        );
        let shown = parse_label(&[]).unwrap();
        assert_eq!((shown.target, shown.text), (None, None));
        assert!(parse_label(&words(&[&"x".repeat(65)])).is_err());
        assert!(parse_label(&words(&[&"x".repeat(64)])).is_ok());
        assert!(parse_label(&words(&["a", "b", "c"])).is_err());
    }

    #[test]
    fn a_name_is_an_id_or_a_label_and_one_that_is_both_is_refused() {
        let all = [
            status("one", Some("Build")),
            status("two", None),
            status("three", Some("one")),
        ];
        let nobody = |_: &[Status]| Origin::None;
        let found = |name: &str| find(&all, Some(name), "usage", nobody).map(|s| s.id.to_string());
        assert_eq!(found("two"), Ok("two".to_owned()));
        // A label, without regard to case; an id exactly.
        assert_eq!(found("build"), Ok("one".to_owned()));
        assert_eq!(found("BUILD"), Ok("one".to_owned()));
        assert!(matches!(found("TWO"), Err(Miss::Trouble(_))));
        // `one` is a session's id and another's label: neither is picked.
        let Err(Miss::Trouble(text)) = found("one") else {
            panic!("an ambiguous name names no session");
        };
        assert!(text.contains("one") && text.contains("three"), "{text}");
        assert!(matches!(found("nothing"), Err(Miss::Trouble(_))));
    }

    #[test]
    fn a_session_with_no_name_is_the_one_this_runs_in() {
        let all = [status("one", None), status("two", None)];
        let inside = |_: &[Status]| Origin::Terminal(SessionId::new("two").unwrap());
        assert_eq!(
            find(&all, None, "usage", inside).map(|s| s.id.to_string()),
            Ok("two".to_owned())
        );
        let outside = |_: &[Status]| Origin::None;
        assert!(matches!(
            find(&all, None, "usage", outside),
            Err(Miss::Usage(_))
        ));
        // A name overrides where this runs.
        assert_eq!(
            find(&all, Some("one"), "usage", inside).map(|s| s.id.to_string()),
            Ok("one".to_owned())
        );
    }

    #[test]
    fn a_broken_session_cannot_be_worked_on() {
        let mut broken = status("one", None);
        broken.state = State::BROKEN;
        let all = [broken];
        let nobody = |_: &[Status]| Origin::None;
        assert!(matches!(
            find(&all, Some("one"), "usage", nobody),
            Err(Miss::Trouble(text)) if text.contains("broken")
        ));
    }

    #[test]
    fn a_relay_from_before_these_commands_is_told_to_be_upgraded() {
        let with = |version: &str| {
            let mut status = status("s", None);
            status.version = version.to_owned();
            status
        };
        assert!(check_relay(&with("0.2.0")).is_ok());
        assert!(check_relay(&with("0.2.1")).is_ok());
        assert!(check_relay(&with("1.0.0-rc1")).is_ok());
        for old in ["0.1.0", "0.1.9", "0.0.1", "", "garbage", "0.2"] {
            let Err(text) = check_relay(&with(old)) else {
                panic!("{old} is too old");
            };
            assert!(text.contains("kablo upgrade s PATH"), "{text}");
        }
        // The release this was built as must be one that takes them: a release
        // number below the threshold would have every new session refused.
        let own = release_numbers(env!("CARGO_PKG_VERSION")).unwrap();
        assert!(own >= DRIVES_SINCE, "{own:?} < {DRIVES_SINCE:?}");
    }

    #[test]
    fn the_end_of_a_text_that_shows_nothing_is_cut() {
        assert_eq!(trimmed(b"a\nb\n\n  \n\n"), b"a\nb\n");
        assert_eq!(trimmed(b"a\nb"), b"a\nb");
        assert_eq!(trimmed(b"a  \n\n"), b"a  \n");
        assert_eq!(trimmed(b"\n \n"), b"");
        assert_eq!(trimmed(b""), b"");
        assert_eq!(trimmed("é\n\n".as_bytes()), "é\n".as_bytes());
    }
}
