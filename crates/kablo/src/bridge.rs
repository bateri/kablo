//! `kablo attach`: the bridge between one client and every session on this
//! machine.
//!
//! The bridge speaks the public protocol on its standard input and output,
//! which is how a client reaches it over `ssh host kablo attach`: one login,
//! one command, and every session on the server travels in it, told apart by
//! the session id each frame carries. (Opening a connection per session would
//! meet the server's limit on sessions per connection, ten by default.)
//!
//! It is a router and nothing else. A request for a session goes to that
//! session's relay over the relay's own socket; what the relay sends comes back
//! to the client as it is. The program's bytes are not looked at, and the bridge
//! keeps no copy of them: a client that comes back asks for its offset again.
//! Three things it answers itself, because they are about the connection and
//! not a session: `ping`; `list`, by asking every relay for its status; and
//! `new`, by starting the session as `kablo new` does and answering with the
//! new session's status.
//!
//! A relay that dies or is replaced while a client is attached does not show:
//! see [`crate::link`]. A client that goes away (the standard input ends) ends
//! the bridge and nothing else; no session is closed by it.
//!
//! The bridge never blocks on a stalled peer. It stops reading relays while the
//! client is not taking what it already has, so a slow connection holds up its
//! own sessions only, and a relay that is not read from drops the bridge's
//! connection and gets it back with the offset where it left off.

use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::time::Instant;

use kablo_proto::v1::{
    ErrorCode, MAX_COLS, MAX_HISTORY, MAX_ROWS, Message, NewSession, Packet, PacketDecoder,
};
use kablo_proto::{SessionId, WIRE};

use crate::handshake::{self, Step};
use crate::launch::{self, Failure};
use crate::link::Link;
use crate::scan::{self, Scan};
use crate::{journal, sockdir, sys};

/// Unsent bytes for the client above which relays are not read.
const OUT_SOFT: usize = 1 << 20;

/// Unsent bytes for the client above which the client is not read either.
const OUT_HARD: usize = 8 << 20;

/// Bytes read from the client at a time.
const READ_CHUNK: usize = 64 * 1024;

/// What a descriptor in the poll set stands for.
enum Source {
    Client,
    Out,
    Link(usize),
    Probe(usize, usize),
}

/// Runs the bridge on standard input and output in socket directory `dir`.
/// Returns the process's exit status.
pub fn run(dir: PathBuf) -> u8 {
    // SAFETY: descriptors 0 and 1 are open for the life of the process; the
    // bridge reads and writes them with plain calls and never closes them.
    let (input, output) = unsafe { (BorrowedFd::borrow_raw(0), BorrowedFd::borrow_raw(1)) };
    for fd in [input, output] {
        // A terminal is shared with whatever started us; a pipe or socket is
        // ours alone. Only the latter is put in non-blocking mode (one that
        // will not take it is written in small pieces below).
        if !sys::is_tty(fd) {
            let _ = sys::set_nonblocking(fd.as_raw_fd());
        }
    }
    let chunk = if sys::is_tty(output) { 512 } else { READ_CHUNK };
    Bridge::new(dir, input, output, chunk).serve()
}

struct Bridge {
    dir: PathBuf,
    input: BorrowedFd<'static>,
    output: BorrowedFd<'static>,
    /// The most bytes written to the client at a time.
    chunk: usize,
    /// Bytes received before the handshake is complete.
    hello: Vec<u8>,
    open: bool,
    decoder: PacketDecoder,
    out: Vec<u8>,
    sent: usize,
    links: Vec<Link>,
    scans: Vec<Scan>,
    stop: Option<u8>,
    read_buf: Vec<u8>,
}

impl Bridge {
    fn new(
        dir: PathBuf,
        input: BorrowedFd<'static>,
        output: BorrowedFd<'static>,
        chunk: usize,
    ) -> Bridge {
        Bridge {
            dir,
            input,
            output,
            chunk,
            hello: Vec::new(),
            open: false,
            decoder: PacketDecoder::new(),
            out: Vec::new(),
            sent: 0,
            links: Vec::new(),
            scans: Vec::new(),
            stop: None,
            read_buf: vec![0u8; READ_CHUNK],
        }
    }

    fn pending(&self) -> usize {
        self.out.len().saturating_sub(self.sent)
    }

    fn serve(&mut self) -> u8 {
        loop {
            if let Some(code) = self.stop {
                return code;
            }
            let mut set = Vec::new();
            let mut sources = Vec::new();
            if self.pending() < OUT_HARD {
                set.push(sys::want(self.input.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Client);
            }
            if self.pending() > 0 {
                set.push(sys::want(self.output.as_raw_fd(), libc::POLLOUT));
                sources.push(Source::Out);
            }
            let reading = self.pending() < OUT_SOFT;
            for (i, link) in self.links.iter().enumerate() {
                if let Some(fd) = link.pollfd(reading) {
                    set.push(fd);
                    sources.push(Source::Link(i));
                }
            }
            for (s, scan) in self.scans.iter().enumerate() {
                for (p, fd) in scan.interest() {
                    set.push(fd);
                    sources.push(Source::Probe(s, p));
                }
            }
            let wait = self
                .next_deadline()
                .map(|at| at.saturating_duration_since(Instant::now()));
            if sys::poll(&mut set, wait).is_err() {
                return 1;
            }
            let now = Instant::now();
            let mut outgoing = Vec::new();
            for (fd, source) in set.iter().zip(&sources) {
                if fd.revents == 0 {
                    continue;
                }
                match source {
                    Source::Client => self.read_client(now, &mut outgoing),
                    Source::Out => {}
                    Source::Link(i) => {
                        if let Some(link) = self.links.get_mut(*i) {
                            link.ready(fd.revents, now, &mut outgoing);
                        }
                    }
                    Source::Probe(s, p) => {
                        if let Some(scan) = self.scans.get_mut(*s) {
                            scan.ready(*p, fd.revents);
                        }
                    }
                }
            }
            for link in &mut self.links {
                link.tick(now, &mut outgoing);
            }
            for scan in &mut self.scans {
                scan.expire(now);
            }
            self.finish_scans(&mut outgoing);
            self.links.retain(|link| !link.is_gone());
            for packet in &outgoing {
                self.emit(packet);
            }
            if !self.flush() && self.stop.is_none() {
                // The client's end is gone; so is the reason to be here.
                self.stop = Some(0);
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.links
            .iter()
            .filter_map(Link::deadline)
            .chain(self.scans.iter().filter_map(Scan::deadline))
            .min()
    }

    fn emit(&mut self, packet: &Packet) {
        let _ = packet.encode(&mut self.out);
    }

    fn flush(&mut self) -> bool {
        while self.pending() > 0 {
            let end = (self.sent + self.chunk).min(self.out.len());
            let waiting = self.out.get(self.sent..end).unwrap_or_default();
            match sys::write_fd(self.output, waiting) {
                Ok(0) => break,
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(_) => return false,
            }
        }
        if self.sent >= self.out.len() {
            self.out.clear();
            self.sent = 0;
        } else if self.sent >= 256 * 1024 {
            self.out.drain(..self.sent);
            self.sent = 0;
        }
        true
    }

    fn finish_scans(&mut self, outgoing: &mut Vec<Packet>) {
        let (done, running): (Vec<Scan>, Vec<Scan>) = std::mem::take(&mut self.scans)
            .into_iter()
            .partition(Scan::is_finished);
        self.scans = running;
        for scan in done {
            outgoing.push(Packet::connection(Message::ListReply(scan.into_statuses())));
        }
    }

    // --- the client ---------------------------------------------------------

    fn read_client(&mut self, now: Instant, outgoing: &mut Vec<Packet>) {
        let mut buf = std::mem::take(&mut self.read_buf);
        match sys::read_fd(self.input, &mut buf) {
            // Nobody is there any more.
            Ok(0) => self.stop = Some(0),
            Ok(n) => self.feed(buf.get(..n).unwrap_or_default(), now, outgoing),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => self.stop = Some(0),
        }
        self.read_buf = buf;
    }

    /// Bytes arrived from the client: the handshake first, then packets.
    fn feed(&mut self, bytes: &[u8], now: Instant, outgoing: &mut Vec<Packet>) {
        if self.open {
            self.decoder.push(bytes);
        } else {
            self.hello.extend_from_slice(bytes);
            match handshake::serve(&WIRE, &self.hello) {
                Ok(Step::Need) => return,
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
                    let _ = self.flush();
                    self.stop = Some(1);
                    return;
                }
                Err(_) => {
                    self.stop = Some(1);
                    return;
                }
            }
        }
        loop {
            match self.decoder.next_packet() {
                Ok(Some(packet)) => self.handle(packet, now, outgoing),
                Ok(None) => return,
                Err(e) if e.is_recoverable() => {
                    outgoing.push(Packet::connection(Message::Error {
                        code: ErrorCode::BAD_REQUEST,
                        text: "malformed message".to_owned(),
                    }));
                }
                // The framing itself is broken: there is no telling where the
                // next frame starts.
                Err(_) => {
                    self.stop = Some(1);
                    return;
                }
            }
        }
    }

    fn handle(&mut self, packet: Packet, now: Instant, outgoing: &mut Vec<Packet>) {
        let Packet { session, message } = packet;
        match (session, message) {
            (session, Message::Ping(bytes)) => outgoing.push(Packet {
                session,
                message: Message::Pong(bytes),
            }),
            (_, Message::List) => match Scan::start(&self.dir, scan::WAIT) {
                Ok(scan) => self.scans.push(scan),
                Err(e) => outgoing.push(failure(None, ErrorCode::UNAVAILABLE, &e.to_string())),
            },
            (session, Message::New(request)) => self.create(session, &request, outgoing),
            (Some(id), message) if is_request(&message) => {
                self.route(id, message, now, outgoing);
            }
            (None, message) if is_request(&message) => outgoing.push(failure(
                None,
                ErrorCode::BAD_REQUEST,
                "this request needs a session id",
            )),
            (session, _) => {
                outgoing.push(failure(session, ErrorCode::BAD_REQUEST, "not a request"))
            }
        }
    }

    /// Sends a request for session `id` to its relay, connecting first if this
    /// is the first one.
    fn route(&mut self, id: SessionId, message: Message, now: Instant, outgoing: &mut Vec<Packet>) {
        let existing = self
            .links
            .iter()
            .position(|link| link.id() == &id && !link.is_gone());
        let index = match existing {
            Some(index) => index,
            None => {
                match message {
                    // Nothing is attached, so there is nothing to stop.
                    Message::Detach => return,
                    // The relay would say the same; no connection is needed
                    // to hear it.
                    Message::Input(_) | Message::Resize(_) => {
                        return outgoing.push(failure(
                            Some(id),
                            ErrorCode::NOT_ATTACHED,
                            "attach first",
                        ));
                    }
                    _ => {}
                }
                match Link::open(&self.dir, id.clone(), now) {
                    Ok(link) => {
                        self.links.push(link);
                        self.links.len() - 1
                    }
                    Err(packet) => return outgoing.push(*packet),
                }
            }
        };
        if let Some(link) = self.links.get_mut(index) {
            link.send(Packet::about(id, message), now, outgoing);
        }
    }

    /// Starts a session, as `kablo new` does, and says how it stands.
    fn create(
        &mut self,
        wanted: Option<SessionId>,
        request: &NewSession,
        outgoing: &mut Vec<Packet>,
    ) {
        // A size the journal cannot have is a bad request, not a server that
        // is busy: asking again will not help, and starting a keeper to find
        // that out would leave nothing but its refusal.
        if request
            .journal_size
            .is_some_and(|size| !(journal::MIN_CAPACITY..=journal::MAX_CAPACITY).contains(&size))
        {
            return outgoing.push(failure(
                wanted,
                ErrorCode::BAD_REQUEST,
                &format!(
                    "the journal holds {} to {} bytes",
                    journal::MIN_CAPACITY,
                    journal::MAX_CAPACITY
                ),
            ));
        }
        if request.size.is_some_and(|size| !size.fits()) {
            return outgoing.push(failure(
                wanted,
                ErrorCode::BAD_REQUEST,
                &format!("a terminal has at most {MAX_COLS} columns and {MAX_ROWS} rows"),
            ));
        }
        if request.history.is_some_and(|rows| rows > MAX_HISTORY) {
            return outgoing.push(failure(
                wanted,
                ErrorCode::BAD_REQUEST,
                &format!("the screen keeps 0 to {MAX_HISTORY} rows of scrollback"),
            ));
        }
        // A name the server makes can collide with a session that is there;
        // one the client chose is the client's to change.
        let tries = if wanted.is_some() { 1 } else { 3 };
        for _ in 0..tries {
            let id = match wanted.clone().map(Ok).unwrap_or_else(launch::fresh_id) {
                Ok(id) => id,
                Err(e) => {
                    return outgoing.push(failure(
                        None,
                        ErrorCode::UNAVAILABLE,
                        &format!("cannot make a session id: {e}"),
                    ));
                }
            };
            match launch::launch(&self.dir, &id, request) {
                Ok(()) => return outgoing.push(created(&self.dir, id)),
                Err(Failure::Exists(taken)) => {
                    if wanted.is_some() {
                        return outgoing.push(failure(
                            Some(taken.clone()),
                            ErrorCode::SESSION_EXISTS,
                            &Failure::Exists(taken).to_string(),
                        ));
                    }
                }
                Err(Failure::Other(why)) => {
                    return outgoing.push(failure(wanted.clone(), ErrorCode::UNAVAILABLE, &why));
                }
            }
        }
        outgoing.push(failure(
            None,
            ErrorCode::UNAVAILABLE,
            "no unused session id could be made",
        ));
    }
}

/// The answer to a `new` that worked: the new session's status.
fn created(dir: &std::path::Path, id: SessionId) -> Packet {
    let found = Scan::of(dir, vec![id.clone()], scan::WAIT).run();
    match found.into_iter().next() {
        Some(status) => Packet::about(id, Message::Created(status)),
        None => failure(
            Some(id),
            ErrorCode::UNAVAILABLE,
            "the session started but does not answer",
        ),
    }
}

/// Whether a client may send this message to a session.
fn is_request(message: &Message) -> bool {
    matches!(
        message,
        Message::Attach { .. }
            | Message::Detach
            | Message::Resize(_)
            | Message::Terminate { .. }
            | Message::Upgrade { .. }
            | Message::Status
            | Message::Input(_)
    )
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

/// Makes sure the socket directory is safe to use if it is there.
pub fn check_dir(dir: &std::path::Path) -> std::io::Result<()> {
    match sockdir::check_dir(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}
