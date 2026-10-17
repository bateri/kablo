//! The bridge's mailbox: where a program on the server asks where the app
//! stands.
//!
//! A bridge carries one app. Beside the sessions, in the socket directory, it
//! keeps a socket of its own ([`sockdir::mailbox_path`]) that a program on the
//! same machine connects to, with the handshake of any client, to hand bytes to
//! that app. Before it does it can `ask` each mailbox on the machine how its
//! app stands: which terminal the asker runs in, whether the app is attached to
//! it, and how long ago the app last used it. The choice among the bridges is
//! the asker's.
//!
//! The terminal an answer is about is found here and never taken from the
//! asker: the connecting process is found by its pid ([`sys::peer_pid`]) and
//! looked up among the running sessions ([`crate::origin`]). That takes a
//! `status` of every session, which is a scan the bridge polls like its own
//! (see [`crate::scan`]); the asker is answered when it ends.
//!
//! Nothing here keeps a timer of its own. A mailbox with nobody asking waits on
//! its listener and its connections alone; the one deadline it ever has is a
//! scan's, while a question is being answered. The bridge's client is not told
//! of any of it: this module is given a way to ask where the client stands
//! ([`Standing`]) and nothing else of the bridge.
//!
//! The connections are bounded, since anything that runs as the user may
//! connect: [`MAX_SENDERS`] at once, [`MAX_ASKS`] questions being answered,
//! and a connection that does not read its answers is closed when [`MAX_OUT`]
//! bytes of them are waiting.

use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

use kablo_proto::v1::stream::{self as wire, Incoming, IncomingDecoder};
use kablo_proto::v1::{ErrorCode, Message, Packet, Standing};
use kablo_proto::{SessionId, WIRE};

use crate::handshake::{self, Step};
use crate::origin::{self, Candidate, Origin};
use crate::scan::{self, Scan};
use crate::{sockdir, sys};

/// How many programs may be connected to a mailbox at once.
const MAX_SENDERS: usize = 32;

/// How many questions may be waiting on a scan at once.
const MAX_ASKS: usize = 16;

/// Unsent bytes above which a connection is closed: it asks and does not read.
const MAX_OUT: usize = 64 * 1024;

/// Bytes read from a connection at a time.
const READ_CHUNK: usize = 16 * 1024;

/// What a program that opens a stream on a mailbox is told while no app takes
/// the mail it would hand over.
const NO_APP: &str = "no app that takes mail is connected";

/// How the bridge's client stands, for the terminal `ask` is about (none: a
/// question about a program that runs in no terminal).
pub type Stand<'a> = dyn Fn(Option<&SessionId>) -> Standing + 'a;

/// What a descriptor of the mailbox in the bridge's poll set stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// The listener.
    Listener,
    /// The connection at this index.
    Sender(usize),
    /// A session being asked: the question at this index, its probe at the
    /// other.
    Probe(usize, usize),
}

/// A program connected to the mailbox.
struct Sender {
    /// A number of its own, for a question that outlives a turn (the indexes
    /// of the list it is in move when another is dropped).
    id: u64,
    sock: UnixStream,
    /// The process at the other end when it connected; `None` when the kernel
    /// would not say.
    pid: Option<u32>,
    /// Bytes received before the handshake is complete.
    hello: Vec<u8>,
    open: bool,
    decoder: IncomingDecoder,
    out: Vec<u8>,
    sent: usize,
    dead: bool,
}

impl Sender {
    fn pending(&self) -> usize {
        self.out.len().saturating_sub(self.sent)
    }

    /// Queues a packet of the session channel.
    fn send(&mut self, packet: &Packet) {
        let _ = packet.encode(&mut self.out);
        self.limit();
    }

    /// Queues a packet of the stream channel.
    fn send_stream(&mut self, packet: &wire::Packet) {
        let _ = packet.encode(wire::Side::Server, &mut self.out);
        self.limit();
    }

    fn limit(&mut self) {
        if self.pending() > MAX_OUT {
            self.dead = true;
        }
    }

    /// Writes what the socket takes.
    fn flush(&mut self) {
        while self.pending() > 0 {
            let waiting = self.out.get(self.sent..).unwrap_or_default();
            match sys::write_fd(self.sock.as_fd(), waiting) {
                Ok(0) => break,
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
        }
        if self.pending() == 0 {
            self.out.clear();
            self.sent = 0;
        }
    }

    /// Reads what has arrived and returns the questions in it: the terminal
    /// each names, or none.
    fn read(&mut self) -> Vec<Option<SessionId>> {
        let mut buf = [0u8; READ_CHUNK];
        match sys::read_fd(self.sock.as_fd(), &mut buf) {
            Ok(0) => {
                self.dead = true;
                Vec::new()
            }
            Ok(n) => self.take(buf.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Vec::new(),
            Err(_) => {
                self.dead = true;
                Vec::new()
            }
        }
    }

    /// Bytes arrived: the handshake first, then packets. What is not a
    /// question is answered here.
    fn take(&mut self, bytes: &[u8]) -> Vec<Option<SessionId>> {
        let mut asks = Vec::new();
        if self.open {
            self.decoder.push(bytes);
        } else {
            self.hello.extend_from_slice(bytes);
            match handshake::serve(&WIRE, &self.hello) {
                Ok(Step::Need) => return asks,
                Ok(Step::Done {
                    reply, consumed, ..
                }) => {
                    self.out.extend_from_slice(&reply);
                    let rest = self.hello.get(consumed..).unwrap_or_default().to_vec();
                    self.decoder.push(&rest);
                    self.hello = Vec::new();
                    self.open = true;
                }
                Ok(Step::Refused { reply }) => {
                    self.out.extend_from_slice(&reply);
                    self.flush();
                    self.dead = true;
                    return asks;
                }
                Err(_) => {
                    self.dead = true;
                    return asks;
                }
            }
        }
        while !self.dead {
            match self.decoder.next_incoming() {
                Ok(Some(Incoming::Session(packet))) => match packet.message {
                    Message::Ping(bytes) => self.send(&Packet {
                        session: packet.session,
                        message: Message::Pong(bytes),
                    }),
                    Message::Ask => asks.push(packet.session),
                    _ => self.send(&failure(
                        packet.session,
                        ErrorCode::BAD_REQUEST,
                        "a mailbox takes ask and open",
                    )),
                },
                Ok(Some(Incoming::Stream {
                    from: wire::Side::Client,
                    packet,
                })) => {
                    if matches!(
                        packet.message,
                        wire::Message::Open { .. }
                            | wire::Message::OpenCommand { .. }
                            | wire::Message::OpenTicket { .. }
                    ) {
                        self.send_stream(&wire::Packet::new(
                            packet.stream,
                            wire::Message::Failed {
                                code: wire::FailCode::UNREACHABLE,
                                text: NO_APP.to_owned(),
                            },
                        ));
                    }
                }
                // Messages of the server's kind come from nobody here.
                Ok(Some(Incoming::Stream { .. })) => {}
                Ok(None) => break,
                Err(e) if e.is_recoverable() => {
                    self.send(&failure(None, ErrorCode::BAD_REQUEST, "malformed message"))
                }
                // The framing itself is broken: there is no telling where the
                // next frame starts.
                Err(_) => self.dead = true,
            }
        }
        asks
    }
}

/// A question whose answer waits on a scan of the sessions.
struct Ask {
    /// The connection that asked ([`Sender::id`]).
    sender: u64,
    /// The process that asked, whose terminal is the answer.
    pid: u32,
    scan: Scan,
}

impl Ask {
    /// The terminal the asker runs in, among the sessions the scan found.
    fn terminal(self) -> Option<SessionId> {
        let candidates: Vec<Candidate> = self
            .scan
            .into_statuses()
            .iter()
            .filter_map(Candidate::of)
            .collect();
        match origin::find(self.pid, &candidates) {
            Origin::Terminal(id) => Some(id),
            Origin::None => None,
        }
    }
}

/// A bridge's mailbox.
pub struct Mailbox {
    dir: PathBuf,
    path: PathBuf,
    listener: UnixListener,
    senders: Vec<Sender>,
    asks: Vec<Ask>,
    next_sender: u64,
    /// Turns the listener is left out of after an `accept` that failed for
    /// want of anything to accept with: a listener that stays readable would
    /// otherwise be polled again at once, for ever.
    backoff: u8,
}

impl Mailbox {
    /// Opens the mailbox of this process in `dir`, making the directory if it
    /// is not there. The mailboxes of bridges that were killed are cleared on
    /// the way. An error is a bridge without a mailbox, which works as before.
    pub fn open(dir: &Path) -> io::Result<Mailbox> {
        sockdir::ensure_dir(dir)?;
        let _ = sockdir::mailboxes(dir);
        let path = sockdir::mailbox_path(dir, std::process::id());
        let listener = sockdir::listen(&path)?;
        // From here the name is ours: dropping the mailbox removes it, so a
        // failure below leaves nothing behind.
        let mailbox = Mailbox {
            dir: dir.to_owned(),
            path,
            listener,
            senders: Vec::new(),
            asks: Vec::new(),
            next_sender: 0,
            backoff: 0,
        };
        mailbox.listener.set_nonblocking(true)?;
        Ok(mailbox)
    }

    /// The descriptors to wait on.
    pub fn interest(&self) -> Vec<(libc::pollfd, Token)> {
        let mut set = Vec::new();
        if self.backoff == 0 && self.senders.len() < MAX_SENDERS {
            set.push((
                sys::want(self.listener.as_raw_fd(), libc::POLLIN),
                Token::Listener,
            ));
        }
        for (i, sender) in self.senders.iter().enumerate() {
            if sender.dead {
                continue;
            }
            let mut events = libc::POLLIN;
            if sender.pending() > 0 {
                events |= libc::POLLOUT;
            }
            set.push((sys::want(sender.sock.as_raw_fd(), events), Token::Sender(i)));
        }
        for (a, ask) in self.asks.iter().enumerate() {
            for (p, fd) in ask.scan.interest() {
                set.push((fd, Token::Probe(a, p)));
            }
        }
        set
    }

    /// When a question that is waiting gives up on the sessions that have not
    /// answered it; `None` when none is waiting, which is what a mailbox that
    /// is not being used has.
    pub fn deadline(&self) -> Option<Instant> {
        self.asks.iter().filter_map(|ask| ask.scan.deadline()).min()
    }

    /// The descriptor of `token` reported `revents`.
    pub fn ready(&mut self, token: Token, revents: libc::c_short, stand: &Stand<'_>) {
        match token {
            Token::Listener => self.accept(),
            Token::Sender(i) => {
                let asks = match self.senders.get_mut(i) {
                    Some(sender) => {
                        sender.flush();
                        sender.read()
                    }
                    None => return,
                };
                for terminal in asks {
                    self.ask(i, terminal, stand);
                }
            }
            Token::Probe(a, p) => {
                if let Some(ask) = self.asks.get_mut(a) {
                    ask.scan.ready(p, revents);
                }
            }
        }
    }

    /// The end of a turn: answers the questions whose scan is over, writes
    /// what is waiting and drops the connections that are gone.
    pub fn settle(&mut self, now: Instant, stand: &Stand<'_>) {
        self.backoff = self.backoff.saturating_sub(1);
        for ask in &mut self.asks {
            ask.scan.expire(now);
        }
        let (done, running): (Vec<Ask>, Vec<Ask>) = std::mem::take(&mut self.asks)
            .into_iter()
            .partition(|ask| ask.scan.is_finished());
        self.asks = running;
        for ask in done {
            let sender = ask.sender;
            let terminal = ask.terminal();
            self.answer(sender, terminal, stand);
        }
        for sender in &mut self.senders {
            sender.flush();
        }
        self.senders.retain(|sender| !sender.dead);
        let senders = &self.senders;
        self.asks
            .retain(|ask| senders.iter().any(|sender| sender.id == ask.sender));
    }

    fn accept(&mut self) {
        while self.senders.len() < MAX_SENDERS {
            match self.listener.accept() {
                Ok((sock, _)) => self.seat(sock),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // A signal, or a connection that went away before it was
                // accepted.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                    ) => {}
                // No descriptor to accept with, or the like: leave the
                // listener alone until something else has happened.
                Err(_) => {
                    self.backoff = 2;
                    return;
                }
            }
        }
    }

    /// Takes in a connection that was accepted, if it is one to serve: the
    /// process at the other end must be ours.
    fn seat(&mut self, sock: UnixStream) {
        let own = sys::effective_uid();
        let ours = sys::peer_uid(sock.as_fd()).is_ok_and(|peer| sys::uid_allowed(peer, own));
        if !ours || sock.set_nonblocking(true).is_err() {
            return;
        }
        let pid = sys::peer_pid(sock.as_fd()).ok();
        self.next_sender += 1;
        self.senders.push(Sender {
            id: self.next_sender,
            sock,
            pid,
            hello: Vec::new(),
            open: false,
            decoder: IncomingDecoder::new(),
            out: Vec::new(),
            sent: 0,
            dead: false,
        });
    }

    /// A question from the connection at index `i`. One that names the
    /// terminal is answered about that terminal at once; one that does not is
    /// about the terminal the asker runs in, which takes a scan to find.
    fn ask(&mut self, i: usize, terminal: Option<SessionId>, stand: &Stand<'_>) {
        let Some(sender) = self.senders.get(i) else {
            return;
        };
        let (id, pid) = (sender.id, sender.pid);
        if terminal.is_some() {
            return self.answer(id, terminal, stand);
        }
        // A process that cannot be found has no terminal to find.
        let Some(pid) = pid else {
            return self.answer(id, None, stand);
        };
        let refused = if self.asks.len() >= MAX_ASKS {
            Some("too many questions are waiting".to_owned())
        } else {
            match Scan::start(&self.dir, scan::WAIT) {
                Ok(scan) => {
                    self.asks.push(Ask {
                        sender: id,
                        pid,
                        scan,
                    });
                    None
                }
                Err(e) => Some(e.to_string()),
            }
        };
        if let (Some(why), Some(sender)) = (refused, self.senders.get_mut(i)) {
            sender.send(&failure(None, ErrorCode::UNAVAILABLE, &why));
        }
    }

    /// Answers the connection numbered `sender` about `terminal`.
    fn answer(&mut self, sender: u64, terminal: Option<SessionId>, stand: &Stand<'_>) {
        let reply = Packet {
            message: Message::AskReply(stand(terminal.as_ref())),
            session: terminal,
        };
        if let Some(sender) = self.senders.iter_mut().find(|s| s.id == sender) {
            sender.send(&reply);
        }
    }
}

impl Drop for Mailbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn failure(session: Option<SessionId>, code: ErrorCode, text: &str) -> Packet {
    Packet {
        session,
        message: Message::Error {
            code,
            text: text.to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kablo_proto::v1::PacketDecoder;
    use std::io::{Read, Write};
    use std::time::Duration;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/kt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// A program connected to a mailbox.
    struct Program {
        sock: UnixStream,
        decoder: PacketDecoder,
    }

    impl Program {
        /// Connects to `mailbox`, past the handshake.
        fn to(mailbox: &mut Mailbox, stand: &Stand<'_>) -> Program {
            let mut sock = UnixStream::connect(&mailbox.path).unwrap();
            let mut hello = Vec::new();
            WIRE.write_hello(&mut hello).unwrap();
            sock.write_all(&hello).unwrap();
            mailbox.ready(Token::Listener, libc::POLLIN, stand);
            let index = mailbox.senders.len() - 1;
            mailbox.ready(Token::Sender(index), libc::POLLIN, stand);
            mailbox.settle(Instant::now(), stand);
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            WIRE.read_reply(&mut sock).unwrap();
            Program {
                sock,
                decoder: PacketDecoder::new(),
            }
        }

        fn send(&mut self, packet: &Packet) {
            let mut bytes = Vec::new();
            packet.encode(&mut bytes).unwrap();
            self.sock.write_all(&bytes).unwrap();
        }

        fn reply(&mut self) -> Packet {
            let mut buf = [0u8; 1024];
            loop {
                if let Some(packet) = self.decoder.next_packet().unwrap() {
                    return packet;
                }
                let n = self.sock.read(&mut buf).unwrap();
                assert!(n > 0, "the mailbox closed the connection");
                self.decoder.push(&buf[..n]);
            }
        }
    }

    #[test]
    fn a_mailbox_that_is_not_asked_waits_on_nothing_but_its_descriptors() {
        let dir = scratch("mb-quiet");
        let stand = |_: Option<&SessionId>| Standing::default();
        let mut mailbox = Mailbox::open(&dir).unwrap();
        assert_eq!(mailbox.deadline(), None);
        let _program = Program::to(&mut mailbox, &stand);
        assert_eq!(mailbox.deadline(), None, "a connection is not a question");
        let tokens: Vec<Token> = mailbox.interest().into_iter().map(|(_, t)| t).collect();
        assert_eq!(tokens, [Token::Listener, Token::Sender(0)]);
        drop(mailbox);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_question_that_names_the_terminal_is_answered_about_it() {
        let dir = scratch("mb-named");
        let named = SessionId::new("here").unwrap();
        let stand = |terminal: Option<&SessionId>| match terminal {
            Some(_) => Standing::idle(Duration::from_millis(7)).with_attached(),
            None => Standing::idle(Duration::from_secs(9)),
        };
        let mut mailbox = Mailbox::open(&dir).unwrap();
        let mut program = Program::to(&mut mailbox, &stand);
        program.send(&Packet::about(named.clone(), Message::Ask));
        mailbox.ready(Token::Sender(0), libc::POLLIN, &stand);
        mailbox.settle(Instant::now(), &stand);
        let answer = program.reply();
        assert_eq!(answer.session, Some(named));
        assert_eq!(
            answer.message,
            Message::AskReply(Standing::idle(Duration::from_millis(7)).with_attached())
        );
        assert_eq!(mailbox.deadline(), None, "nothing was scanned");
        drop(mailbox);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_question_that_names_nothing_waits_for_the_scan_and_a_stranger_has_no_terminal() {
        let dir = scratch("mb-unnamed");
        let stand = |terminal: Option<&SessionId>| {
            assert!(terminal.is_none(), "this test runs in no session");
            Standing::idle(Duration::from_millis(3))
        };
        let mut mailbox = Mailbox::open(&dir).unwrap();
        let mut program = Program::to(&mut mailbox, &stand);
        program.send(&Packet::connection(Message::Ask));
        mailbox.ready(Token::Sender(0), libc::POLLIN, &stand);
        // The directory has no session, so the scan is over at once.
        mailbox.settle(Instant::now(), &stand);
        let answer = program.reply();
        assert_eq!(answer.session, None);
        assert_eq!(
            answer.message,
            Message::AskReply(Standing::idle(Duration::from_millis(3)))
        );
        drop(mailbox);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn what_is_not_a_question_is_answered_and_the_connection_stays() {
        let dir = scratch("mb-other");
        let stand = |_: Option<&SessionId>| Standing::default();
        let mut mailbox = Mailbox::open(&dir).unwrap();
        let mut program = Program::to(&mut mailbox, &stand);
        program.send(&Packet::connection(Message::Features));
        program.send(&Packet::connection(Message::Ping(vec![1, 2])));
        mailbox.ready(Token::Sender(0), libc::POLLIN, &stand);
        mailbox.settle(Instant::now(), &stand);
        assert!(matches!(
            program.reply().message,
            Message::Error {
                code: ErrorCode::BAD_REQUEST,
                ..
            }
        ));
        assert_eq!(program.reply().message, Message::Pong(vec![1, 2]));
        assert_eq!(mailbox.senders.len(), 1);
        drop(mailbox);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn dropping_the_mailbox_removes_its_name_and_a_connection_that_leaves_is_forgotten() {
        let dir = scratch("mb-drop");
        let stand = |_: Option<&SessionId>| Standing::default();
        let mut mailbox = Mailbox::open(&dir).unwrap();
        let path = mailbox.path.clone();
        assert!(path.exists());
        let program = Program::to(&mut mailbox, &stand);
        drop(program);
        mailbox.ready(Token::Sender(0), libc::POLLIN, &stand);
        mailbox.settle(Instant::now(), &stand);
        assert!(mailbox.senders.is_empty());
        drop(mailbox);
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
