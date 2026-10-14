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
//!
//! It also connects to TCP ports for the client (see [`crate::streams`]), which
//! is what `ssh -L` does without asking sshd for anything. Those connections
//! last as long as the client's does, and what they carry shares the pipe with
//! the sessions without starving either (see [`crate::outbound`]). A server's
//! administrator can turn them off by putting a file in [`NO_FORWARDING`]; the
//! file is looked at again for every `open`, so the decision reaches a bridge
//! that is already running.

use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::Instant;

use kablo_proto::v1::stream::{self as wire, Incoming, IncomingDecoder};
use kablo_proto::v1::{
    ErrorCode, Features, MAX_COLS, MAX_HISTORY, MAX_ROWS, Message, NewSession, Packet,
};
use kablo_proto::{SessionId, WIRE};

use crate::handshake::{self, Step};
use crate::launch::{self, Failure};
use crate::link::Link;
use crate::outbound::{self, Outbound};
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
            for (fd, token) in self.streams.interest() {
                set.push(fd);
                sources.push(Source::Stream(token));
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
            (_, Message::Features) => outgoing.push(Packet::connection(Message::FeaturesReply(
                features(Path::new(NO_FORWARDING)),
            ))),
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

    /// A message about a stream. Whether streams are on is asked again for
    /// every `open`, so that an administrator's decision reaches streams
    /// opened after it.
    fn handle_stream(&mut self, from: wire::Side, packet: wire::Packet) {
        if matches!(packet.message, wire::Message::Open { .. }) {
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

/// What the server can do beyond sessions: streams, unless the administrator
/// has turned them off by the file at `switch`.
fn features(switch: &Path) -> Features {
    match forwarding_off(switch) {
        Some(why) => Features::streams_off(why),
        None => Features::with_streams(),
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
        assert!(features(&path).streams);
        assert_eq!(features(&path).reason, None);

        // Its contents do not matter, and it is looked at again each time.
        std::fs::write(&path, "").unwrap();
        assert_eq!(forwarding_off(&path), Some(FORWARDING_OFF));
        let off = features(&path);
        assert!(!off.streams);
        assert_eq!(off.reason.as_deref(), Some(FORWARDING_OFF));

        std::fs::remove_file(&path).unwrap();
        assert_eq!(forwarding_off(&path), None);
        assert!(features(&path).streams);
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
        assert!(!features(&path).streams);
        assert_eq!(
            features(&path).reason.as_deref(),
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
}
