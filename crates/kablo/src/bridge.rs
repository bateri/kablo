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
//! Four things it answers itself, because they are about the connection and
//! not a session: `ping`, unless it names a session the client has a link to (the
//! relay answers that one, after what was written to it: it is how a client
//! learns that keys were typed); `list`, by asking every relay for its status;
//! `new`, by starting the session as `kablo new` does and answering with the
//! new session's status; and `label`, by writing the session's label file.
//!
//! A relay that dies or is replaced while a client is attached does not show:
//! see [`crate::link`]. A client that goes away (the standard input ends) ends
//! the bridge and nothing else; no session is closed by it.
//!
//! The bridge never blocks on a stalled peer. It stops reading relays while the
//! client is not taking what it already has, so a slow connection holds up its
//! own sessions only, and a relay that is not read from drops the bridge's
//! connection and gets it back with the offset where it left off.
//!
//! It also connects to TCP ports for the client (see [`crate::streams`]), which
//! is what `ssh -L` does without asking sshd for anything. Those connections
//! last as long as the client's does, and what they carry shares the pipe with
//! the sessions without starving either (see [`crate::outbound`]). A server's
//! administrator can turn them off by putting a file in [`NO_FORWARDING`]; the
//! file is looked at again for every `open`, so the decision reaches a bridge
//! that is already running.
//!
//! A program on the server can hand bytes to the app the bridge carries. The
//! bridge keeps a mailbox for that, a socket of its own beside the sessions' (see
//! [`crate::mailbox`]): a program connects to it, asks where the app stands, and
//! opens a stream; the app is given a ticket for the stream, which says the
//! terminal the program runs in, and answers it by opening a stream of its own.
//! The bridge joins the two ([`crate::conversation`]) and passes what each
//! sends on to the other, without looking at it and without keeping any: the
//! windows are the two ends' own. What it needs to say of the app is kept here,
//! since only the bridge sees what its client does: when the client last
//! attached to a session or typed into one, and whether it takes tickets.
//!
//! The same streams run programs for the client, as `ssh host command` does
//! over a login that is already there. Those are children of the bridge and
//! last as long as the connection: when the client goes away they are told to
//! end, and the bridge leaves once they have. The administrator's file does not
//! touch them, since whoever may log in may run a program. (A bridge that is
//! killed rather than asked to stop leaves its programs behind, as any process
//! leaves what it started.)

use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::Instant;

use kablo_proto::v1::stream::{self as wire, Incoming, IncomingDecoder};
use kablo_proto::v1::{
    ErrorCode, Features, MAX_COLS, MAX_HISTORY, MAX_ROWS, Message, NewSession, Packet, Standing,
};
use kablo_proto::{SessionId, WIRE};

use crate::handshake::{self, Step};
use crate::launch::{self, Failure};
use crate::link::Link;
use crate::mailbox::{self, Mailbox};
use crate::outbound::{self, Outbound};
use crate::peer;
use crate::scan::{self, Scan};
use crate::streams::{self, Streams};
use crate::{journal, sockdir, sys};

/// The file whose presence turns streams off. Its contents are not looked at.
pub const NO_FORWARDING: &str = "/etc/kablo/no-forwarding";

/// What a client is told when [`NO_FORWARDING`] is there.
pub const FORWARDING_OFF: &str = "turned off by the server's administrator";

/// What a client is told when [`NO_FORWARDING`] cannot be looked at (see
/// [`forwarding_off`]).
pub const FORWARDING_UNCHECKED: &str = "the server's administrator switch cannot be checked";

/// Unsent bytes for the client above which relays are not read.
const OUT_SOFT: usize = 1 << 20;

/// Unsent bytes for the client above which the client is not read either.
const OUT_HARD: usize = outbound::HARD;

/// Bytes read from the client at a time.
const READ_CHUNK: usize = outbound::CHUNK;

/// What a descriptor in the poll set stands for.
enum Source {
    Client,
    Out,
    Link(usize),
    Probe(usize, usize),
    Stream(streams::Token),
    Mail(mailbox::Token),
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
    /// Bytes received before the handshake is complete.
    hello: Vec<u8>,
    open: bool,
    decoder: IncomingDecoder,
    out: Outbound,
    streams: Streams,
    /// What the streams said this turn that is not `data`.
    stream_out: Vec<wire::Packet>,
    links: Vec<Link>,
    scans: Vec<Scan>,
    /// The bridge's mailbox, if it could be opened.
    mailbox: Option<Mailbox>,
    /// When the client last used any session: attached to it or typed into it.
    /// A bridge is started by a client that has just connected, and that is a
    /// use, so this starts as the start.
    used: Instant,
    /// Whether the client takes tickets for the mail that comes to the mailbox
    /// (it said so in its `features`). A client that has not said so is given
    /// none.
    tickets: bool,
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
            mailbox: Mailbox::open(&dir).ok(),
            used: Instant::now(),
            tickets: false,
            dir,
            input,
            output,
            hello: Vec::new(),
            open: false,
            decoder: IncomingDecoder::new(),
            out: Outbound::new(chunk),
            streams: Streams::new(wire::Side::Server, streams::Config::default()),
            stream_out: Vec::new(),
            links: Vec::new(),
            scans: Vec::new(),
            stop: None,
            read_buf: vec![0u8; READ_CHUNK],
        }
    }

    fn pending(&self) -> usize {
        self.out.pending()
    }

    /// The answer to a `features`.
    fn features_reply(&self) -> Packet {
        Packet::connection(Message::FeaturesReply(features(
            Path::new(NO_FORWARDING),
            self.mailbox.is_some(),
        )))
    }

    fn serve(&mut self) -> u8 {
        let code = self.turns();
        // The mail is for the client, and the client has gone: the programs
        // that were handing it over see their connections end.
        self.mailbox = None;
        // Programs run for the client last as long as the client's connection.
        self.streams.shutdown();
        code
    }

    fn turns(&mut self) -> u8 {
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
            for (fd, token) in self.streams.interest() {
                set.push(fd);
                sources.push(Source::Stream(token));
            }
            if let Some(mailbox) = &self.mailbox {
                // What the programs send is for the client, so they are not
                // read while it has more than it is taking.
                for (fd, token) in mailbox.interest(reading) {
                    set.push(fd);
                    sources.push(Source::Mail(token));
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
                    Source::Stream(token) => {
                        self.streams.ready(*token, fd.revents, &mut self.stream_out);
                    }
                    Source::Mail(token) => {
                        if let Some(mailbox) = self.mailbox.as_mut() {
                            let standings =
                                Standings::new(&self.links, self.used, self.tickets, now);
                            mailbox.ready(*token, fd.revents, &|t| standings.of(t));
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
            if let Some(mailbox) = self.mailbox.as_mut() {
                let standings = Standings::new(&self.links, self.used, self.tickets, now);
                mailbox.settle(now, &|t| standings.of(t));
                // The tickets are said on the session channel with the rest;
                // the streams' messages follow the other streams'.
                mailbox.drain(&mut outgoing, &mut self.stream_out);
            }
            self.streams.settle(&mut self.stream_out);
            self.streams.expire(now, &mut self.stream_out);
            for packet in &outgoing {
                self.emit(packet);
            }
            for packet in std::mem::take(&mut self.stream_out) {
                let _ = packet.encode(wire::Side::Server, self.out.buffer());
            }
            // A stream's frame goes in behind the sessions' bytes every turn,
            // unless the client is so far behind that nothing is read from it
            // either.
            self.out.end_turn(&mut self.streams);
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
            .chain(self.streams.deadline())
            .chain(self.mailbox.as_ref().and_then(Mailbox::deadline))
            .min()
    }

    fn emit(&mut self, packet: &Packet) {
        let _ = packet.encode(self.out.buffer());
    }

    fn flush(&mut self) -> bool {
        let output = self.output;
        self.out
            .flush(&mut self.streams, &mut |bytes| sys::write_fd(output, bytes))
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
                    self.out.push(&reply);
                    let rest = self.hello.get(consumed..).unwrap_or_default().to_vec();
                    self.decoder.push(&rest);
                    self.hello = Vec::new();
                    self.open = true;
                }
                Ok(Step::Refused { reply }) => {
                    self.out.push(&reply);
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
            match self.decoder.next_incoming() {
                Ok(Some(Incoming::Session(packet))) => self.handle(packet, now, outgoing),
                Ok(Some(Incoming::Stream { from, packet })) => self.handle_stream(from, packet),
                // A channel this version does not have: skipped, as the decoder
                // skips a frame it cannot read.
                Ok(Some(_)) => {}
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
            // A ping that names a session a `write` of the client's is waiting to
            // be confirmed at goes the way the `write` did, so that its answer comes
            // after the error the `write` has, if it has one. Any other ping is the
            // bridge's own, as it always was: a pulse is answered whether or not a
            // relay is being replaced.
            (Some(id), Message::Ping(bytes)) if self.is_confirming(&id) => {
                self.route(id, Message::Ping(bytes), now, outgoing);
            }
            (session, Message::Ping(bytes)) => outgoing.push(Packet {
                session,
                message: Message::Pong(bytes),
            }),
            (_, Message::Features) => outgoing.push(self.features_reply()),
            // The body of a `features` says what the client takes. A `features`
            // without one asks and says nothing of it.
            (_, Message::FeaturesWith(offers)) => {
                self.tickets = offers.tickets;
                outgoing.push(self.features_reply());
            }
            (_, Message::Decline { ticket, text }) => {
                if let Some(mailbox) = self.mailbox.as_mut() {
                    mailbox.decline(ticket, text);
                }
            }
            (_, Message::List) => match Scan::start(&self.dir, scan::WAIT) {
                Ok(scan) => self.scans.push(scan),
                Err(e) => outgoing.push(failure(None, ErrorCode::UNAVAILABLE, &e.to_string())),
            },
            (session, Message::New(request)) => self.create(session, &request, outgoing),
            (Some(id), Message::Label(label)) => self.label(id, &label, outgoing),
            (None, Message::Label(_)) => outgoing.push(failure(
                None,
                ErrorCode::BAD_REQUEST,
                "this request needs a session id",
            )),
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

    /// A message about a stream. Whether streams are on is asked again for
    /// every `open`, so that an administrator's decision reaches streams
    /// opened after it.
    fn handle_stream(&mut self, from: wire::Side, packet: wire::Packet) {
        let mut packet = packet;
        if let (wire::Side::Client, Some(mailbox)) = (from, self.mailbox.as_mut()) {
            // An `open` that answers a ticket is numbered among the streams
            // like any other, and the streams it makes count with theirs.
            if matches!(packet.message, wire::Message::OpenTicket { .. })
                && !self.streams.reserve(packet.stream)
            {
                return;
            }
            match mailbox.from_app(packet, self.streams.len()) {
                None => return,
                Some(not_mail) => packet = not_mail,
            }
            self.streams.hold(mailbox.joined());
        }
        if matches!(
            packet.message,
            wire::Message::Open { .. } | wire::Message::OpenTicket { .. }
        ) {
            self.streams
                .set_closed(forwarding_off(Path::new(NO_FORWARDING)).map(str::to_owned));
        }
        self.streams.handle(from, packet, &mut self.stream_out);
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
                    Message::Input(_)
                    | Message::Resize(_)
                    | Message::Forget
                    | Message::DropInput => {
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
            // Attaching and typing are what using a session is.
            if matches!(
                message,
                Message::Attach { .. } | Message::Input(_) | Message::Write(_)
            ) {
                link.touch(now);
                self.used = now;
            }
            link.send(Packet::about(id, message), now, outgoing);
        }
    }

    /// Whether the client has a link to session `id`, that is there, with a
    /// `write` that waits to be confirmed.
    fn is_confirming(&self, id: &SessionId) -> bool {
        self.links
            .iter()
            .any(|link| link.id() == id && !link.is_gone() && link.awaits_confirmation())
    }

    /// Gives session `id` this label, or none if it is empty; says nothing when
    /// it is done. The label is a file the session's relay reads when it is asked
    /// how the session stands, so there is nothing to tell the relay.
    fn label(&mut self, id: SessionId, label: &str, outgoing: &mut Vec<Packet>) {
        // A label for a session that is not there would be a file nobody reads.
        if let Err(e) = peer::connect_ours(&sockdir::socket_path(&self.dir, &id)) {
            return outgoing.push(Link::refusal(&id, &e));
        }
        if let Err(e) = sockdir::write_label(&self.dir, &id, label) {
            outgoing.push(failure(
                Some(id),
                ErrorCode::UNAVAILABLE,
                &format!("the label cannot be written: {e}"),
            ));
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

/// Where the client stands, as the mailbox is asked: what [`Standing`] says of
/// it, for a terminal that is a session of this bridge's.
struct Standings<'a> {
    links: &'a [Link],
    /// When the client last used any session.
    used: Instant,
    /// Whether the client takes tickets.
    tickets: bool,
    now: Instant,
}

impl<'a> Standings<'a> {
    fn new(links: &'a [Link], used: Instant, tickets: bool, now: Instant) -> Standings<'a> {
        Standings {
            links,
            used,
            tickets,
            now,
        }
    }

    /// The client is attached to the terminal when its link to that session is
    /// attached; it has then been idle since it last used that session.
    /// Otherwise it has been idle since it last used any: a program whose app
    /// is not at its terminal is told how close the app is to the machine.
    fn of(&self, terminal: Option<&SessionId>) -> Standing {
        let attached = terminal.and_then(|id| {
            self.links
                .iter()
                .find(|link| link.id() == id && !link.is_gone() && link.is_attached())
        });
        let standing = match attached {
            Some(link) => Standing::idle(link.idle(self.now)).with_attached(),
            None => Standing::idle(self.now.saturating_duration_since(self.used)),
        };
        if self.tickets {
            standing.with_tickets()
        } else {
            standing
        }
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

/// What the server can do beyond sessions: streams, unless the administrator
/// has turned them off by the file at `switch`, and programs, whatever it says;
/// and mail, when the bridge could open its mailbox (`mailbox`), which the
/// switch does not touch either, since whoever may log in may talk to the app.
fn features(switch: &Path, mailbox: bool) -> Features {
    let features = match forwarding_off(switch) {
        Some(why) => Features::streams_off(why),
        None => Features::with_streams(),
    }
    .with_commands();
    if mailbox {
        features.with_mailbox()
    } else {
        features
    }
}

/// Why streams are off, if the file at `switch` says so by being there, or if
/// there is no telling.
///
/// Only a file that is **not there** leaves streams on. A bridge runs as the
/// user, who may not be able to look in the directory (an administrator who
/// hides the switch behind `0700` on its directory has still made a decision):
/// an error that is not "no such file" is not read as "no file", or the
/// decision would bind everyone but the users it is meant for.
pub fn forwarding_off(switch: &Path) -> Option<&'static str> {
    use std::io::ErrorKind;
    match std::fs::symlink_metadata(switch) {
        Ok(_) => Some(FORWARDING_OFF),
        // A parent that is a file, not a directory, has nothing in it either.
        Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => None,
        Err(_) => Some(FORWARDING_UNCHECKED),
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
            | Message::Write(_)
            | Message::Read(_)
            | Message::Wait
            | Message::Forget
            | Message::DropInput
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A path that is not there, in a directory of its own.
    fn switch(label: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("kablo-bridge-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        (dir.clone(), dir.join("no-forwarding"))
    }

    #[test]
    fn streams_are_on_until_the_file_is_there() {
        let (dir, path) = switch("on");
        assert_eq!(forwarding_off(&path), None);
        assert!(features(&path, false).streams);
        assert_eq!(features(&path, false).reason, None);
        assert!(features(&path, false).commands);

        // Its contents do not matter, and it is looked at again each time.
        std::fs::write(&path, "").unwrap();
        assert_eq!(forwarding_off(&path), Some(FORWARDING_OFF));
        let off = features(&path, false);
        assert!(!off.streams);
        assert_eq!(off.reason.as_deref(), Some(FORWARDING_OFF));
        assert!(
            off.commands,
            "the switch is about connections, not programs"
        );

        std::fs::remove_file(&path).unwrap();
        assert_eq!(forwarding_off(&path), None);
        assert!(features(&path, false).streams);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn mail_is_said_when_there_is_a_mailbox_and_the_switch_does_not_touch_it() {
        let (dir, path) = switch("mail");
        assert!(!features(&path, false).mailbox);
        assert!(features(&path, true).mailbox);
        std::fs::write(&path, "").unwrap();
        let off = features(&path, true);
        assert!(!off.streams && off.mailbox);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_switch_that_cannot_be_looked_for_is_not_taken_for_one_that_is_not_there() {
        use std::os::unix::fs::PermissionsExt;
        // SAFETY: `geteuid` takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } == 0 {
            // Root can look anywhere: there is no such directory for it.
            return;
        }
        let (dir, path) = switch("hidden");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert_eq!(forwarding_off(&path), Some(FORWARDING_UNCHECKED));
        assert!(!features(&path, false).streams);
        assert_eq!(
            features(&path, false).reason.as_deref(),
            Some(FORWARDING_UNCHECKED)
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(forwarding_off(&path), Some(FORWARDING_OFF));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_parent_that_is_a_file_has_no_switch_in_it() {
        let (dir, file) = switch("parent");
        std::fs::write(&file, "").unwrap();
        assert_eq!(forwarding_off(&file.join("no-forwarding")), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_directory_that_is_not_there_leaves_streams_on() {
        let (dir, path) = switch("none");
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(forwarding_off(&path), None);
    }

    /// A bridge that reads nothing, over `dir`.
    fn bridge_over(dir: &Path) -> Bridge {
        // SAFETY: the descriptors are only named, to build the bridge; these
        // tests read and write none of them.
        let (input, output) = unsafe { (BorrowedFd::borrow_raw(0), BorrowedFd::borrow_raw(1)) };
        Bridge::new(dir.to_owned(), input, output, 512)
    }

    #[test]
    fn a_mailbox_that_is_in_use_but_not_asked_a_question_sets_no_timer() {
        use std::os::unix::net::UnixStream;
        let (dir, _) = switch("mailbox-timer");
        std::fs::remove_dir_all(&dir).unwrap();
        let mut bridge = bridge_over(&dir);
        assert_eq!(bridge.next_deadline(), None, "nobody is connected");

        let mailbox = sockdir::mailbox_path(&dir, std::process::id());
        let _program = UnixStream::connect(&mailbox).unwrap();
        let stand = |_: Option<&SessionId>| Standing::default();
        let open = bridge.mailbox.as_mut().unwrap();
        open.ready(mailbox::Token::Listener, libc::POLLIN, &stand);
        assert_eq!(open.interest(true).len(), 2, "the listener and the program");
        open.settle(Instant::now(), &stand);
        assert_eq!(bridge.next_deadline(), None, "a program is connected");
        drop(bridge);
        assert!(!mailbox.exists(), "the name goes with the bridge");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_client_is_attached_to_a_terminal_from_its_attach_and_idle_from_its_last_use() {
        use std::os::unix::net::UnixListener;
        use std::time::Duration;
        let (dir, _) = switch("standing");
        std::fs::remove_dir_all(&dir).unwrap();
        sockdir::ensure_dir(&dir).unwrap();
        let id = SessionId::new("shell").unwrap();
        let _session = UnixListener::bind(sockdir::socket_path(&dir, &id)).unwrap();
        let start = Instant::now();
        let mut out = Vec::new();
        let mut link = Link::open(&dir, id.clone(), start).unwrap();

        let later = start + Duration::from_secs(10);
        let standing =
            Standings::new(std::slice::from_ref(&link), start, false, later).of(Some(&id));
        assert!(!standing.attached);
        assert_eq!(standing.idle, Duration::from_secs(10), "since any use");

        let attach = Packet::about(
            id.clone(),
            Message::Attach {
                offset: 0,
                view: None,
            },
        );
        link.send(attach, later, &mut out);
        link.touch(later);
        let used = later + Duration::from_secs(3);
        let any = Standings::new(std::slice::from_ref(&link), later, true, used);
        let standing = any.of(Some(&id));
        assert!(standing.attached);
        assert_eq!(standing.idle, Duration::from_secs(3), "since the attach");
        let other = SessionId::new("other").unwrap();
        assert!(!any.of(Some(&other)).attached, "not to another");
        assert!(!any.of(None).attached, "nor to no terminal");
        assert!(standing.tickets, "the client said it takes tickets");
        assert!(!Standings::new(&[], later, false, used).of(None).tickets);
        let _ = std::fs::remove_dir_all(dir);
    }
}
