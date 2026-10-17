//! The bridge's mailbox: where a program on the server hands bytes to the app,
//! and asks where the app stands before it does.
//!
//! A bridge carries one app. Beside the sessions, in the socket directory, it
//! keeps a socket of its own ([`sockdir::mailbox_path`]) that a program on the
//! same machine connects to, with the handshake of any client, to hand bytes to
//! that app. Before it does it can `ask` each mailbox on the machine how its
//! app stands: which terminal the asker runs in, whether the app is attached to
//! it, and how long ago the app last used it. The choice among the bridges is
//! the asker's.
//!
//! The program opens a stream on the mailbox, and the app is given a ticket for
//! it that says which terminal the program runs in; the app answers the ticket
//! by opening a stream of its own, and the two are one conversation
//! ([`crate::conversation`]), joined here and nowhere else. The mailbox is
//! where the sockets are: it sends what a conversation says to the connection it
//! is for and hands what is for the app to the bridge ([`Mailbox::drain`]).
//!
//! The terminal an answer or a ticket is about is found here and never taken
//! from the asker: the connecting process is found by its pid
//! ([`sys::peer_pid`]) and looked up among the running sessions
//! ([`crate::origin`]). That takes a `status` of every session, which is a scan
//! the bridge polls like its own (see [`crate::scan`]); the asker is answered
//! when it ends.
//!
//! Nothing here keeps a timer of its own. A mailbox with nobody asking waits on
//! its listener and its connections alone; the one deadline it ever has is a
//! scan's, while a question is being answered. A program that is handed no
//! answer by the app waits as long as it will: giving up is its own choice, and
//! the ticket is forgotten when it does. The bridge's client is not told of any
//! of it but the tickets: this module is given a way to ask where the client
//! stands ([`Standing`]) and nothing else of the bridge.
//!
//! The connections are bounded, since anything that runs as the user may
//! connect: [`MAX_SENDERS`] at once, [`MAX_ASKS`] questions being answered, and
//! a connection that does not read its answers is closed when [`MAX_OUT`] bytes
//! of them are waiting. The bytes of a conversation are not among them: they are
//! kept within the window the connection gave for them, which the conversation
//! holds the app to, and a connection that is slow to take them is not closed
//! for it.

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

use kablo_proto::v1::stream::{self as wire, FailCode, Incoming, IncomingDecoder};
use kablo_proto::v1::{ErrorCode, Message, Packet, Standing};
use kablo_proto::{SessionId, WIRE};

use crate::conversation::{Conversations, Said};
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

/// What a connection said that the mailbox acts on.
enum Event {
    /// A question: the terminal it names, or none.
    Ask(Option<SessionId>),
    /// A message about a stream.
    Stream(wire::Packet),
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
    /// Bytes written to the socket since the connection began, for telling
    /// where in `out` the frames of `bulk` are.
    written: u64,
    /// The frames of a conversation's `data` waiting in `out`: where each ends
    /// (in the bytes of `written`) and how large it is. They are kept in check
    /// by the window the connection gave for them and not by [`MAX_OUT`],
    /// which is for what a connection asks for and does not read.
    bulk: VecDeque<(u64, usize)>,
    /// What `bulk` comes to.
    bulk_bytes: usize,
    /// The highest stream number it has opened: they rise, so that a frame of
    /// a stream that is over is not taken for one of a later stream.
    highest: u32,
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
        let before = self.out.len();
        let _ = packet.encode(wire::Side::Server, &mut self.out);
        if matches!(packet.message, wire::Message::Data(_)) {
            let size = self.out.len().saturating_sub(before);
            let end = self.written + self.pending() as u64;
            self.bulk.push_back((end, size));
            self.bulk_bytes += size;
        }
        self.limit();
    }

    fn limit(&mut self) {
        if self.pending().saturating_sub(self.bulk_bytes) > MAX_OUT {
            self.dead = true;
        }
    }

    /// Takes the number of a stream the connection opens, if it is above the
    /// ones it has: one that is not is a leftover and is not answered.
    fn number(&mut self, stream: u32) -> bool {
        if stream <= self.highest {
            return false;
        }
        self.highest = stream;
        true
    }

    /// Writes what the socket takes.
    fn flush(&mut self) {
        while self.pending() > 0 {
            let waiting = self.out.get(self.sent..).unwrap_or_default();
            match sys::write_fd(self.sock.as_fd(), waiting) {
                Ok(0) => break,
                Ok(n) => {
                    self.sent += n;
                    self.written += n as u64;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
        }
        while let Some(&(end, size)) = self.bulk.front() {
            if end > self.written {
                break;
            }
            self.bulk_bytes = self.bulk_bytes.saturating_sub(size);
            self.bulk.pop_front();
        }
        if self.pending() == 0 {
            self.out.clear();
            self.sent = 0;
        }
    }

    /// Reads what has arrived and returns what the mailbox is to act on.
    fn read(&mut self) -> Vec<Event> {
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

    /// Bytes arrived: the handshake first, then packets. What the mailbox does
    /// not act on is answered here.
    fn take(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        if self.open {
            self.decoder.push(bytes);
        } else {
            self.hello.extend_from_slice(bytes);
            match handshake::serve(&WIRE, &self.hello) {
                Ok(Step::Need) => return events,
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
                    return events;
                }
                Err(_) => {
                    self.dead = true;
                    return events;
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
                    Message::Ask => events.push(Event::Ask(packet.session)),
                    _ => self.send(&failure(
                        packet.session,
                        ErrorCode::BAD_REQUEST,
                        "a mailbox takes ask and open",
                    )),
                },
                Ok(Some(Incoming::Stream {
                    from: wire::Side::Client,
                    packet,
                })) => events.push(Event::Stream(packet)),
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
        events
    }
}

/// What a question is for once the terminal is found.
#[derive(Clone, Copy)]
enum Then {
    /// Answer the asker, who wanted to know.
    Answer,
    /// Give the app a ticket for the sender's stream of this number.
    Hand(u32),
}

/// A question whose answer waits on a scan of the sessions.
struct Ask {
    /// The connection that asked ([`Sender::id`]).
    sender: u64,
    /// The process that asked, whose terminal is the answer.
    pid: u32,
    scan: Scan,
    then: Then,
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
    /// What the programs and the app are in the middle of.
    conversations: Conversations,
    /// What is for the app: the tickets, and the stream messages of the
    /// conversations, until the bridge takes them ([`Mailbox::drain`]).
    tickets: Vec<Packet>,
    to_app: Vec<wire::Packet>,
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
            conversations: Conversations::new(),
            tickets: Vec::new(),
            to_app: Vec::new(),
            backoff: 0,
        };
        mailbox.listener.set_nonblocking(true)?;
        Ok(mailbox)
    }

    /// The descriptors to wait on. The connections are read only while `reading`
    /// is set: what they say becomes bytes for the app, and the bridge does not
    /// take more of them while it has more than the app is taking already.
    pub fn interest(&self, reading: bool) -> Vec<(libc::pollfd, Token)> {
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
            let mut events = if reading { libc::POLLIN } else { 0 };
            if sender.pending() > 0 {
                events |= libc::POLLOUT;
            }
            if events != 0 {
                set.push((sys::want(sender.sock.as_raw_fd(), events), Token::Sender(i)));
            }
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
                let events = match self.senders.get_mut(i) {
                    Some(sender) => {
                        sender.flush();
                        // A descriptor that was only waited on for writing has
                        // nothing to be read, and is not read when the bridge
                        // would rather it was not.
                        if revents & !libc::POLLOUT != 0 {
                            sender.read()
                        } else {
                            Vec::new()
                        }
                    }
                    None => return,
                };
                for event in events {
                    match event {
                        Event::Ask(terminal) => self.ask(i, terminal, stand),
                        Event::Stream(packet) => self.stream(i, packet, stand),
                    }
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
    /// what is waiting, drops the connections that are gone and what they were
    /// in the middle of.
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
            let (sender, then) = (ask.sender, ask.then);
            let terminal = ask.terminal();
            match then {
                Then::Answer => self.answer(sender, terminal, stand),
                Then::Hand(stream) => self.hand(sender, stream, terminal, stand),
            }
        }
        for sender in &mut self.senders {
            sender.flush();
        }
        self.senders.retain(|sender| !sender.dead);
        let senders = &self.senders;
        self.asks
            .retain(|ask| senders.iter().any(|sender| sender.id == ask.sender));
        let mut said = Said::default();
        self.conversations
            .gone(|id| senders.iter().any(|sender| sender.id == id), &mut said);
        self.say(said);
    }

    /// What is for the app: the tickets (the session channel) and the stream
    /// messages of the conversations, appended to `session` and `streams`.
    pub fn drain(&mut self, session: &mut Vec<Packet>, streams: &mut Vec<wire::Packet>) {
        session.append(&mut self.tickets);
        streams.append(&mut self.to_app);
    }

    /// How many conversations have a stream of the app's, which count against
    /// the limit on the streams it has.
    pub fn joined(&self) -> usize {
        self.conversations.joined()
    }

    /// A stream message from the app. One that opens a stream to answer a
    /// ticket, or is about one a conversation has, is taken here (`held` is how
    /// many other streams the app has, for the limit on them); any other is
    /// given back for the streams the bridge keeps itself.
    pub fn from_app(&mut self, packet: wire::Packet, held: usize) -> Option<wire::Packet> {
        let wire::Packet { stream, message } = packet;
        let mut said = Said::default();
        match message {
            wire::Message::OpenTicket { window, ticket } => {
                self.conversations
                    .answer(stream, window, ticket, held, &mut said);
            }
            message => {
                if !self.conversations.owns(stream) {
                    return Some(wire::Packet { stream, message });
                }
                self.conversations.from_app(stream, message, &mut said);
            }
        }
        self.say(said);
        None
    }

    /// The app refused a ticket, in its own words.
    pub fn decline(&mut self, ticket: u32, text: String) {
        let mut said = Said::default();
        self.conversations.decline(ticket, text, &mut said);
        self.say(said);
    }

    /// Sends what a conversation said to the connections it is for, and keeps
    /// what is for the app.
    fn say(&mut self, said: Said) {
        for (id, packet) in said.to_senders {
            if let Some(sender) = self.senders.iter_mut().find(|s| s.id == id) {
                sender.send_stream(&packet);
            }
        }
        self.tickets.extend(said.tickets);
        self.to_app.extend(said.to_app);
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
            written: 0,
            bulk: VecDeque::new(),
            bulk_bytes: 0,
            highest: 0,
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
                        then: Then::Answer,
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

    /// A message about a stream from the connection at index `i`: an `open` of
    /// no target begins a conversation, and the rest are the sender's side of
    /// one that has.
    fn stream(&mut self, i: usize, packet: wire::Packet, stand: &Stand<'_>) {
        let wire::Packet { stream, message } = packet;
        let Some(sender) = self.senders.get_mut(i) else {
            return;
        };
        let id = sender.id;
        let mut said = Said::default();
        match message {
            wire::Message::Open {
                window,
                port: 0,
                host,
            } if host.is_empty() => {
                if sender.number(stream) {
                    self.begin(i, stream, window, stand, &mut said);
                }
            }
            // A target to connect to or a command to run is not what a mailbox
            // is for.
            wire::Message::Open { .. }
            | wire::Message::OpenCommand { .. }
            | wire::Message::OpenTicket { .. } => {
                if sender.number(stream) {
                    said.failed(
                        id,
                        stream,
                        FailCode::BAD_REQUEST,
                        "a mailbox takes mail and nothing to connect to or run",
                    );
                }
            }
            message @ (wire::Message::Data(_)
            | wire::Message::Credit(_)
            | wire::Message::Eof
            | wire::Message::Close) => {
                self.conversations
                    .from_sender(id, stream, message, &mut said);
            }
            _ => {}
        }
        self.say(said);
    }

    /// The connection at index `i` opened its stream `stream`, to hand bytes to
    /// the app. The app is given a ticket once the terminal the program runs in
    /// is known.
    fn begin(&mut self, i: usize, stream: u32, window: u32, stand: &Stand<'_>, said: &mut Said) {
        let Some(sender) = self.senders.get(i) else {
            return;
        };
        let (id, pid) = (sender.id, sender.pid);
        if !stand(None).tickets {
            return said.failed(id, stream, FailCode::UNREACHABLE, NO_APP);
        }
        if let Err((code, why)) = self.conversations.begin(id, stream, window) {
            return said.failed(id, stream, code, why);
        }
        // A process that cannot be found has no terminal to find.
        let Some(pid) = pid else {
            return self.conversations.located(id, stream, None, said);
        };
        if self.asks.len() >= MAX_ASKS {
            return self.conversations.refuse(
                id,
                stream,
                FailCode::LIMIT,
                "too many questions are waiting",
                said,
            );
        }
        match Scan::start(&self.dir, scan::WAIT) {
            Ok(scan) => self.asks.push(Ask {
                sender: id,
                pid,
                scan,
                then: Then::Hand(stream),
            }),
            Err(e) => {
                self.conversations
                    .refuse(id, stream, FailCode::UNREACHABLE, &e.to_string(), said)
            }
        }
    }

    /// The terminal the sender of conversation `stream` runs in is found: the
    /// app is given the ticket, unless it has stopped taking them since.
    fn hand(&mut self, sender: u64, stream: u32, terminal: Option<SessionId>, stand: &Stand<'_>) {
        let mut said = Said::default();
        if stand(terminal.as_ref()).tickets {
            self.conversations
                .located(sender, stream, terminal, &mut said);
        } else {
            self.conversations
                .refuse(sender, stream, FailCode::UNREACHABLE, NO_APP, &mut said);
        }
        self.say(said);
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
        let tokens: Vec<Token> = mailbox.interest(true).into_iter().map(|(_, t)| t).collect();
        assert_eq!(tokens, [Token::Listener, Token::Sender(0)]);
        drop(mailbox);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_connections_are_not_read_while_the_bridge_would_rather_they_were_not() {
        let dir = scratch("mb-reading");
        let stand = |_: Option<&SessionId>| Standing::default();
        let mut mailbox = Mailbox::open(&dir).unwrap();
        let _program = Program::to(&mut mailbox, &stand);
        let events = |reading| -> Vec<(Token, libc::c_short)> {
            mailbox
                .interest(reading)
                .into_iter()
                .map(|(fd, token)| (token, fd.events))
                .collect()
        };
        assert_eq!(
            events(true),
            [
                (Token::Listener, libc::POLLIN),
                (Token::Sender(0), libc::POLLIN)
            ]
        );
        assert_eq!(
            events(false),
            [(Token::Listener, libc::POLLIN)],
            "nothing waits to be written, so nothing is waited on"
        );
        drop(mailbox);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A connection to a mailbox, as the mailbox holds it, with a peer that does
    /// not read.
    fn silent(mailbox: &mut Mailbox, stand: &Stand<'_>) -> (UnixStream, usize) {
        let peer = UnixStream::connect(&mailbox.path).unwrap();
        mailbox.ready(Token::Listener, libc::POLLIN, stand);
        (peer, mailbox.senders.len() - 1)
    }

    #[test]
    fn the_bytes_of_a_conversation_waiting_for_a_slow_reader_do_not_close_it_but_other_bytes_do() {
        let dir = scratch("mb-bulk");
        let stand = |_: Option<&SessionId>| Standing::default();
        let mut mailbox = Mailbox::open(&dir).unwrap();
        let (_peer, i) = silent(&mut mailbox, &stand);
        let sender = &mut mailbox.senders[i];

        // A window's worth of data far over what a connection may leave unread.
        for _ in 0..(4 * MAX_OUT / wire::MAX_DATA) {
            sender.send_stream(&wire::Packet::new(
                1,
                wire::Message::Data(vec![0; wire::MAX_DATA]),
            ));
        }
        assert!(sender.pending() > 3 * MAX_OUT);
        assert!(!sender.dead, "bytes the window allows are waited for");
        // The frames around them count as what they are.
        sender.send_stream(&wire::Packet::new(1, wire::Message::Credit(1)));
        assert!(!sender.dead);
        for _ in 0..(MAX_OUT / 13 + 1) {
            sender.send_stream(&wire::Packet::new(1, wire::Message::Credit(1)));
        }
        assert!(sender.dead, "answers nobody reads are not waited for");
        drop(mailbox);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_accounting_of_a_conversations_bytes_follows_what_is_written() {
        let dir = scratch("mb-written");
        let stand = |_: Option<&SessionId>| Standing::default();
        let mut mailbox = Mailbox::open(&dir).unwrap();
        let (mut peer, i) = silent(&mut mailbox, &stand);
        let sender = &mut mailbox.senders[i];
        let packet = wire::Packet::new(1, wire::Message::Data(vec![7; 1000]));
        sender.send_stream(&packet);
        sender.send_stream(&packet);
        let size = sender.bulk_bytes / 2;
        assert_eq!(sender.bulk_bytes, 2 * size);
        sender.flush();
        assert_eq!((sender.pending(), sender.bulk_bytes), (0, 0));
        // And what the socket would not take stays counted.
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut got = vec![0u8; 2 * size];
        peer.read_exact(&mut got).unwrap();
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
