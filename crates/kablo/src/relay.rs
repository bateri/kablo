//! The relay: the part of a session that talks to clients.
//!
//! A warden starts a relay on a socket it keeps (standard input) and, once the
//! two have shaken hands on the inner wire, gives it the session: the
//! terminal's master side, the journal and the listening socket. The relay
//! says it is set up, waits to be told it is the one that reads the terminal,
//! and then runs a single `poll` loop over the warden's channel, the
//! listening socket, the terminal and its clients.
//!
//! **Output is served from the journal.** Every attached client holds one
//! number, the offset of the next byte it is owed, and the relay sends it
//! `journal[offset..total]` in frames whenever its socket takes bytes. Replay
//! after a reconnect and live output are one path, "the bytes it missed,
//! exactly" falls out of it, and the buffer a slow client costs is the
//! journal's, not another. A client that falls behind by more than the ring
//! holds is dropped (it comes back with its offset and is told, by
//! `truncated`, what it lost); it never slows the terminal's reading.
//!
//! **Nothing is closed by silence.** A client that goes away takes only its
//! own state with it; the session ends only on an explicit `terminate`
//! (forwarded to the warden, which owns the shell) or when the shell exits.
//!
//! The relay can be replaced while the shell lives: it keeps no state that a
//! successor cannot rebuild from the journal's header and the descriptors.
//! A client's `upgrade` is passed to the warden, which starts the other binary
//! as a second relay; when that one is set up this one is told to quit
//! (`Quit`): it stops reading the terminal, drops its clients by leaving, and
//! says where the journal ends. The second relay serves nobody until the warden
//! tells it `Go`, which is after that, so exactly one relay accepts clients and
//! exactly one reads the terminal at any time; a client that was here comes
//! back, to the relay that took over, with the offset it had.

use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};

use kablo_proto::v1::{
    ErrorCode, Exit, Message, OFFSET_END, Packet, PacketDecoder, Size, State, Status,
};
use kablo_proto::{SessionId, WIRE};

use crate::handshake::{self, Step};
use crate::inner::{self, ADOPT_DESCRIPTORS, Decoder, INNER, ToRelay, ToWarden};
use crate::journal::Journal;
use crate::procinfo;
use crate::sys;

/// Program bytes in one `output` frame.
const OUT_CHUNK: usize = 64 * 1024;

/// Frames queued for one client in one turn before the others get theirs.
const FRAMES_PER_TURN: usize = 8;

/// Unsent bytes of replies one client may have queued before it is dropped
/// (a client that asks and never reads).
const OUT_LIMIT: usize = 4 << 20;

/// Typed bytes waiting for the program before clients are no longer read.
const INPUT_LIMIT: usize = 1 << 20;

/// Connections at once. Past it the listening socket is left alone.
const MAX_CLIENTS: usize = 64;

/// Terminal reads in one turn before the other descriptors get a look.
const READ_BUDGET: usize = 16;

/// Runs the relay of session `id`; the warden's channel is standard input.
/// Returns the process's exit status.
pub fn run(id: SessionId) -> u8 {
    match Relay::set_up(&id) {
        Ok(relay) => relay.serve(),
        Err(_) => 1,
    }
}

struct Client {
    sock: UnixStream,
    open: bool,
    /// Bytes received before the handshake is complete.
    hello: Vec<u8>,
    decoder: PacketDecoder,
    /// Encoded bytes for the client; `out[sent..]` is not yet written.
    out: Vec<u8>,
    sent: usize,
    attach: Option<Attach>,
    /// The `ended` message was queued for this client.
    ended_queued: bool,
    /// Flush what is queued, then close.
    closing: bool,
    dead: bool,
    /// It asked for the relay to be replaced and has not been told how that
    /// went (when it went well it is told by the connection closing).
    upgrading: bool,
}

struct Attach {
    /// The offset of the next byte owed.
    next: u64,
}

struct Relay {
    id: SessionId,
    chan: UnixStream,
    chan_alive: bool,
    chan_out: Vec<u8>,
    decoder: Decoder<ToRelay>,
    master: OwnedFd,
    journal: Journal,
    listener: UnixListener,
    child_pid: u32,
    exit: Option<Exit>,
    /// This relay is the one that serves (it was told `Go`): it accepts
    /// clients. Until then the listening socket is left to its other holders.
    live: bool,
    /// This relay reads the terminal (it was told `Go`).
    reading: bool,
    master_done: bool,
    /// The terminal was read to its end after the shell exited.
    final_drained: bool,
    input: Vec<u8>,
    input_sent: usize,
    clients: Vec<Client>,
    delivered_sent: bool,
    quit: bool,
    chunk: usize,
}

enum Source {
    Chan,
    Listener,
    Master,
    Client(usize),
}

impl Relay {
    /// Shakes hands with the warden, takes the session it gives and says it
    /// is ready. All of it blocks: the relay has nothing else to do yet.
    fn set_up(id: &SessionId) -> io::Result<Relay> {
        // SAFETY: descriptor 0 is the channel the warden made this process's
        // standard input; nothing else in the process uses it.
        let chan = unsafe { UnixStream::from_raw_fd(0) };
        {
            let (mut input, mut output) = (&chan, &chan);
            INNER
                .client(&mut input, &mut output)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        }
        let mut decoder = Decoder::<ToRelay>::new();
        let mut fds = Vec::new();
        let mut buf = [0u8; 1024];
        let adopt = loop {
            let n = sys::recv_with_fds(chan.as_fd(), &mut buf, &mut fds)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            decoder.push(buf.get(..n).unwrap_or_default());
            let message = decoder
                .next_message()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
            match message {
                Some(ToRelay::Adopt(adopt)) => break adopt,
                Some(_) => return Err(io::Error::other("expected the session first")),
                None => {}
            }
        };
        if adopt.id != *id || fds.len() != ADOPT_DESCRIPTORS {
            return Err(io::Error::other(
                "not the session this relay was started for",
            ));
        }
        // The order is the inner wire's: terminal, journal, listening socket.
        let mut fds = fds.into_iter();
        let (Some(master), Some(journal), Some(listener)) = (fds.next(), fds.next(), fds.next())
        else {
            return Err(io::Error::other("too few descriptors"));
        };
        let journal = Journal::open(journal)?;
        let listener = UnixListener::from(listener);
        listener.set_nonblocking(true)?;
        let chunk = usize::try_from((64u64 * 1024).min(journal.capacity() / 4)).unwrap_or(4096);
        inner::write(&mut &chan, &ToWarden::Ready)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
        chan.set_nonblocking(true)?;
        Ok(Relay {
            id: adopt.id,
            chan,
            chan_alive: true,
            chan_out: Vec::new(),
            decoder,
            master,
            journal,
            listener,
            child_pid: adopt.child_pid,
            exit: adopt.exit,
            live: false,
            reading: false,
            master_done: false,
            final_drained: false,
            input: Vec::new(),
            input_sent: 0,
            clients: Vec::new(),
            delivered_sent: false,
            quit: false,
            chunk,
        })
    }

    fn serve(mut self) -> u8 {
        loop {
            let mut set = Vec::with_capacity(4 + self.clients.len());
            let mut sources = Vec::with_capacity(4 + self.clients.len());
            if self.chan_alive {
                let out = if self.chan_out.is_empty() {
                    0
                } else {
                    libc::POLLOUT
                };
                set.push(sys::want(self.chan.as_raw_fd(), libc::POLLIN | out));
                sources.push(Source::Chan);
            }
            if self.live && !self.quit && self.clients.len() < MAX_CLIENTS {
                set.push(sys::want(self.listener.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Listener);
            }
            let mut events = 0;
            if self.reading && !self.master_done {
                events |= libc::POLLIN;
            }
            if self.input_pending() > 0 {
                events |= libc::POLLOUT;
            }
            if events != 0 {
                set.push(sys::want(self.master.as_raw_fd(), events));
                sources.push(Source::Master);
            }
            for (i, client) in self.clients.iter().enumerate() {
                let mut events = 0;
                if self.wants_read(client) {
                    events |= libc::POLLIN;
                }
                if self.wants_write(client) {
                    events |= libc::POLLOUT;
                }
                if events != 0 {
                    set.push(sys::want(client.sock.as_raw_fd(), events));
                    sources.push(Source::Client(i));
                }
            }
            if sys::poll(&mut set, None).is_err() {
                return 1;
            }
            for (fd, source) in set.iter().zip(&sources) {
                if fd.revents == 0 {
                    continue;
                }
                match source {
                    Source::Chan => self.on_chan(fd.revents),
                    Source::Listener => self.accept(),
                    Source::Master => {
                        if fd.revents & libc::POLLOUT != 0 {
                            self.flush_input();
                        }
                        if fd.revents != libc::POLLOUT {
                            self.read_terminal();
                        }
                    }
                    Source::Client(i) => {
                        if sys::is_ready(fd.revents, libc::POLLIN) {
                            self.client_readable(*i);
                        }
                    }
                }
            }
            for i in 0..self.clients.len() {
                self.drive(i);
            }
            self.clients.retain(|client| !client.dead);
            if self.quit {
                // The terminal is not read any more: what was typed and not
                // yet taken is the last thing this relay does for it.
                self.flush_input();
                let offset = self.journal.total();
                self.send_warden(&ToWarden::Released { offset });
                return 0;
            }
        }
    }

    // --- the warden ---------------------------------------------------------

    fn on_chan(&mut self, revents: libc::c_short) {
        if revents & libc::POLLOUT != 0 {
            self.flush_chan();
        }
        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0 {
            return;
        }
        let mut buf = [0u8; 1024];
        let mut stray = Vec::new();
        loop {
            match sys::recv_with_fds(self.chan.as_fd(), &mut buf, &mut stray) {
                Ok(0) => {
                    // The warden is gone. The session goes on as it is; what
                    // only the warden can do (terminate) is refused.
                    self.chan_alive = false;
                    return;
                }
                Ok(n) => {
                    stray.clear();
                    self.decoder.push(buf.get(..n).unwrap_or_default());
                    loop {
                        match self.decoder.next_message() {
                            Ok(Some(message)) => self.on_warden(message),
                            Ok(None) => break,
                            Err(e) if e.is_recoverable() => {}
                            Err(_) => {
                                self.chan_alive = false;
                                return;
                            }
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.chan_alive = false;
                    return;
                }
            }
        }
    }

    fn on_warden(&mut self, message: ToRelay) {
        match message {
            ToRelay::Go => {
                self.live = true;
                self.reading = true;
                if self.exit.is_some() {
                    self.drain_to_the_end();
                }
            }
            ToRelay::Quit => {
                self.quit = true;
                self.reading = false;
            }
            ToRelay::ChildEnded(exit) => {
                self.exit = Some(exit);
                if self.reading {
                    self.drain_to_the_end();
                }
            }
            ToRelay::UpgradeFailed(why) => self.upgrade_failed(&why),
            ToRelay::Adopt(_) => {}
        }
    }

    /// The upgrade a client asked for did not happen: every client that asked
    /// is told why.
    fn upgrade_failed(&mut self, why: &str) {
        let asked: Vec<usize> = self
            .clients
            .iter()
            .enumerate()
            .filter(|(_, client)| client.upgrading)
            .map(|(i, _)| i)
            .collect();
        for i in asked {
            if let Some(client) = self.clients.get_mut(i) {
                client.upgrading = false;
            }
            self.error(i, ErrorCode::UPGRADE_FAILED, why);
        }
    }

    fn send_warden(&mut self, message: &ToWarden) {
        if !self.chan_alive || inner::write(&mut self.chan_out, message).is_err() {
            return;
        }
        self.flush_chan();
    }

    fn flush_chan(&mut self) {
        while !self.chan_out.is_empty() {
            match sys::write_fd(self.chan.as_fd(), &self.chan_out) {
                Ok(n) if n > 0 => {
                    self.chan_out.drain(..n.min(self.chan_out.len()));
                }
                Ok(_) => return,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => {
                    self.chan_alive = false;
                    self.chan_out.clear();
                    return;
                }
            }
        }
    }

    // --- the terminal -------------------------------------------------------

    fn read_terminal(&mut self) {
        if !self.reading || self.master_done {
            return;
        }
        for _ in 0..READ_BUDGET {
            match self.journal.fill_from(self.master.as_fd(), self.chunk) {
                Ok(0) => {
                    self.master_done = true;
                    return;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // `EIO` is how Linux says the program's side is closed.
                Err(_) => {
                    self.master_done = true;
                    return;
                }
            }
        }
    }

    /// The shell is gone: take what it left in the terminal and read no more.
    /// (A background process that kept the terminal open may write later; the
    /// session is over and that is not part of it.)
    fn drain_to_the_end(&mut self) {
        loop {
            match self.journal.fill_from(self.master.as_fd(), self.chunk) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
        self.reading = false;
        self.master_done = true;
        self.final_drained = true;
        self.input.clear();
        self.input_sent = 0;
    }

    fn input_pending(&self) -> usize {
        self.input.len().saturating_sub(self.input_sent)
    }

    fn flush_input(&mut self) {
        while self.input_pending() > 0 {
            let pending = self.input.get(self.input_sent..).unwrap_or_default();
            match sys::write_fd(self.master.as_fd(), pending) {
                Ok(n) if n > 0 => self.input_sent += n,
                Ok(_) => return,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // The program's side is closed: nobody will read these.
                Err(_) => break,
            }
        }
        self.input.clear();
        self.input_sent = 0;
    }

    // --- clients ------------------------------------------------------------

    fn wants_read(&self, client: &Client) -> bool {
        !client.dead && !client.closing && self.input_pending() < INPUT_LIMIT
    }

    fn wants_write(&self, client: &Client) -> bool {
        if client.dead {
            return false;
        }
        if client.sent < client.out.len() {
            return true;
        }
        client.attach.as_ref().is_some_and(|attach| {
            attach.next < self.journal.total()
                || (self.exit.is_some() && self.final_drained && !client.ended_queued)
        })
    }

    fn accept(&mut self) {
        while self.live && !self.quit && self.clients.len() < MAX_CLIENTS {
            let sock = match self.listener.accept() {
                Ok((sock, _)) => sock,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                // A connection that went away before it was accepted.
                Err(e) if e.kind() == io::ErrorKind::ConnectionAborted => continue,
                Err(_) => return,
            };
            let own = sys::effective_uid();
            let allowed = sys::peer_uid(sock.as_fd()).is_ok_and(|peer| sys::uid_allowed(peer, own));
            if !allowed || sock.set_nonblocking(true).is_err() {
                continue;
            }
            self.clients.push(Client {
                sock,
                open: false,
                hello: Vec::new(),
                decoder: PacketDecoder::new(),
                out: Vec::new(),
                sent: 0,
                attach: None,
                ended_queued: false,
                closing: false,
                dead: false,
                upgrading: false,
            });
        }
    }

    fn client_readable(&mut self, i: usize) {
        let mut buf = [0u8; 16 * 1024];
        let read = match self.clients.get_mut(i) {
            Some(client) if !client.dead => (&client.sock).read(&mut buf),
            _ => return,
        };
        match read {
            Ok(0) => self.drop_client(i),
            Ok(n) => self.feed(i, buf.get(..n).unwrap_or_default()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => self.drop_client(i),
        }
    }

    fn drop_client(&mut self, i: usize) {
        if let Some(client) = self.clients.get_mut(i) {
            client.dead = true;
        }
    }

    /// Bytes arrived from client `i`: the handshake first, then packets.
    fn feed(&mut self, i: usize, bytes: &[u8]) {
        let Some(client) = self.clients.get_mut(i) else {
            return;
        };
        if client.open {
            client.decoder.push(bytes);
        } else {
            client.hello.extend_from_slice(bytes);
            match handshake::serve(&WIRE, &client.hello) {
                Ok(Step::Need) => return,
                Ok(Step::Done {
                    reply, consumed, ..
                }) => {
                    client.out.extend_from_slice(&reply);
                    let rest = client.hello.get(consumed..).unwrap_or_default().to_vec();
                    client.decoder.push(&rest);
                    client.hello.clear();
                    client.open = true;
                }
                Ok(Step::Refused { reply }) => {
                    client.out.extend_from_slice(&reply);
                    client.closing = true;
                    return;
                }
                Err(_) => {
                    client.dead = true;
                    return;
                }
            }
        }
        loop {
            let next = match self.clients.get_mut(i) {
                Some(client) if !client.dead => client.decoder.next_packet(),
                _ => return,
            };
            match next {
                Ok(Some(packet)) => self.handle(i, packet),
                Ok(None) => return,
                Err(e) if e.is_recoverable() => {
                    self.error(i, ErrorCode::BAD_REQUEST, "malformed message");
                }
                Err(_) => {
                    self.drop_client(i);
                    return;
                }
            }
        }
    }

    /// Queues `message` for client `i`.
    fn queue(&mut self, i: usize, session: Option<SessionId>, message: Message) {
        let Some(client) = self.clients.get_mut(i) else {
            return;
        };
        let packet = Packet { session, message };
        if packet.encode(&mut client.out).is_err()
            || client.out.len().saturating_sub(client.sent) > OUT_LIMIT
        {
            client.dead = true;
        }
    }

    fn error(&mut self, i: usize, code: ErrorCode, text: &str) {
        let session = Some(self.id.clone());
        self.queue(
            i,
            session,
            Message::Error {
                code,
                text: text.to_owned(),
            },
        );
    }

    fn handle(&mut self, i: usize, packet: Packet) {
        if packet.session.as_ref().is_some_and(|s| *s != self.id) {
            return self.error(
                i,
                ErrorCode::UNKNOWN_SESSION,
                "no such session on this socket",
            );
        }
        let session = Some(self.id.clone());
        match packet.message {
            Message::Ping(bytes) => self.queue(i, packet.session, Message::Pong(bytes)),
            Message::List => {
                let status = self.status();
                self.queue(i, None, Message::ListReply(vec![status]));
            }
            Message::Status => {
                let status = self.status();
                self.queue(i, session, Message::StatusReply(status));
            }
            Message::New(_) => self.error(
                i,
                ErrorCode::BAD_REQUEST,
                "this socket serves one session; it cannot start another",
            ),
            Message::Attach { offset } => self.attach(i, offset),
            Message::Detach => {
                if let Some(client) = self.clients.get_mut(i) {
                    client.attach = None;
                }
            }
            Message::Resize(size) => self.resize(i, size),
            Message::Input(bytes) => self.type_input(i, &bytes),
            Message::Terminate { force } => {
                if self.chan_alive {
                    self.send_warden(&ToWarden::Terminate { force });
                } else {
                    self.error(i, ErrorCode::UNAVAILABLE, "the session's warden is gone");
                }
            }
            Message::Upgrade { path } => {
                if !self.chan_alive {
                    return self.error(i, ErrorCode::UNAVAILABLE, "the session's warden is gone");
                }
                if let Some(client) = self.clients.get_mut(i) {
                    client.upgrading = true;
                }
                self.send_warden(&ToWarden::Upgrade { path });
            }
            _ => self.error(i, ErrorCode::BAD_REQUEST, "not a request"),
        }
    }

    fn is_attached(&self, i: usize) -> bool {
        self.clients
            .get(i)
            .is_some_and(|client| client.attach.is_some())
    }

    fn attach(&mut self, i: usize, offset: u64) {
        let total = self.journal.total();
        let (start, truncated) = if offset == OFFSET_END {
            (total, false)
        } else if offset > total {
            return self.error(
                i,
                ErrorCode::BAD_REQUEST,
                "the offset is past the end of the stream",
            );
        } else if offset < self.journal.retained_from() {
            // The start of what was asked for is gone: begin at a line.
            (self.journal.line_start(), true)
        } else {
            (offset, false)
        };
        if let Some(client) = self.clients.get_mut(i) {
            client.attach = Some(Attach { next: start });
            client.ended_queued = false;
        }
        let session = Some(self.id.clone());
        self.queue(
            i,
            session,
            Message::Attached {
                offset: start,
                truncated,
            },
        );
        if truncated && self.exit.is_none() {
            self.redraw();
        }
    }

    /// Makes full-screen programs draw again: the terminal is made a row
    /// smaller and back, and each change reaches the program as `SIGWINCH`.
    fn redraw(&self) {
        let Ok(size) = sys::window_size(self.master.as_fd()) else {
            return;
        };
        if size.rows == 0 {
            return;
        }
        let nudged = Size {
            rows: if size.rows > 1 {
                size.rows - 1
            } else {
                size.rows + 1
            },
            ..size
        };
        let _ = sys::set_window_size(self.master.as_fd(), nudged);
        let _ = sys::set_window_size(self.master.as_fd(), size);
    }

    fn resize(&mut self, i: usize, size: Size) {
        if !self.is_attached(i) {
            return self.error(i, ErrorCode::NOT_ATTACHED, "attach first");
        }
        if size.cols == 0 || size.rows == 0 {
            return self.error(i, ErrorCode::BAD_REQUEST, "a terminal has rows and columns");
        }
        if self.exit.is_none() {
            let _ = sys::set_window_size(self.master.as_fd(), size);
        }
    }

    fn type_input(&mut self, i: usize, bytes: &[u8]) {
        if !self.is_attached(i) {
            return self.error(i, ErrorCode::NOT_ATTACHED, "attach first");
        }
        if self.exit.is_some() {
            return;
        }
        self.input.extend_from_slice(bytes);
        self.flush_input();
    }

    fn status(&self) -> Status {
        let mut status = Status::new(self.id.clone());
        status.pid = self.child_pid;
        if self.exit.is_none() {
            let foreground = procinfo::foreground(self.master.as_fd(), self.child_pid);
            status.program = foreground.program;
            status.cwd = foreground.cwd;
        }
        status.size = sys::window_size(self.master.as_fd()).unwrap_or_default();
        status.clients = self
            .clients
            .iter()
            .filter(|client| !client.dead && client.attach.is_some())
            .count() as u32;
        status.state = if self.exit.is_some() {
            State::ENDED
        } else {
            State::RUNNING
        };
        status.offset = self.journal.total();
        status.retained_from = self.journal.retained_from();
        status.exit = self.exit;
        status.version = env!("CARGO_PKG_VERSION").to_owned();
        status
    }

    /// Writes what client `i` can take and queues what it is owed.
    fn drive(&mut self, i: usize) {
        let ended_ready = self.exit.is_some() && self.final_drained;
        for _ in 0..FRAMES_PER_TURN {
            if !self.flush(i) {
                // Blocked or dead; the lapped check below still applies.
                break;
            }
            let total = self.journal.total();
            let Some(client) = self.clients.get_mut(i) else {
                return;
            };
            if client.closing {
                client.dead = true;
                return;
            }
            let Some(attach) = client.attach.as_mut() else {
                break;
            };
            if attach.next < total {
                let owed = (total - attach.next).min(OUT_CHUNK as u64);
                let mut data = vec![0u8; usize::try_from(owed).unwrap_or(OUT_CHUNK)];
                match self.journal.copy_out(attach.next, &mut data) {
                    Some(n) if n > 0 => {
                        data.truncate(n);
                        let offset = attach.next;
                        attach.next += n as u64;
                        let packet =
                            Packet::about(self.id.clone(), Message::Output { offset, data });
                        if packet.encode(&mut client.out).is_err() {
                            client.dead = true;
                            return;
                        }
                    }
                    // The ring was lapped under it.
                    _ => {
                        client.dead = true;
                        return;
                    }
                }
            } else if let (true, Some(exit), false) = (ended_ready, self.exit, client.ended_queued)
            {
                let packet = Packet::about(self.id.clone(), Message::Ended(exit));
                if packet.encode(&mut client.out).is_err() {
                    client.dead = true;
                    return;
                }
                client.ended_queued = true;
            } else {
                break;
            }
        }
        let Some(client) = self.clients.get_mut(i) else {
            return;
        };
        // A client the ring has left behind is dropped even when it is not
        // reading: it never gets what it has not got, and it must not hold a
        // seat for the whole session.
        let oldest = self.journal.retained_from();
        if client
            .attach
            .as_ref()
            .is_some_and(|attach| attach.next < oldest)
        {
            client.dead = true;
            return;
        }
        if client.ended_queued && client.out.is_empty() {
            // The end of the shell has reached a client: the session may go.
            if !self.delivered_sent {
                self.delivered_sent = true;
                self.send_warden(&ToWarden::Delivered);
            }
        }
    }

    /// Writes client `i`'s queue. `true` when nothing is left to write.
    fn flush(&mut self, i: usize) -> bool {
        let Some(client) = self.clients.get_mut(i) else {
            return false;
        };
        if client.dead {
            return false;
        }
        while client.sent < client.out.len() {
            let pending = client.out.get(client.sent..).unwrap_or_default();
            match (&client.sock).write(pending) {
                Ok(0) => {
                    client.dead = true;
                    return false;
                }
                Ok(n) => client.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return false,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    client.dead = true;
                    return false;
                }
            }
        }
        client.out.clear();
        client.sent = 0;
        true
    }
}
