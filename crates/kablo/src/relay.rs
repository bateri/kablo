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
//! **The relay keeps the screen too.** Every byte read from the terminal goes
//! to the journal and, right after it, to a [`Screen`] (the parser bateri uses),
//! so the screen is always the screen of exactly the journal's `total`. A client
//! that comes for bytes the ring has dropped, or asks for one, is sent a picture
//! of that screen (VT bytes that build it) and then the journal from the
//! picture's offset; the program is not asked to draw again. The terminal's own
//! questions (device attributes, the cursor position) are answered by that
//! screen only while no client is attached, so a program that asks while nobody
//! looks is not left waiting and one that is watched hears one answer, the
//! client's. A relay that starts (a replacement, or a restart after a crash) has
//! no screen to carry over and builds it again from what the journal holds.
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
    DEFAULT_HISTORY, ErrorCode, Exit, MAX_COLS, MAX_HISTORY, MAX_ROWS, Message, OFFSET_END, Packet,
    PacketDecoder, Size, State, Status, View,
};
use kablo_proto::{SessionId, WIRE};
use kablo_screen::Screen;

use crate::handshake::{self, Step};
use crate::inner::{self, ADOPT_DESCRIPTORS, Decoder, INNER, ToRelay, ToWarden};
use crate::journal::Journal;
use crate::procinfo;
use crate::pty;
use crate::sys;

/// Program bytes in one `output` frame.
const OUT_CHUNK: usize = 64 * 1024;

/// Frames queued for one client in one turn before the others get theirs.
const FRAMES_PER_TURN: usize = 8;

/// Unsent bytes of replies one client may have queued before it is dropped
/// (a client that asks and never reads).
const OUT_LIMIT: usize = 4 << 20;

/// Typed bytes the relay holds for a program that is not reading them. An
/// `input` that would take what is waiting past it is refused whole, with an
/// error to its sender; clients are read all the same, because what a stuck
/// program is owed must never keep a person from asking about the session or
/// ending it.
const INPUT_LIMIT: usize = 1 << 20;

/// Whose questions the bytes given to the screen may hold.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Feed {
    /// Bytes this relay has just read: what the program asks in them is this
    /// relay's to answer.
    Live,
    /// Bytes the journal held already: those after the offset it says it
    /// answered up to were read while no relay was (by the warden, in the gap
    /// before this relay or between two), and their questions are answered now;
    /// the others were answered by whoever read them.
    Held,
    /// Bytes nobody is to answer (the last output of a program that has ended).
    Spent,
}

/// Bytes of the journal given to the screen in one piece.
const SCREEN_CHUNK: usize = 64 * 1024;

/// How much of the journal a relay that takes over reads to build its screen:
/// the last this many bytes. Replaying a journal of a gigabyte before the first
/// client is served would be a pause the swap is meant not to have, and what
/// the screen shows rarely depends on output that old.
const REPLAY_LIMIT: u64 = 64 << 20;

/// How many times the screen is begun again while a relay that is still
/// writing laps it. Past it the screen goes on as it is: it may be wrong, and
/// the next picture is the best there is.
const SCREEN_RESTARTS: u32 = 8;

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
    /// The picture of the screen that goes before it, while it is being sent.
    picture: Option<Picture>,
}

/// A picture of the screen on its way to a client, in pieces.
struct Picture {
    /// The offset of the program's output it is of.
    offset: u64,
    cols: u16,
    rows: u16,
    bytes: Vec<u8>,
    /// How many of `bytes` are queued.
    sent: usize,
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
    /// The screen of the stream up to `screen_at`.
    screen: Screen,
    /// The offset after the last journal byte the screen has had.
    screen_at: u64,
    /// The scrollback rows the screen keeps.
    history: usize,
    /// Reused to carry journal bytes to the screen.
    scratch: Vec<u8>,
    clients: Vec<Client>,
    delivered_sent: bool,
    quit: bool,
    /// The listening socket is left out of the poll set because accepting
    /// failed (no descriptors left): it stays readable for as long as that
    /// lasts, and waiting on it would spin. A client going frees a descriptor
    /// and ends the pause.
    accept_paused: bool,
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
        let history = usize::try_from(
            journal
                .history()
                .unwrap_or(DEFAULT_HISTORY)
                .min(MAX_HISTORY),
        )
        .unwrap_or_default();
        let size = sys::window_size(master.as_fd())
            .ok()
            .filter(|size| size.cols > 0 && size.rows > 0)
            .unwrap_or(pty::DEFAULT_SIZE);
        // A terminal bigger than a screen may be (one a session was given
        // before the limit) is shown at the most.
        let screen = Screen::new(size.cols.min(MAX_COLS), size.rows.min(MAX_ROWS), history);
        let screen_at = replay_start(&journal, REPLAY_LIMIT);
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
            screen,
            screen_at,
            history,
            scratch: vec![0; SCREEN_CHUNK],
            clients: Vec::new(),
            delivered_sent: false,
            quit: false,
            accept_paused: false,
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
            if self.live && !self.quit && !self.accept_paused && self.clients.len() < MAX_CLIENTS {
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
            let seated = self.clients.len();
            self.clients.retain(|client| !client.dead);
            if self.clients.len() < seated {
                self.accept_paused = false;
            }
            if self.is_orphaned() {
                self.quit = true;
            }
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
                self.feed_screen(Feed::Held);
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
                // Before the next read: a read may lap a small ring.
                Ok(_) => self.feed_screen(Feed::Live),
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
                Ok(_) => self.feed_screen(Feed::Spent),
            }
        }
        self.reading = false;
        self.master_done = true;
        self.final_drained = true;
        self.input.clear();
        self.input_sent = 0;
    }

    // --- the screen ---------------------------------------------------------

    /// Gives the screen the journal bytes it has not had, and the program the
    /// terminal's answers to the questions in them that are its to hear (`mode`
    /// says which).
    fn feed_screen(&mut self, mode: Feed) {
        let mut restarts = 0;
        loop {
            let total = self.journal.total();
            if self.screen_at >= total {
                return;
            }
            let mut wanted = usize::try_from(total - self.screen_at)
                .unwrap_or(usize::MAX)
                .min(self.scratch.len());
            let answers = match (mode, self.journal.answered()) {
                (Feed::Live, _) => true,
                (Feed::Spent, _) | (Feed::Held, None) => false,
                (Feed::Held, Some(upto)) if self.screen_at >= upto => true,
                // Up to where it was answered, and no further in one piece.
                (Feed::Held, Some(upto)) => {
                    wanted = wanted.min(usize::try_from(upto - self.screen_at).unwrap_or(wanted));
                    false
                }
            };
            let got = self
                .scratch
                .get_mut(..wanted)
                .and_then(|room| self.journal.copy_out(self.screen_at, room));
            match got {
                Some(0) => return,
                Some(n) => {
                    self.screen.feed(self.scratch.get(..n).unwrap_or_default());
                    self.screen_at += n as u64;
                    self.answer(answers);
                    if answers || mode == Feed::Spent {
                        self.journal.set_answered(self.screen_at);
                    }
                }
                // The ring was lapped under the screen (a relay that is still
                // writing, while this one catches up): begin again from what
                // is left.
                None if restarts < SCREEN_RESTARTS => {
                    restarts += 1;
                    let (cols, rows) = self.screen.size();
                    self.screen = Screen::new(cols, rows, self.history);
                    self.screen_at = replay_start(&self.journal, REPLAY_LIMIT);
                }
                None => {
                    self.screen_at = total;
                    return;
                }
            }
        }
    }

    /// Sends the terminal's own answers to the program, when nobody else will:
    /// a client attached answers with its terminal, and two answers to one
    /// question would be typed into the program.
    fn answer(&mut self, wanted: bool) {
        let answers = self.screen.take_replies();
        if answers.is_empty() || !wanted || self.exit.is_some() || self.attached() > 0 {
            return;
        }
        // The program that does not read what it is owed gets no more.
        let _ = self.queue_input(&answers);
    }

    /// How many clients are attached.
    fn attached(&self) -> usize {
        self.clients
            .iter()
            .filter(|client| !client.dead && client.attach.is_some())
            .count()
    }

    /// The terminal takes this size, and so does the screen.
    fn set_size(&mut self, size: Size) {
        if self.exit.is_some() {
            return;
        }
        let _ = sys::set_window_size(self.master.as_fd(), size);
        if self.screen.size() != (size.cols, size.rows) {
            self.screen.resize(size.cols, size.rows);
        }
    }

    /// A picture of the screen as it is at the end of the journal.
    fn picture(&mut self) -> Picture {
        self.feed_screen(Feed::Spent);
        let (cols, rows) = self.screen.size();
        Picture {
            offset: self.screen_at,
            cols,
            rows,
            bytes: self.screen.image(),
            sent: 0,
        }
    }

    /// Nothing is left for this relay to do: the warden is gone (nobody can
    /// tell it to quit, or keep the session for a client that comes later),
    /// the program's side of the terminal is closed and every client has what
    /// there is to give it. A warden that is gone while the program lives
    /// leaves a session that still works, so that case goes on.
    fn is_orphaned(&self) -> bool {
        !self.chan_alive
            && self.master_done
            && self.clients.iter().all(|client| !self.wants_write(client))
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
        !client.dead && !client.closing
    }

    fn wants_write(&self, client: &Client) -> bool {
        if client.dead {
            return false;
        }
        if client.sent < client.out.len() {
            return true;
        }
        client.attach.as_ref().is_some_and(|attach| {
            attach.picture.is_some()
                || attach.next < self.journal.total()
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
                Err(_) => {
                    self.accept_paused = true;
                    return;
                }
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
            Message::Attach { offset, view } => self.attach(i, offset, view),
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

    fn attach(&mut self, i: usize, offset: u64, view: Option<View>) {
        if view.is_some_and(|view| !view.size.fits()) {
            return self.too_big(i);
        }
        let total = self.journal.total();
        if offset != OFFSET_END && offset > total {
            return self.error(
                i,
                ErrorCode::BAD_REQUEST,
                "the offset is past the end of the stream",
            );
        }
        // The client's terminal first: the picture is made for it.
        if let Some(view) = view
            && view.size.cols > 0
            && view.size.rows > 0
        {
            self.set_size(view.size);
        }
        // The start of what was asked for is gone, or a picture was asked for:
        // the screen stands in for what the stream would have shown.
        let truncated = offset != OFFSET_END && offset < self.journal.retained_from();
        let picture = (truncated || view.is_some_and(|view| view.picture)).then(|| self.picture());
        let start = match &picture {
            Some(picture) => picture.offset,
            None if offset == OFFSET_END => total,
            None => offset,
        };
        if let Some(client) = self.clients.get_mut(i) {
            client.attach = Some(Attach {
                next: start,
                picture,
            });
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
    }

    fn resize(&mut self, i: usize, size: Size) {
        if !self.is_attached(i) {
            return self.error(i, ErrorCode::NOT_ATTACHED, "attach first");
        }
        if size.cols == 0 || size.rows == 0 {
            return self.error(i, ErrorCode::BAD_REQUEST, "a terminal has rows and columns");
        }
        if !size.fits() {
            return self.too_big(i);
        }
        self.set_size(size);
    }

    /// A terminal larger than a screen may be: the relay's screen holds every
    /// cell of it, and a size is no reason to run out of memory.
    fn too_big(&mut self, i: usize) {
        self.error(
            i,
            ErrorCode::BAD_REQUEST,
            &format!("a terminal has at most {MAX_COLS} columns and {MAX_ROWS} rows"),
        );
    }

    fn type_input(&mut self, i: usize, bytes: &[u8]) {
        if !self.is_attached(i) {
            return self.error(i, ErrorCode::NOT_ATTACHED, "attach first");
        }
        if self.exit.is_some() {
            return;
        }
        if !self.queue_input(bytes) {
            self.error(i, ErrorCode::INPUT_FULL, "the program is not taking input");
        }
    }

    /// Holds `bytes` for the program, which takes them when it reads. `false`
    /// when that would take what is waiting past [`INPUT_LIMIT`]: none of it is
    /// kept.
    fn queue_input(&mut self, bytes: &[u8]) -> bool {
        // The program may have started reading since the poll: settle what is
        // waiting before counting it.
        self.flush_input();
        if self.input_pending().saturating_add(bytes.len()) > INPUT_LIMIT {
            return false;
        }
        if self.input_sent > 0 {
            // Only what is still owed counts against the limit, and only that
            // is kept.
            self.input.drain(..self.input_sent);
            self.input_sent = 0;
        }
        self.input.extend_from_slice(bytes);
        self.flush_input();
        true
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
            if let Some(picture) = attach.picture.as_mut() {
                // The screen goes first, a piece at a time like the output.
                let end = picture.bytes.len().min(picture.sent + OUT_CHUNK);
                let piece = Message::Picture {
                    offset: picture.offset,
                    cols: picture.cols,
                    rows: picture.rows,
                    more: end < picture.bytes.len(),
                    data: picture
                        .bytes
                        .get(picture.sent..end)
                        .unwrap_or_default()
                        .to_vec(),
                };
                picture.sent = end;
                if end >= picture.bytes.len() {
                    attach.picture = None;
                }
                if Packet::about(self.id.clone(), piece)
                    .encode(&mut client.out)
                    .is_err()
                {
                    client.dead = true;
                    return;
                }
            } else if attach.next < total {
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

/// Where the screen of a journal begins: the start of the stream when nothing
/// was dropped and no more than `limit` bytes are there, else the first line
/// break in the last `limit` bytes that are still in the ring, so that the
/// screen does not start inside an escape sequence.
fn replay_start(journal: &Journal, limit: u64) -> u64 {
    let from = journal.total().saturating_sub(limit);
    if from == 0 && journal.retained_from() == 0 {
        0
    } else {
        journal.line_start_from(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::MIN_CAPACITY;

    /// What the program would find waiting to be read after the relay took
    /// over a session whose journal is `journal`: the answers to its questions.
    fn answers_after_taking_over(journal: Journal) -> Vec<u8> {
        let pty = pty::open(pty::DEFAULT_SIZE).unwrap();
        // The program's side reads what arrives as it is.
        // SAFETY: `slave` is an open terminal and `raw` a termios the kernel fills.
        unsafe {
            let mut raw: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(pty.slave.as_raw_fd(), &raw mut raw), 0);
            libc::cfmakeraw(&raw mut raw);
            assert_eq!(
                libc::tcsetattr(pty.slave.as_raw_fd(), libc::TCSANOW, &raw),
                0
            );
        }
        let (chan, _warden) = UnixStream::pair().unwrap();
        static SERIAL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("kr{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let mut relay = Relay {
            id: SessionId::new("held").unwrap(),
            chan,
            chan_alive: true,
            chan_out: Vec::new(),
            decoder: Decoder::<ToRelay>::new(),
            master: pty.master,
            screen: Screen::new(80, 24, 10),
            screen_at: replay_start(&journal, REPLAY_LIMIT),
            history: 10,
            scratch: vec![0; SCREEN_CHUNK],
            journal,
            listener,
            child_pid: 1,
            exit: None,
            live: false,
            reading: false,
            master_done: false,
            final_drained: false,
            input: Vec::new(),
            input_sent: 0,
            clients: Vec::new(),
            delivered_sent: false,
            quit: false,
            accept_paused: false,
            chunk: 4096,
        };
        relay.on_warden(ToRelay::Go);
        let _ = std::fs::remove_file(&path);
        let mut set = [sys::want(pty.slave.as_raw_fd(), libc::POLLIN)];
        let mut got = Vec::new();
        while sys::poll(&mut set, Some(std::time::Duration::from_millis(300))).unwrap() > 0 {
            let mut buf = [0u8; 64];
            match sys::read_fd(pty.slave.as_fd(), &mut buf) {
                Ok(n) if n > 0 => got.extend_from_slice(buf.get(..n).unwrap_or_default()),
                _ => break,
            }
        }
        got
    }

    #[test]
    fn what_the_warden_read_while_no_relay_did_is_answered_by_the_relay_that_comes() {
        // The program asked at once, before any relay was serving: the journal
        // holds the question and says nobody answered anything.
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        assert_eq!(answers_after_taking_over(journal), b"\x1b[1;1R");
    }

    #[test]
    fn what_an_earlier_relay_answered_is_not_answered_again() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.set_answered(journal.total());
        journal.append(b"text");
        assert_eq!(answers_after_taking_over(journal), b"");
    }

    #[test]
    fn only_the_part_after_what_was_answered_is_answered() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.set_answered(journal.total());
        // Read by the warden afterwards, with one more question in it.
        journal.append(b"more\x1b[5nmore");
        // The device status report: "terminal OK".
        assert_eq!(answers_after_taking_over(journal), b"\x1b[0n");
    }

    #[test]
    fn a_journal_that_keeps_no_track_is_not_answered_late() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.forget_answers();
        assert_eq!(answers_after_taking_over(journal), b"");
    }

    #[test]
    fn a_screen_is_built_from_the_start_when_all_of_it_is_there() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"one\ntwo\nthree");
        assert_eq!(replay_start(&journal, 1 << 20), 0);
    }

    #[test]
    fn a_screen_is_built_from_the_last_of_a_long_journal_at_a_line() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"one\ntwo\nthree\nfour");
        // The last nine bytes are "hree\nfour": the next line is "four".
        assert_eq!(replay_start(&journal, 9), 14);
        // No line break in the last three: where the limit says.
        assert_eq!(replay_start(&journal, 3), 15);
    }

    #[test]
    fn a_screen_is_built_from_the_first_line_of_a_ring_that_wrapped() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        let cap = usize::try_from(MIN_CAPACITY).unwrap();
        let mut data = vec![b'x'; cap + 100];
        data[cap / 2] = b'\n';
        journal.append(&data);
        assert_eq!(journal.retained_from(), 100);
        assert_eq!(replay_start(&journal, 1 << 30), (cap / 2) as u64 + 1);
    }
}
