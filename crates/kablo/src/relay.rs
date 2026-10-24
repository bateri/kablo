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
//! client's. A client whose terminal answers what it is shown and cannot hold
//! its answers back says so in its view, and is sent a picture rather than
//! bytes that hold a question already answered (`last_reply`); any other
//! client is told how far the questions were answered (`answered`) and keeps
//! its own answers back. A relay that starts (a replacement, or a restart after
//! a crash) has no screen to carry over and builds it again from what the
//! journal holds.
//!
//! **The terminal's state is told, not polled.** What only the server can see
//! of the terminal (the program in the foreground, whether a password is being
//! asked for, where an ssh session goes) is told to every attached client as a
//! `state` when it differs from what was last told: right after `attached`,
//! at the end of a turn that read output, and once more [`STATE_SETTLE`] after
//! the last output or typed input (a program that says nothing). Nobody
//! attached, nothing looked at, and no timer: an idle session waits in `poll`
//! for ever.
//!
//! **A session can be driven without attaching.** A connection that has not
//! attached may type (`write`), read the screen (`read`) and ask to be told when
//! the program ends (`wait`), and is not a client for it: the relay goes on
//! answering the program's questions as it does while nobody is there, a client
//! that attaches meanwhile answers its own, and no look at the terminal or timer
//! is started for it. A `read` is a question, answered at the end of the turn
//! like `status`; a `wait` is answered once the last output is on the screen,
//! and being told is not delivery, so the session is kept for the clients that
//! have not been told. The label of a session is a file the relay reads when it
//! says how the session stands and takes away when the session is over.
//!
//! **An ended session is kept for the clients that have not been told, and for
//! a while longer for the ones that asked.** The warden removes the session
//! once a client that was attached has been told how the program ended. A client
//! whose view sets `keep_after_end` is a program's reader on its way: what it
//! shows may be read by another program a moment after it was told (`capture`,
//! `wait`), so when such a client has been told (or another has, while it is
//! attached and behind), delivery waits [`END_KEEP`] (counted from that moment,
//! once) whoever else was told, and the warden's day is the most it can wait. The wait is a timer with a stop condition, it fires
//! once and delivers, and without a client that asked it does not exist.
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
use std::time::{Duration, Instant};

use kablo_proto::v1::{
    Capture, ClientName, DEFAULT_HISTORY, ErrorCode, Exit, MAX_CLIENTS, MAX_COLS, MAX_HISTORY,
    MAX_ROWS, Message, OFFSET_END, Packet, PacketDecoder, Size, State, Status, Stroke,
    TerminalState, View,
};
use kablo_proto::{SessionId, WIRE};
use kablo_screen::Screen;

use crate::handshake::{self, Step};
use crate::inner::{self, ADOPT_DESCRIPTORS, Decoder, INNER, ToRelay, ToWarden};
use crate::journal::Journal;
use crate::procinfo;
use crate::pty;
use crate::sockdir;
use crate::sys;
use crate::typing;

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

/// How long after the last output, or the last input a client typed, the
/// terminal is looked at once more. The look at the end of a turn that read
/// output cannot see a program that starts and says nothing (`sleep 100`), nor
/// one the shell has not yet made the foreground when its echo of the line
/// arrives (the shell forks after the newline is read, and the program is the
/// foreground a moment later); this one does, a moment after the line was
/// typed. The wait is short enough to feel like part of the keystroke and long
/// enough for the shell's fork and `exec` to be done.
const STATE_SETTLE: Duration = Duration::from_millis(200);

/// The one look at the terminal that follows output or input by
/// [`STATE_SETTLE`].
///
/// This is the only timer the relay has, and it has a stop condition: it is
/// armed by output the relay read or input a client typed, it fires **once**,
/// and then it is not armed again until there is more of either. A session
/// nobody is attached to never has it armed, and neither does one that has
/// ended, so an idle session is a `poll` with no timeout.
#[derive(Default)]
struct Settle {
    due: Option<Instant>,
}

impl Settle {
    /// Output or input arrived at `now`: look once more when it has been quiet
    /// for [`STATE_SETTLE`].
    fn arm(&mut self, now: Instant) {
        self.due = Some(now + STATE_SETTLE);
    }

    fn cancel(&mut self) {
        self.due = None;
    }

    /// How long a `poll` may wait at `now`: for ever unless armed.
    fn timeout(&self, now: Instant) -> Option<Duration> {
        self.due.map(|due| due.saturating_duration_since(now))
    }

    /// Whether it is time to look; a look that is due is due once.
    fn fire(&mut self, now: Instant) -> bool {
        if self.due.is_some_and(|due| due <= now) {
            self.due = None;
            return true;
        }
        false
    }
}

/// How long an ended session is kept for a program to read, from the moment a
/// client that asked for it (`keep_after_end`) was told how the program ended.
/// Long enough for the program that watches a terminal's end to come back and
/// read what was on the screen, short enough that a script that opens a hundred
/// terminals does not leave a hundred behind.
const END_KEEP: Duration = Duration::from_secs(10 * 60);

/// The window of [`END_KEEP`] in this process (a test may shorten it with
/// [`crate::ENV_TEST_END_KEEP`]).
fn end_keep() -> Duration {
    std::env::var(crate::ENV_TEST_END_KEEP)
        .ok()
        .and_then(|ms| ms.parse().ok())
        .map_or(END_KEEP, Duration::from_millis)
}

/// The wait that follows the telling of the end to a client that asked for the
/// session to be kept.
///
/// The relay's second timer, with a stop condition of its own: it starts once,
/// when the first such client has been told (a client that is told later does
/// not push it back), it fires once, at its end, and delivery follows; a session
/// no such client was told about has none, and neither does a relay that is
/// replaced (the client that is told again after it starts a new one).
#[derive(Default)]
struct EndKeep {
    until: Option<Instant>,
}

impl EndKeep {
    /// A client that asked was told at `now`; the first one starts the wait.
    fn begin(&mut self, now: Instant, window: Duration) {
        self.until.get_or_insert(now + window);
    }

    /// Whether a wait has been started, over or not.
    fn started(&self) -> bool {
        self.until.is_some()
    }

    /// Whether the wait is over at `now`.
    fn over(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| until <= now)
    }

    /// How long a `poll` may wait at `now` for the wait to end, if one runs.
    fn timeout(&self, now: Instant) -> Option<Duration> {
        self.until.map(|until| until.saturating_duration_since(now))
    }
}

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

/// Terminal reads in one turn before the other descriptors get a look.
const READ_BUDGET: usize = 16;

/// Reads of one client's socket in one turn, at most. A client that leaves
/// with more than this many reads' worth unread is seen to have left a turn
/// later, so a question asked in between still counts it.
const CLIENT_READS: usize = 4;

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
    /// What it asked about the session and has not been answered yet; what
    /// it sent after the question waits for the answer.
    asked: Option<Question>,
    /// It asked to be told when the program ends (`wait`) and has not been.
    /// Telling it is not delivery: the session is kept for the clients that
    /// were attached to it ([`Relay::drive`]).
    waiting: bool,
}

impl Client {
    /// The end of the program was queued for it and has left: it has been told.
    fn told(&self) -> bool {
        self.ended_queued && self.out.is_empty()
    }

    /// It is attached as a client that asked for the session to be kept for a
    /// while after it was told the program ended.
    fn keeps(&self) -> bool {
        self.attach
            .as_ref()
            .is_some_and(|attach| attach.keep_after_end)
    }
}

/// A question about how the session stands. It is answered at the end of the
/// turn it was read in ([`Relay::answer_questions`]), after everything the
/// relay found on every connection in that turn.
#[derive(Clone, Copy)]
enum Question {
    Status,
    List,
    /// The screen, as text or as a picture.
    Read(Capture),
}

struct Attach {
    /// The offset of the next byte owed.
    next: u64,
    /// The client asked for the session to be kept for a while once it has been
    /// told the program ended (`keep_after_end` in its view).
    keep_after_end: bool,
    /// What the client called itself in its view, for `status` to tell who is
    /// attached. It goes with the attach: a client that attaches again with no
    /// name has none, and one that detached is not listed.
    name: Option<ClientName>,
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
    /// When the program `child_pid` is started (Unix seconds), read while it
    /// lived: it cannot be read once the program has ended. `None` when it
    /// could not be.
    started: Option<u64>,
    /// When this relay last read output from the terminal (Unix seconds);
    /// `None` until it has. A relay that takes over starts without it.
    output_at: Option<u64>,
    exit: Option<Exit>,
    /// The version of the warden that keeps the session; empty if it did not
    /// say.
    warden_version: String,
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
    /// The journal's floor (where the past was last forgotten) that the screen has been
    /// brought to: it forgot its scrollback on reaching it, or began after it and had
    /// none from before to forget. A relay that starts builds its screen from what the ring
    /// still holds and forgets at the floor on the way, as the one before it did when it was
    /// asked.
    forgotten: u64,
    /// The end of the last stretch of output that held a question the terminal
    /// had an answer for, whoever sent it to the program (this relay with
    /// nobody attached, or a client's own terminal). A relay that takes over
    /// finds it again by feeding the screen the journal it holds, and counts
    /// what it cannot replay as answered: a picture costs less than a program
    /// answered twice.
    last_reply: u64,
    /// The scrollback rows the screen keeps.
    history: usize,
    /// Reused to carry journal bytes to the screen.
    scratch: Vec<u8>,
    clients: Vec<Client>,
    /// The end of the program has reached a client, so the warden may be told
    /// it can remove the session (when no wait stands in the way).
    ended_told: bool,
    /// The wait for the clients that asked to have the session kept.
    keep: EndKeep,
    /// How long that wait is.
    keep_for: Duration,
    delivered_sent: bool,
    /// An upgrade was asked of the warden through this relay and has not failed:
    /// if it is told to quit, it is being replaced and not ended.
    replacing: bool,
    quit: bool,
    /// The listening socket is left out of the poll set because there is no
    /// descriptor for another client (or accepting failed): it stays readable
    /// while connections wait, and waiting on it would spin. A client going
    /// frees a descriptor and ends the pause.
    accept_paused: bool,
    /// What the terminal was last found to be, and told to the clients that
    /// were attached then (`None` before the first look). A client that
    /// attaches later is told on its own, whatever this says.
    told: Option<TerminalState>,
    /// Looks at the terminal, and remembers the command line it read.
    watch: procinfo::Watch,
    /// The look that follows output or input by a moment.
    settle: Settle,
    /// Output was read in this turn of the loop: the terminal is looked at
    /// once at its end.
    output_read: bool,
    /// A descriptor set aside for the next connection, let go just before
    /// each `accept` so that one is free for it. An `accept` that finds none
    /// does not leave the connection waiting everywhere: macOS drops it, and
    /// a client that was waiting for a seat would be turned away. `None` when
    /// there was none to set aside: the relay is full.
    spare: Option<OwnedFd>,
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
        let last_reply = replayed_mark(&journal, screen_at);
        inner::write(&mut &chan, &ToWarden::Ready)
            .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
        // Which of its records this binary reads: what a warden that renews
        // itself needs to know before it does.
        inner::write(
            &mut &chan,
            &ToWarden::CanRenew {
                min: inner::RENEWAL_MIN_VERSION,
                max: inner::RENEWAL_VERSION,
            },
        )
        .map_err(|e| io::Error::new(io::ErrorKind::BrokenPipe, e.to_string()))?;
        chan.set_nonblocking(true)?;
        let mut relay = Relay {
            id: adopt.id,
            chan,
            chan_alive: true,
            chan_out: Vec::new(),
            decoder,
            master,
            journal,
            listener,
            child_pid: adopt.child_pid,
            started: if adopt.exit.is_none() {
                sys::started_at(adopt.child_pid)
            } else {
                None
            },
            output_at: None,
            exit: adopt.exit,
            warden_version: adopt.warden_version.unwrap_or_default(),
            live: false,
            reading: false,
            master_done: false,
            final_drained: false,
            input: Vec::new(),
            input_sent: 0,
            screen,
            screen_at,
            forgotten: 0,
            last_reply,
            history,
            scratch: vec![0; SCREEN_CHUNK],
            clients: Vec::new(),
            ended_told: false,
            keep: EndKeep::default(),
            keep_for: end_keep(),
            delivered_sent: false,
            replacing: false,
            quit: false,
            accept_paused: false,
            told: None,
            watch: procinfo::Watch::default(),
            settle: Settle::default(),
            output_read: false,
            spare: None,
            chunk,
        };
        relay.hold_spare();
        Ok(relay)
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
            // The look after output or input waits on the clock, and only while
            // there is someone to tell.
            if self.exit.is_some() || self.attached() == 0 {
                self.settle.cancel();
            }
            if sys::poll(&mut set, self.wake_in(Instant::now())).is_err() {
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
            self.answer_questions();
            self.look_at_the_terminal();
            for i in 0..self.clients.len() {
                self.drive(i);
            }
            let seated = self.clients.len();
            self.clients.retain(|client| !client.dead);
            if self.clients.len() < seated {
                self.hold_spare();
                self.accept_paused = false;
            }
            self.deliver_if_due(Instant::now());
            if self.is_orphaned() {
                self.quit = true;
            }
            if self.quit {
                // The terminal is not read any more: what was typed and not
                // yet taken is the last thing this relay does for it.
                self.flush_input();
                if self.leaving_for_good() {
                    self.forget_label();
                }
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
                // Nothing runs in the terminal any more: what the clients were
                // told (a password being asked, an ssh session) is over, and
                // no look at it follows.
                let now = self.terminal_now();
                self.tell_the_others(&now, usize::MAX);
            }
            ToRelay::UpgradeFailed(why) => self.upgrade_failed(&why),
            ToRelay::Renewed(version) => self.warden_version = version,
            ToRelay::Adopt(_) => {}
        }
    }

    /// The upgrade a client asked for did not happen: every client that asked
    /// is told why.
    fn upgrade_failed(&mut self, why: &str) {
        self.replacing = false;
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
                Ok(_) => {
                    self.feed_screen(Feed::Live);
                    self.output_read = true;
                    // The only clock this takes; what was read is not looked at.
                    self.output_at = sys::unix_now();
                    self.settle.arm(Instant::now());
                }
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

    // --- the terminal's state -----------------------------------------------

    /// The terminal as it is now; nothing once the program has ended.
    fn terminal_now(&mut self) -> TerminalState {
        if self.exit.is_some() {
            return TerminalState::default();
        }
        self.watch.look(self.master.as_fd(), self.child_pid)
    }

    /// At the end of a turn in which output was read, or when the look after
    /// output or input is due: looks at the terminal once, and tells the
    /// attached clients if it is not what they were told. At most one look in
    /// a turn, whichever of the two brought it, and none when nobody is
    /// attached (`status` looks when it is asked).
    fn look_at_the_terminal(&mut self) {
        let output = std::mem::take(&mut self.output_read);
        let due = self.settle.fire(Instant::now());
        if !(output || due) || self.exit.is_some() || self.attached() == 0 {
            return;
        }
        let now = self.terminal_now();
        self.tell_the_others(&now, usize::MAX);
    }

    /// Tells every attached client but `except` that the terminal is `now`,
    /// if that is not what it was last found to be. (The client that has just
    /// attached is told on its own: it has been told nothing before.)
    fn tell_the_others(&mut self, now: &TerminalState, except: usize) {
        if self.told.as_ref() == Some(now) {
            return;
        }
        self.told = Some(now.clone());
        for i in 0..self.clients.len() {
            if i != except && self.is_attached(i) {
                let session = Some(self.id.clone());
                self.queue(i, session, Message::TerminalState(now.clone()));
            }
        }
    }

    // --- the screen ---------------------------------------------------------

    /// Gives the screen the journal bytes it has not had, and the program the
    /// terminal's answers to the questions in them that are its to hear (`mode`
    /// says which).
    fn feed_screen(&mut self, mode: Feed) {
        let mut restarts = 0;
        loop {
            let total = self.journal.total();
            self.reach_floor();
            if self.screen_at >= total {
                return;
            }
            let mut wanted = usize::try_from(total - self.screen_at)
                .unwrap_or(usize::MAX)
                .min(self.scratch.len());
            // Not past the floor in one piece: the screen forgets its scrollback at it.
            let floor = self.journal.floor();
            if self.forgotten < floor {
                wanted = wanted.min(usize::try_from(floor - self.screen_at).unwrap_or(wanted));
            }
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
                    if self.answer(answers) && mode != Feed::Spent {
                        self.last_reply = self.last_reply.max(self.screen_at);
                    }
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
                    self.forgotten = 0;
                }
                None => {
                    self.screen_at = total;
                    return;
                }
            }
        }
    }

    /// Has the screen forget its scrollback if it stands at the journal's floor, where
    /// the past was forgotten: the rows before are gone from what a client is given, and
    /// the picture of the screen must not bring them back. A screen that began after the
    /// floor has none from before it.
    fn reach_floor(&mut self) {
        let floor = self.journal.floor();
        if self.forgotten < floor && self.screen_at >= floor {
            if self.screen_at == floor {
                self.screen.forget_history();
            }
            self.forgotten = floor;
        }
    }

    /// Sends the terminal's own answers to the program, when nobody else will:
    /// a client attached answers with its terminal, and two answers to one
    /// question would be typed into the program. Says whether the bytes just
    /// given to the screen held a question it has an answer for, sent or not.
    fn answer(&mut self, wanted: bool) -> bool {
        let answers = self.screen.take_replies();
        if answers.is_empty() {
            return false;
        }
        if wanted && self.exit.is_none() && self.attached() == 0 {
            // The program that does not read what it is owed gets no more.
            let _ = self.queue_input(&answers);
        }
        true
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
        let ended = self.exit.is_some() && self.final_drained;
        (ended && client.waiting)
            || client.attach.as_ref().is_some_and(|attach| {
                attach.picture.is_some()
                    || attach.next < self.journal.total()
                    || (ended && !client.ended_queued)
            })
    }

    /// Sets a descriptor aside for the next connection, if there is not one
    /// already; whether there is one now.
    fn hold_spare(&mut self) -> bool {
        if self.spare.is_none() {
            self.spare = self.listener.as_fd().try_clone_to_owned().ok();
        }
        self.spare.is_some()
    }

    fn accept(&mut self) {
        while self.live && !self.quit && self.clients.len() < MAX_CLIENTS {
            // The connection gets the spare's descriptor.
            self.spare = None;
            let accepted = self.listener.accept();
            // And another is set aside for the next one. None left means the
            // relay is full: it accepts nobody until a client goes.
            let room = self.hold_spare();
            let sock = match accepted {
                Ok((sock, _)) => Some(sock),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // A signal, or a connection that went away before it was
                // accepted.
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::Interrupted | io::ErrorKind::ConnectionAborted
                    ) =>
                {
                    None
                }
                // No descriptor after all (there was no spare to let go).
                Err(_) => {
                    self.accept_paused = true;
                    return;
                }
            };
            if let Some(sock) = sock {
                self.seat(sock);
            }
            if !room {
                self.accept_paused = true;
                return;
            }
        }
    }

    /// Takes in a connection that was accepted, if it is one to serve.
    fn seat(&mut self, sock: UnixStream) {
        let own = sys::effective_uid();
        let allowed = sys::peer_uid(sock.as_fd()).is_ok_and(|peer| sys::uid_allowed(peer, own));
        if !allowed || sock.set_nonblocking(true).is_err() {
            return;
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
            asked: None,
            waiting: false,
        });
    }

    /// Reads what client `i` has sent, until its socket is empty or it has
    /// gone ([`CLIENT_READS`] reads at most, so one busy client does not keep
    /// the others waiting): a client that says something and leaves is seen to
    /// have left in the same turn, before a question asked in it is answered.
    fn client_readable(&mut self, i: usize) {
        let mut buf = [0u8; 16 * 1024];
        for _ in 0..CLIENT_READS {
            let read = match self.clients.get_mut(i) {
                Some(client) if !client.dead && !client.closing => (&client.sock).read(&mut buf),
                _ => return,
            };
            match read {
                Ok(0) => return self.drop_client(i),
                Ok(n) => self.feed(i, buf.get(..n).unwrap_or_default()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return self.drop_client(i),
            }
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
        self.take_packets(i);
    }

    /// Acts on what client `i` has sent, in the order it was sent, up to its
    /// first question: what comes after one waits until it is answered.
    fn take_packets(&mut self, i: usize) {
        loop {
            let next = match self.clients.get_mut(i) {
                Some(client) if !client.dead && client.asked.is_none() => {
                    client.decoder.next_packet()
                }
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
        match packet.message {
            Message::Ping(bytes) => self.queue(i, packet.session, Message::Pong(bytes)),
            Message::List => self.ask(i, Question::List),
            Message::Status => self.ask(i, Question::Status),
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
            // These three are served to a connection that has not attached and
            // do not attach it: nothing about the session changes because one
            // of them is asked.
            Message::Write(strokes) => self.write(i, &strokes),
            Message::Read(capture) => self.ask(i, Question::Read(capture)),
            Message::Wait => {
                if let Some(client) = self.clients.get_mut(i) {
                    client.waiting = true;
                }
            }
            Message::Forget => self.forget(i),
            Message::DropInput => self.drop_input(i),
            Message::Label(_) => self.error(
                i,
                ErrorCode::BAD_REQUEST,
                "a label is set through the bridge, or by the command that makes one",
            ),
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
                self.replacing = true;
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
        // the screen stands in for what the stream would have shown. So it does
        // for a terminal that answers what it is shown and cannot hold its
        // answers back, when the bytes hold a question that was answered
        // already: it would answer a second time.
        let answered_twice = offset != OFFSET_END
            && offset < self.last_reply
            && view.is_some_and(|view| view.picture_if_answered);
        let truncated =
            (offset != OFFSET_END && offset < self.journal.retained_from()) || answered_twice;
        let picture = (truncated || view.is_some_and(|view| view.picture)).then(|| self.picture());
        let start = match &picture {
            Some(picture) => picture.offset,
            None if offset == OFFSET_END => total,
            None => offset,
        };
        if let Some(client) = self.clients.get_mut(i) {
            client.attach = Some(Attach {
                next: start,
                keep_after_end: view.is_some_and(|view| view.keep_after_end),
                name: view.and_then(|view| view.name),
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
                // A journal that keeps no track of answers counts as all
                // answered, as it does for the relay (`replayed_mark`).
                answered: self.journal.answered().unwrap_or(total),
            },
        );
        // Then what the terminal is doing: before the picture and the
        // output, which `drive` queues after it.
        let now = self.terminal_now();
        self.tell_the_others(&now, i);
        self.queue(i, Some(self.id.clone()), Message::TerminalState(now));
        if let Some(view) = view
            && view.take
        {
            self.let_the_others_go(i, view.name);
        }
    }

    /// Client `i` took the terminal: every other client that is attached is told
    /// so, with the name `i` gave itself (empty if it gave none), and is no longer
    /// attached. Their connections stay: they may ask what they could before
    /// (`status`, `read`, a `wait`), attach again, and the same connection of a
    /// bridge goes on serving its other sessions. What was queued for them stays
    /// queued and comes first; nothing more of the stream follows, a picture that
    /// was half sent is not finished, and what they type or resize is refused as
    /// from any connection that is not attached. A connection that is not attached
    /// (one that only waits or asks) is not a client of the terminal and is left
    /// alone.
    fn let_the_others_go(&mut self, i: usize, name: Option<ClientName>) {
        let text = name.map_or_else(String::new, |name| name.as_str().to_owned());
        for other in 0..self.clients.len() {
            if other == i || !self.is_attached(other) {
                continue;
            }
            if let Some(client) = self.clients.get_mut(other) {
                client.attach = None;
            }
            self.error(other, ErrorCode::TAKEN, &text);
        }
    }

    /// A client asks the session to forget its past: the screen drops its scrollback
    /// and a client that comes for bytes before this moment is sent a picture, as if the
    /// ring had lapped. Those who are attached are not touched (see the journal's header).
    fn forget(&mut self, i: usize) {
        if !self.is_attached(i) {
            return self.error(i, ErrorCode::NOT_ATTACHED, "attach first");
        }
        // The past is what the screen has had; its scrollback is forgotten at the offset
        // it stands at, which is also the floor.
        self.feed_screen(Feed::Spent);
        self.journal.set_floor(self.screen_at);
        self.reach_floor();
    }

    /// A client asks the session to drop the input the program has not read: what the
    /// terminal still holds for it, and what this relay still holds back for the terminal
    /// (the terminal takes only so much before the rest waits in `input`). The program's
    /// output, the screen and the journal are not touched, and nothing is said, whether
    /// there was anything or not.
    fn drop_input(&mut self, i: usize) {
        if !self.is_attached(i) {
            return self.error(i, ErrorCode::NOT_ATTACHED, "attach first");
        }
        // With the program gone there is nobody to read what is left, and the terminal's
        // far side may be nobody's: not opened again.
        if self.exit.is_some() || self.master_done {
            return;
        }
        self.input.clear();
        self.input_sent = 0;
        // A failure is a terminal with nothing on the other side: nothing to read, nothing
        // to say (an `error` here would be read as the end of the attachment by a client
        // that has no request of this kind to answer).
        let _ = pty::drop_unread_input(self.master.as_fd());
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
            return;
        }
        // What is typed may start a program that says nothing.
        self.settle.arm(Instant::now());
    }

    /// Types what a `write` says, as one piece, for a connection that has not
    /// attached (and for one that has: who types it does not matter). A key is
    /// the bytes the screen's modes make of it, so the screen is first given
    /// everything the program has written.
    fn write(&mut self, i: usize, strokes: &[Stroke]) {
        if self.exit.is_some() {
            return self.error(i, ErrorCode::ENDED, "the program has ended");
        }
        self.catch_up();
        let typed = match typing::bytes(strokes, &self.screen.modes()) {
            Ok(typed) => typed,
            Err(why) => return self.error(i, ErrorCode::BAD_REQUEST, &why),
        };
        if !self.queue_input(&typed) {
            return self.error(i, ErrorCode::INPUT_FULL, "the program is not taking input");
        }
        // What is typed may start a program that says nothing; there is a look
        // to be made only for somebody who is shown the result.
        if self.attached() > 0 {
            self.settle.arm(Instant::now());
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

    fn ask(&mut self, i: usize, question: Question) {
        if let Some(client) = self.clients.get_mut(i) {
            client.asked = Some(question);
        }
    }

    /// Answers the questions asked in this turn, now that everything every
    /// client sent in it has been acted on: a client that left or detached
    /// before somebody asked how the session stands is not counted, whichever
    /// connection either of them came on. (Answered when it was read, a
    /// question would miss what the connections looked at after its own
    /// brought in the same turn.) Each asker's requests after its question
    /// are then taken up, in order, and a question among them is answered in
    /// the next round.
    fn answer_questions(&mut self) {
        loop {
            let asked: Vec<(usize, Question)> = self
                .clients
                .iter_mut()
                .enumerate()
                .filter_map(|(i, client)| client.asked.take().map(|question| (i, question)))
                .collect();
            if asked.is_empty() {
                return;
            }
            // Looking at the terminal is for the ones that ask how the session
            // stands, once for all of them.
            let status = asked
                .iter()
                .any(|(_, question)| matches!(question, Question::Status | Question::List))
                .then(|| self.status());
            for (i, question) in asked {
                match (question, &status) {
                    (Question::Status, Some(status)) => self.queue(
                        i,
                        Some(self.id.clone()),
                        Message::StatusReply(status.clone()),
                    ),
                    (Question::List, Some(status)) => {
                        self.queue(i, None, Message::ListReply(vec![status.clone()]));
                    }
                    (Question::Read(capture), _) => self.answer_read(i, capture),
                    _ => {}
                }
                self.take_packets(i);
            }
        }
    }

    /// The screen as of now, for client `i`: the text of it, or the bytes that
    /// build it, in pieces. It is asked at the end of a turn, so it is of
    /// everything this relay has read from the program; the screen is given what
    /// it has not had first, as the output is (`Live`, or `Held` for a relay that
    /// was not reading yet), because a question in those bytes is still the
    /// program's to have answered: a screen that took them as spent would mark
    /// it answered and never answer it.
    fn answer_read(&mut self, i: usize, capture: Capture) {
        // A client that asks and does not read what it is sent is dropped, as
        // for any other answer; one answer is as large as a screen is.
        if self
            .clients
            .get(i)
            .is_some_and(|client| client.out.len().saturating_sub(client.sent) > OUT_LIMIT)
        {
            return self.drop_client(i);
        }
        self.catch_up();
        let (cols, rows) = self.screen.size();
        let offset = self.screen_at;
        let picture = capture.picture;
        let bytes = if picture {
            self.screen.image()
        } else {
            let history = usize::try_from(capture.history).unwrap_or(usize::MAX);
            let mut text = String::new();
            for line in self.screen.text(history, capture.unwrap) {
                text.push_str(&line);
                text.push('\n');
            }
            text.into_bytes()
        };
        let Some(client) = self.clients.get_mut(i) else {
            return;
        };
        let mut pieces = bytes.chunks(OUT_CHUNK).peekable();
        loop {
            let data = pieces.next().unwrap_or_default().to_vec();
            let more = pieces.peek().is_some();
            let message = if picture {
                Message::Picture {
                    offset,
                    cols,
                    rows,
                    more,
                    data,
                }
            } else {
                Message::ScreenText {
                    offset,
                    cols,
                    rows,
                    more,
                    data,
                }
            };
            if Packet::about(self.id.clone(), message)
                .encode(&mut client.out)
                .is_err()
            {
                client.dead = true;
                return;
            }
            if !more {
                return;
            }
        }
    }

    /// Gives the screen what it has not had of the journal.
    fn catch_up(&mut self) {
        self.feed_screen(if self.reading { Feed::Live } else { Feed::Held });
    }

    /// The session's label: what its file says (see [`sockdir::label_path`]).
    /// The directory is the listening socket's, so that this does not depend on
    /// where the relay was started from.
    fn label(&self) -> Option<String> {
        let address = self.listener.local_addr().ok()?;
        let dir = address.as_pathname()?.parent()?;
        sockdir::read_label(dir, &self.id)
    }

    /// Whether this relay going is the end of the session, and not its being
    /// handed to another relay. The label is the session's and not this relay's:
    /// a relay that is replaced leaves it, one that leaves a session that is over
    /// takes it away. The warden only ends a session whose program has ended (its
    /// end was delivered, or nobody came for it in a day), and refuses to replace
    /// the relay of one, so a relay told to quit that knows the program ended is
    /// the last, unless it is the one that asked to be replaced (the program may
    /// end while the swap is under way). One whose warden is gone and whose
    /// terminal is closed has nothing left to serve either.
    fn leaving_for_good(&self) -> bool {
        self.is_orphaned() || (self.exit.is_some() && !self.replacing)
    }

    /// Takes the label file away: the session is gone.
    fn forget_label(&self) {
        if let Some(address) = self.listener.local_addr().ok()
            && let Some(dir) = address.as_pathname().and_then(std::path::Path::parent)
        {
            let _ = sockdir::remove_label(dir, &self.id);
        }
    }

    fn status(&mut self) -> Status {
        let mut status = Status::new(self.id.clone());
        status.pid = self.child_pid;
        if self.exit.is_none() {
            // Read late if it could not be read at the start: it is lost once
            // the program has ended.
            if self.started.is_none() {
                self.started = sys::started_at(self.child_pid);
            }
            let (terminal, front) = self
                .watch
                .look_with_pid(self.master.as_fd(), self.child_pid);
            status.program = terminal.program;
            status.cwd = terminal.cwd;
            status.secret = terminal.secret;
            status.remote = terminal.remote;
            status.remote_port = terminal.remote_port;
            let own = front == self.child_pid;
            status.front_is_own = Some(own);
            status.front_started = if own {
                self.started
            } else {
                sys::started_at(front)
            };
            status.output_at = self.output_at;
        }
        status.started = self.started;
        status.size = sys::window_size(self.master.as_fd()).unwrap_or_default();
        // The count and the names are one pass over the clients, so a client
        // that is counted is named (or said to have no name) in the same answer.
        status.clients_named = self
            .clients
            .iter()
            .filter(|client| !client.dead)
            .filter_map(|client| client.attach.as_ref())
            .take(MAX_CLIENTS)
            .map(|attach| attach.name)
            .collect();
        status.clients = status.clients_named.len() as u32;
        status.state = if self.exit.is_some() {
            State::ENDED
        } else {
            State::RUNNING
        };
        status.offset = self.journal.total();
        status.retained_from = self.journal.retained_from();
        status.exit = self.exit;
        status.version = crate::release();
        status.warden_version.clone_from(&self.warden_version);
        status.label = self.label();
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
                // A connection that has not attached is owed the end of the
                // program if it waits for it, and nothing else.
                if let (true, Some(exit), true) = (ended_ready, self.exit, client.waiting) {
                    let packet = Packet::about(self.id.clone(), Message::Ended(exit));
                    if packet.encode(&mut client.out).is_err() {
                        client.dead = true;
                        return;
                    }
                    client.waiting = false;
                    continue;
                }
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
                client.waiting = false;
            } else if let (true, Some(exit), true) = (ended_ready, self.exit, client.waiting) {
                // It asked to be told after it was (or without being attached,
                // and then attached): told again, and the end it was told as
                // an attached client stays the one that counts as delivered.
                let packet = Packet::about(self.id.clone(), Message::Ended(exit));
                if packet.encode(&mut client.out).is_err() {
                    client.dead = true;
                    return;
                }
                client.waiting = false;
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
        let oldest = self.journal.ring_from();
        if client
            .attach
            .as_ref()
            .is_some_and(|attach| attach.next < oldest)
        {
            client.dead = true;
            return;
        }
        if client.told() {
            // The end of the shell has reached a client. Whether the session
            // may go now is `deliver_if_due`'s to say; a client that asked for
            // it to be kept starts the wait that holds it.
            let keeps = client.keeps();
            self.ended_told = true;
            if keeps {
                self.keep.begin(Instant::now(), self.keep_for);
            }
        }
    }

    /// How long a `poll` may wait at `now`: for ever unless a look at the
    /// terminal or the end of a session's wait is due.
    fn wake_in(&self, now: Instant) -> Option<Duration> {
        let keep = if self.delivered_sent {
            None
        } else {
            self.keep.timeout(now)
        };
        match (self.settle.timeout(now), keep) {
            (Some(look), Some(keep)) => Some(look.min(keep)),
            (look, keep) => look.or(keep),
        }
    }

    /// Tells the warden the session may be removed, once, when it is time.
    ///
    /// The end has to have reached a client. If one that asked for the session
    /// to be kept has been told, the time is the end of the wait it started,
    /// whoever else has been told and whether or not that client is still here:
    /// the program it was for comes later. A client that asked and is attached
    /// but has not been told yet (the program ended while its socket was full)
    /// is not left with nothing to read when another is told first: that
    /// telling starts the wait as well, so a connection that is behind holds the
    /// session for the wait and no longer.
    fn deliver_if_due(&mut self, now: Instant) {
        if self.delivered_sent || !self.ended_told {
            return;
        }
        if !self.keep.started()
            && self
                .clients
                .iter()
                .any(|client| !client.dead && !client.told() && client.keeps())
        {
            self.keep.begin(now, self.keep_for);
        }
        let due = if self.keep.started() {
            self.keep.over(now)
        } else {
            true
        };
        if due {
            self.delivered_sent = true;
            self.send_warden(&ToWarden::Delivered);
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

/// Where `last_reply` starts for a relay that feeds its screen from
/// `replay_start` on: what it will not replay counts as answered, so does all
/// of a journal that keeps no track of answers (an older warden's), and the
/// replay moves the mark to the end of what held a question.
fn replayed_mark(journal: &Journal, replay_start: u64) -> u64 {
    match journal.answered() {
        Some(_) => replay_start,
        None => journal.total(),
    }
}

/// Where the screen of a journal begins: the start of the stream when nothing
/// was dropped and no more than `limit` bytes are there, else the first line
/// break in the last `limit` bytes that are still in the ring, so that the
/// screen does not start inside an escape sequence.
fn replay_start(journal: &Journal, limit: u64) -> u64 {
    let from = journal.total().saturating_sub(limit);
    if from == 0 && journal.ring_from() == 0 {
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
        take_over(journal).0
    }

    /// As [`answers_after_taking_over`], and where the relay then says the last
    /// question it knows of was answered.
    fn take_over(journal: Journal) -> (Vec<u8>, u64) {
        let (mut relay, slave) = relay_over(journal);
        relay.on_warden(ToRelay::Go);
        (typed_to_the_program(&slave), relay.last_reply)
    }

    /// What the program finds waiting to be read: what was typed to it.
    fn typed_to_the_program(slave: &OwnedFd) -> Vec<u8> {
        let mut set = [sys::want(slave.as_raw_fd(), libc::POLLIN)];
        let mut got = Vec::new();
        while sys::poll(&mut set, Some(std::time::Duration::from_millis(300))).unwrap() > 0 {
            let mut buf = [0u8; 64];
            match sys::read_fd(slave.as_fd(), &mut buf) {
                Ok(n) if n > 0 => got.extend_from_slice(buf.get(..n).unwrap_or_default()),
                _ => break,
            }
        }
        got
    }

    /// A relay over a terminal whose program reads what arrives as it is, and
    /// the program's side of it; the relay has been told nothing yet.
    fn relay_over(journal: Journal) -> (Relay, OwnedFd) {
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
        let relay = Relay {
            id: SessionId::new("held").unwrap(),
            chan,
            chan_alive: true,
            chan_out: Vec::new(),
            decoder: Decoder::<ToRelay>::new(),
            master: pty.master,
            screen: Screen::new(80, 24, 10),
            screen_at: replay_start(&journal, REPLAY_LIMIT),
            forgotten: 0,
            last_reply: replayed_mark(&journal, replay_start(&journal, REPLAY_LIMIT)),
            history: 10,
            scratch: vec![0; SCREEN_CHUNK],
            journal,
            listener,
            child_pid: 1,
            started: None,
            output_at: None,
            exit: None,
            warden_version: String::new(),
            live: false,
            reading: false,
            master_done: false,
            final_drained: false,
            input: Vec::new(),
            input_sent: 0,
            clients: Vec::new(),
            ended_told: false,
            keep: EndKeep::default(),
            keep_for: end_keep(),
            delivered_sent: false,
            replacing: false,
            quit: false,
            accept_paused: false,
            told: None,
            watch: procinfo::Watch::default(),
            settle: Settle::default(),
            output_read: false,
            spare: None,
            chunk: 4096,
        };
        let _ = std::fs::remove_file(&path);
        (relay, pty.slave)
    }

    #[test]
    fn reading_the_screen_does_not_take_a_question_in_the_last_bytes_for_answered() {
        // The program asked at the very end of what it wrote, and the screen has
        // not had those bytes: a read gives them to it as the relay's own
        // reading would, so the question is answered, once, and the read finds
        // the cursor where the answer says it is. (Given as spent, they would
        // be marked answered and never answered.)
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"text\x1b[6n");
        let (mut relay, slave) = relay_over(journal);
        relay.live = true;
        relay.reading = true;
        let (ours, _theirs) = UnixStream::pair().unwrap();
        relay.seat(ours);
        relay.answer_read(0, Capture::default());
        assert_eq!(typed_to_the_program(&slave), b"\x1b[1;5R");
        assert_eq!(relay.journal.answered(), Some(relay.journal.total()));
        assert_eq!(relay.screen_at, relay.journal.total());
        let answer = relay.clients.first().map(|client| client.out.clone());
        let mut decoder = PacketDecoder::new();
        decoder.push(&answer.unwrap());
        let packet = decoder.next_packet().unwrap().unwrap();
        assert!(matches!(
            packet.message,
            Message::ScreenText {
                offset: 8,
                more: false,
                ..
            }
        ));
    }

    #[test]
    fn a_relay_leaves_the_label_when_it_is_replaced_and_takes_it_when_the_session_is_over() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        let (mut relay, _slave) = relay_over(journal);
        let dir = std::env::temp_dir();
        let id = relay.id.clone();
        let label = sockdir::label_path(&dir, &id);
        std::fs::write(&label, "mine").unwrap();
        assert_eq!(relay.label().as_deref(), Some("mine"));
        // The program lives: whatever tells this relay to quit, it is a swap.
        assert!(!relay.leaving_for_good());
        // It ended and nobody asked for a swap: the warden is ending the session
        // (its end was delivered, or the day it is kept for is over).
        relay.exit = Some(Exit::Code(0));
        assert!(relay.leaving_for_good());
        // It ended while this relay was being replaced.
        relay.replacing = true;
        assert!(!relay.leaving_for_good());
        // The upgrade failed: it is not being replaced after all.
        relay.upgrade_failed("no");
        assert!(relay.leaving_for_good());
        relay.forget_label();
        assert_eq!(relay.label(), None);
        assert!(!label.exists());
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
    fn a_relay_that_takes_over_finds_the_last_question_in_what_it_replays() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.set_answered(journal.total());
        journal.append(b"text");
        // Answered by whoever read it, and found again when the screen is fed:
        // the text after it holds no question.
        assert_eq!(take_over(journal), (Vec::new(), 4));
    }

    #[test]
    fn the_mark_is_the_end_of_the_piece_of_output_that_held_the_question() {
        let mut journal = Journal::create(1 << 18).unwrap();
        journal.append(b"\x1b[6n");
        journal.append(&vec![b'x'; SCREEN_CHUNK + 100]);
        let total = journal.total();
        let (_, mark) = take_over(journal);
        assert_eq!(
            mark, SCREEN_CHUNK as u64,
            "a piece at a time, not all of it"
        );
        assert!(mark < total);
    }

    #[test]
    fn a_question_the_relay_answers_moves_the_mark_to_the_end_of_what_it_read() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        assert_eq!(take_over(journal), (b"\x1b[1;1R".to_vec(), 4));
    }

    #[test]
    fn output_with_no_question_leaves_the_mark_where_it_was() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"nothing asked here");
        assert_eq!(take_over(journal), (Vec::new(), 0));
    }

    #[test]
    fn a_journal_that_keeps_no_track_counts_all_of_it_as_answered() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.forget_answers();
        assert_eq!(take_over(journal), (Vec::new(), 4));
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

    #[test]
    fn the_look_after_output_is_a_timer_that_fires_once_and_rests() {
        let start = Instant::now();
        let mut settle = Settle::default();
        // Not armed: the relay's `poll` waits for ever.
        assert_eq!(settle.timeout(start), None);
        assert!(!settle.fire(start + STATE_SETTLE * 10));

        settle.arm(start);
        assert_eq!(settle.timeout(start), Some(STATE_SETTLE));
        let half = STATE_SETTLE / 2;
        assert_eq!(settle.timeout(start + half), Some(STATE_SETTLE - half));
        assert!(!settle.fire(start + half));
        // Once it is due the poll does not wait, and the look happens once.
        assert_eq!(
            settle.timeout(start + STATE_SETTLE * 2),
            Some(Duration::ZERO)
        );
        assert!(settle.fire(start + STATE_SETTLE));
        assert!(!settle.fire(start + STATE_SETTLE * 2));
        assert_eq!(settle.timeout(start + STATE_SETTLE * 2), None);

        // More output pushes it back; nobody to tell cancels it.
        settle.arm(start);
        settle.arm(start + half);
        assert!(!settle.fire(start + STATE_SETTLE));
        assert!(settle.fire(start + half + STATE_SETTLE));
        settle.arm(start);
        settle.cancel();
        assert_eq!(settle.timeout(start), None);
    }

    #[test]
    fn the_wait_after_the_end_is_a_timer_that_starts_once_and_fires_once() {
        let start = Instant::now();
        let window = Duration::from_secs(600);
        let mut keep = EndKeep::default();
        // Not started: the relay's `poll` waits for ever.
        assert!(!keep.started());
        assert_eq!(keep.timeout(start), None);
        assert!(!keep.over(start + window * 10));

        keep.begin(start, window);
        assert!(keep.started());
        assert_eq!(keep.timeout(start), Some(window));
        assert!(!keep.over(start + window - Duration::from_millis(1)));
        assert!(keep.over(start + window));
        assert_eq!(keep.timeout(start + window * 2), Some(Duration::ZERO));
        // A client that is told later does not push the end back.
        keep.begin(start + window / 2, window);
        assert_eq!(keep.timeout(start), Some(window));
    }

    /// A client over a socket whose far end is returned (it must outlive the
    /// test, or the relay's writes find it closed), attached as one that asks
    /// for the session to be kept or as one that does not, and not yet told.
    fn client_of(keeps: bool) -> (Client, UnixStream) {
        let (sock, far) = UnixStream::pair().unwrap();
        sock.set_nonblocking(true).unwrap();
        let client = Client {
            sock,
            open: true,
            hello: Vec::new(),
            decoder: PacketDecoder::new(),
            out: Vec::new(),
            sent: 0,
            attach: Some(Attach {
                next: 0,
                keep_after_end: keeps,
                name: None,
                picture: None,
            }),
            ended_queued: false,
            closing: false,
            dead: false,
            upgrading: false,
            asked: None,
            waiting: false,
        };
        (client, far)
    }

    /// A relay whose program has ended, with no client yet and a wait of ten
    /// minutes for the ones that ask.
    fn ended_relay() -> (Relay, OwnedFd) {
        let (mut relay, slave) = relay_over(Journal::create(MIN_CAPACITY).unwrap());
        relay.exit = Some(Exit::Code(0));
        relay.final_drained = true;
        relay.keep_for = END_KEEP;
        (relay, slave)
    }

    #[test]
    fn a_client_that_did_not_ask_is_delivered_to_at_once() {
        let (mut relay, _slave) = ended_relay();
        let (client, _far) = client_of(false);
        relay.clients.push(client);
        let now = Instant::now();
        // Nothing to tell yet: nothing is delivered and the relay waits for ever.
        relay.deliver_if_due(now);
        assert!(!relay.delivered_sent);
        assert_eq!(relay.wake_in(now), None);

        relay.drive(0);
        relay.deliver_if_due(now);
        assert!(relay.delivered_sent, "told, so the session may go");
        assert_eq!(relay.wake_in(now), None);
    }

    #[test]
    fn a_client_that_asked_keeps_the_session_for_the_wait_whoever_else_is_told() {
        let (mut relay, _slave) = ended_relay();
        let (keeper, _far_keeper) = client_of(true);
        let (plain, _far_plain) = client_of(false);
        relay.clients.push(plain);
        relay.clients.push(keeper);
        relay.drive(0);
        relay.drive(1);
        let now = Instant::now();
        relay.deliver_if_due(now);
        assert!(!relay.delivered_sent, "told, but one of them asked");
        let due = relay.wake_in(now).unwrap();
        assert!(
            due > END_KEEP - Duration::from_secs(5) && due <= END_KEEP,
            "{due:?}"
        );

        // Nobody is here any more: the end of the wait is what delivers.
        relay.clients.clear();
        relay.deliver_if_due(now + END_KEEP - Duration::from_secs(5));
        assert!(!relay.delivered_sent);
        relay.deliver_if_due(now + END_KEEP + Duration::from_secs(1));
        assert!(relay.delivered_sent);
        assert_eq!(relay.wake_in(now), None, "it fired once");
    }

    #[test]
    fn a_client_that_asked_and_has_not_been_told_is_not_left_with_nothing_by_the_others() {
        let (mut relay, _slave) = ended_relay();
        let (plain, _far_plain) = client_of(false);
        let (keeper, _far_keeper) = client_of(true);
        relay.clients.push(plain);
        relay.clients.push(keeper);
        // The plain one has been told; the other's socket was full.
        relay.drive(0);
        let now = Instant::now();
        relay.deliver_if_due(now);
        assert!(!relay.delivered_sent, "one that asked is still to be told");
        // The wait started with the telling of the other, so a connection that
        // is behind holds the session for it and no longer.
        let due = relay.wake_in(now).unwrap();
        assert!(due <= END_KEEP, "{due:?}");

        // Told in time, the one that asked has what is left of it.
        relay.drive(1);
        relay.deliver_if_due(now);
        assert!(!relay.delivered_sent);
        relay.deliver_if_due(now + END_KEEP + Duration::from_secs(1));
        assert!(relay.delivered_sent);
    }

    #[test]
    fn a_client_that_asked_and_never_reads_holds_the_session_for_the_wait_and_no_longer() {
        let (mut relay, _slave) = ended_relay();
        let (plain, _far_plain) = client_of(false);
        let (keeper, _far_keeper) = client_of(true);
        relay.clients.push(plain);
        relay.clients.push(keeper);
        relay.drive(0);
        let now = Instant::now();
        relay.deliver_if_due(now);
        assert!(!relay.delivered_sent);
        // It is still attached and still has not been told.
        relay.deliver_if_due(now + END_KEEP + Duration::from_secs(1));
        assert!(relay.delivered_sent);
    }

    #[test]
    fn a_client_that_asked_and_went_before_it_was_told_does_not_hold_the_others() {
        let (mut relay, _slave) = ended_relay();
        let (plain, _far_plain) = client_of(false);
        let (mut keeper, _far_keeper) = client_of(true);
        keeper.dead = true;
        relay.clients.push(plain);
        relay.clients.push(keeper);
        relay.drive(0);
        relay.deliver_if_due(Instant::now());
        assert!(relay.delivered_sent);
    }
    /// The numbered lines `from..=to`, each ended the way a terminal ends one.
    fn rows(from: u32, to: u32) -> Vec<u8> {
        (from..=to)
            .flat_map(|n| format!("row{n:02}\r\n").into_bytes())
            .collect()
    }

    #[test]
    fn a_forget_leaves_the_client_that_is_owed_bytes_attached_and_owed() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(&rows(1, 5));
        let (mut relay, _slave) = relay_over(journal);
        let (client, _far) = client_of(false);
        relay.clients.push(client);
        let total = relay.journal.total();
        relay.forget(0);
        assert_eq!(relay.journal.floor(), total);
        assert_eq!(relay.journal.retained_from(), total);
        // It had not been given a byte: the ring has them, and it is not the one the
        // ring has left behind.
        relay.drive(0);
        let client = relay.clients.first().unwrap();
        assert!(!client.dead);
        assert_eq!(client.attach.as_ref().unwrap().next, total);
        // One that comes for what the floor passed is told it is gone.
        let (mut late, _far) = client_of(false);
        late.attach = None;
        relay.clients.push(late);
        relay.attach(1, 0, None);
        let queued = relay.clients.get(1).unwrap().out.clone();
        let mut decoder = PacketDecoder::new();
        decoder.push(&queued);
        let packet = decoder.next_packet().unwrap().unwrap();
        assert!(
            matches!(
                packet.message,
                Message::Attached {
                    truncated: true,
                    ..
                }
            ),
            "{:?}",
            packet.message
        );
    }

    #[test]
    fn a_forget_from_a_connection_that_is_not_attached_changes_nothing() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(&rows(1, 40));
        let (mut relay, _slave) = relay_over(journal);
        let (mut client, _far) = client_of(false);
        client.attach = None;
        relay.clients.push(client);
        relay.feed_screen(Feed::Spent);
        let before = relay.screen.text(10, false);
        relay.forget(0);
        assert_eq!(relay.journal.floor(), 0);
        assert_eq!(relay.screen.text(10, false), before);
        let mut decoder = PacketDecoder::new();
        decoder.push(&relay.clients.first().unwrap().out);
        let packet = decoder.next_packet().unwrap().unwrap();
        assert!(
            matches!(
                packet.message,
                Message::Error {
                    code: ErrorCode::NOT_ATTACHED,
                    ..
                }
            ),
            "{:?}",
            packet.message
        );
    }

    #[test]
    fn a_relay_that_comes_after_a_forget_forgets_at_the_same_place() {
        // 40 lines, the past forgotten, three more lines. The screen is 24 rows and keeps
        // 10 rows of scrollback; the lines the floor passed are not among them, the ones
        // on the screen at the floor are, and a screen built from the floor would not
        // have those on it.
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(&rows(1, 40));
        journal.set_floor(journal.total());
        journal.append(&rows(41, 43));
        assert_eq!(replay_start(&journal, REPLAY_LIMIT), 0);
        let (mut relay, _slave) = relay_over(journal);
        relay.feed_screen(Feed::Spent);
        let text = relay.screen.text(10, false);
        assert_eq!(relay.forgotten, relay.journal.floor());
        assert_eq!(text.len(), 24 + 3, "{text:?}");
        assert_eq!(text.first().map(String::as_str), Some("row18"), "{text:?}");
        assert!(text.iter().any(|row| row == "row40"), "{text:?}");

        // The same journal without the floor keeps the lines before it.
        let mut plain = Journal::create(MIN_CAPACITY).unwrap();
        plain.append(&rows(1, 43));
        let (mut relay, _slave) = relay_over(plain);
        relay.feed_screen(Feed::Spent);
        assert_eq!(relay.screen.text(10, false).len(), 24 + 10);
    }

    #[test]
    fn a_screen_that_begins_after_the_floor_has_nothing_from_before_it_to_forget() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(&rows(1, 10));
        journal.set_floor(journal.total());
        journal.append(&rows(11, 30));
        let (mut relay, _slave) = relay_over(journal);
        // The replay starts at a line past the floor (a long journal's limit).
        relay.screen_at = relay.journal.line_start_from(relay.journal.floor() + 3);
        relay.feed_screen(Feed::Spent);
        assert_eq!(relay.forgotten, relay.journal.floor());
        assert_eq!(relay.screen_at, relay.journal.total());
    }

    /// Client 0 of `relay` sends `message` to it.
    fn dropping(relay: &mut Relay, message: Message) {
        let id = relay.id.clone();
        relay.handle(0, Packet::about(id, message));
    }

    #[test]
    fn the_input_a_program_has_not_read_is_dropped_wherever_it_waits_and_what_comes_later_arrives()
    {
        let (mut relay, slave) = relay_over(Journal::create(MIN_CAPACITY).unwrap());
        let (client, _far) = client_of(false);
        relay.clients.push(client);

        // More than a terminal takes: part of it is in the terminal and part is held back
        // by the relay until the terminal takes it, and a flush of the terminal alone would
        // leave the relay writing the rest.
        assert!(relay.queue_input(&vec![b'x'; 200 * 1024]));
        assert!(relay.input_pending() > 0, "some of it waits in the relay");
        // The program has written something that nobody has read yet.
        assert_eq!(sys::write_fd(slave.as_fd(), b"output").unwrap(), 6);

        dropping(&mut relay, Message::DropInput);
        assert_eq!(relay.input_pending(), 0);
        assert!(relay.clients[0].out.is_empty(), "nothing is said");
        assert_eq!(
            typed_to_the_program(&slave),
            b"",
            "the program finds nothing"
        );

        // The terminal takes input as before.
        assert!(relay.queue_input(b"later"));
        assert_eq!(typed_to_the_program(&slave), b"later");

        // What the program wrote is still there for the relay to read.
        let mut buf = [0u8; 64];
        let n = sys::read_fd(relay.master.as_fd(), &mut buf).unwrap();
        assert_eq!(&buf[..n], b"output");

        // Nothing waiting is nothing to do.
        dropping(&mut relay, Message::DropInput);
        assert!(relay.clients[0].out.is_empty());
        assert_eq!(typed_to_the_program(&slave), b"");
        assert!(relay.queue_input(b"again"));
        assert_eq!(typed_to_the_program(&slave), b"again");
    }

    #[test]
    fn a_drop_of_input_from_a_connection_that_is_not_attached_is_refused_and_drops_nothing() {
        let (mut relay, slave) = relay_over(Journal::create(MIN_CAPACITY).unwrap());
        let (mut client, _far) = client_of(false);
        client.attach = None;
        relay.clients.push(client);
        assert!(relay.queue_input(b"kept"));

        dropping(&mut relay, Message::DropInput);
        let mut said = PacketDecoder::new();
        said.push(&relay.clients[0].out);
        let Some(Packet {
            message: Message::Error { code, .. },
            ..
        }) = said.next_packet().unwrap()
        else {
            panic!("an error was expected");
        };
        assert_eq!(code, ErrorCode::NOT_ATTACHED);
        assert_eq!(typed_to_the_program(&slave), b"kept");
    }

    #[test]
    fn a_drop_of_input_after_the_program_has_ended_does_nothing() {
        let (mut relay, _slave) = ended_relay();
        let (client, _far) = client_of(false);
        relay.clients.push(client);
        dropping(&mut relay, Message::DropInput);
        assert!(relay.clients[0].out.is_empty(), "nothing is said");
    }
}
