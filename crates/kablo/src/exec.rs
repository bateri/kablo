//! `kablo exec`: runs a program on the server over the one connection of a
//! bridge, with no terminal, for people and scripts that have no terminal app
//! to do it.
//!
//! It is `ssh host program` for a connection that is already there. It runs a
//! command that ends in a bridge (`ssh host kablo attach`, the carrier; see
//! [`crate::carrier`]), asks the bridge to start the program, and joins the
//! program to this process's own standard input, output and error: what is
//! typed or piped in goes to the program, what the program writes comes out
//! where it would have if it ran here, and this process ends with the
//! program's exit status. The program is an argument vector, never a shell
//! string, so nothing in it is read as a shell would read it.
//!
//! The windows, queues and descriptors are [`crate::streams`] and
//! [`crate::outbound`], the same code the bridge runs; this module is the loop
//! around them and the command line. A program lasts as long as the connection
//! and no longer: when the carrier ends the program is ended with it, and so
//! does this, with a status of 255 (as `ssh` ends when it is its own trouble
//! and not the program's). Work that has to outlive the connection is a
//! session.

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;

use kablo_proto::v1::stream::{self as wire, Command, Incoming, IncomingDecoder};
use kablo_proto::v1::{Exit, Features, Message};

use crate::carrier::{self, End, READ_CHUNK, Wire, clean, describe, signal_in};
use crate::outbound::Outbound;
use crate::streams::{self, Cause, Event, Local, Streams};
use crate::sys;

/// What the `ping` that follows the `features` carries.
const PING: &[u8] = b"kablo exec";

/// The status of `kablo exec` when it could not get the program run or lost the
/// connection to it: it is not the program's, though a program may end with it
/// too.
pub const EXIT_TROUBLE: u8 = 255;

// --- the command line ----------------------------------------------------------

/// What `kablo exec` was asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The command that reaches the bridge, program first.
    pub carrier: Vec<OsString>,
    /// The program to run on the server, with where and in what.
    pub command: Command,
}

/// Reads the arguments after `kablo exec`:
/// `[--cwd DIR] [--env NAME=VALUE]... -- CARRIER... -- PROGRAM [ARGUMENT]...`.
///
/// The first `--` ends the options and starts the carrier, the second ends the
/// carrier and starts the program, whose arguments may hold `--` themselves.
pub fn parse(args: &[OsString]) -> Result<Options, String> {
    let mut command = Command::default();
    let mut words = args.iter();
    loop {
        let Some(word) = words.next() else {
            return Err("the command that reaches the server goes after `--`".to_owned());
        };
        match word.to_str() {
            Some("--") => break,
            Some("--cwd") => {
                let dir = words
                    .next()
                    .filter(|dir| *dir != "--" && !dir.is_empty())
                    .ok_or("--cwd takes a directory on the server")?;
                command.cwd = Some(dir.as_bytes().to_vec());
            }
            Some("--env") => {
                let var = words.next().ok_or("--env takes NAME=VALUE")?.as_bytes();
                let named = var.iter().position(|&b| b == b'=').is_some_and(|at| at > 0);
                if !named {
                    return Err("--env takes NAME=VALUE".to_owned());
                }
                command.env.push(var.to_vec());
            }
            Some(flag) if flag.starts_with('-') => return Err(format!("unknown option {flag}")),
            _ => {
                return Err(
                    "options go before the first `--`, then the command that reaches the server"
                        .to_owned(),
                );
            }
        }
    }
    let mut carrier = Vec::new();
    let mut separated = false;
    for word in words.by_ref() {
        if word == "--" {
            separated = true;
            break;
        }
        carrier.push(word.clone());
    }
    if carrier.is_empty() {
        return Err("the command after the first `--` is missing".to_owned());
    }
    if !separated {
        return Err("the program to run goes after a second `--`".to_owned());
    }
    command.argv = words.map(|word| word.as_bytes().to_vec()).collect();
    if command.argv.is_empty() {
        return Err("the program to run is missing".to_owned());
    }
    command.check().map_err(|e| format!("not a command: {e}"))?;
    Ok(Options { carrier, command })
}

// --- the run -------------------------------------------------------------------

/// What a descriptor in the poll set stands for.
enum Source {
    Signal,
    Carrier,
    Out,
    Stream(streams::Token),
}

/// The words a program is called by in what is said to a person.
fn name_of(command: &Command) -> String {
    command
        .argv
        .first()
        .map(|program| clean(&String::from_utf8_lossy(program)))
        .unwrap_or_default()
}

/// A bridge that runs commands says so in its features.
fn verdict(first: &Message) -> Result<Features, End> {
    streams::judge_commands(first).ok_or_else(|| {
        End::Declined(
            "the kablo on the server is too old to run commands: it needs a newer one".to_owned(),
        )
    })
}

/// Runs `command` on the bridge `carrier` reaches, joined to `local`, until the
/// program ends (how it ended is the answer) or something else ends it.
///
/// `signals`, when there is one, is the read end of a pipe of signal numbers
/// (see [`sys::signal_pipe`]); the first one read ends this, and the program
/// with it.
pub fn serve(
    carrier: Wire,
    command: Command,
    local: Local,
    signals: Option<BorrowedFd<'_>>,
    err: &mut dyn Write,
) -> Result<Exit, End> {
    let mut decoder = IncomingDecoder::new();
    let features = carrier::negotiate(&carrier, signals, &mut decoder, PING, verdict)?;

    // The limits are the server's, within this build's.
    let mut config = streams::Config::default();
    if features.max_window != 0 {
        config.window = config.window.min(features.max_window);
    }
    let Wire { input, output } = carrier;
    for fd in [&input, &output] {
        sys::set_nonblocking(fd.as_raw_fd())
            .map_err(|e| End::Failed(format!("cannot set up the command's pipes: {e}")))?;
    }
    let program = name_of(&command);
    let mut streams = Streams::new(wire::Side::Client, config);
    let mut control = Vec::new();
    streams
        .open_command(local, command, &mut control)
        .map_err(|e| End::Failed(format!("cannot ask for {program}: {e}")))?;
    Executing {
        input,
        output,
        signals,
        decoder,
        out: Outbound::new(READ_CHUNK),
        streams,
        control,
        buf: vec![0u8; READ_CHUNK],
        program,
        gone: false,
        err,
    }
    .run()
}

struct Executing<'a> {
    input: OwnedFd,
    output: OwnedFd,
    signals: Option<BorrowedFd<'a>>,
    decoder: IncomingDecoder,
    out: Outbound,
    streams: Streams,
    /// What the streams said this turn that is not `data`.
    control: Vec<wire::Packet>,
    buf: Vec<u8>,
    /// What the program is called.
    program: String,
    /// The carrier has ended or stopped taking bytes. That ends this unless the
    /// server had already said how the program ended: what it wrote may still
    /// be being written here, and that is not lost to the connection.
    gone: bool,
    err: &'a mut dyn Write,
}

impl Executing<'_> {
    fn run(&mut self) -> Result<Exit, End> {
        // Only one stream is ever opened, and the first number is 1.
        const STREAM: u32 = 1;
        // The `open` is waiting to be said.
        self.send();
        loop {
            // A carrier that is gone with nothing more to hear from it: there
            // would be nothing to wake the poll below (a stream that is still
            // opening waits for no descriptor).
            if self.gone && !self.streams.ended(STREAM) {
                return Err(End::Carrier);
            }
            let mut set = Vec::new();
            let mut sources = Vec::new();
            if let Some(signals) = self.signals {
                set.push(sys::want(signals.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Signal);
            }
            if !self.gone {
                set.push(sys::want(self.input.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Carrier);
                if self.out.pending() > 0 {
                    set.push(sys::want(self.output.as_raw_fd(), libc::POLLOUT));
                    sources.push(Source::Out);
                }
            }
            for (fd, token) in self.streams.interest() {
                set.push(fd);
                sources.push(Source::Stream(token));
            }
            if let Err(e) = sys::poll(&mut set, None) {
                return Err(End::Failed(format!("cannot wait for the command: {e}")));
            }
            for (fd, source) in set.iter().zip(&sources) {
                if fd.revents == 0 {
                    continue;
                }
                match source {
                    Source::Signal => {
                        if let Some(signal) = self.signals.and_then(signal_in) {
                            // Best effort: the program ends with the connection
                            // all the same.
                            self.streams.close(STREAM, &mut self.control);
                            self.send();
                            return Err(End::Signal(signal));
                        }
                    }
                    Source::Carrier => self.read_carrier()?,
                    Source::Out => {}
                    Source::Stream(token) => {
                        self.streams.ready(*token, fd.revents, &mut self.control);
                    }
                }
            }
            self.streams.settle(&mut self.control);
            let outcome = self.events();
            if !self.gone {
                self.send();
            }
            if let Some(outcome) = outcome {
                return outcome;
            }
        }
    }

    /// What the server said about the stream, as far as it ends this.
    fn events(&mut self) -> Option<Result<Exit, End>> {
        for event in self.streams.take_events() {
            match event {
                // (A conversation of mail is not a command.)
                Event::Opened(_) | Event::Over { .. } => {}
                Event::Failed { code, text, .. } => {
                    let why = if code == wire::FailCode::START_FAILED {
                        format!("cannot run {}: {}", self.program, clean(&text))
                    } else {
                        format!(
                            "the server would not run {}: {}",
                            self.program,
                            clean(&text)
                        )
                    };
                    return Some(Err(End::Declined(why)));
                }
                Event::Exited { exit, .. } => return Some(Ok(exit)),
                // A reader that went away ends a writer, quietly, with the
                // status the system gives one in a pipeline (this process
                // ignores the signal that would have; its status is the same).
                Event::Closed {
                    cause: Cause::Local(io::ErrorKind::BrokenPipe),
                    ..
                } => return Some(Err(End::Signal(libc::SIGPIPE))),
                Event::Closed {
                    cause: Cause::Local(kind),
                    ..
                } => {
                    return Some(Err(End::Failed(format!(
                        "cannot write what {} wrote ({kind}); it was ended",
                        self.program
                    ))));
                }
                Event::Closed {
                    cause: Cause::Server,
                    ..
                } => {
                    return Some(Err(End::Failed(format!(
                        "the server ended {} without saying how it ended",
                        self.program
                    ))));
                }
            }
        }
        None
    }

    /// Puts what is to be said on the wire as far as the carrier takes it.
    fn send(&mut self) {
        for packet in std::mem::take(&mut self.control) {
            let _ = packet.encode(wire::Side::Client, self.out.buffer());
        }
        // The bridge's rule, from the same code: a stream's frame goes in
        // behind the control frames every turn, unless the carrier is so far
        // behind that more would only wait.
        self.out.end_turn(&mut self.streams);
        let output = self.output.as_fd();
        if !self
            .out
            .flush(&mut self.streams, &mut |bytes| sys::write_fd(output, bytes))
        {
            self.gone = true;
        }
    }

    fn read_carrier(&mut self) -> Result<(), End> {
        let n = match sys::read_fd(self.input.as_fd(), &mut self.buf) {
            Ok(n) if n > 0 => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            // The end of what the carrier says, or a carrier that fails.
            _ => {
                self.gone = true;
                return Ok(());
            }
        };
        self.decoder.push(self.buf.get(..n).unwrap_or_default());
        loop {
            match self.decoder.next_incoming() {
                Ok(Some(Incoming::Stream { from, packet })) => {
                    self.streams.handle(from, packet, &mut self.control);
                }
                Ok(Some(Incoming::Session(packet))) => {
                    // A `pong` or an `error` for something nobody here is
                    // waiting on.
                    if let Message::Error { text, .. } = packet.message {
                        let _ = writeln!(self.err, "kablo: the server said: {}", clean(&text));
                    }
                }
                Ok(None) => return Ok(()),
                Err(e) if e.is_recoverable() => {}
                // The framing is broken: there is no telling where the next
                // frame starts.
                Err(e) => {
                    return Err(End::Failed(format!(
                        "what the server sent is not the protocol: {e}"
                    )));
                }
            }
        }
    }
}

// --- the command ----------------------------------------------------------------

/// A copy of descriptor `fd` of this process for the engine to own, so that
/// closing it closes nothing of ours. (Rust's runtime already opens `/dev/null`
/// in place of a standard descriptor that was closed, so there is almost
/// always one; `/dev/null` is also what a copy that fails falls back to, for
/// this to be used where that is not so.)
fn own(fd: RawFd, writing: bool) -> io::Result<OwnedFd> {
    // SAFETY: the descriptor is borrowed for the one call that copies it; one
    // that is not open is an error, not a fault.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    match borrowed.try_clone_to_owned() {
        Ok(copy) => Ok(copy),
        Err(_) => {
            let null = std::fs::OpenOptions::new()
                .read(!writing)
                .write(writing)
                .open("/dev/null")?;
            Ok(OwnedFd::from(null))
        }
    }
}

/// This process's own standard input, output and error, as the descriptors a
/// command is joined to.
pub fn standard() -> io::Result<Local> {
    Ok(Local {
        input: own(0, false)?,
        output: own(1, true)?,
        errors: own(2, true)?,
    })
}

/// How a program's end is the status of this process: its code, or 128 and the
/// signal's number (the server's own numbering).
pub fn status_of(exit: Exit) -> u8 {
    match exit {
        Exit::Code(code) => u8::try_from(code & 0xff).unwrap_or(EXIT_TROUBLE),
        Exit::Signal(signal) => u8::try_from(128 + signal).unwrap_or(EXIT_TROUBLE),
    }
}

/// Starts the carrier and runs the program until it ends. Returns the status:
/// the program's, or 255 when it could not be run or the connection to it was
/// lost; after a signal, 128 and its number.
pub fn run(options: Options, err: &mut dyn Write) -> u8 {
    let signals = match sys::signal_pipe(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = writeln!(err, "kablo: cannot watch for signals: {e}");
            return EXIT_TROUBLE;
        }
    };
    let local = match standard() {
        Ok(local) => local,
        Err(e) => {
            let _ = writeln!(err, "kablo: cannot use standard input and output: {e}");
            return EXIT_TROUBLE;
        }
    };
    let (mut carrier, wire) = match carrier::start(&options.carrier) {
        Ok(started) => started,
        Err(why) => {
            let _ = writeln!(err, "kablo: {why}");
            return EXIT_TROUBLE;
        }
    };
    // The carrier's pipes are gone when this returns, and with them the
    // command's input: the end of it is how a bridge is told to finish.
    let result = serve(wire, options.command, local, Some(signals.as_fd()), err);
    // The carrier is not asked to end: it finishes when its input has ended,
    // and quietly (`ssh` says it was killed by a signal it was sent). One that
    // does not is killed after a short while.
    let status = carrier.reap(false);
    match result {
        Ok(exit) => status_of(exit),
        Err(End::Signal(signal)) => u8::try_from(128 + signal).unwrap_or(EXIT_TROUBLE),
        Err(End::Carrier) => {
            let _ = writeln!(
                err,
                "kablo: {} {}; the connection to the server is gone",
                carrier.name(),
                describe(status)
            );
            EXIT_TROUBLE
        }
        Err(End::Declined(why) | End::Failed(why)) => {
            let _ = writeln!(err, "kablo: {why}");
            EXIT_TROUBLE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    fn argv(args: &[&str]) -> Vec<Vec<u8>> {
        args.iter().map(|a| a.as_bytes().to_vec()).collect()
    }

    #[test]
    fn the_first_separator_starts_the_carrier_and_the_second_the_program() {
        let options = parse(&words(&[
            "--cwd", "/srv", "--env", "A=1", "--env", "B=", "--", "ssh", "host", "kablo", "attach",
            "--", "tar", "-cf", "-", "--", "dir",
        ]))
        .unwrap();
        assert_eq!(options.carrier, words(&["ssh", "host", "kablo", "attach"]));
        assert_eq!(
            options.command.argv,
            argv(&["tar", "-cf", "-", "--", "dir"])
        );
        assert_eq!(options.command.env, argv(&["A=1", "B="]));
        assert_eq!(options.command.cwd, Some(b"/srv".to_vec()));
    }

    #[test]
    fn a_program_may_be_started_with_options_of_its_own() {
        let options = parse(&words(&[
            "--", "kablo", "attach", "--", "ls", "-l", "--all",
        ]))
        .unwrap();
        assert_eq!(options.command.argv, argv(&["ls", "-l", "--all"]));
        assert_eq!(options.command.cwd, None);
        assert!(options.command.env.is_empty());
    }

    #[test]
    fn what_is_not_a_command_line_is_said() {
        for bad in [
            &[][..],
            &["ssh", "host", "kablo", "attach", "--", "ls"],
            &["--"],
            &["--", "ssh"],
            &["--", "--", "ls"],
            &["--", "ssh", "--"],
            &["--cwd", "--", "ssh", "--", "ls"],
            &["--cwd"],
            &["--env", "NOEQUALS", "--", "ssh", "--", "ls"],
            &["--env", "=v", "--", "ssh", "--", "ls"],
            &["--env"],
            &["--bogus", "--", "ssh", "--", "ls"],
            &["--", "ssh", "--", ""],
        ] {
            assert!(parse(&words(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_exit_is_a_status_and_a_signal_is_128_and_its_number() {
        assert_eq!(status_of(Exit::Code(0)), 0);
        assert_eq!(status_of(Exit::Code(3)), 3);
        assert_eq!(status_of(Exit::Code(255)), 255);
        assert_eq!(status_of(Exit::Signal(9)), 137);
        assert_eq!(status_of(Exit::Signal(15)), 143);
    }

    #[test]
    fn a_program_is_called_by_its_name_without_control_characters() {
        let command = Command {
            argv: argv(&["pro\u{1b}gram", "x"]),
            ..Command::default()
        };
        assert_eq!(name_of(&command), "program");
        assert_eq!(name_of(&Command::default()), "");
    }
}
