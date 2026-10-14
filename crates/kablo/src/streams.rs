//! Streams: TCP connections carried over the connection to a bridge.
//!
//! This is the one engine of both ends. The bridge (the [`Side::Server`]) makes
//! the connection to the target an `open` names; a client (the
//! [`Side::Client`]) hands it a socket it has accepted and has the bridge open
//! the other half. Once a stream is open the two ends are alike, and so is the
//! code: bytes move between a socket and the wire under a **window** each way.
//! Nothing here reads or writes the wire itself. The owner feeds it what
//! arrived ([`Streams::handle`]), asks which descriptors to wait for
//! ([`Streams::interest`]), tells it which are ready ([`Streams::ready`]) and
//! writes what it makes: control packets, in order, into the list it passes
//! in, and `data` frames through [`crate::outbound`].
//!
//! **Windows.** Each end tells the other how many bytes it may send before it
//! has to wait, per stream, and adds to it with `credit` as it takes bytes
//! (the server by writing them to the target's socket, the client by handing
//! them to its own). So an end keeps at most one window of a stream's bytes in
//! memory and never has to stop reading the connection to make room for one
//! stream: a socket that does not take what it is sent holds up its own stream
//! only. Credit is given for bytes that were written, in steps of a quarter of
//! the window, so the other end always has most of its window left and a
//! `credit` is not sent for every small `data`.
//!
//! **What is read when.** A socket is read only while the stream has window to
//! send into and fewer than [`QUEUED_SOFT`] bytes of `data` frames wait for the
//! wire, so a transfer the wire cannot keep up with stops being read instead of
//! being kept. A `data` is made of one read, at most [`MAX_DATA`] bytes: the
//! pieces are small so that frames of other kinds can go between them.
//!
//! **Order.** The `eof` that follows a stream's last `data` is queued with the
//! data frames, behind them; every other control packet goes in the list the
//! owner passed in, which the owner puts in front of the data (a `credit` must
//! not wait for a transfer in the other direction). A `close` takes the
//! stream's queued frames with it.
//!
//! **A stream that is not valid** (data past its window, past its end or
//! before it was opened, a credit past the largest window) is ended with a
//! `close` and the connection goes on. Frames for a stream this end does not
//! have are ignored: an end that sent `close` has frames of the stream still on
//! their way to the other.
//!
//! **Opening** (server end). The target is reached on a thread of its own
//! ([`crate::dial`]), so nothing blocks the loop, and the answer comes back
//! over a descriptor the loop waits on. A stream is connecting for at most the
//! connect timeout; the loop asks for the deadline ([`Streams::deadline`]) and
//! calls [`Streams::expire`], which answers `failed` (timed out) even if the
//! thread is still resolving a name. The thread itself is not interrupted: it
//! counts as connecting until it reports, which is what bounds how many there
//! are.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use kablo_proto::v1::stream::{
    CONNECT_TIMEOUT, FailCode, MAX_CONNECTING, MAX_DATA, MAX_HOST, MAX_STREAMS, MAX_WINDOW,
    Message, Packet, Side,
};
use kablo_proto::v1::{Features, Message as SessionMessage};

use crate::dial::{self, Refusal, Target};
use crate::outbound::Frames;
use crate::sys;

/// Bytes of `data` frames that may wait for the wire before sockets are not
/// read.
pub const QUEUED_SOFT: usize = 4 * MAX_DATA;

/// How a target is reached: its address and the time by which it must be.
/// Tests put their own here.
pub type Dial = Arc<dyn Fn(&Target, Instant) -> Result<TcpStream, Refusal> + Send + Sync>;

/// What an end is willing to do.
#[derive(Clone)]
pub struct Config {
    /// The window offered for each stream: what the other end may send before
    /// it is given credit.
    pub window: u32,
    /// The most streams open at once, the ones still connecting included.
    pub max_streams: usize,
    /// The most being connected at once.
    pub max_connecting: usize,
    /// How long a target has to answer.
    pub connect_timeout: Duration,
    /// How targets are reached.
    pub dial: Dial,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            window: MAX_WINDOW,
            max_streams: MAX_STREAMS as usize,
            max_connecting: MAX_CONNECTING as usize,
            connect_timeout: CONNECT_TIMEOUT,
            dial: Arc::new(dial::connect),
        }
    }
}

/// What a descriptor in the owner's poll set stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// The socket of this stream.
    Stream(u32),
    /// The answers of the threads that connect.
    Connector,
}

/// Something that happened to a stream that the owner may want to say (client
/// end).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The server connected the stream.
    Opened(u32),
    /// The server could not.
    Failed {
        /// The stream.
        stream: u32,
        /// Why.
        code: FailCode,
        /// In a few words.
        text: String,
    },
}

/// One open stream.
struct Stream {
    socket: TcpStream,
    /// Client end: `open` was sent and `opened` has not come.
    opening: bool,
    /// Bytes the other end still lets this end send.
    send_window: u32,
    /// Bytes this end still lets the other end send.
    recv_window: u32,
    /// Bytes received and not yet written to the socket.
    inbox: VecDeque<u8>,
    /// Bytes written to the socket that the other end has not been told of.
    uncredited: u32,
    /// This end has sent `eof`: the socket said it had nothing more.
    sent_eof: bool,
    /// The other end has sent `eof`.
    got_eof: bool,
    /// The writing half of the socket is shut.
    shut: bool,
}

impl Stream {
    fn new(socket: TcpStream, send_window: u32, recv_window: u32, opening: bool) -> Stream {
        Stream {
            socket,
            opening,
            send_window,
            recv_window,
            inbox: VecDeque::new(),
            uncredited: 0,
            sent_eof: false,
            got_eof: false,
            shut: false,
        }
    }

    /// Whether the socket may be read: the stream is open, has window to send
    /// into, and its socket has not said it has nothing more.
    fn can_send(&self) -> bool {
        !self.opening && !self.sent_eof && self.send_window > 0
    }
}

/// Where a connecting stream stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The thread is working and the client has not given up.
    Pending,
    /// The client closed the stream; the result is dropped when it comes.
    Cancelled,
    /// The client has been told it timed out; the result is dropped.
    Expired,
}

struct Connecting {
    /// The window the client offered: what the server may send.
    window: u32,
    deadline: Instant,
    phase: Phase,
}

/// A thread's answer.
struct Done {
    stream: u32,
    result: Result<TcpStream, Refusal>,
}

/// The threads that connect and how they report.
struct Connector {
    sender: Sender<Done>,
    results: Receiver<Done>,
    /// Becomes readable when a thread has reported.
    wake_read: UnixStream,
    wake_write: UnixStream,
}

impl Connector {
    fn new() -> io::Result<Connector> {
        let (sender, results) = mpsc::channel();
        let (wake_read, wake_write) = UnixStream::pair()?;
        wake_read.set_nonblocking(true)?;
        wake_write.set_nonblocking(true)?;
        Ok(Connector {
            sender,
            results,
            wake_read,
            wake_write,
        })
    }

    fn dial(&self, stream: u32, target: Target, deadline: Instant, how: Dial) -> io::Result<()> {
        let sender = self.sender.clone();
        let wake = self.wake_write.try_clone()?;
        std::thread::Builder::new()
            .name("dial".to_owned())
            .spawn(move || {
                let result = how(&target, deadline);
                let _ = sender.send(Done { stream, result });
                // A full pipe means a wake-up is already waiting.
                let _ = (&wake).write(&[1]);
            })
            .map(|_| ())
    }

    /// What the threads have reported. The wake-up is emptied first: a report
    /// is sent before its wake-up is written, so one that arrives after this
    /// leaves its wake-up behind for the next look.
    fn reports(&self) -> Vec<Done> {
        let mut scratch = [0u8; 64];
        while let Ok(n) = (&self.wake_read).read(&mut scratch) {
            if n == 0 {
                break;
            }
        }
        self.results.try_iter().collect()
    }
}

/// What a client concludes about a bridge from the first thing it says after
/// the client asked `features` and, right behind it, `ping`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The bridge takes streams, within these limits.
    Streams(Features),
    /// It does not: it is a bridge that predates streams, or its administrator
    /// has turned them off (and says so in the text).
    Without(Option<String>),
}

/// Judges the first message that came back after `features` and `ping` were
/// sent. A bridge that has streams answers `features` as it reads it, so
/// before the `pong`; one that does not skips a request it does not know and
/// answers the `ping` only. Anything but a `features_reply` first means no
/// streams, an `error` included: the client waits for nothing and never
/// guesses.
pub fn judge(first: &SessionMessage) -> Verdict {
    match first {
        SessionMessage::FeaturesReply(features) if features.streams => {
            Verdict::Streams(features.clone())
        }
        SessionMessage::FeaturesReply(features) => Verdict::Without(features.reason.clone()),
        _ => Verdict::Without(None),
    }
}

/// A frame waiting for the wire.
struct Queued {
    stream: u32,
    frame: Vec<u8>,
}

/// The streams of one connection.
pub struct Streams {
    side: Side,
    config: Config,
    streams: BTreeMap<u32, Stream>,
    /// Client end: the number of the last stream opened.
    last_id: u32,
    /// Server end: the highest number an `open` has had.
    highest: u32,
    connecting: BTreeMap<u32, Connecting>,
    connector: Option<Connector>,
    /// Server end: why streams are off, while they are.
    closed: Option<String>,
    data: VecDeque<Queued>,
    data_bytes: usize,
    /// Streams that have been given bytes (or an `eof`) to write.
    dirty: BTreeSet<u32>,
    events: Vec<Event>,
    scratch: Vec<u8>,
}

/// The least a quarter of `window` and one byte: how many written bytes are
/// kept from the other end before they are given back.
fn credit_step(window: u32) -> u32 {
    (window / 4).max(1)
}

impl Streams {
    /// No streams, for the `side` this end is.
    pub fn new(side: Side, config: Config) -> Streams {
        Streams {
            side,
            config,
            streams: BTreeMap::new(),
            last_id: 0,
            highest: 0,
            connecting: BTreeMap::new(),
            connector: None,
            closed: None,
            data: VecDeque::new(),
            data_bytes: 0,
            dirty: BTreeSet::new(),
            events: Vec::new(),
            scratch: vec![0; MAX_DATA],
        }
    }

    /// The side this end speaks as: the one to encode the packets it makes with.
    pub fn side(&self) -> Side {
        self.side
    }

    /// How many streams there are, the ones still connecting included.
    pub fn len(&self) -> usize {
        self.streams.len() + self.connecting.len()
    }

    /// Whether there are no streams.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Server end: turns streams off, with the reason `open` is answered with
    /// (or on again with `None`). Open streams are not touched.
    pub fn set_closed(&mut self, why: Option<String>) {
        self.closed = why;
    }

    /// What has happened to opened streams since this was last asked.
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.events)
    }

    // --- opening ------------------------------------------------------------

    /// Client end: opens a stream for `socket`, which is to be carried to
    /// `host:port`. The socket is not read until the server has said `opened`.
    /// Returns the stream's number; the `open` is in `out`.
    pub fn open(
        &mut self,
        socket: TcpStream,
        host: &str,
        port: u16,
        out: &mut Vec<Packet>,
    ) -> io::Result<u32> {
        if self.streams.len() >= self.config.max_streams {
            return Err(io::Error::other("too many streams"));
        }
        let id = self
            .last_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("out of stream numbers"))?;
        let packet = Packet::new(
            id,
            Message::Open {
                window: self.config.window,
                port,
                host: host.to_owned(),
            },
        );
        // The writer refuses a target no server could use.
        packet
            .encode(self.side, &mut Vec::new())
            .map_err(io::Error::other)?;
        socket.set_nonblocking(true)?;
        let _ = socket.set_nodelay(true);
        self.last_id = id;
        self.streams
            .insert(id, Stream::new(socket, 0, self.config.window, true));
        out.push(packet);
        Ok(id)
    }

    /// Server end: an `open`.
    fn open_requested(
        &mut self,
        id: u32,
        window: u32,
        port: u16,
        host: String,
        out: &mut Vec<Packet>,
    ) {
        // A number that is not above every one seen is a leftover of a stream
        // that is over, or a mistake: not answered, so that it cannot be taken
        // for the answer to a stream that is not.
        if id <= self.highest {
            return;
        }
        self.highest = id;
        let mut fail = |code: FailCode, text: &str| {
            out.push(Packet::new(
                id,
                Message::Failed {
                    code,
                    text: text.to_owned(),
                },
            ));
        };
        if let Some(why) = &self.closed {
            return fail(FailCode::FORBIDDEN, why);
        }
        if port == 0 || host.is_empty() || host.len() > MAX_HOST || host.contains('\0') {
            return fail(FailCode::BAD_REQUEST, "not a place to connect to");
        }
        if window > MAX_WINDOW {
            return fail(FailCode::LIMIT, "window too large");
        }
        if self.len() >= self.config.max_streams {
            return fail(FailCode::LIMIT, "too many streams");
        }
        if self.connecting.len() >= self.config.max_connecting {
            return fail(FailCode::LIMIT, "too many connections being made");
        }
        if self.connector.is_none() {
            match Connector::new() {
                Ok(connector) => self.connector = Some(connector),
                Err(_) => return fail(FailCode::UNREACHABLE, "cannot start a connection"),
            }
        }
        let deadline = Instant::now() + self.config.connect_timeout;
        let started = match &self.connector {
            Some(connector) => connector.dial(
                id,
                Target { host, port },
                deadline,
                Arc::clone(&self.config.dial),
            ),
            None => Err(io::Error::other("no connector")),
        };
        match started {
            Ok(()) => {
                self.connecting.insert(
                    id,
                    Connecting {
                        window,
                        deadline,
                        phase: Phase::Pending,
                    },
                );
            }
            Err(_) => fail(FailCode::UNREACHABLE, "cannot start a connection"),
        }
    }

    /// A thread reported.
    fn connected(&mut self, done: Done, out: &mut Vec<Packet>) {
        let Some(entry) = self.connecting.remove(&done.stream) else {
            return;
        };
        if entry.phase != Phase::Pending {
            return;
        }
        let id = done.stream;
        let refusal = match done.result {
            Ok(socket) => match socket.set_nonblocking(true) {
                Ok(()) => {
                    let _ = socket.set_nodelay(true);
                    self.streams.insert(
                        id,
                        Stream::new(socket, entry.window, self.config.window, false),
                    );
                    out.push(Packet::new(
                        id,
                        Message::Opened {
                            window: self.config.window,
                        },
                    ));
                    return;
                }
                Err(_) => Refusal::new(FailCode::UNREACHABLE, "could not connect"),
            },
            Err(refusal) => refusal,
        };
        out.push(Packet::new(
            id,
            Message::Failed {
                code: refusal.code,
                text: refusal.text,
            },
        ));
    }

    /// Server end: the earliest time a stream that is connecting runs out of
    /// time.
    pub fn deadline(&self) -> Option<Instant> {
        self.connecting
            .values()
            .filter(|c| c.phase == Phase::Pending)
            .map(|c| c.deadline)
            .min()
    }

    /// Server end: answers `failed` (timed out) for every stream that has
    /// been connecting past its deadline.
    pub fn expire(&mut self, now: Instant, out: &mut Vec<Packet>) {
        for (&id, entry) in &mut self.connecting {
            if entry.phase == Phase::Pending && entry.deadline <= now {
                entry.phase = Phase::Expired;
                out.push(Packet::new(
                    id,
                    Message::Failed {
                        code: FailCode::TIMED_OUT,
                        text: "timed out".to_owned(),
                    },
                ));
            }
        }
    }

    // --- what arrives -------------------------------------------------------

    /// A message about a stream that arrived from `from`. What this end says
    /// in answer (`opened`, `failed`, `credit`, `close`) is pushed to `out`.
    ///
    /// Bytes for a socket are only queued here; [`Streams::settle`] writes them,
    /// so a window is judged against what has been received and not yet
    /// credited, however the frames came.
    pub fn handle(&mut self, from: Side, packet: Packet, out: &mut Vec<Packet>) {
        // A message of this end's own kind can only be a mistake.
        if from == self.side {
            return;
        }
        let Packet {
            stream: id,
            message,
        } = packet;
        match (self.side, message) {
            (Side::Server, Message::Open { window, port, host }) => {
                self.open_requested(id, window, port, host, out);
            }
            (Side::Client, Message::Opened { window }) => self.opened(id, window, out),
            (Side::Client, Message::Failed { code, text }) => self.failed(id, code, text),
            (_, Message::Data(bytes)) => self.data_in(id, bytes, out),
            (_, Message::Credit(bytes)) => self.credit_in(id, bytes, out),
            (_, Message::Eof) => self.eof_in(id, out),
            (_, Message::Close) => self.close_in(id),
            // `open` going to a client, `opened` or `failed` to a server.
            _ => {}
        }
    }

    fn opened(&mut self, id: u32, window: u32, out: &mut Vec<Packet>) {
        let known = match self.streams.get_mut(&id) {
            Some(stream) if stream.opening => {
                stream.opening = false;
                stream.send_window = window.min(MAX_WINDOW);
                true
            }
            // `opened` for a stream that is open or was never opened.
            Some(_) => {
                self.reset(id, out);
                return;
            }
            None => false,
        };
        if known {
            self.events.push(Event::Opened(id));
        }
    }

    fn failed(&mut self, id: u32, code: FailCode, text: String) {
        let opening = self.streams.get(&id).is_some_and(|s| s.opening);
        if opening {
            self.forget(id);
            self.events.push(Event::Failed {
                stream: id,
                code,
                text,
            });
        }
    }

    fn data_in(&mut self, id: u32, bytes: Vec<u8>, out: &mut Vec<Packet>) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        let size = bytes.len();
        if stream.opening
            || stream.got_eof
            || size > MAX_DATA
            || u64::try_from(size).unwrap_or(u64::MAX) > u64::from(stream.recv_window)
        {
            self.reset(id, out);
            return;
        }
        stream.recv_window -= u32::try_from(size).unwrap_or(0);
        stream.inbox.extend(bytes);
        self.dirty.insert(id);
    }

    fn credit_in(&mut self, id: u32, bytes: u32, out: &mut Vec<Packet>) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        match stream.send_window.checked_add(bytes) {
            Some(window) if !stream.opening && window <= MAX_WINDOW => stream.send_window = window,
            _ => self.reset(id, out),
        }
    }

    fn eof_in(&mut self, id: u32, out: &mut Vec<Packet>) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        if stream.opening {
            self.reset(id, out);
            return;
        }
        stream.got_eof = true;
        self.dirty.insert(id);
    }

    fn close_in(&mut self, id: u32) {
        if let Some(entry) = self.connecting.get_mut(&id) {
            entry.phase = Phase::Cancelled;
            return;
        }
        self.forget(id);
    }

    // --- sockets ------------------------------------------------------------

    /// The descriptors to wait for, and what each stands for.
    pub fn interest(&self) -> Vec<(libc::pollfd, Token)> {
        let mut found = Vec::new();
        for (&id, stream) in &self.streams {
            let mut events = 0;
            if !stream.inbox.is_empty() {
                events |= libc::POLLOUT;
            }
            if self.may_read(stream) {
                events |= libc::POLLIN;
            }
            if events != 0 {
                found.push((
                    sys::want(stream.socket.as_raw_fd(), events),
                    Token::Stream(id),
                ));
            }
        }
        let waiting = !self.connecting.is_empty();
        if let Some(connector) = self.connector.as_ref().filter(|_| waiting) {
            found.push((
                sys::want(connector.wake_read.as_raw_fd(), libc::POLLIN),
                Token::Connector,
            ));
        }
        found
    }

    fn may_read(&self, stream: &Stream) -> bool {
        stream.can_send() && self.data_bytes < QUEUED_SOFT
    }

    /// A descriptor from [`Streams::interest`] is ready.
    pub fn ready(&mut self, token: Token, revents: libc::c_short, out: &mut Vec<Packet>) {
        match token {
            Token::Connector => {
                let reports = match &self.connector {
                    Some(connector) => connector.reports(),
                    None => return,
                };
                for done in reports {
                    self.connected(done, out);
                }
            }
            Token::Stream(id) => {
                let broken = libc::POLLHUP | libc::POLLERR;
                if revents & (libc::POLLOUT | broken) != 0 {
                    self.drain(id, out);
                }
                if revents & (libc::POLLIN | broken) != 0 {
                    self.fill(id, out);
                }
            }
        }
    }

    /// Writes what has been received to the sockets that will take it. Called
    /// once the frames of a turn have been handled.
    pub fn settle(&mut self, out: &mut Vec<Packet>) {
        for id in std::mem::take(&mut self.dirty) {
            self.drain(id, out);
        }
    }

    /// Writes a stream's inbox to its socket as far as it goes, gives credit
    /// for what went, and shuts the socket's writing half once the other end
    /// has said it is done and everything is written.
    fn drain(&mut self, id: u32, out: &mut Vec<Packet>) {
        let step = credit_step(self.config.window);
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        let mut broken = false;
        while !stream.inbox.is_empty() {
            let (front, _) = stream.inbox.as_slices();
            match stream.socket.write(front) {
                Ok(0) => break,
                Ok(n) => {
                    stream.inbox.drain(..n);
                    stream.uncredited = stream
                        .uncredited
                        .saturating_add(u32::try_from(n).unwrap_or(u32::MAX));
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    broken = true;
                    break;
                }
            }
        }
        if broken {
            self.reset(id, out);
            return;
        }
        if stream.uncredited >= step {
            let bytes = std::mem::take(&mut stream.uncredited);
            stream.recv_window = stream.recv_window.saturating_add(bytes);
            out.push(Packet::new(id, Message::Credit(bytes)));
        }
        if stream.got_eof && stream.inbox.is_empty() && !stream.shut {
            let _ = stream.socket.shutdown(Shutdown::Write);
            stream.shut = true;
        }
        self.finish(id);
    }

    /// Reads a stream's socket once, if the stream has window to send into and
    /// the wire is not far behind, and queues what it said.
    fn fill(&mut self, id: u32, out: &mut Vec<Packet>) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        if !stream.can_send() || self.data_bytes >= QUEUED_SOFT {
            return;
        }
        let want = usize::try_from(stream.send_window)
            .unwrap_or(MAX_DATA)
            .min(MAX_DATA);
        let Some(buf) = self.scratch.get_mut(..want) else {
            return;
        };
        let read = loop {
            match stream.socket.read(buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => break other,
            }
        };
        match read {
            Ok(0) => {
                stream.sent_eof = true;
                if !self.queue(id, Message::Eof) {
                    self.reset(id, out);
                    return;
                }
                self.finish(id);
            }
            Ok(n) => {
                stream.send_window -= u32::try_from(n).unwrap_or(0);
                let bytes = buf.get(..n).unwrap_or_default().to_vec();
                if !self.queue(id, Message::Data(bytes)) {
                    self.reset(id, out);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => self.reset(id, out),
        }
    }

    /// A stream both ends have said all they have to say on, and whose socket
    /// has been told so, is over.
    fn finish(&mut self, id: u32) {
        let over = self
            .streams
            .get(&id)
            .is_some_and(|s| s.sent_eof && s.got_eof && s.shut);
        if over {
            // Its `eof` may still be queued; that is the point of keeping the
            // frames.
            self.streams.remove(&id);
            self.dirty.remove(&id);
        }
    }

    /// Ends a stream that cannot go on, and says so.
    fn reset(&mut self, id: u32, out: &mut Vec<Packet>) {
        self.forget(id);
        out.push(Packet::new(id, Message::Close));
    }

    /// Drops a stream and everything queued for it.
    fn forget(&mut self, id: u32) {
        self.streams.remove(&id);
        self.dirty.remove(&id);
        self.data.retain(|queued| queued.stream != id);
        self.data_bytes = self.data.iter().map(|queued| queued.frame.len()).sum();
    }

    /// Queues a frame of `data` or `eof` for the wire.
    fn queue(&mut self, id: u32, message: Message) -> bool {
        let mut frame = Vec::new();
        if Packet::new(id, message)
            .encode(self.side, &mut frame)
            .is_err()
        {
            return false;
        }
        self.data_bytes += frame.len();
        self.data.push_back(Queued { stream: id, frame });
        true
    }
}

impl Frames for Streams {
    fn next_len(&self) -> Option<usize> {
        self.data.front().map(|queued| queued.frame.len())
    }

    fn take(&mut self) -> Option<Vec<u8>> {
        let queued = self.data.pop_front()?;
        self.data_bytes = self.data_bytes.saturating_sub(queued.frame.len());
        Some(queued.frame)
    }
}

#[cfg(test)]
mod tests;
