//! `kablo app`: a program on the server hands bytes to the app that is
//! connected to it, and gets bytes back.
//!
//! This is the sender's side of mail (`docs/PROTOCOL.md`, section 5): it joins
//! this process's standard input and output to one conversation with the app
//! and does nothing else. It does not look at the bytes, and what they mean is
//! the app's and the program's. It finds the bridges that carry an app (each
//! keeps a mailbox in the socket directory, [`crate::mailbox`]), asks each where
//! its app stands, picks one, and opens a conversation on the mailbox of the
//! one it picked.
//!
//! The choice is the sender's (the bridges only answer): among the apps that
//! take mail, the one that was last used at the terminal this runs in; when
//! none is at it, or this runs in no terminal, the one that was last used
//! anywhere on this server. Which terminal this runs in is the bridge's word,
//! from the kernel's word on this process, and never this program's.
//!
//! The conversation is the stream engine of [`crate::streams`], the same code
//! the bridge runs, with this process's input and output as its ends. Nothing
//! here sets a timer while it goes on: only the two waits for an answer (a
//! bridge's to a question, the app's to a ticket) have a limit, because the
//! other end may be a bridge that is stopped or an app that has gone to sleep,
//! and a sender that waited for either would wait for ever.

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use kablo_proto::WIRE;
use kablo_proto::v1::stream::{self as wire, FailCode, Incoming, IncomingDecoder};
use kablo_proto::v1::{Message, Packet, Standing};

use crate::carrier::{READ_CHUNK, clean, signal_in};
use crate::handshake::{self, ClientStep};
use crate::outbound::Outbound;
use crate::peer;
use crate::sockdir::{self, MailboxFile};
use crate::streams::{self, Cause, Event, Local, Streams};
use crate::sys;

/// The status when the conversation was had and the app closed it, or when
/// both had said all they had.
pub const EXIT_OVER: u8 = 0;

/// The status when no app that takes mail is connected (sysexits
/// `EX_UNAVAILABLE`).
pub const EXIT_NO_APP: u8 = 69;

/// The status of everything else that kept the conversation from being had or
/// from ending well: the connection was lost, the app declined or did not
/// answer, what the app said could not be written. The words are on standard
/// error.
pub const EXIT_TROUBLE: u8 = 1;

/// How long a bridge gets to answer a question: longer than a session gets to
/// answer the bridge (`scan::WAIT`), since the bridge may be asking the
/// sessions before it can answer. A bridge that has not by then is left out of
/// the choice.
pub const ASK_WAIT: Duration = Duration::from_secs(5);

/// How long the app gets to answer a ticket, which an app does at once. One
/// that has not (it is asleep, its connection is half dead) is given up on,
/// and the conversation is closed.
pub const ANSWER_WAIT: Duration = Duration::from_secs(10);

/// The two waits, for a test to shorten.
#[derive(Clone, Copy, Debug)]
pub struct Patience {
    /// How long a bridge gets to answer a question.
    pub ask: Duration,
    /// How long the app gets to answer a ticket.
    pub answer: Duration,
}

impl Patience {
    /// What `kablo app` waits.
    pub const STANDARD: Patience = Patience {
        ask: ASK_WAIT,
        answer: ANSWER_WAIT,
    };
}

/// Reads the arguments after `kablo app`: there are none.
pub fn parse(args: &[OsString]) -> Result<(), String> {
    match args.first() {
        None => Ok(()),
        Some(arg) => Err(format!(
            "kablo app takes no arguments (not {}): its input is what it hands over",
            clean(&arg.to_string_lossy())
        )),
    }
}

// --- the run -------------------------------------------------------------------

/// Hands this process's standard input to the app and prints what the app says,
/// until the conversation is over. Returns the status.
pub fn run(err: &mut dyn Write) -> u8 {
    let signals = match sys::signal_pipe(&[libc::SIGINT, libc::SIGTERM, libc::SIGHUP]) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = writeln!(err, "kablo: cannot watch for signals: {e}");
            return EXIT_TROUBLE;
        }
    };
    let local = match crate::exec::standard() {
        Ok(local) => local,
        Err(e) => {
            let _ = writeln!(err, "kablo: cannot use standard input and output: {e}");
            return EXIT_TROUBLE;
        }
    };
    let dir = match sockdir::socket_dir() {
        Ok(dir) => dir,
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            return EXIT_TROUBLE;
        }
    };
    talk(&dir, local, Some(signals.as_fd()), Patience::STANDARD, err)
}

/// [`run`] for the mailboxes in `dir`, with `local` for the ends of the
/// conversation and `signals` (the read end of a pipe of signal numbers, see
/// [`sys::signal_pipe`]) to end it.
pub fn talk(
    dir: &Path,
    local: Local,
    signals: Option<BorrowedFd<'_>>,
    patience: Patience,
    err: &mut dyn Write,
) -> u8 {
    let boxes = match mailboxes(dir) {
        Ok(boxes) => boxes,
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            return EXIT_TROUBLE;
        }
    };
    if boxes.is_empty() {
        return no_app(err, "no app is connected to this server through kablo");
    }
    let asked = match ask_all(&boxes, signals, patience.ask) {
        Ok(asked) => asked,
        Err(signal) => return status_of_signal(signal),
    };
    let silent = boxes.len().saturating_sub(asked.len());
    let standings: Vec<Standing> = asked.iter().map(|bridge| bridge.standing).collect();
    let Some(chosen) = choose(&standings) else {
        let what = match silent {
            0 => "no app that takes mail is connected".to_owned(),
            1 => "no app that takes mail is connected (one bridge did not answer)".to_owned(),
            n => format!("no app that takes mail is connected ({n} bridges did not answer)"),
        };
        return no_app(err, &what);
    };
    // Only the one that was chosen is kept: the others hear the connection end.
    let Some(bridge) = asked.into_iter().nth(chosen) else {
        return EXIT_TROUBLE;
    };
    match Conversing::new(bridge, signals, patience.answer, err).run(local) {
        Ended::Over => EXIT_OVER,
        Ended::NoApp(what) => no_app(err, &what),
        Ended::Failed(why) => {
            let _ = writeln!(err, "kablo: {why}");
            EXIT_TROUBLE
        }
        Ended::Signal(signal) => status_of_signal(signal),
    }
}

/// The status after a signal ended it: 128 and its number, as `kablo exec`'s.
fn status_of_signal(signal: i32) -> u8 {
    u8::try_from(128 + signal).unwrap_or(EXIT_TROUBLE)
}

/// Says there is no app to hand bytes to, and why that may be so, and returns
/// the status for it. The second line is for the one case that looks like a
/// fault and is not: a bridge from a kablo that did not have mail keeps no
/// mailbox, and is not renewed by `kablo upgrade`.
fn no_app(err: &mut dyn Write, what: &str) -> u8 {
    let _ = writeln!(err, "kablo: {what}");
    let _ = writeln!(
        err,
        "kablo: (an app connected through a kablo from before mail has no mailbox to \
         reach it by, and `kablo upgrade` does not change a bridge that is running: \
         the app has to connect again)"
    );
    EXIT_NO_APP
}

/// The mailboxes of the bridges that are running. A directory that is not there
/// has none.
fn mailboxes(dir: &Path) -> io::Result<Vec<MailboxFile>> {
    match sockdir::check_dir(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    }
    match sockdir::mailboxes(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        other => other,
    }
}

/// Which of the apps that were asked gets the bytes: among those that take
/// mail, the one at the sender's terminal that was used last, and when none is
/// at it, the one used last anywhere. The first of equals wins (the bridge
/// that has run the longest, as they are listed by pid).
fn choose(standings: &[Standing]) -> Option<usize> {
    let takers = || standings.iter().enumerate().filter(|(_, s)| s.tickets);
    takers()
        .filter(|(_, standing)| standing.attached)
        .min_by_key(|(_, standing)| standing.idle)
        .or_else(|| takers().min_by_key(|(_, standing)| standing.idle))
        .map(|(at, _)| at)
}

// --- asking ----------------------------------------------------------------------

/// A bridge's mailbox that has answered: the connection to it, and where its
/// app stands.
struct Bridge {
    sock: OwnedFd,
    /// What the bridge has sent beyond the answer.
    decoder: IncomingDecoder,
    standing: Standing,
}

/// A question to a mailbox, and what has come back.
struct Question {
    sock: OwnedFd,
    /// The handshake and the question, as far as they are written.
    out: Vec<u8>,
    sent: usize,
    /// What has come before the handshake's reply is whole.
    hello: Vec<u8>,
    open: bool,
    decoder: IncomingDecoder,
    standing: Option<Standing>,
    /// It will answer nothing: the connection ended or the bridge did not
    /// speak the protocol.
    over: bool,
}

impl Question {
    /// Connects to `mailbox` and queues the handshake and `ask` behind it.
    /// Whether anybody answers is for later: a connection that is made sits
    /// in the bridge's queue until it is accepted.
    fn to(mailbox: &MailboxFile) -> io::Result<Question> {
        let sock = peer::connect_ours(&mailbox.path)?;
        let mut out = Vec::new();
        WIRE.write_hello(&mut out).map_err(io::Error::other)?;
        Packet::connection(Message::Ask)
            .encode(&mut out)
            .map_err(io::Error::other)?;
        Ok(Question {
            sock,
            out,
            sent: 0,
            hello: Vec::new(),
            open: false,
            decoder: IncomingDecoder::new(),
            standing: None,
            over: false,
        })
    }

    fn waiting(&self) -> bool {
        self.standing.is_none() && !self.over
    }

    /// Writes what the socket takes.
    fn write(&mut self) {
        while self.sent < self.out.len() {
            let rest = self.out.get(self.sent..).unwrap_or_default();
            match sys::write_fd(self.sock.as_fd(), rest) {
                Ok(0) => break,
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    self.over = true;
                    break;
                }
            }
        }
    }

    /// Reads what the socket has and looks for the answer in it.
    fn read(&mut self, buf: &mut [u8]) {
        let n = match sys::read_fd(self.sock.as_fd(), buf) {
            Ok(0) => {
                self.over = true;
                return;
            }
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                return;
            }
            Err(_) => {
                self.over = true;
                return;
            }
        };
        self.take(buf.get(..n).unwrap_or_default());
    }

    fn take(&mut self, bytes: &[u8]) {
        if self.open {
            self.decoder.push(bytes);
        } else {
            self.hello.extend_from_slice(bytes);
            match handshake::reply(&WIRE, &self.hello) {
                Ok(ClientStep::Need) => return,
                Ok(ClientStep::Done { consumed, .. }) => {
                    let rest = self.hello.get(consumed..).unwrap_or_default().to_vec();
                    self.decoder.push(&rest);
                    self.hello = Vec::new();
                    self.open = true;
                }
                Ok(ClientStep::Refused) | Err(_) => {
                    self.over = true;
                    return;
                }
            }
        }
        // Whatever comes after the answer is for the conversation.
        while self.standing.is_none() {
            match self.decoder.next_incoming() {
                Ok(Some(Incoming::Session(Packet {
                    message: Message::AskReply(standing),
                    ..
                }))) => self.standing = Some(standing),
                // A bridge that refuses the question (it has too many waiting)
                // has answered: there is nothing more to wait for from it.
                Ok(Some(Incoming::Session(Packet {
                    message: Message::Error { .. },
                    ..
                }))) => {
                    self.over = true;
                    break;
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(e) if e.is_recoverable() => {}
                Err(_) => {
                    self.over = true;
                    break;
                }
            }
        }
    }
}

/// Asks every mailbox at once where its app stands, and returns the bridges
/// that answered within `wait`, in the order of `boxes`. A mailbox that cannot
/// be connected to (its bridge is gone, or it is not ours) or that does not
/// answer is not among them. `Err` is the signal that ended the wait.
fn ask_all(
    boxes: &[MailboxFile],
    signals: Option<BorrowedFd<'_>>,
    wait: Duration,
) -> Result<Vec<Bridge>, i32> {
    let mut questions: Vec<Question> = boxes
        .iter()
        .filter_map(|mailbox| Question::to(mailbox).ok())
        .collect();
    let deadline = Instant::now() + wait;
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !questions.iter().any(Question::waiting) {
            break;
        }
        let mut set = Vec::new();
        let mut who = Vec::new();
        if let Some(signals) = signals {
            set.push(sys::want(signals.as_raw_fd(), libc::POLLIN));
            who.push(None);
        }
        for (at, question) in questions.iter().enumerate() {
            if question.waiting() {
                let mut events = libc::POLLIN;
                if question.sent < question.out.len() {
                    events |= libc::POLLOUT;
                }
                set.push(sys::want(question.sock.as_raw_fd(), events));
                who.push(Some(at));
            }
        }
        if sys::poll(&mut set, Some(left)).is_err() {
            break;
        }
        for (fd, at) in set.iter().zip(&who) {
            if fd.revents == 0 {
                continue;
            }
            match at {
                None => {
                    if let Some(signal) = signals.and_then(signal_in) {
                        return Err(signal);
                    }
                }
                Some(at) => {
                    if let Some(question) = questions.get_mut(*at) {
                        question.write();
                        question.read(&mut buf);
                    }
                }
            }
        }
    }
    Ok(questions
        .into_iter()
        .filter_map(|question| {
            let standing = question.standing?;
            Some(Bridge {
                sock: question.sock,
                decoder: question.decoder,
                standing,
            })
        })
        .collect())
}

// --- the conversation ------------------------------------------------------------

/// How a conversation ended.
enum Ended {
    /// The app closed it, or both ends said all they had.
    Over,
    /// There is no app to have it with, in words for a person.
    NoApp(String),
    /// It did not go well, in words for a person.
    Failed(String),
    /// A signal asked to stop.
    Signal(i32),
}

/// What a descriptor in the poll set stands for.
enum Source {
    Signal,
    Bridge,
    Stream(streams::Token),
}

/// The conversation on a bridge's mailbox: one stream, and the connection it
/// runs on.
struct Conversing<'a> {
    sock: OwnedFd,
    signals: Option<BorrowedFd<'a>>,
    decoder: IncomingDecoder,
    out: Outbound,
    streams: Streams,
    /// What the streams said this turn that is not `data`.
    control: Vec<wire::Packet>,
    buf: Vec<u8>,
    /// The connection has ended or stopped taking bytes.
    gone: bool,
    /// The app has taken the conversation.
    opened: bool,
    answer: Duration,
    err: &'a mut dyn Write,
}

impl<'a> Conversing<'a> {
    fn new(
        bridge: Bridge,
        signals: Option<BorrowedFd<'a>>,
        answer: Duration,
        err: &'a mut dyn Write,
    ) -> Conversing<'a> {
        Conversing {
            sock: bridge.sock,
            signals,
            decoder: bridge.decoder,
            out: Outbound::new(READ_CHUNK),
            streams: Streams::new(wire::Side::Client, streams::Config::default()),
            control: Vec::new(),
            buf: vec![0u8; READ_CHUNK],
            gone: false,
            opened: false,
            answer,
            err,
        }
    }

    fn run(&mut self, local: Local) -> Ended {
        // The conversation has no errors to speak of; what is given for them is
        // not written to.
        let id = match self.streams.open_mail(local, &mut self.control) {
            Ok(id) => id,
            Err(e) => return Ended::Failed(format!("cannot start the conversation: {e}")),
        };
        let answer_by = Instant::now() + self.answer;
        self.send();
        if let Some(ended) = self.decode() {
            return ended;
        }
        loop {
            if self.gone && !self.streams.ended(id) {
                return Ended::Failed(if self.opened {
                    "the app went away".to_owned()
                } else {
                    "the connection to the app was lost before it took the conversation".to_owned()
                });
            }
            let mut set = Vec::new();
            let mut sources = Vec::new();
            if let Some(signals) = self.signals {
                set.push(sys::want(signals.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Signal);
            }
            if !self.gone {
                let mut events = libc::POLLIN;
                if self.out.pending() > 0 {
                    events |= libc::POLLOUT;
                }
                set.push(sys::want(self.sock.as_raw_fd(), events));
                sources.push(Source::Bridge);
            }
            for (fd, token) in self.streams.interest() {
                set.push(fd);
                sources.push(Source::Stream(token));
            }
            // The wait for an answer is the one limit, and only until it
            // comes.
            let waiting = self.streams.opening() > 0;
            let timeout = waiting.then(|| answer_by.saturating_duration_since(Instant::now()));
            if let Err(e) = sys::poll(&mut set, timeout) {
                return Ended::Failed(format!("cannot wait for the app: {e}"));
            }
            for (fd, source) in set.iter().zip(&sources) {
                if fd.revents == 0 {
                    continue;
                }
                match source {
                    Source::Signal => {
                        if let Some(signal) = self.signals.and_then(signal_in) {
                            // Best effort: the app is told it is over.
                            self.streams.close(id, &mut self.control);
                            self.send();
                            return Ended::Signal(signal);
                        }
                    }
                    Source::Bridge => {
                        if let Some(ended) = self.read_bridge() {
                            return ended;
                        }
                    }
                    Source::Stream(token) => {
                        self.streams.ready(*token, fd.revents, &mut self.control);
                    }
                }
            }
            self.streams.settle(&mut self.control);
            let ended = self.events();
            if !self.gone {
                self.send();
            }
            if let Some(ended) = ended {
                return ended;
            }
            // Only once what was ready has been handled: an answer that came
            // in the poll that ran out is an answer.
            if waiting && self.streams.opening() > 0 && Instant::now() >= answer_by {
                self.streams.close(id, &mut self.control);
                self.send();
                return Ended::Failed(
                    "the app did not take the conversation; it was given up on".to_owned(),
                );
            }
        }
    }

    /// What the streams said about the conversation, as far as it ends it.
    fn events(&mut self) -> Option<Ended> {
        for event in self.streams.take_events() {
            match event {
                Event::Opened(_) => self.opened = true,
                Event::Over { .. } => return Some(Ended::Over),
                Event::Failed { code, text, .. } => {
                    let text = clean(&text);
                    return Some(match code {
                        // What the bridge says when it has no app that takes
                        // mail, which the app may have just stopped being.
                        FailCode::UNREACHABLE => Ended::NoApp(text),
                        FailCode::DECLINED => Ended::Failed(format!("the app declined: {text}")),
                        _ => Ended::Failed(format!("the bridge would not hand it over: {text}")),
                    });
                }
                // A reader that went away ends a writer, quietly, with the status
                // the system gives one in a pipeline (as `kablo exec` does).
                Event::Closed {
                    cause: Cause::Local(io::ErrorKind::BrokenPipe),
                    ..
                } => return Some(Ended::Signal(libc::SIGPIPE)),
                Event::Closed {
                    cause: Cause::Local(kind),
                    ..
                } => {
                    return Some(Ended::Failed(format!(
                        "cannot write what the app said ({kind}); the conversation was ended"
                    )));
                }
                // The app's own `close` is `Over`: this is the bridge or the app
                // breaking the rules, or a conversation ended before it began.
                Event::Closed {
                    cause: Cause::Server,
                    ..
                } => {
                    return Some(Ended::Failed(if self.opened {
                        "what the app sent is not allowed by the protocol; the conversation \
                         was ended"
                            .to_owned()
                    } else {
                        "the bridge ended the conversation before the app took it".to_owned()
                    }));
                }
                Event::Exited { .. } => {}
            }
        }
        None
    }

    /// Puts what is to be said on the wire as far as the connection takes it.
    fn send(&mut self) {
        for packet in std::mem::take(&mut self.control) {
            let _ = packet.encode(wire::Side::Client, self.out.buffer());
        }
        self.out.end_turn(&mut self.streams);
        let sock = self.sock.as_fd();
        if !self
            .out
            .flush(&mut self.streams, &mut |bytes| sys::write_fd(sock, bytes))
        {
            self.gone = true;
        }
    }

    fn read_bridge(&mut self) -> Option<Ended> {
        match sys::read_fd(self.sock.as_fd(), &mut self.buf) {
            Ok(n) if n > 0 => {
                self.decoder.push(self.buf.get(..n).unwrap_or_default());
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return None,
            // The end of what the bridge says, or a connection that fails.
            _ => {
                self.gone = true;
                return None;
            }
        }
        self.decode()
    }

    /// Hands what has come to the streams.
    fn decode(&mut self) -> Option<Ended> {
        loop {
            match self.decoder.next_incoming() {
                Ok(Some(Incoming::Stream { from, packet })) => {
                    self.streams.handle(from, packet, &mut self.control);
                }
                Ok(Some(Incoming::Session(packet))) => {
                    // Something the bridge has to say about a message it could
                    // not use.
                    if let Message::Error { text, .. } = packet.message {
                        let _ = writeln!(self.err, "kablo: the bridge said: {}", clean(&text));
                    }
                }
                // A channel this version does not have: skipped, as the decoder
                // skips a frame it cannot read.
                Ok(Some(_)) => {}
                Ok(None) => return None,
                Err(e) if e.is_recoverable() => {}
                // The framing is broken: there is no telling where the next
                // frame starts.
                Err(e) => {
                    return Some(Ended::Failed(format!(
                        "what the bridge sent is not the protocol: {e}"
                    )));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn takes(idle_ms: u64) -> Standing {
        Standing::idle(Duration::from_millis(idle_ms)).with_tickets()
    }

    #[test]
    fn nothing_is_asked_for_by_arguments() {
        assert!(parse(&[]).is_ok());
        assert!(parse(&[OsString::from("--now")]).is_err());
    }

    #[test]
    fn the_app_at_the_terminal_that_was_used_last_gets_it() {
        let standings = [
            takes(50).with_attached(),
            takes(10).with_attached(),
            takes(900),
        ];
        assert_eq!(choose(&standings), Some(1));
    }

    #[test]
    fn an_app_at_the_terminal_wins_over_one_used_more_lately_elsewhere() {
        let standings = [takes(5), takes(5000).with_attached()];
        assert_eq!(choose(&standings), Some(1));
    }

    #[test]
    fn with_none_at_the_terminal_the_one_used_last_anywhere_gets_it() {
        let standings = [takes(300), takes(20), takes(40)];
        assert_eq!(choose(&standings), Some(1));
    }

    #[test]
    fn an_app_that_does_not_take_mail_is_never_chosen() {
        let silent = Standing::idle(Duration::ZERO).with_attached();
        assert_eq!(choose(&[silent]), None);
        assert_eq!(choose(&[silent, takes(9000)]), Some(1));
        assert_eq!(choose(&[]), None);
    }

    #[test]
    fn of_equals_the_first_wins() {
        assert_eq!(choose(&[takes(7), takes(7)]), Some(0));
    }

    #[test]
    fn a_signal_is_128_and_its_number() {
        assert_eq!(status_of_signal(libc::SIGINT), 130);
        assert_eq!(status_of_signal(libc::SIGPIPE), 141);
    }
}
