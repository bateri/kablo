//! `kablo bateri`: what an agent (Claude Code, Codex) asks of the bateri that shows this machine's
//! panes, run as a part of kablo and not as a program of its own.
//!
//! **Who does what.** The terminal is the machine's: `kablo new` starts it here, it gets a number
//! (`p9`), and `send`, `send-key`, `capture` and `wait` are kablo's own, with no bateri app anywhere. The
//! bateri app only decides where a pane that shows the terminal goes, and every verb that needs its
//! windows (`focus`, `zoom`, `swap`, `move`, `resize`, `rename tab`, `close tab`) is asked of it as
//! one `bateri 2` message on kablo's app channel ([`crate::app::converse`], the text is
//! `kablo-bateri`'s). The words of every verb, and the exit codes, are the ones the command has
//! always had: 0, 1, 64 (a mistake in the command), 69 (what needs the bateri app, with none connected),
//! 77 (refused), 128+n, and for `wait` the program's own, 124 when the time ran out, 255 for
//! kablo's trouble.

mod machine;
mod open;
mod state;
mod verbs;
mod words;

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use kablo_bateri::message::{Failure, Kind, Reply, Request};

use crate::{app, drive};

/// How a command ends when it does not end well, or ends with a status of its own.
#[derive(Debug)]
pub struct Exit {
    /// The status.
    pub code: u8,
    /// What is said on standard error, if anything.
    pub text: Option<String>,
}

impl Exit {
    /// Ends with `code` after saying `text`.
    pub fn new(code: u8, text: impl Into<String>) -> Exit {
        Exit {
            code,
            text: Some(text.into()),
        }
    }

    /// Ends with `code` and nothing to say (what a command run for it has said already).
    pub fn status(code: u8) -> Exit {
        Exit { code, text: None }
    }

    /// A mistake in the command (64).
    pub fn mistake(text: impl Into<String>) -> Exit {
        Exit::new(64, text)
    }

    /// Anything else that went wrong (1).
    pub fn failed(text: impl Into<String>) -> Exit {
        Exit::new(1, text)
    }
}

/// What a verb ends with: `Ok` is 0.
pub type Done = Result<(), Exit>;

/// The two ends a command talks to.
pub struct Io<'a> {
    /// Standard output.
    pub out: &'a mut dyn Write,
    /// Standard error.
    pub err: &'a mut dyn Write,
}

impl Io<'_> {
    /// `bateri: TEXT` on standard error.
    pub fn say(&mut self, text: &str) {
        let _ = writeln!(self.err, "bateri: {text}");
    }
}

/// What a command knows of where it runs.
pub struct Ctx {
    /// `$HOME`.
    home: PathBuf,
    /// The state this keeps: `~/.local/share/bateri` ([`kablo_bateri::paths`]).
    pub state: PathBuf,
    /// The command's words came from the app of another machine (`--hex`): what it asks, that
    /// app is asked.
    afar: bool,
}

/// What the bateri app made of a request.
#[derive(Debug)]
pub enum Asked {
    /// It answered, and the reply is one this build reads.
    Reply(Reply),
    /// It answered with something that is no reply.
    Unreadable,
    /// The bateri app is not connected (69).
    NoApp,
    /// The conversation did not come to an answer.
    Silent,
}

/// What a conversation's status and reply come to: the bateri app's answer, no bateri app (69), a conversation
/// that did not come to an answer, or the signal that ended it (128 and its number: the command
/// ends with it).
fn asked_of(status: u8, reply: &[u8]) -> Result<Asked, Exit> {
    match status {
        0 => Ok(match Reply::parse(&String::from_utf8_lossy(reply)) {
            Ok(reply) => Asked::Reply(reply),
            Err(_) => Asked::Unreadable,
        }),
        app::EXIT_NO_APP => Ok(Asked::NoApp),
        signal if signal > 128 => Err(Exit::status(signal)),
        _ => Ok(Asked::Silent),
    }
}

impl Ctx {
    fn new() -> Result<Ctx, Exit> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| Exit::failed("HOME is not set"))?;
        Ok(Ctx {
            state: home.join(kablo_bateri::paths::DIR),
            home,
            afar: false,
        })
    }

    /// `$HOME`.
    pub fn home(&self) -> PathBuf {
        self.home.clone()
    }

    /// What is kept under `~/.local/share/bateri`.
    pub fn state(&self) -> state::State {
        state::State::new(self.state.clone())
    }

    /// One conversation with the bateri app: `text` is a message, as it is written. A signal that ends
    /// the wait ends the command with the signal's status (128 and its number), and nothing else
    /// is done after it: a `split` that is interrupted does not go on to start a terminal.
    pub fn ask_text(&self, text: &str) -> Result<Asked, Exit> {
        let (status, reply) = app::converse(text.as_bytes());
        asked_of(status, &reply)
    }

    /// One conversation with the bateri app, for a request.
    pub fn ask(&self, request: &Request) -> Result<Asked, Exit> {
        let text = encode(request)?;
        self.ask_text(&text)
    }

    /// The terminal this runs in, under whatever name it has (none outside every terminal): the
    /// ones before the numbers (`b-` and 32 digits) and the ones a person made with `kablo new` are
    /// as much a terminal as `p4`.
    pub fn here(&self) -> Option<String> {
        let id = drive::here_session(&mut std::io::sink()).ok()?.to_string();
        words::is_name(&id).then_some(id)
    }

    /// The caller's own terminal, or the mistake that there is none: `what` says what to write.
    pub fn need_here(&self, what: &str) -> Result<String, Exit> {
        self.here()
            .ok_or_else(|| Exit::mistake(format!("this is run in no terminal; {what}")))
    }
}

/// A request as the text it is written as; one that cannot be is a mistake in the command.
pub fn encode(request: &Request) -> Result<String, Exit> {
    request
        .encode()
        .map_err(|failure| Exit::mistake(words::clean(&failure.text)))
}

/// The end of a command the bateri app answered with an error: the kind says the status.
pub fn failure(failure: &Failure) -> Exit {
    let code = match failure.kind {
        Kind::Usage => 64,
        Kind::Refused => 77,
        _ => 1,
    };
    Exit::new(code, words::clean(&failure.text))
}

/// What a reply that is no answer to the request says.
pub fn unread() -> Exit {
    Exit::failed("the bateri app answered with something this does not read")
}

const USAGE: &str = "usage: bateri split [PANE | --tab TAB] [--left|--right|--above|--below] [--ratio PERCENT]
                    [--label TEXT] [--cwd DIR] [--keep-open] [--wait] [--focus] [--json]
                    [-S SERVER] [-- PROGRAM [ARG]...]
       bateri tab [PANE] [--group GROUP | --window WINDOW] [--new-group NAME] [--label TEXT]
                  [--cwd DIR] [--keep-open] [--wait] [--focus] [--json] [-S SERVER]
                  [-- PROGRAM [ARG]...]
       bateri window [--cwd DIR] [--keep-open] [--wait] [--focus] [--json] [-S SERVER]
                     [-- PROGRAM [ARG]...]
       bateri close pane [PANE] [--force]
       bateri close tab|group|window [ID]
       bateri ls [--json]
       bateri notify TEXT...
       bateri send [PANE] [TEXT | -]
       bateri send-key [PANE] KEY...
       bateri capture [PANE] [--history ROWS] [--unwrap] [--trim] [--vt]
       bateri wait PANE [--for TIME]
       bateri focus pane|tab|group|window [ID]
       bateri zoom [PANE]
       bateri swap PANE PANE
       bateri rename pane|tab|group ID LABEL
       bateri move pane [PANE] --beside PANE --left|--right|--above|--below [--json]
       bateri move pane [PANE] --tab TAB [--left|--right|--above|--below] [--json]
       bateri move pane [PANE] --new-tab [NAME] [--group GROUP | --window WINDOW] [--new-group NAME]
                               [--json]
       bateri move tab [TAB] --group GROUP | --window WINDOW | --new-group NAME [--window WINDOW] [--json]
       bateri resize [PANE] --left|--right|--above|--below [--by N]
An id is a letter and a number: p9 a pane, t3 a tab, g1 a group, w1 a window; bateri ls shows them.
A pane is p and its number (p7); with none named it is the one this runs in. One of another
machine is written with the machine after an @ (p9@prod), as bateri ls writes it; -S starts a
terminal on a server, or on this computer from a server (-S @local) (full level only).
Without a program, split, tab and window open a shell. A shell named alone (-- zsh, -- fish) is
the same, with its dock where bateri has an integration for it. bateri ls lists what is open.
";

/// `kablo bateri VERB ARGS...`: returns the status.
pub fn run(args: &[OsString], out: &mut dyn Write, err: &mut dyn Write) -> u8 {
    let mut io = Io { out, err };
    match dispatch(args, &mut io) {
        Ok(()) => 0,
        Err(exit) => {
            if let Some(text) = &exit.text {
                io.say(text);
            }
            exit.code
        }
    }
}

fn dispatch(args: &[OsString], io: &mut Io<'_>) -> Done {
    let ctx = Ctx::new()?;
    // The counter: the number of the next terminal, for whoever else numbers terminals on this
    // machine (the bateri app does, for the ones it opens itself), so that no number is given twice.
    if args.first().is_some_and(|arg| arg == "--next-number") {
        if args.len() != 1 {
            return Err(Exit::mistake("--next-number takes nothing"));
        }
        return match ctx.state().next_number() {
            Some(number) => {
                let _ = writeln!(io.out, "{number}");
                Ok(())
            }
            None => Err(Exit::status(1)),
        };
    }
    // The words as another machine's app wrote them: `x` and the hex of their bytes, so that no
    // shell on the way reads what the caller wrote. What such a command asks of an app, the app of
    // that machine is asked ([`open::Asker::RemoteApp`]).
    if args.first().is_some_and(|arg| arg == "--hex") {
        let ctx = Ctx { afar: true, ..ctx };
        let hexed: Vec<String> = args
            .iter()
            .skip(1)
            .map(|word| word.to_string_lossy().into_owned())
            .collect();
        let words = machine::unhex_words(&hexed)?;
        return run_words(&ctx, io, &words);
    }
    dispatch_with(&ctx, args, io)
}

/// The command of `words`.
pub fn run_words(ctx: &Ctx, io: &mut Io<'_>, words: &[String]) -> Done {
    let args: Vec<OsString> = words.iter().map(OsString::from).collect();
    dispatch_with(ctx, &args, io)
}

fn dispatch_with(ctx: &Ctx, args: &[OsString], io: &mut Io<'_>) -> Done {
    let Some((verb, rest)) = args.split_first() else {
        let _ = write!(io.err, "{USAGE}");
        return Err(Exit::status(64));
    };
    let verb = verb.to_string_lossy().into_owned();
    // `--help` anywhere a command's own words are options; what comes after `--`, and the text of
    // the verbs that take any text, is not ours to read.
    if !matches!(verb.as_str(), "send" | "send-key" | "notify" | "rename") {
        for word in rest {
            match word.to_str() {
                Some("--") => break,
                Some("-h" | "--help") => {
                    let _ = write!(io.out, "{USAGE}");
                    return Ok(());
                }
                _ => {}
            }
        }
    }
    let words: Vec<String> = rest
        .iter()
        .map(|word| word.to_string_lossy().into_owned())
        .collect();
    match verb.as_str() {
        "split" => open::open(ctx, io, open::Mode::Split, &words),
        "tab" => open::open(ctx, io, open::Mode::Tab, &words),
        "window" => open::open(ctx, io, open::Mode::Window, &words),
        "close" => verbs::close(ctx, io, &words),
        "focus" => verbs::focus(ctx, io, &words),
        "zoom" => verbs::zoom(ctx, io, &words),
        "swap" => verbs::swap(ctx, io, &words),
        "rename" => verbs::rename(ctx, io, &words),
        "move" => verbs::moving(ctx, io, &words),
        "resize" => verbs::resize(ctx, io, &words),
        "ls" => verbs::ls(ctx, io, &words),
        "notify" => verbs::notify(ctx, &words),
        "send" => verbs::send(ctx, io, rest, &words),
        "send-key" => verbs::send_key(ctx, io, rest, &words),
        "capture" => verbs::capture(ctx, io, rest, &words),
        "wait" => verbs::wait(ctx, io, rest, &words),
        "help" | "-h" | "--help" => {
            let _ = write!(io.out, "{USAGE}");
            Ok(())
        }
        other => {
            io.say(&format!("no command '{}'", words::clean(other)));
            let _ = write!(io.err, "{USAGE}");
            Err(Exit::status(64))
        }
    }
}

/// A name as a JSON value: quoted, or `null` where there is none (the bateri app has not said).
pub fn json_name(name: Option<impl std::fmt::Display>) -> String {
    name.map_or_else(|| "null".to_owned(), |name| format!("\"{name}\""))
}

/// A command of kablo's own, run in this process, said by its status: `0` is done.
pub fn finish(code: u8) -> Done {
    if code == 0 {
        Ok(())
    } else {
        Err(Exit::status(code))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a conversation's status is: an answer, no bateri app, silence, or a signal that ends the
    /// command with its own status (a Ctrl-C during the wait is 130, not "the bateri app did not answer").
    #[test]
    fn a_signal_that_ends_the_wait_ends_the_command_with_its_status() {
        assert!(matches!(asked_of(69, b""), Ok(Asked::NoApp)));
        assert!(matches!(asked_of(1, b""), Ok(Asked::Silent)));
        assert!(matches!(asked_of(0, b"not a reply"), Ok(Asked::Unreadable)));
        for signal in [129u8, 130, 143] {
            let exit = asked_of(signal, b"").expect_err("a signal ends it");
            assert_eq!((exit.code, exit.text), (signal, None));
        }
    }
}
