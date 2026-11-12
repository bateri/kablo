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
//! holds is let go (it comes back with its offset and is told, by
//! `truncated`, what it lost); it never slows the terminal's reading.
//!
//! **The relay keeps the screen too.** Every byte read from the terminal goes
//! to the journal and to a [`Screen`] (the parser bateri uses): right after the
//! read while nobody is attached, and behind it, by a thread of its own, while a
//! client is (see [`crate::mirror`]), so the read-and-forward path never waits for
//! the parser. Whatever looks at the screen first gives it what it has not had, so
//! it is always the screen of exactly the journal's `total` when seen. A client
//! that comes for bytes the ring has dropped, or asks for one, is sent a picture
//! of that screen (VT bytes that build it) and then the journal from the
//! picture's offset; the program is not asked to draw again. The terminal's own
//! questions (device attributes, the cursor position) are answered by that
//! screen only while no client is attached, so a program that asks while nobody
//! looks is not left waiting and one that is watched hears one answer, the
//! client's. A session that was started to wait for its first client
//! (`NewSession::wait_for_client`) answers nothing until that client has attached
//! or the wait has passed: a program started with its session asks before anybody
//! can have attached, and is to hear the answer of the terminal it was started
//! for, not the screen's. That client is shown what was kept: from the offset it
//! asks for, or after a picture of the screen as it stood before. A client whose
//! terminal answers what it is shown and cannot hold
//! its answers back says so in its view, and is sent a picture rather than
//! bytes that hold a question already answered (`last_reply`); any other
//! client is told how far the questions were answered (`answered`) and keeps
//! its own answers back. A relay that starts (a replacement, or a restart after
//! a crash) has no screen to carry over and builds it again from what the
//! journal holds; what the journal may no longer hold is the shell's marks, and
//! a relay that is replaced leaves those to the next in a file. After a crash
//! they are lost, and the shell tells them again at its next prompt.
//!
//! **The terminal's state is told, not polled.** What only the server can see
//! of the terminal (the program in the foreground, whether a password is being
//! asked for, where an ssh session goes) is told to every attached client as a
//! `state` when it differs from what was last told: right after `attached`,
//! at the end of a turn that read output (not more often than [`LOOK_INTERVAL`]),
//! and once more [`STATE_SETTLE`] after the last output or typed input (a program
//! that says nothing). Nobody
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
//! successor cannot rebuild from the journal's header, the descriptors and the
//! marks it left ([`sockdir::marks_path`]).
//! A client's `upgrade` is passed to the warden, which starts the other binary
//! as a second relay; when that one is set up this one is told to quit
//! (`Quit`): it stops reading the terminal, drops its clients by leaving, and
//! says where the journal ends. The second relay serves nobody until the warden
//! tells it `Go`, which is after that, so exactly one relay accepts clients and
//! exactly one reads the terminal at any time; a client that was here comes
//! back, to the relay that took over, with the offset it had.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kablo_proto::v1::{
    Capture, ClientName, DEFAULT_HISTORY, ErrorCode, Exit, MAX_CLIENTS, MAX_COLS, MAX_HISTORY,
    MAX_ROWS, Message, OFFSET_END, Packet, PacketDecoder, Size, State, Status, Stroke,
    TerminalState, View,
};
use kablo_proto::{SessionId, WIRE};
use kablo_screen::Screen;

use crate::handshake::{Opening, Served};
use crate::inner::{self, ADOPT_DESCRIPTORS, Decoder, INNER, ToRelay, ToWarden};
use crate::journal::Journal;
use crate::mirror::{
    Core, Feed, Piece, REPLAY_LIMIT, SCREEN_CHUNK, SCREEN_RESTARTS, ScreenState, Worker,
    replay_start, replayed_mark,
};
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

/// `ping`s one client may have waiting for the terminal to take its input.
const PINGS_WAITING: usize = 1024;

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
/// This is the timer that output and input arm (the others are [`EndKeep`]'s,
/// the pause's, and the one for a client that was dropped behind), and it has a
/// stop condition: it is armed by output the relay read or input a client
/// typed, it fires **once**, and then it is not armed again until there is more
/// of either. A session nobody is attached to never has it armed, and neither
/// does one that has ended, so an idle session is a `poll` with no timeout.
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

/// The least time between two looks at the terminal that output brings. Output keeps the
/// look prompt for the first of it after quiet (a program that starts and prints at once is
/// seen at once), but a stream that runs for seconds changes nothing a person can tell by
/// being looked at every turn: each look is three system calls on the process table and the
/// terminal, which at a stream's pace is a good part of what the relay costs. What a look
/// skipped would have found is found by the next one, or by [`Settle`]'s once the output
/// has stopped.
const LOOK_INTERVAL: Duration = Duration::from_millis(50);

/// When the terminal was last looked at, for the rule of [`LOOK_INTERVAL`].
#[derive(Default)]
struct Looks {
    last: Option<Instant>,
}

impl Looks {
    /// Whether output read at `now` may bring a look: none was made yet, or the interval
    /// has passed since the last.
    fn output_due(&self, now: Instant) -> bool {
        self.last
            .is_none_or(|last| now.saturating_duration_since(last) >= LOOK_INTERVAL)
    }

    /// A look was made at `now`, whatever brought it.
    fn made(&mut self, now: Instant) {
        self.last = Some(now);
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

/// Terminal reads in one turn before the other descriptors get a look. A kernel read of a
/// terminal is small (a macOS one returns at most a kibibyte), so a stream needs [`OUT_CHUNK`]
/// over that many of them to fill a frame, and each may come after [`REFILL_RETRIES`] that
/// found nothing: a turn ends on the bytes and not on this.
const READ_BUDGET: usize = (REFILL_RETRIES + 2) * (OUT_CHUNK / 1024);

/// How many times a turn that is gathering reads again at once after a full read left the
/// terminal empty, before it sleeps ([`REFILL_WAIT`]). The program refills it within
/// microseconds, and a sleep and a wake-up cost several more: sleeping at every gap puts the
/// reader and the program in lockstep at a kibibyte a wake-up. A read that finds nothing is
/// a fraction of a microsecond, and these are only for a stream that fills the kernel's read,
/// so a quiet or interactive terminal never does them. Past this many the gain flattens out.
const REFILL_RETRIES: usize = 16;

/// How long a turn that is gathering sleeps for the terminal to be readable again, once
/// [`REFILL_RETRIES`] found it empty. A turn that ended there would send what it has as a
/// frame of a few kibibytes and come back for the next, which costs the relay and the client
/// more than the bytes do. A millisecond is the finest wait `poll` has. When nothing comes
/// the stream has stopped, the turn ends and the relay sleeps with no timeout.
const REFILL_WAIT: Duration = Duration::from_millis(1);

/// How long a turn goes on gathering. A flood fills a frame well inside this. A program that
/// writes a full read every so often does not: without a limit its bytes would sit here for
/// as many waits as a frame has reads, which is a delay anyone watching it can see.
const GATHER_LIMIT: Duration = Duration::from_millis(2);

/// How much the screen may be behind the reads before the worker is woken from its rest.
const WAKE_BACKLOG: u64 = (SCREEN_CHUNK / 2) as u64;

/// A terminal read of this many bytes is a full one: the kernel gave all it was willing
/// to in a read, so the program is very likely writing faster than the relay reads. A
/// shorter read ended because the program had no more at that moment: a keystroke's echo,
/// a line of output, a redraw. A macOS pseudo-terminal returns at most a kibibyte per
/// read. A Linux one returns what its line discipline holds, which under a flood is 4095
/// bytes at a time, so a kibibyte is no sign of a flood there: a full-screen program's
/// redraw after a keystroke is a few kibibytes and must not be taken for one.
const FULL_READ: usize = if cfg!(target_os = "linux") {
    4095
} else {
    1024
};

/// The most the screen may be behind the reads (bytes of the journal the worker has not
/// parsed) before the relay stops reading the terminal until the worker has caught up. The
/// reads can outrun the parser by a long way (a program that clears the screen in a loop is
/// parsed at a fraction of the pace it is read at), and whoever needs the screen as it is
/// (a resize, an attach, a picture, a typed key) parses what is missing itself: a limit
/// bounds how long that takes, and with it how long the relay can be deaf to its clients,
/// its warden and its quit. The program that is not read waits in the terminal's own buffer,
/// as it does for any reader that is slower than it. Never more than half the ring (see
/// [`Relay::behind_limit`]).
const BEHIND_LIMIT: u64 = 256 * 1024;

/// How long the relay waits before it looks again at whether the worker has brought the
/// screen under [`BEHIND_LIMIT`], while the terminal is not read for lack of that. It only
/// runs with a backlog, and a backlog is only ever the worker's to work off, so it ends
/// with it: an idle session has none.
const THROTTLE_WAIT: Duration = Duration::from_millis(1);

/// How long a client that asked for every byte (`View::every_byte`) may be waited for
/// without moving: the terminal is not read while it is behind by more than half the ring,
/// and a client that has stopped reading (frozen, or stopped in a debugger) must not hold a
/// program for good. Once its position has stood still this long it is not waited for any
/// more, the ring laps it and it is sent a picture as any client is, until it reads again.
/// The timer runs only while the relay is waiting for such a client: not while another that
/// asked keeps up ([`Relay::paused`]).
const STALL_LIMIT: Duration = Duration::from_secs(1);

/// How long a client that was dropped behind ([`Client::drop_behind`]) may take to notice and
/// close its side. A client that reads gets there when it has shown what was on its way, which over a
/// slow link is seconds; one that does not read gives its seat back after this.
const DROPPED_BEHIND_LIMIT: Duration = Duration::from_secs(30);

/// Reads of one client's socket in one turn, at most. A client that leaves
/// with more than this many reads' worth unread is seen to have left a turn
/// later, so a question asked in between still counts it.
const CLIENT_READS: usize = 4;

/// Reads of the socket of a client that was dropped behind in one turn, at most: as much as
/// may wait for the program ([`INPUT_LIMIT`]). What it typed there goes to the program before
/// what comes on the connection it comes back on, which is read after it in the same turn.
const DROPPED_BEHIND_READS: usize = 64;

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
    opening: Opening,
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
    /// The `ping`s it sent that name the session and are not answered yet: each
    /// waits for the program's terminal to take the input that was queued before
    /// it. `(owed, session, bytes)`: answered once [`Relay::input_taken`] has
    /// reached `owed`.
    pings: VecDeque<(u64, Option<SessionId>, Vec<u8>)>,
    /// What [`Relay::input_queued`] was after the last `input` it sent: the bytes
    /// of it are the ones a `ping` of its own waits for.
    typed_to: u64,
    /// When the ring left it behind and it was dropped ([`Client::drop_behind`]).
    dropped_behind_at: Option<Instant>,
}

impl Client {
    /// The ring has left this client behind, so it gets no more of the stream. It is told by
    /// the end of the stream, after what is on its way, and comes back with the offset it has.
    ///
    /// Only this side is closed. The client does not know until it has read that far, and over
    /// a slow link it writes for seconds before it has: a key on a connection that was closed
    /// is lost, and a key pressed while a program's output outruns the link is a `^C`. What it
    /// types until it closes is read and goes to the program.
    fn drop_behind(&mut self, now: Instant) {
        // A failure is a socket with nobody on it, which the next read says.
        let _ = self.sock.shutdown(std::net::Shutdown::Write);
        self.attach = None;
        self.out.clear();
        self.sent = 0;
        self.asked = None;
        self.waiting = false;
        self.pings.clear();
        self.dropped_behind_at = Some(now);
    }

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
    /// The client reads as fast as it can and wants every byte, so the terminal is not read
    /// while it is behind by more than half the ring (`every_byte` in its view).
    every_byte: bool,
    /// When the relay began to wait for the client because it was behind, if it did and the
    /// client has not moved since: the start of [`STALL_LIMIT`].
    waited_since: Option<Instant>,
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

/// What the relay is waiting for instead of reading the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pause {
    /// The screen's thread. It is no descriptor, so the relay looks again after
    /// [`THROTTLE_WAIT`].
    Parser,
    /// A client that asked for every byte. Its socket taking bytes wakes the relay; `until` is
    /// when it stops being waited for if it does not move ([`STALL_LIMIT`]).
    Client { until: Instant },
}

struct Relay {
    id: SessionId,
    chan: UnixStream,
    chan_alive: bool,
    chan_out: Vec<u8>,
    decoder: Decoder<ToRelay>,
    master: OwnedFd,
    journal: Arc<Journal>,
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
    /// Until when the terminal's questions are kept for the session's first client
    /// (`NewSession::wait_for_client`): while it stands and no client has attached, the
    /// screen is given nothing, so it answers no question and counts none as answered, and
    /// the client that comes for the bytes, or for a picture and what follows it, is told so
    /// and answers them itself. `None` for a
    /// session that keeps none, and from the first attach or the end of the time on.
    wait_for_client: Option<Instant>,
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
    /// Every byte of input ever queued for the program (what [`Self::input`]
    /// holds and what the terminal has taken of it already). The terminal has
    /// taken `input_queued - input_pending()` of them.
    input_queued: u64,
    /// The screen of the stream, and the thread that parses it while a client is attached.
    core: Arc<Core>,
    /// Parses behind the reads while someone is attached; started by the first read that
    /// finds one, and never when no thread could be started.
    worker: Option<Worker>,
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
    /// When the terminal was last looked at.
    looks: Looks,
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
        let journal = Arc::new(journal);
        let core = Core::new(
            ScreenState::new(screen, screen_at, last_reply, history),
            Arc::clone(&journal),
        );
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
            wait_for_client: adopt
                .wait_for_client
                .map(|left| Instant::now() + Duration::from_millis(u64::from(left))),
            live: false,
            reading: false,
            master_done: false,
            final_drained: false,
            input: Vec::new(),
            input_sent: 0,
            input_queued: 0,
            core,
            worker: None,
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
            looks: Looks::default(),
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
            // Not while something is too far behind to be read past: the program waits in
            // the terminal's own buffer, and whatever ends the wait wakes the poll (a
            // client's socket taking bytes) or has a timer in `wake_in`.
            let now = Instant::now();
            self.note_laggards(now);
            let pause = self.paused(now);
            if self.reading && !self.master_done && pause.is_none() {
                events |= libc::POLLIN;
            }
            if matches!(pause, Some(Pause::Parser)) {
                // Never leave the worker asleep with the relay waiting for it.
                self.core.wake_worker();
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
            // The pause that left the terminal out of the set, and not a second look at it:
            // the screen's thread moves by itself, and a pause that ended in between would
            // leave the terminal out and the timeout for it gone, with `poll` deaf to the
            // terminal until something else woke it.
            if sys::poll(&mut set, self.wake_in(now, pause)).is_err() {
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
            self.answer_pings();
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
            self.stop_waiting_for_a_client(Instant::now());
            if self.is_orphaned() {
                self.quit = true;
            }
            if self.quit {
                // The terminal is not read any more: what was typed and not
                // yet taken is the last thing this relay does for it.
                self.flush_input();
                if self.leaving_for_good() {
                    self.forget_label();
                    self.forget_marks();
                } else {
                    self.leave_marks();
                }
                // The screen goes with this process and a successor builds its own from
                // the journal, every byte of which that a client was shown is at or below
                // `answered` already: parsing what is left would only delay the release. The
                // marks say how far they reach, and the successor reads the rest.
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
                self.take_marks();
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
        // Whether a client is shown what is read: its terminal answers the program's
        // questions, and the screen is only a record that a thread can keep behind the
        // reads. Nobody can attach in the middle of this.
        let watched = self.attached() > 0;
        let mut saturated = false;
        let mut retries = 0;
        // A turn ends at a frame's worth, and at no more than a fraction of the ring: reads
        // that no `drive` follows would lap a client on a ring they fill in a turn.
        let turn_limit = (OUT_CHUNK as u64).min(self.journal.capacity() / 8);
        let mut read = 0u64;
        let now = Instant::now();
        for _ in 0..READ_BUDGET {
            if read >= turn_limit || self.paused(now).is_some() {
                break;
            }
            match self.journal.fill_from(self.master.as_fd(), self.chunk) {
                Ok(0) => {
                    self.master_done = true;
                    break;
                }
                Ok(n) => {
                    read += n as u64;
                    // Before the next read: a read may lap a small ring.
                    if watched {
                        self.leave_to_the_worker();
                    } else {
                        self.feed_screen(Feed::Live);
                    }
                    saturated = n >= FULL_READ;
                    retries = 0;
                    // What a client is shown ends the turn on a short read: the program
                    // had no more, and a keystroke's echo goes out in the turn it came.
                    if watched && !saturated {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // A full read that left the terminal empty is a stream that refills it
                    // within microseconds: wait for it here, to send one frame and not many.
                    if !watched || !saturated || now.elapsed() >= GATHER_LIMIT {
                        break;
                    }
                    // Sleeping costs more than the gap does: read again a few times first.
                    if retries < REFILL_RETRIES {
                        retries += 1;
                        continue;
                    }
                    retries = 0;
                    if !self.refills() {
                        break;
                    }
                }
                // `EIO` is how Linux says the program's side is closed.
                Err(_) => {
                    self.master_done = true;
                    break;
                }
            }
        }
        if read > 0 {
            self.output_read = true;
            // The only clock this takes; what was read is not looked at.
            self.output_at = sys::unix_now();
            self.settle.arm(Instant::now());
        }
    }

    /// Waits [`REFILL_WAIT`] for the terminal to be readable again. Anything but silence
    /// (a hang-up too) is for the next `read` to say.
    fn refills(&self) -> bool {
        let mut set = [sys::want(self.master.as_raw_fd(), libc::POLLIN)];
        sys::poll(&mut set, Some(REFILL_WAIT)).is_ok_and(|ready| ready > 0)
    }

    /// What was just read goes to a client that is shown it: the client's terminal answers
    /// its questions, which is to say the questions up to the end of it are answered, and
    /// the worker parses it. The offset moves right after the read, as it does when the
    /// relay feeds the screen itself, so a relay that takes over answers nothing twice.
    ///
    /// This thread never parses for it: when the screen falls [`Relay::behind_limit`]
    /// behind, the terminal is not read any more until the worker has brought it back
    /// ([`Relay::paused`]), which also keeps the ring from lapping the screen.
    fn leave_to_the_worker(&mut self) {
        self.journal.set_answered(self.journal.total());
        if self.worker.is_none() {
            match Worker::start(&self.core) {
                Ok(worker) => self.worker = Some(worker),
                // No thread to be had: the screen is fed here, as it is for nobody.
                Err(_) => return self.catch_up(),
            }
        }
        let behind = self.journal.total().saturating_sub(self.core.parsed());
        // Not for every kernel read: a wake-up per kibibyte costs the reads more than the
        // parser's thread saves them, and what is left below this is small for whoever
        // looks at the screen next to give it. It is never more than the limit, so a
        // relay that stops reading for the backlog has woken the worker.
        if behind >= WAKE_BACKLOG.min(self.journal.capacity() / 4) {
            self.core.wake_worker();
        }
    }

    /// How far behind the reads the screen may be: [`BEHIND_LIMIT`], and no more than half
    /// the ring. A read is at most a quarter of it, so the ring cannot lap the screen.
    fn behind_limit(&self) -> u64 {
        BEHIND_LIMIT.min(self.journal.capacity() / 2)
    }

    /// Why the terminal is not read at `now`, if it is not: something is further behind the
    /// reads than the ring can bear, and the program is made to wait in the terminal's own
    /// buffer, as it does for any reader that is slower than it, rather than the relay
    /// writing over what that has not got to or doing its work on the read path.
    fn paused(&self, now: Instant) -> Option<Pause> {
        if !self.reading || self.master_done {
            return None;
        }
        let total = self.journal.total();
        // The screen's thread has more to parse than [`Relay::behind_limit`].
        if self.worker.is_some() && total.saturating_sub(self.core.parsed()) >= self.behind_limit()
        {
            return Some(Pause::Parser);
        }
        // A client that asked for every byte is behind by more than half the ring, and has
        // not stood still for [`STALL_LIMIT`]: the reads that would lap it wait for it. Unless
        // another that asked keeps up: the one furthest along sets the pace, and a slow
        // terminal is lapped beside it as without the bit, not a brake on it and on the
        // program. (A client that did not ask has no say, whether it is ahead or behind, and
        // one still taking its picture has not been keeping up with anything.)
        let half = self.journal.capacity() / 2;
        let asked = || {
            self.clients
                .iter()
                .filter(|client| !client.dead)
                .filter_map(|client| client.attach.as_ref())
                .filter(|attach| attach.every_byte)
        };
        let behind = |attach: &Attach| total.saturating_sub(attach.next) > half;
        if asked().any(|attach| attach.picture.is_none() && !behind(attach)) {
            return None;
        }
        asked()
            .filter(|attach| behind(attach))
            .map(|attach| attach.waited_since.unwrap_or(now) + STALL_LIMIT)
            .filter(|until| *until > now)
            .min()
            .map(|until| Pause::Client { until })
    }

    /// Notes at `now` which clients that asked for every byte the relay is waiting for: the
    /// ones behind by more than half the ring begin to be counted against [`STALL_LIMIT`],
    /// and the ones that are not behind (any more) stop.
    fn note_laggards(&mut self, now: Instant) {
        let total = self.journal.total();
        let half = self.journal.capacity() / 2;
        for attach in self
            .clients
            .iter_mut()
            .filter(|client| !client.dead)
            .filter_map(|client| client.attach.as_mut())
            .filter(|attach| attach.every_byte)
        {
            if total.saturating_sub(attach.next) > half {
                attach.waited_since.get_or_insert(now);
            } else {
                attach.waited_since = None;
            }
        }
    }

    /// Writes what each client that asked for every byte can take.
    fn drive_those_who_asked_for_every_byte(&mut self) {
        for i in 0..self.clients.len() {
            let asked = self
                .clients
                .get(i)
                .and_then(|client| client.attach.as_ref())
                .is_some_and(|attach| attach.every_byte);
            if asked {
                self.drive(i);
            }
        }
    }

    /// The shell is gone: take what it left in the terminal and read no more.
    /// (A background process that kept the terminal open may write later; the
    /// session is over and that is not part of it.)
    fn drain_to_the_end(&mut self) {
        // What was read while a client was shown it comes first, in stream order.
        self.catch_up();
        loop {
            // The program is gone, so there is no pause to wait out: a client that asked
            // for every byte is given what its socket takes between the fills, and a ring a
            // few fills long (the smallest is a Linux terminal's buffer) is not turned over
            // under one that stood half a ring behind when the program ended, as long as
            // its socket has room. Nothing here waits for one that does not read.
            self.drive_those_who_asked_for_every_byte();
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

    /// The terminal as it is now; nothing once the program has ended. Counts as a look for
    /// [`LOOK_INTERVAL`].
    fn terminal_now(&mut self) -> TerminalState {
        if self.exit.is_some() {
            return TerminalState::default();
        }
        self.looks.made(Instant::now());
        self.watch.look(self.master.as_fd(), self.child_pid)
    }

    /// At the end of a turn in which output was read, or when the look after
    /// output or input is due: looks at the terminal once, and tells the
    /// attached clients if it is not what they were told. At most one look in
    /// a turn, whichever of the two brought it, and none when nobody is
    /// attached (`status` looks when it is asked). Output brings a look only
    /// when [`LOOK_INTERVAL`] has passed since the last one; the look after
    /// output is not held back, so the last of a burst is seen.
    fn look_at_the_terminal(&mut self) {
        self.look_at_the_terminal_at(Instant::now());
    }

    fn look_at_the_terminal_at(&mut self, now: Instant) {
        let output = std::mem::take(&mut self.output_read) && self.looks.output_due(now);
        let due = self.settle.fire(now);
        if !(output || due) || self.exit.is_some() || self.attached() == 0 {
            return;
        }
        let state = self.terminal_now();
        self.looks.made(now);
        self.tell_the_others(&state, usize::MAX);
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
    /// says which), in stream order: whoever else is feeding the screen is waited for.
    fn feed_screen(&mut self, mode: Feed) {
        // The questions are the first client's while it is waited for: the screen is given
        // nothing of what this relay or the warden read, so it answers none of it and none
        // of it counts as answered. (What nobody is to answer is given as ever.)
        if matches!(mode, Feed::Live | Feed::Held) && self.keeps_questions(Instant::now()) {
            return;
        }
        let core = Arc::clone(&self.core);
        let journal = Arc::clone(&self.journal);
        let mut state = core.lock();
        let mut restarts = 0;
        loop {
            match state.feed_piece(&journal, mode) {
                Piece::Caught => break,
                Piece::Fed { replies, answers } => {
                    core.parsed_to(state.at);
                    // The program that does not read what it is owed gets no more.
                    if answers && !replies.is_empty() && self.exit.is_none() && self.attached() == 0
                    {
                        let _ = self.queue_input(&replies);
                    }
                }
                // The ring was lapped under the screen (a relay that is still
                // writing, while this one catches up): begin again from what
                // is left.
                Piece::Lapped if restarts < SCREEN_RESTARTS => {
                    restarts += 1;
                    state.restart(&journal);
                    core.parsed_to(state.at);
                }
                Piece::Lapped => {
                    state.at = journal.total();
                    core.parsed_to(state.at);
                    break;
                }
            }
        }
    }

    /// Whether the terminal's questions are being kept for the session's first client.
    fn keeps_questions(&self, now: Instant) -> bool {
        self.wait_for_client.is_some_and(|until| now < until)
    }

    /// The first client did not come in time: the screen is given what was kept for it and
    /// answers it, as it does for a program nobody watches.
    fn stop_waiting_for_a_client(&mut self, now: Instant) {
        if self.wait_for_client.is_some_and(|until| now >= until) {
            self.wait_for_client = None;
            if self.live {
                self.catch_up();
            }
        }
    }

    /// How many clients are attached.
    fn attached(&self) -> usize {
        self.clients
            .iter()
            .filter(|client| !client.dead && client.attach.is_some())
            .count()
    }

    /// The terminal takes this size, and so does the screen, once it has all the program
    /// wrote at the old one.
    fn set_size(&mut self, size: Size) {
        if self.exit.is_some() {
            return;
        }
        let _ = sys::set_window_size(self.master.as_fd(), size);
        self.catch_up();
        let mut state = self.core.lock();
        if state.screen.size() != (size.cols, size.rows) {
            state.screen.resize(size.cols, size.rows);
        }
    }

    /// A picture of the screen as it is at the end of the journal.
    fn picture(&mut self) -> Picture {
        self.catch_up();
        self.feed_screen(Feed::Spent);
        let mut state = self.core.lock();
        let (cols, rows) = state.screen.size();
        Picture {
            offset: state.at,
            cols,
            rows,
            bytes: state.screen.image(),
            sent: 0,
        }
    }

    /// A picture of the screen as it stood when the terminal's questions began to be kept,
    /// at the size of the client they were kept for: nothing of what was kept is in it, and
    /// the stream goes on from where it stands, so the client is sent the bytes that were
    /// kept after it and answers what they ask. The screen is then given those bytes, as
    /// ones that are not its to answer. (It takes the client's size before them, as the
    /// client's own terminal reads them at that size.)
    fn picture_before_what_was_kept(&mut self, size: Option<Size>) -> Picture {
        let picture = {
            let mut state = self.core.lock();
            if let Some(size) = size
                && size.cols > 0
                && size.rows > 0
                && state.screen.size() != (size.cols, size.rows)
            {
                state.screen.resize(size.cols, size.rows);
            }
            let (cols, rows) = state.screen.size();
            Picture {
                offset: state.at,
                cols,
                rows,
                bytes: state.screen.image(),
                sent: 0,
            }
        };
        self.feed_screen(Feed::Spent);
        picture
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
            opening: Opening::default(),
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
            pings: VecDeque::new(),
            typed_to: 0,
            dropped_behind_at: None,
        });
    }

    /// Reads what client `i` has sent, until its socket is empty or it has
    /// gone ([`CLIENT_READS`] reads at most, so one busy client does not keep
    /// the others waiting): a client that says something and leaves is seen to
    /// have left in the same turn, before a question asked in it is answered.
    fn client_readable(&mut self, i: usize) {
        let mut buf = [0u8; 16 * 1024];
        let reads = if self.was_dropped_behind(i) {
            DROPPED_BEHIND_READS
        } else {
            CLIENT_READS
        };
        for _ in 0..reads {
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
        match client
            .opening
            .serve(&WIRE, bytes, |stream| client.decoder.push(stream))
        {
            Ok(Served::Waiting) => return,
            Ok(Served::Open(reply)) => client.out.extend_from_slice(&reply),
            Ok(Served::Refused(reply)) => {
                client.out.extend_from_slice(&reply);
                client.closing = true;
                return;
            }
            Err(_) => {
                client.dead = true;
                return;
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
                // Dropped behind, all a client can still do is type: what else it wants it asks again
                // on the connection it comes back on, and here it would not be told.
                Ok(Some(packet))
                    if self.was_dropped_behind(i)
                        && !matches!(packet.message, Message::Input(_) | Message::Write(_)) => {}
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
        // The end of the stream was the last thing it is told.
        if client.dropped_behind_at.is_some() {
            return;
        }
        let packet = Packet { session, message };
        if packet.encode(&mut client.out).is_err()
            || client.out.len().saturating_sub(client.sent) > OUT_LIMIT
        {
            client.dead = true;
        }
    }

    /// Answers a `ping`. One that names the session is answered once the
    /// program's terminal has taken the `input` this client sent before it: it
    /// is how a sender that types more than a terminal holds learns how much of
    /// it has arrived. (A `write` is not waited for: it is queued whole or
    /// refused whole, and its `pong` says which.) One with no session is a
    /// pulse and is answered at once.
    fn ping(&mut self, i: usize, session: Option<SessionId>, bytes: Vec<u8>) {
        let owed = self.clients.get(i).map_or(0, |client| client.typed_to);
        if session.is_none() || self.input_taken() >= owed {
            return self.queue(i, session, Message::Pong(bytes));
        }
        let Some(client) = self.clients.get_mut(i) else {
            return;
        };
        if client.pings.len() >= PINGS_WAITING {
            return self.error(i, ErrorCode::INPUT_FULL, "too many pings are waiting");
        }
        client.pings.push_back((owed, session, bytes));
    }

    /// How many of the bytes queued for the program its terminal has taken (or
    /// that were dropped because nobody will read them).
    fn input_taken(&self) -> u64 {
        self.input_queued
            .saturating_sub(self.input_pending() as u64)
    }

    /// Answers the `ping`s whose input the terminal has taken, in the order
    /// they came.
    fn answer_pings(&mut self) {
        let taken = self.input_taken();
        let mut due = Vec::new();
        for (i, client) in self.clients.iter_mut().enumerate() {
            while client
                .pings
                .front()
                .is_some_and(|(owed, ..)| *owed <= taken)
            {
                if let Some((_, session, bytes)) = client.pings.pop_front() {
                    due.push((i, session, bytes));
                }
            }
        }
        for (i, session, bytes) in due {
            self.queue(i, session, Message::Pong(bytes));
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
            Message::Ping(bytes) => self.ping(i, packet.session, bytes),
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

    fn was_dropped_behind(&self, i: usize) -> bool {
        self.clients
            .get(i)
            .is_some_and(|client| client.dropped_behind_at.is_some())
    }

    fn attach(&mut self, i: usize, offset: u64, view: Option<View>) {
        if view.is_some_and(|view| !view.size.fits()) {
            return self.too_big(i);
        }
        // A client that comes while the terminal's questions are kept for it is the one that
        // answers them, and is told how far the session answered, which is where the keeping
        // began. It comes for the bytes from there, or for a picture, as a pane does that is
        // made for a terminal that runs already: the picture is then of the screen as it stood
        // when the keeping began, and the bytes that were kept follow it. Either way the
        // screen is given what was kept before anything else of this attach reads it (the
        // client's size does), as bytes that are not its to answer: it answers none of them,
        // and they do not count as answered already, which would have a terminal that
        // answers what it is shown sent a picture in their place. Whether they were kept is
        // decided here, once: the wait may end while this attach is made. A client that
        // comes for the end alone, or for bytes the ring no longer holds, is not shown the
        // questions: for it the screen answers them first, as it does when nobody comes.
        let wants_picture = view.is_some_and(|view| view.picture);
        let kept = self
            .keeps_questions(Instant::now())
            .then(|| self.journal.answered())
            .flatten()
            .filter(|answered| {
                if wants_picture {
                    // The screen stands where the keeping began, and the ring holds the
                    // stream from there.
                    *answered >= self.journal.retained_from()
                        && self.core.lock().at == *answered
                        && (offset == OFFSET_END || offset <= self.journal.total())
                } else {
                    offset != OFFSET_END
                        && offset >= self.journal.retained_from()
                        && offset <= *answered
                }
            });
        self.wait_for_client = None;
        // The screen has all that was read, and `last_reply` says what was answered of it,
        // before a client is added: it is told by them.
        let picture_before = match kept {
            Some(_) if wants_picture => {
                Some(self.picture_before_what_was_kept(view.map(|view| view.size)))
            }
            Some(_) => {
                self.feed_screen(Feed::Spent);
                None
            }
            None => {
                self.catch_up();
                None
            }
        };
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
        let last_reply = self.core.lock().last_reply;
        let answered_twice = offset != OFFSET_END
            && offset < last_reply
            && view.is_some_and(|view| view.picture_if_answered);
        let truncated =
            (offset != OFFSET_END && offset < self.journal.retained_from()) || answered_twice;
        let picture =
            picture_before.or_else(|| (truncated || wants_picture).then(|| self.picture()));
        let start = match &picture {
            Some(picture) => picture.offset,
            None if offset == OFFSET_END => total,
            None => offset,
        };
        if let Some(client) = self.clients.get_mut(i) {
            // A client that reads flat out and wants every byte is quicker than a relay that
            // can put only 8 KiB at a time in a macOS socket: the relay's end holds a
            // megabyte, so a frame is written and the terminal read again while the client
            // is still on the one before. (The client's end does not matter for what the
            // relay writes, and a client that did not ask is not given the room: on a slow
            // link it would be what the link carries before the prompt after a `^C`.)
            if view.is_some_and(|view| view.every_byte) {
                sys::widen_send_buffer(client.sock.as_fd());
            }
            client.attach = Some(Attach {
                next: start,
                keep_after_end: view.is_some_and(|view| view.keep_after_end),
                every_byte: view.is_some_and(|view| view.every_byte),
                waited_since: None,
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
                // answered, as it does for the relay (`replayed_mark`). What was kept
                // for this client is its to answer: it is told where the keeping began.
                answered: kept.or(self.journal.answered()).unwrap_or(total),
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
        self.catch_up();
        self.feed_screen(Feed::Spent);
        let mut state = self.core.lock();
        self.journal.set_floor(state.at);
        state.reach_floor(&self.journal);
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
        if !self.is_attached(i) && !self.was_dropped_behind(i) {
            return self.error(i, ErrorCode::NOT_ATTACHED, "attach first");
        }
        if self.exit.is_some() {
            return;
        }
        if !self.queue_input(bytes) {
            self.error(i, ErrorCode::INPUT_FULL, "the program is not taking input");
            return;
        }
        let queued = self.input_queued;
        if let Some(client) = self.clients.get_mut(i) {
            client.typed_to = queued;
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
        let modes = self.core.lock().screen.modes();
        let typed = match typing::bytes(strokes, &modes) {
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
        self.input_queued += bytes.len() as u64;
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
        let mut state = self.core.lock();
        let (cols, rows) = state.screen.size();
        let offset = state.at;
        let picture = capture.picture;
        let bytes = if picture {
            state.screen.image()
        } else {
            let history = usize::try_from(capture.history).unwrap_or(usize::MAX);
            let mut text = String::new();
            for line in state.screen.text(history, capture.unwrap) {
                text.push_str(&line);
                text.push('\n');
            }
            text.into_bytes()
        };
        drop(state);
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

    /// Gives the screen what it has not had of the journal, in stream order: what the worker
    /// has not got to, and what was read while nobody could answer it. What was read while a
    /// client was shown it is below the offset answered and nobody answers it again.
    fn catch_up(&mut self) {
        self.feed_screen(if self.reading { Feed::Live } else { Feed::Held });
    }

    /// The session's label: what its file says (see [`sockdir::label_path`]).
    fn label(&self) -> Option<String> {
        sockdir::read_label(&self.directory()?, &self.id)
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

    /// The socket directory: the listening socket's, so that this does not depend on where the
    /// relay was started from.
    fn directory(&self) -> Option<std::path::PathBuf> {
        let address = self.listener.local_addr().ok()?;
        Some(address.as_pathname()?.parent()?.to_path_buf())
    }

    /// Takes the label file away: the session is gone.
    fn forget_label(&self) {
        if let Some(dir) = self.directory() {
            let _ = sockdir::remove_label(&dir, &self.id);
        }
    }

    /// Leaves the shell's marks to the relay that takes over, as far as the screen has read:
    /// the ring may no longer hold them.
    fn leave_marks(&self) {
        let Some(dir) = self.directory() else {
            return;
        };
        let state = self.core.lock();
        let left = state.screen.marks_left();
        let _ = sockdir::write_marks(&dir, &self.id, state.at, &left);
    }

    /// Takes what a relay left for a session that is over: nobody comes for it.
    fn forget_marks(&self) {
        if let Some(dir) = self.directory() {
            let _ = sockdir::take_marks(&dir, &self.id);
        }
    }

    /// The marks the relay before this one left, once the screen has read what the ring holds:
    /// they take the place of the ones it found there, with what came after them. Marks that
    /// reach past what this screen has read are not of this stream.
    fn take_marks(&mut self) {
        let Some((reach, left)) = self
            .directory()
            .and_then(|dir| sockdir::take_marks(&dir, &self.id))
        else {
            return;
        };
        let journal = Arc::clone(&self.journal);
        let mut state = self.core.lock();
        if reach > state.at {
            return;
        }
        // What the ring has dropped of the stream after them is lost with its marks.
        let from = reach.max(journal.total().saturating_sub(journal.capacity()));
        let mut since = vec![0; usize::try_from(state.at.saturating_sub(from)).unwrap_or(0)];
        let got = journal.copy_out(from, &mut since).unwrap_or(0);
        since.truncate(got);
        state.screen.take_marks(&left, &since);
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
        // Dropped behind: it is owed nothing more, and it is for it to notice.
        if let Some(client) = self.clients.get_mut(i)
            && let Some(at) = client.dropped_behind_at
        {
            if at.elapsed() >= DROPPED_BEHIND_LIMIT {
                client.dead = true;
            }
            return;
        }
        let ended_ready = self.exit.is_some() && self.final_drained;
        for _ in 0..FRAMES_PER_TURN {
            if !self.flush(i) {
                // Blocked or dead; the check for a ring that left it behind still applies.
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
                // It moved: whatever it was waited for is over. A picture is taken a piece
                // at a time while `next` stands still, and a client that drains a large one
                // steadily is not a client that has stopped.
                attach.waited_since = None;
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
                        // It moved: whatever it was waited for is over.
                        attach.waited_since = None;
                        let packet =
                            Packet::about(self.id.clone(), Message::Output { offset, data });
                        if packet.encode(&mut client.out).is_err() {
                            client.dead = true;
                            return;
                        }
                    }
                    // The ring was lapped under it.
                    _ => {
                        client.drop_behind(Instant::now());
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
            client.drop_behind(Instant::now());
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

    /// How long a `poll` may wait at `now`, with the terminal left out of it for `pause`: for
    /// ever unless a look at the terminal, the end of a session's wait, the end of the pause
    /// (the screen's thread, a client that is waited for) or the end of a client that was
    /// dropped behind is due.
    fn wake_in(&self, now: Instant, pause: Option<Pause>) -> Option<Duration> {
        let keep = if self.delivered_sent {
            None
        } else {
            self.keep.timeout(now)
        };
        let pause = pause.map(|pause| match pause {
            Pause::Parser => THROTTLE_WAIT,
            Pause::Client { until } => until.saturating_duration_since(now),
        });
        let dropped_behind = self
            .clients
            .iter()
            .filter_map(|client| client.dropped_behind_at)
            .map(|at| (at + DROPPED_BEHIND_LIMIT).saturating_duration_since(now))
            .min();
        // The end of the wait for a first client: what was kept for it is answered then.
        let first_client = self
            .wait_for_client
            .map(|until| until.saturating_duration_since(now));
        [
            self.settle.timeout(now),
            keep,
            pause,
            dropped_behind,
            first_client,
        ]
        .into_iter()
        .flatten()
        .min()
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
        (typed_to_the_program(&slave), relay.core.lock().last_reply)
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
        let journal = Arc::new(journal);
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
            core: Core::new(
                ScreenState::new(
                    Screen::new(80, 24, 10),
                    replay_start(&journal, REPLAY_LIMIT),
                    replayed_mark(&journal, replay_start(&journal, REPLAY_LIMIT)),
                    10,
                ),
                Arc::clone(&journal),
            ),
            worker: None,
            journal,
            listener,
            child_pid: 1,
            started: None,
            output_at: None,
            exit: None,
            warden_version: String::new(),
            wait_for_client: None,
            live: false,
            reading: false,
            master_done: false,
            final_drained: false,
            input: Vec::new(),
            input_sent: 0,
            input_queued: 0,
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
            looks: Looks::default(),
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
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"text\x1b[6n");
        let (mut relay, slave) = relay_over(journal);
        relay.live = true;
        relay.reading = true;
        let (ours, _theirs) = UnixStream::pair().unwrap();
        relay.seat(ours);
        relay.answer_read(0, Capture::default());
        assert_eq!(typed_to_the_program(&slave), b"\x1b[1;5R");
        assert_eq!(relay.journal.answered(), Some(relay.journal.total()));
        assert_eq!(relay.core.lock().at, relay.journal.total());
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
        // What it may have left in a swap that did not end goes with the session.
        let marks = sockdir::marks_path(&dir, &id);
        sockdir::write_marks(&dir, &id, 0, b"").unwrap();
        relay.forget_marks();
        assert!(!marks.exists());
    }

    #[test]
    fn what_the_warden_read_while_no_relay_did_is_answered_by_the_relay_that_comes() {
        // The program asked at once, before any relay was serving: the journal
        // holds the question and says nobody answered anything.
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        assert_eq!(answers_after_taking_over(journal), b"\x1b[1;1R");
    }

    #[test]
    fn what_an_earlier_relay_answered_is_not_answered_again() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.set_answered(journal.total());
        journal.append(b"text");
        assert_eq!(answers_after_taking_over(journal), b"");
    }

    #[test]
    fn only_the_part_after_what_was_answered_is_answered() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.set_answered(journal.total());
        // Read by the warden afterwards, with one more question in it.
        journal.append(b"more\x1b[5nmore");
        // The device status report: "terminal OK".
        assert_eq!(answers_after_taking_over(journal), b"\x1b[0n");
    }

    #[test]
    fn a_journal_that_keeps_no_track_is_not_answered_late() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.forget_answers();
        assert_eq!(answers_after_taking_over(journal), b"");
    }

    #[test]
    fn a_relay_that_takes_over_finds_the_last_question_in_what_it_replays() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.set_answered(journal.total());
        journal.append(b"text");
        // Answered by whoever read it, and found again when the screen is fed:
        // the text after it holds no question.
        assert_eq!(take_over(journal), (Vec::new(), 4));
    }

    #[test]
    fn the_mark_is_the_end_of_the_piece_of_output_that_held_the_question() {
        let journal = Journal::create(1 << 18).unwrap();
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
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        assert_eq!(take_over(journal), (b"\x1b[1;1R".to_vec(), 4));
    }

    #[test]
    fn output_with_no_question_leaves_the_mark_where_it_was() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"nothing asked here");
        assert_eq!(take_over(journal), (Vec::new(), 0));
    }

    #[test]
    fn a_journal_that_keeps_no_track_counts_all_of_it_as_answered() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"\x1b[6n");
        journal.forget_answers();
        assert_eq!(take_over(journal), (Vec::new(), 4));
    }

    #[test]
    fn a_screen_is_built_from_the_start_when_all_of_it_is_there() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"one\ntwo\nthree");
        assert_eq!(replay_start(&journal, 1 << 20), 0);
    }

    #[test]
    fn a_screen_is_built_from_the_last_of_a_long_journal_at_a_line() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"one\ntwo\nthree\nfour");
        // The last nine bytes are "hree\nfour": the next line is "four".
        assert_eq!(replay_start(&journal, 9), 14);
        // No line break in the last three: where the limit says.
        assert_eq!(replay_start(&journal, 3), 15);
    }

    #[test]
    fn a_screen_is_built_from_the_first_line_of_a_ring_that_wrapped() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
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
    fn output_brings_a_look_at_once_after_quiet_and_then_at_most_one_in_the_interval() {
        let start = Instant::now();
        let mut looks = Looks::default();
        // Nothing looked at yet: the first output is seen at once.
        assert!(looks.output_due(start));
        looks.made(start);
        // A burst inside the interval brings none, to the last millisecond.
        for ms in 0..50 {
            assert!(!looks.output_due(start + Duration::from_millis(ms)), "{ms}");
        }
        assert!(looks.output_due(start + LOOK_INTERVAL));
        // Any look, the settle's included, starts the interval again.
        looks.made(start + LOOK_INTERVAL * 3);
        assert!(!looks.output_due(start + LOOK_INTERVAL * 3 + LOOK_INTERVAL / 2));
        assert!(looks.output_due(start + LOOK_INTERVAL * 4));
    }

    #[test]
    fn a_burst_of_output_looks_at_the_terminal_once_in_the_interval_and_the_settle_look_follows() {
        let (mut relay, _slave) = relay_over(Journal::create(MIN_CAPACITY).unwrap());
        let (client, _far) = client_of(false);
        relay.clients.push(client);
        let start = Instant::now();
        // Output after quiet: looked at at once.
        relay.output_read = true;
        relay.settle.arm(start);
        relay.look_at_the_terminal_at(start);
        assert_eq!(relay.looks.last, Some(start));
        // Eight more turns of output inside the interval look at nothing; the clock of the
        // last look stays where it was.
        for ms in 1..=8 {
            let at = start + Duration::from_millis(ms * 5);
            relay.output_read = true;
            relay.settle.arm(at);
            relay.look_at_the_terminal_at(at);
            assert_eq!(relay.looks.last, Some(start), "{ms}");
        }
        // The output stops; the look after it is not held back by the interval.
        let settled = start + Duration::from_millis(40) + STATE_SETTLE;
        relay.look_at_the_terminal_at(settled);
        assert_eq!(relay.looks.last, Some(settled));
        // And the next output after that is inside the interval of that look.
        relay.output_read = true;
        relay.look_at_the_terminal_at(settled + LOOK_INTERVAL / 2);
        assert_eq!(relay.looks.last, Some(settled));
    }

    /// The program writes `bytes` to its terminal from a thread of its own: a terminal
    /// holds little, and the write waits for the relay to read.
    fn program_writes(slave: &OwnedFd, bytes: Vec<u8>) -> std::thread::JoinHandle<()> {
        let slave = slave.try_clone().unwrap();
        std::thread::spawn(move || {
            let mut at = 0;
            while at < bytes.len() {
                match sys::write_fd(slave.as_fd(), bytes.get(at..).unwrap_or_default()) {
                    Ok(n) => at += n,
                    Err(_) => return,
                }
            }
        })
    }

    /// Reads turns of the relay until the journal holds `total` bytes.
    fn read_until(relay: &mut Relay, total: u64) {
        let start = Instant::now();
        while relay.journal.total() < total {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "the stream stopped"
            );
            // As the loop of the relay does: a terminal that is not read is not waited on,
            // and the clock looks again.
            if relay.paused(Instant::now()).is_some() {
                std::thread::sleep(THROTTLE_WAIT);
                continue;
            }
            let mut set = [sys::want(relay.master.as_raw_fd(), libc::POLLIN)];
            if sys::poll(&mut set, Some(Duration::from_millis(100))).unwrap() > 0 {
                relay.read_terminal();
            }
        }
    }

    /// The bytes of program output the far end of a client's socket has received.
    fn received(far: &UnixStream, decoder: &mut PacketDecoder) -> usize {
        far.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 65536];
        let mut got = 0;
        while let Ok(n) = (&*far).read(&mut buf) {
            if n == 0 {
                break;
            }
            decoder.push(buf.get(..n).unwrap_or_default());
        }
        while let Ok(Some(packet)) = decoder.next_packet() {
            if let Message::Output { data, .. } = packet.message {
                got += data.len();
            }
        }
        got
    }

    /// Drives the client and reads its socket until `want` bytes of output have come, the
    /// socket taking only so much at a time.
    fn delivered(relay: &mut Relay, far: &UnixStream, want: usize) -> usize {
        let mut decoder = PacketDecoder::new();
        let mut got = 0;
        let start = Instant::now();
        while got < want && start.elapsed() < Duration::from_secs(5) {
            relay.drive(0);
            got += received(far, &mut decoder);
        }
        got
    }

    /// How long `serve` would let `poll` wait at `now`.
    fn timer(relay: &Relay, now: Instant) -> Option<Duration> {
        relay.wake_in(now, relay.paused(now))
    }

    /// A relay reading its terminal, with one client attached from the start of the
    /// stream, and the far end of the client's socket.
    fn relay_with_a_client() -> (Relay, OwnedFd, UnixStream) {
        let (mut relay, slave) = relay_over(Journal::create(MIN_CAPACITY).unwrap());
        relay.live = true;
        relay.reading = true;
        let (client, far) = client_of(false);
        relay.clients.push(client);
        (relay, slave, far)
    }

    /// The relay over a ring of `capacity` bytes, as a session over one reads it.
    fn on_a_ring(relay: &mut Relay, capacity: u64) {
        let journal = Arc::new(Journal::create(capacity).unwrap());
        relay.core = Core::new(
            ScreenState::new(Screen::new(80, 24, 10), 0, 0, 10),
            Arc::clone(&journal),
        );
        relay.journal = journal;
        relay.chunk = usize::try_from((64u64 * 1024).min(capacity / 4)).unwrap();
    }

    /// A relay with a client that reads a pipe where it would read the terminal, so that
    /// a read is as big as what was written, and the pipe's writing end.
    fn relay_over_a_pipe(capacity: u64, chunk: usize) -> (Relay, OwnedFd, UnixStream) {
        let (mut relay, _slave, far) = relay_with_a_client();
        on_a_ring(&mut relay, capacity);
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for the two descriptors `pipe` fills in.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: both descriptors were just made and belong to nobody else.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        sys::set_nonblocking(read.as_raw_fd()).unwrap();
        relay.master = read;
        relay.chunk = chunk;
        (relay, write, far)
    }

    #[test]
    fn a_short_read_is_delivered_in_the_turn_it_was_read() {
        let (mut relay, slave, far) = relay_with_a_client();
        program_writes(&slave, b"hello\r\n".to_vec());
        read_until(&mut relay, 7);
        assert_eq!(delivered(&mut relay, &far, 7), 7);
        // And nothing waits on the clock for it.
        let now = Instant::now();
        assert_eq!(timer(&relay, now), relay.settle.timeout(now));
    }

    #[test]
    fn a_short_read_ends_the_turn_of_a_client_that_is_shown_it_and_a_full_one_does_not() {
        // Reads of a quarter of a full one: the first is short, and what is left in the
        // pipe is for the next turn.
        let (mut relay, write, _far) = relay_over_a_pipe(1 << 20, FULL_READ / 4);
        sys::write_fd(write.as_fd(), &[b'x'; FULL_READ]).unwrap();
        relay.read_terminal();
        assert_eq!(relay.journal.total(), (FULL_READ / 4) as u64);
        // Nobody to show it to: the turn reads on until the terminal is empty.
        relay.clients.clear();
        relay.read_terminal();
        assert_eq!(relay.journal.total(), FULL_READ as u64);
        // Reads of a full one go on while there are some.
        let (mut relay, write, _far) = relay_over_a_pipe(1 << 20, FULL_READ);
        sys::write_fd(write.as_fd(), &vec![b'y'; FULL_READ * 3]).unwrap();
        relay.read_terminal();
        assert_eq!(relay.journal.total(), (FULL_READ * 3) as u64);
    }

    #[test]
    fn a_stream_that_stops_on_a_full_read_goes_out_within_the_wait_and_the_relay_sleeps() {
        let (mut relay, write, far) = relay_over_a_pipe(1 << 20, FULL_READ);
        sys::write_fd(write.as_fd(), &[b'x'; FULL_READ]).unwrap();
        let turn = Instant::now();
        relay.read_terminal();
        let took = turn.elapsed();
        // One wait for more, nothing came, and the turn was over.
        assert!(took < Duration::from_millis(250), "{took:?}");
        assert_eq!(relay.journal.total(), FULL_READ as u64);
        assert_eq!(delivered(&mut relay, &far, FULL_READ), FULL_READ);
        // What is left to wait for is the look after output; with that gone the relay's
        // `poll` has no timeout.
        let now = Instant::now();
        assert_eq!(timer(&relay, now), relay.settle.timeout(now));
        relay.settle.cancel();
        assert_eq!(timer(&relay, now), None);
    }

    #[test]
    fn a_program_that_writes_a_full_read_every_so_often_is_not_gathered_for_long() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let (mut relay, write, far) = relay_over_a_pipe(1 << 20, FULL_READ);
        let stop = Arc::new(AtomicBool::new(false));
        let writer = std::thread::spawn({
            let stop = Arc::clone(&stop);
            move || {
                let data = vec![b'x'; FULL_READ];
                while !stop.load(Ordering::Relaxed) {
                    if sys::write_fd(write.as_fd(), &data).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_micros(500));
                }
            }
        });
        // Reads a little under a millisecond apart: to the end of a frame that is tens of
        // milliseconds, and the limit is a fraction of that. A loaded machine can let one turn
        // find a frame's worth waiting and end another early on a writer that slept long, so
        // the middle turn is what is judged: past half a frame only when the limit is gone.
        let frame = (OUT_CHUNK as u64).min(relay.journal.capacity() / 8);
        let mut turns = Vec::new();
        for _ in 0..9 {
            // A turn that finds nothing is over at once: this one starts on a read. (A signal
            // ends a `poll` with nothing ready.)
            let mut set = [sys::want(relay.master.as_raw_fd(), libc::POLLIN)];
            let start = Instant::now();
            while sys::poll(&mut set, Some(Duration::from_secs(1))).unwrap() == 0 {
                assert!(
                    start.elapsed() < Duration::from_secs(5),
                    "the writer stopped"
                );
            }
            let before = relay.journal.total();
            relay.read_terminal();
            let gathered = relay.journal.total() - before;
            turns.push(gathered);
            let gathered = usize::try_from(gathered).unwrap_or(usize::MAX);
            assert_eq!(delivered(&mut relay, &far, gathered), gathered);
        }
        turns.sort_unstable();
        assert!(turns[turns.len() / 2] < frame / 2, "{turns:?}");
        // The read end goes with the relay, so a writer in a full pipe is let go.
        stop.store(true, Ordering::Relaxed);
        drop(relay);
        writer.join().unwrap();
    }

    #[test]
    fn a_saturated_stream_goes_out_in_frames_of_about_the_size_of_a_chunk() {
        let (mut relay, write, far) = relay_over_a_pipe(8 << 20, FULL_READ);
        let flood = 1 << 20;
        let writer = std::thread::spawn(move || {
            let data = vec![b'x'; FULL_READ];
            for _ in 0..flood / FULL_READ {
                if sys::write_fd(write.as_fd(), &data).is_err() {
                    return;
                }
            }
        });
        // Blocking writes into a pipe the relay reads: it refills as fast as the relay reads.
        let start = Instant::now();
        let (mut decoder, mut sizes, mut turns) = (PacketDecoder::new(), Vec::new(), Vec::new());
        let mut before = 0;
        while relay.journal.total() < (flood / FULL_READ * FULL_READ) as u64 {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "the stream stopped"
            );
            let mut set = [sys::want(relay.master.as_raw_fd(), libc::POLLIN)];
            if sys::poll(&mut set, Some(Duration::from_millis(50))).unwrap() > 0 {
                relay.read_terminal();
                turns.push(relay.journal.total() - before);
                before = relay.journal.total();
            }
            relay.drive(0);
            far.set_nonblocking(true).unwrap();
            let mut buf = vec![0u8; 1 << 20];
            while let Ok(n @ 1..) = (&far).read(&mut buf) {
                decoder.push(buf.get(..n).unwrap_or_default());
            }
            while let Ok(Some(packet)) = decoder.next_packet() {
                if let Message::Output { data, .. } = packet.message {
                    sizes.push(data.len());
                }
            }
        }
        writer.join().unwrap();
        // A turn ends at a frame's worth and a read over it, never past.
        let longest = turns.iter().copied().max().unwrap();
        assert!(
            (OUT_CHUNK as u64..OUT_CHUNK as u64 + FULL_READ as u64).contains(&longest),
            "the longest turn read {longest}"
        );
        assert!(
            sizes.iter().any(|&size| size >= OUT_CHUNK),
            "no frame of a chunk in {} frames",
            sizes.len()
        );
    }

    #[test]
    fn what_a_client_types_reaches_the_program_while_a_flood_is_read() {
        let (mut relay, slave, _far) = relay_with_a_client();
        on_a_ring(&mut relay, 8 << 20);
        let stream = b"some text of the flood, a line of it\r\n".repeat(1 << 15);
        let writer = program_writes(&slave, stream.clone());
        let (mut worst, mut typed) = (Duration::ZERO, 0);
        let start = Instant::now();
        while relay.journal.total() < stream.len() as u64 {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "the stream stopped"
            );
            if relay.paused(Instant::now()).is_some() {
                std::thread::sleep(THROTTLE_WAIT);
                continue;
            }
            let mut set = [sys::want(relay.master.as_raw_fd(), libc::POLLIN)];
            if sys::poll(&mut set, Some(Duration::from_millis(5))).unwrap() > 0 {
                let turn = Instant::now();
                relay.read_terminal();
                worst = worst.max(turn.elapsed());
            }
            // The relay's loop would act on a client's input between two turns.
            if typed < 20 {
                relay.type_input(0, b"k");
                typed += 1;
            }
            relay.drive(0);
        }
        writer.join().unwrap();
        // One key a turn, and a program that keeps the terminal full ends the stream in fewer
        // than twenty of them.
        assert_eq!(typed_to_the_program(&slave), b"k".repeat(typed));
        // A turn waits for more to gather, but only as many times as the budget has: what
        // is typed is not kept from the program for long.
        assert!(worst < Duration::from_millis(250), "a turn took {worst:?}");
    }

    /// A stream that scrolls, colours, moves the cursor and switches screens, `lines` lines
    /// of it.
    fn flood(lines: usize) -> Vec<u8> {
        // A header that stays where it is while the region under it scrolls: a screen
        // begun again part of the way through the stream does not have it.
        let mut stream = b"\x1b[2J\x1b[1;1Hheader row\x1b[5;20r\x1b[5;1H".to_vec();
        for n in 0..lines {
            stream.extend_from_slice(
                format!(
                    "\x1b[3{}mline {n} of the flood \u{e7}ay \u{65e5}\x1b[0m\r\n",
                    n % 8
                )
                .as_bytes(),
            );
            if n % 97 == 0 {
                stream.extend_from_slice(b"\x1b[?1049h\x1b[2J\x1b[5;7Halt\x1b[?1049l");
            }
            if n % 211 == 0 {
                stream.extend_from_slice(b"\x1b[3;1H\x1b[K\x1b[2Aup\x1b]2;title\x07");
            }
        }
        stream
    }

    fn reference_image(stream: &[u8]) -> Vec<u8> {
        let mut screen = Screen::new(80, 24, 10);
        screen.feed(stream);
        screen.image()
    }

    #[test]
    fn a_flood_through_the_smallest_ring_leaves_the_screen_of_the_same_bytes_fed_in_order() {
        // 16 KiB of ring and 4 KiB reads: the ring turns over every few reads, and the
        // screen is parsed behind them by another thread.
        let (mut relay, slave, _far) = relay_with_a_client();
        let stream = flood(12000);
        assert!(stream.len() > 100 * MIN_CAPACITY as usize / 4);
        // The parser is kept busy for a while (the lock is held), so that the reads
        // run ahead of the screen by more than the ring holds, unless the relay stops
        // them.
        let core = Arc::clone(&relay.core);
        let (held, locked) = std::sync::mpsc::channel();
        let stall = std::thread::spawn(move || {
            let guard = core.lock();
            held.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            drop(guard);
        });
        locked.recv().unwrap();
        let writer = program_writes(&slave, stream.clone());
        read_until(&mut relay, stream.len() as u64);
        writer.join().unwrap();
        stall.join().unwrap();
        assert!(
            relay.worker.is_some(),
            "the screen was parsed behind the reads"
        );
        relay.catch_up();
        let picture = relay.picture();
        assert_eq!(picture.offset, stream.len() as u64);
        assert!(picture.bytes == reference_image(&stream));
    }

    #[test]
    fn a_picture_taken_during_a_flood_is_the_screen_of_the_bytes_before_it_fed_in_order() {
        let (mut relay, slave, _far) = relay_with_a_client();
        // A ring that keeps all of it, to build what the picture is compared with.
        let journal = Arc::new(Journal::create(8 << 20).unwrap());
        relay.core = Core::new(
            ScreenState::new(Screen::new(80, 24, 10), 0, 0, 10),
            Arc::clone(&journal),
        );
        relay.journal = journal;
        let stream = flood(20000);
        let writer = program_writes(&slave, stream.clone());
        let mut taken = 0;
        let mut next = 16 * 1024;
        while relay.journal.total() < stream.len() as u64 {
            read_until(&mut relay, next.min(stream.len()) as u64);
            let picture = relay.picture();
            let mut seen = vec![0u8; usize::try_from(picture.offset).unwrap()];
            assert_eq!(relay.journal.copy_out(0, &mut seen), Some(seen.len()));
            assert!(
                picture.bytes == reference_image(&seen),
                "at {}",
                picture.offset
            );
            taken += 1;
            next += 16 * 1024;
        }
        writer.join().unwrap();
        assert!(taken >= 3, "{taken} pictures");
    }

    /// A program asked a question while a client was shown the stream; the client leaves
    /// before the screen has been given those bytes, and the program asks again.
    fn the_question_a_client_was_shown_is_not_answered_when_it_leaves(through_a_read: bool) {
        let (mut relay, slave, _far) = relay_with_a_client();
        if through_a_read {
            program_writes(&slave, b"text\x1b[6n".to_vec());
            read_until(&mut relay, 8);
        } else {
            // As the read leaves it: in the journal, answered, and not yet parsed.
            relay.journal.append(b"text\x1b[6n");
            relay.journal.set_answered(8);
        }
        assert!(relay.attached() > 0);
        relay.clients[0].dead = true;
        assert_eq!(relay.attached(), 0);
        program_writes(&slave, b"more\x1b[6n".to_vec());
        read_until(&mut relay, 16);
        // One answer, for the question nobody was shown, from where the cursor is by then.
        assert_eq!(typed_to_the_program(&slave), b"\x1b[1;9R");
        assert_eq!(relay.journal.answered(), Some(16));
        assert_eq!(relay.core.lock().last_reply, 16);
    }

    #[test]
    fn a_question_in_a_backlog_when_the_client_leaves_is_not_answered_by_the_relay_and_not_lost_for_what_follows()
     {
        the_question_a_client_was_shown_is_not_answered_when_it_leaves(true);
        the_question_a_client_was_shown_is_not_answered_when_it_leaves(false);
    }

    #[test]
    fn a_program_that_clears_the_screen_in_a_loop_does_not_stall_the_relay_for_the_parser() {
        let (mut relay, slave, _far) = relay_with_a_client();
        // A ring that is far larger than the limit: the backlog the parser could be given
        // is not bounded by it.
        on_a_ring(&mut relay, 8 << 20);
        let stream = b"\x1b[2J\x1b[H".repeat(80 * 1024);
        let writer = program_writes(&slave, stream.clone());
        let (mut worst, mut throttled, mut typed) = (Duration::ZERO, 0, 0);
        let start = Instant::now();
        while relay.journal.total() < stream.len() as u64 {
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "the stream stopped"
            );
            let turn = Instant::now();
            if relay.paused(Instant::now()).is_some() {
                throttled += 1;
                relay.core.wake_worker();
                std::thread::sleep(THROTTLE_WAIT);
                worst = worst.max(turn.elapsed());
            } else {
                let mut set = [sys::want(relay.master.as_raw_fd(), libc::POLLIN)];
                if sys::poll(&mut set, Some(Duration::from_millis(5))).unwrap() > 0 {
                    let turn = Instant::now();
                    relay.read_terminal();
                    worst = worst.max(turn.elapsed());
                }
            }
            // A client types all the while, and what it types reaches the program.
            if typed < 20 {
                relay.type_input(0, b"k");
                typed += 1;
            }
            // The backlog the parser has is never more than the limit and one read.
            let behind = relay.journal.total() - relay.core.parsed();
            assert!(
                behind < relay.behind_limit() + relay.chunk as u64,
                "{behind}"
            );
        }
        writer.join().unwrap();
        assert_eq!(typed_to_the_program(&slave), b"k".repeat(20));
        assert!(
            worst < Duration::from_millis(500),
            "a turn took {worst:?} (the terminal was not read in {throttled} of them)"
        );
        // And the screen is the one of the bytes in order.
        relay.catch_up();
        assert!(relay.picture().bytes == reference_image(&stream));
    }

    #[test]
    fn a_client_that_reads_flat_out_is_not_dropped_on_a_small_ring() {
        for kib in [16u64, 32, 64, 128, 256, 512] {
            let capacity = kib * 1024;
            let (mut relay, slave, far) = relay_with_a_client();
            let journal = Arc::new(Journal::create(capacity).unwrap());
            relay.core = Core::new(
                ScreenState::new(Screen::new(80, 24, 10), 0, 0, 10),
                Arc::clone(&journal),
            );
            relay.journal = journal;
            // The chunk a session over a ring of this size reads.
            relay.chunk = usize::try_from((64u64 * 1024).min(capacity / 4)).unwrap();
            let stream = b"some text of the flood, a line of it\r\n".repeat(2 << 15);
            let writer = program_writes(&slave, stream.clone());
            let (mut decoder, mut got) = (PacketDecoder::new(), 0);
            let start = Instant::now();
            while got < stream.len() {
                assert!(
                    start.elapsed() < Duration::from_secs(60),
                    "{kib}K: {got} of {}",
                    stream.len()
                );
                // One turn of the relay's loop, and then the client reads all that turn read, as
                // one that reads flat out does: the socket holds less than a turn when this
                // thread comes late to a terminal the program has filled.
                if relay.paused(Instant::now()).is_none() {
                    let mut set = [sys::want(relay.master.as_raw_fd(), libc::POLLIN)];
                    if sys::poll(&mut set, Some(Duration::from_millis(1))).unwrap() > 0 {
                        relay.read_terminal();
                    }
                }
                while (got as u64) < relay.journal.total() {
                    assert!(
                        start.elapsed() < Duration::from_secs(60),
                        "{kib}K: {got} of {}",
                        stream.len()
                    );
                    relay.drive(0);
                    assert!(
                        relay.clients[0].attach.is_some(),
                        "{kib}K: dropped behind after {got} bytes"
                    );
                    got += received(&far, &mut decoder);
                }
            }
            writer.join().unwrap();
            assert_eq!(got, stream.len(), "{kib}K");
        }
    }

    /// What a client's socket has brought, at most `limit` bytes of it: the bytes of program
    /// output in order, and whether a picture came.
    fn read_slowly(
        far: &UnixStream,
        decoder: &mut PacketDecoder,
        limit: usize,
        got: &mut Vec<u8>,
        picture: &mut bool,
    ) {
        far.set_nonblocking(true).unwrap();
        let mut buf = vec![0u8; limit];
        if let Ok(n @ 1..) = (&*far).read(&mut buf) {
            decoder.push(buf.get(..n).unwrap_or_default());
        }
        while let Ok(Some(packet)) = decoder.next_packet() {
            match packet.message {
                Message::Output { offset, data } => {
                    assert_eq!(offset, got.len() as u64, "bytes out of order or missing");
                    got.extend_from_slice(&data);
                }
                Message::Picture { .. } => *picture = true,
                _ => {}
            }
        }
    }

    #[test]
    fn a_slow_client_that_asked_for_every_byte_is_not_lapped_and_gets_every_byte_in_order() {
        for kib in [16u64, 64, 512] {
            let capacity = kib * 1024;
            let (mut relay, slave, far) = relay_with_a_client();
            on_a_ring(&mut relay, capacity);
            if let Some(attach) = relay.clients[0].attach.as_mut() {
                attach.every_byte = true;
            }
            let wrote = usize::try_from(4 * capacity).unwrap().max(256 * 1024);
            let stream: Vec<u8> = (0..wrote).map(|i| (i % 95) as u8 + 32).collect();
            let writer = program_writes(&slave, stream.clone());
            let (mut decoder, mut got, mut picture) = (PacketDecoder::new(), Vec::new(), false);
            let start = Instant::now();
            while got.len() < stream.len() {
                assert!(
                    start.elapsed() < Duration::from_secs(60),
                    "{kib}K: {} of {}",
                    got.len(),
                    stream.len()
                );
                let now = Instant::now();
                relay.note_laggards(now);
                if relay.paused(now).is_none() {
                    let mut set = [sys::want(relay.master.as_raw_fd(), libc::POLLIN)];
                    if sys::poll(&mut set, Some(Duration::from_millis(1))).unwrap() > 0 {
                        relay.read_terminal();
                    }
                } else {
                    std::thread::sleep(Duration::from_micros(200));
                }
                relay.drive(0);
                assert!(
                    relay.clients[0].attach.is_some(),
                    "{kib}K: lapped after {}",
                    got.len()
                );
                // The client reads a little each turn: far slower than the program writes.
                read_slowly(&far, &mut decoder, 2048, &mut got, &mut picture);
            }
            writer.join().unwrap();
            assert!(got == stream, "{kib}K: not the bytes that were written");
            assert!(!picture, "{kib}K: a picture in place of bytes");
        }
    }

    #[test]
    fn a_client_that_did_not_ask_is_lapped_by_a_program_that_outruns_it_and_gets_a_picture() {
        let (mut relay, slave, _far) = relay_with_a_client();
        on_a_ring(&mut relay, MIN_CAPACITY);
        // Sockets as the kernel makes them, and more bytes than they hold.
        let (client, _far) = client_of(false);
        relay.clients = vec![client];
        let stream = vec![b'x'; 4 << 20];
        let writer = program_writes(&slave, stream.clone());
        let start = Instant::now();
        while relay.journal.total() < stream.len() as u64 {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "the stream stopped"
            );
            let now = Instant::now();
            relay.note_laggards(now);
            // Nothing in the way of the reads, however far behind the client is.
            assert!(relay.paused(now).is_none() || relay.worker.is_some());
            let mut set = [sys::want(relay.master.as_raw_fd(), libc::POLLIN)];
            if relay.paused(now).is_none()
                && sys::poll(&mut set, Some(Duration::from_millis(1))).unwrap() > 0
            {
                relay.read_terminal();
            } else {
                std::thread::sleep(THROTTLE_WAIT);
            }
            // The client is not reading at all.
            relay.drive(0);
        }
        writer.join().unwrap();
        assert!(
            relay.clients[0].dropped_behind_at.is_some(),
            "the ring turned over under it"
        );
        // It comes back for what it missed and is sent a picture of the screen.
        let (mut client, far) = client_of(false);
        client.attach = None;
        relay.clients = vec![client];
        relay.attach(0, 0, None);
        relay.drive(0);
        let (mut decoder, mut got, mut picture) = (PacketDecoder::new(), Vec::new(), false);
        read_slowly(&far, &mut decoder, 1 << 20, &mut got, &mut picture);
        assert!(picture, "a client that is lapped gets a picture");
    }

    /// A client attached from the start of a ring that has just turned over under it.
    fn a_client_the_ring_left_behind() -> (Relay, OwnedFd, UnixStream) {
        let (mut relay, slave, far) = relay_with_a_client();
        relay.settle.cancel();
        relay.journal.append(&vec![b'x'; 2 * MIN_CAPACITY as usize]);
        relay.drive(0);
        assert!(relay.clients[0].dropped_behind_at.is_some());
        (relay, slave, far)
    }

    #[test]
    fn a_client_dropped_behind_reads_the_end_and_what_it_types_still_reaches_the_program() {
        let (mut relay, _slave, far) = a_client_the_ring_left_behind();
        assert!(!relay.clients[0].dead);
        assert_eq!(relay.attached(), 0);
        // Nothing wakes the relay for it until there is something to read.
        let mut set = [sys::want(relay.clients[0].sock.as_raw_fd(), libc::POLLIN)];
        assert_eq!(
            sys::poll(&mut set, Some(Duration::from_millis(20))).unwrap(),
            0
        );
        // It reads to the end of the stream, and its own side is still open.
        far.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 4096];
        assert_eq!((&far).read(&mut buf).unwrap(), 0);
        let typed = Packet::about(relay.id.clone(), Message::Input(vec![3]));
        let mut bytes = Vec::new();
        typed.encode(&mut bytes).unwrap();
        (&far).write_all(&bytes).unwrap();
        relay.client_readable(0);
        assert_eq!(relay.input_queued, 1);
        assert_eq!(relay.input_pending(), 0, "the terminal took it");
        // A `write` is typing too.
        let mut bytes = Vec::new();
        Packet::about(relay.id.clone(), Message::Write(vec![Stroke::text("a")]))
            .encode(&mut bytes)
            .unwrap();
        (&far).write_all(&bytes).unwrap();
        relay.client_readable(0);
        assert_eq!(relay.input_queued, 2);
        // Anything else is not acted on, and nothing goes to it.
        let mut bytes = Vec::new();
        Packet::about(relay.id.clone(), Message::Status)
            .encode(&mut bytes)
            .unwrap();
        (&far).write_all(&bytes).unwrap();
        relay.client_readable(0);
        assert!(relay.clients[0].asked.is_none());
        relay.error(0, ErrorCode::BAD_REQUEST, "not for it");
        assert!(relay.clients[0].out.is_empty());
        // It closes and comes back on another connection: the seat goes with this one.
        drop(far);
        relay.client_readable(0);
        relay.drive(0);
        assert!(relay.clients[0].dead);
    }

    #[test]
    fn a_client_dropped_behind_is_read_to_the_end_of_what_it_typed_in_one_turn() {
        let (mut relay, _slave, far) = a_client_the_ring_left_behind();
        sys::widen_send_buffer(far.as_fd());
        let mut bytes = Vec::new();
        let count = 200;
        for _ in 0..count {
            Packet::about(relay.id.clone(), Message::Input(vec![b'x'; 1024]))
                .encode(&mut bytes)
                .unwrap();
        }
        (&far).write_all(&bytes).unwrap();
        assert!(bytes.len() > CLIENT_READS * 16 * 1024);
        relay.client_readable(0);
        assert_eq!(relay.input_queued, count * 1024);
    }

    #[test]
    fn a_client_dropped_behind_that_does_not_close_gives_its_seat_back_after_the_limit() {
        let (mut relay, _slave, _far) = a_client_the_ring_left_behind();
        let now = Instant::now();
        relay.clients[0].dropped_behind_at = Some(now);
        assert_eq!(timer(&relay, now), Some(DROPPED_BEHIND_LIMIT));
        relay.drive(0);
        assert!(!relay.clients[0].dead);
        relay.clients[0].dropped_behind_at = now.checked_sub(DROPPED_BEHIND_LIMIT);
        relay.drive(0);
        assert!(relay.clients[0].dead);
    }

    #[test]
    fn of_the_clients_that_asked_for_every_byte_the_one_that_keeps_up_sets_the_pace() {
        let (mut relay, _slave, _far) = relay_with_a_client();
        on_a_ring(&mut relay, MIN_CAPACITY);
        if let Some(attach) = relay.clients[0].attach.as_mut() {
            attach.every_byte = true;
        }
        let (other, _other_far) = client_of(false);
        relay.clients.push(other);
        relay.journal.append(&vec![b'x'; 12 * 1024]);
        let total = relay.journal.total();
        let now = Instant::now();
        relay.note_laggards(now);
        let waits = |relay: &Relay| matches!(relay.paused(now), Some(Pause::Client { .. }));
        assert!(waits(&relay));
        // One that did not ask has no say, however far along it is: a client that just came,
        // or one that was lapped and is back at the end, has read nothing yet.
        relay.clients[1].attach.as_mut().unwrap().next = total;
        assert!(waits(&relay));
        // One that asked and is still taking its picture has not kept up with anything.
        let beside = relay.clients[1].attach.as_mut().unwrap();
        beside.every_byte = true;
        beside.picture = Some(Picture {
            offset: total,
            cols: 80,
            rows: 24,
            bytes: Vec::new(),
            sent: 0,
        });
        assert!(waits(&relay));
        // One that asked and keeps up sets the pace.
        relay.clients[1].attach.as_mut().unwrap().picture = None;
        assert_eq!(relay.paused(now), None);
    }

    #[test]
    fn a_client_that_asked_for_every_byte_and_froze_releases_the_program_after_the_limit() {
        let (mut relay, _slave, far) = relay_with_a_client();
        on_a_ring(&mut relay, MIN_CAPACITY);
        relay.settle.cancel();
        if let Some(attach) = relay.clients[0].attach.as_mut() {
            attach.every_byte = true;
        }
        // Behind by more than half the ring.
        relay.journal.append(&vec![b'x'; 12 * 1024]);
        let start = Instant::now();
        relay.note_laggards(start);
        assert_eq!(
            relay.paused(start),
            Some(Pause::Client {
                until: start + STALL_LIMIT
            })
        );
        // The wait for it is the one timer, and it is for as long as is left.
        assert_eq!(timer(&relay, start), Some(STALL_LIMIT));
        let later = start + STALL_LIMIT / 2;
        relay.note_laggards(later);
        assert_eq!(timer(&relay, later), Some(STALL_LIMIT / 2));
        // Not moving for the limit: the program is read again, and there is no timer.
        let over = start + STALL_LIMIT;
        relay.note_laggards(over);
        assert_eq!(relay.paused(over), None);
        assert_eq!(timer(&relay, over), None);
        // It reads again: the wait is for it once more, from now.
        let mut decoder = PacketDecoder::new();
        let (mut got, mut picture) = (Vec::new(), false);
        for _ in 0..8 {
            relay.drive(0);
            read_slowly(&far, &mut decoder, 1 << 20, &mut got, &mut picture);
        }
        assert_eq!(got.len(), 12 * 1024);
        relay.journal.append(&vec![b'y'; 12 * 1024]);
        let again = over + Duration::from_millis(10);
        relay.note_laggards(again);
        assert_eq!(
            relay.paused(again),
            Some(Pause::Client {
                until: again + STALL_LIMIT
            })
        );
    }

    #[test]
    fn the_pause_and_the_timer_that_ends_it_are_the_same_at_every_instant() {
        let (mut relay, _slave, _far) = relay_with_a_client();
        on_a_ring(&mut relay, MIN_CAPACITY);
        relay.settle.cancel();
        if let Some(attach) = relay.clients[0].attach.as_mut() {
            attach.every_byte = true;
        }
        relay.journal.append(&vec![b'x'; 12 * 1024]);
        let start = Instant::now();
        relay.note_laggards(start);
        // The terminal is left out of the set exactly when there is a pause: so wherever the
        // pause is about to end, a timer must be set for it, or `poll` would sleep on with
        // nothing to wake it.
        let nanosecond = Duration::from_nanos(1);
        for at in [
            start,
            start + STALL_LIMIT / 2,
            start + STALL_LIMIT - nanosecond,
            start + STALL_LIMIT,
            start + STALL_LIMIT + nanosecond,
        ] {
            let pause = relay.paused(at);
            let timer = relay.wake_in(at, pause);
            assert_eq!(pause.is_some(), timer.is_some(), "{at:?}");
            if let Some(Pause::Client { until }) = pause {
                assert_eq!(timer, Some(until - at));
            }
        }
    }

    #[test]
    fn a_client_that_asked_for_every_byte_is_not_lapped_by_what_the_program_left_at_its_end() {
        // The smallest ring, which a Linux terminal's buffer fills in a few reads, and a
        // client that stood half of it and a byte behind when the program ended.
        let chunk = usize::try_from(MIN_CAPACITY / 4).unwrap();
        let (mut relay, write, far) = relay_over_a_pipe(MIN_CAPACITY, chunk);
        let half = usize::try_from(MIN_CAPACITY / 2).unwrap();
        sys::widen_send_buffer(relay.clients[0].sock.as_fd());
        if let Some(attach) = relay.clients[0].attach.as_mut() {
            attach.every_byte = true;
        }
        relay.journal.append(&vec![b'a'; half + 1]);
        let left = 3 * chunk;
        sys::write_fd(write.as_fd(), &vec![b'b'; left]).unwrap();
        drop(write);
        // More than the ring holds, in all.
        assert!(half + 1 + left > MIN_CAPACITY as usize);
        relay.drain_to_the_end();
        assert!(
            relay.clients[0].attach.is_some(),
            "lapped while the program's last bytes were read"
        );
        assert_eq!(
            delivered(&mut relay, &far, half + 1 + left),
            half + 1 + left
        );
        assert!(relay.clients[0].attach.as_ref().unwrap().picture.is_none());
    }

    #[test]
    fn a_client_that_takes_a_picture_a_piece_at_a_time_is_not_one_that_stood_still() {
        let (mut relay, _slave, _far) = relay_with_a_client();
        let start = Instant::now();
        let Some(attach) = relay.clients[0].attach.as_mut() else {
            panic!("the client is attached");
        };
        attach.every_byte = true;
        attach.picture = Some(Picture {
            offset: 0,
            cols: 80,
            rows: 24,
            bytes: vec![b'p'; 3 * OUT_CHUNK],
            sent: 0,
        });
        // It was waited for since before the picture began: `next` does not move while a
        // picture is sent, so a piece taken is the only sign that it is reading.
        attach.waited_since = Some(start);
        relay.drive(0);
        let attach = relay.clients[0].attach.as_ref().unwrap();
        assert!(
            attach
                .picture
                .as_ref()
                .is_none_or(|picture| picture.sent > 0)
        );
        assert_eq!(attach.waited_since, None);
    }

    #[test]
    fn a_session_with_a_client_that_asked_for_every_byte_and_is_up_to_date_has_no_timeout() {
        let (mut relay, _slave, _far) = relay_with_a_client();
        relay.settle.cancel();
        if let Some(attach) = relay.clients[0].attach.as_mut() {
            attach.every_byte = true;
        }
        let now = Instant::now();
        relay.note_laggards(now);
        assert_eq!(relay.paused(now), None);
        assert_eq!(timer(&relay, now), None);
        // Caught up after being behind: the count against it is gone.
        relay.journal.append(&vec![b'x'; 12 * 1024]);
        relay.note_laggards(now);
        relay.clients[0].attach.as_mut().unwrap().next = relay.journal.total();
        relay.note_laggards(now);
        assert_eq!(relay.clients[0].attach.as_ref().unwrap().waited_since, None);
        assert_eq!(timer(&relay, now), None);
    }

    #[test]
    fn the_view_a_client_attaches_with_says_whether_it_wants_every_byte() {
        for asked in [false, true] {
            let (mut relay, _slave, _far) = relay_with_a_client();
            relay.clients[0].attach = None;
            let mut view = View::new(Size {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            });
            view.every_byte = asked;
            relay.attach(0, OFFSET_END, Some(view));
            let attach = relay.clients[0].attach.as_ref().unwrap();
            assert_eq!(attach.every_byte, asked);
        }
    }

    #[test]
    fn only_a_client_that_asked_for_every_byte_is_given_a_wide_socket() {
        // How much of a stream the relay's end of the socket takes before it is full.
        let room_after_attach = |asked: bool| {
            let (mut relay, _slave, _far) = relay_with_a_client();
            relay.clients[0].attach = None;
            let mut view = View::new(Size {
                cols: 80,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            });
            view.every_byte = asked;
            relay.attach(0, OFFSET_END, Some(view));
            let mut room = 0;
            let chunk = [0u8; 4096];
            while let Ok(n) = (&relay.clients[0].sock).write(&chunk) {
                room += n;
                if room > 4 << 20 {
                    break;
                }
            }
            room
        };
        let (plain, wide) = (room_after_attach(false), room_after_attach(true));
        // Linux grants no more than `net.core.wmem_max`, which may be what it already was.
        assert!(wide >= plain, "{plain} then {wide}");
        // Where the default is 8 KiB, a megabyte is given.
        #[cfg(target_os = "macos")]
        assert!(
            plain < 64 * 1024 && wide > 512 * 1024,
            "{plain} then {wide}"
        );
    }

    #[test]
    fn a_pause_for_the_screen_keeps_its_timer_when_the_screen_catches_up_before_the_poll() {
        let (mut relay, _slave, _far) = relay_with_a_client();
        relay.worker = Some(Worker::start(&relay.core).unwrap());
        relay
            .journal
            .append(&vec![b'x'; usize::try_from(relay.behind_limit()).unwrap()]);
        let now = Instant::now();
        let pause = relay.paused(now);
        assert_eq!(pause, Some(Pause::Parser));
        // The worker moves between the look that left the terminal out and the `poll`.
        relay.core.parsed_to(relay.journal.total());
        assert_eq!(relay.paused(now), None);
        assert_eq!(relay.wake_in(now, pause), Some(THROTTLE_WAIT));
    }

    #[test]
    fn the_worker_parses_only_what_a_client_was_shown_and_rests_when_it_has_caught_up() {
        let (relay, _slave, _far) = relay_with_a_client();
        relay.journal.append(b"abc\x1b[6n");
        // Nothing is answered: the worker has nothing to do, and the relay's thread feeds.
        relay.journal.set_answered(3);
        let worker = Worker::start(&relay.core).unwrap();
        relay.core.wake_worker();
        let start = Instant::now();
        while relay.core.parsed() < 3 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the worker did not parse"
            );
            std::thread::yield_now();
        }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(relay.core.lock().at, 3, "not past what was answered");
        // More shown: it wakes, takes it, and stops again.
        relay.journal.set_answered(7);
        relay.core.wake_worker();
        while relay.core.parsed() < 7 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the worker did not wake"
            );
            std::thread::yield_now();
        }
        // Dropped, it is gone and the screen is the relay's alone again.
        drop(worker);
        assert_eq!(relay.core.lock().at, 7);
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

    /// The start of a connection whose handshake is over.
    fn opened() -> Opening {
        let mut hello = Vec::new();
        WIRE.write_hello(&mut hello).unwrap();
        let mut opening = Opening::default();
        opening.serve(&WIRE, &hello, |_| {}).unwrap();
        opening
    }

    /// A client over a socket whose far end is returned (it must outlive the
    /// test, or the relay's writes find it closed), attached as one that asks
    /// for the session to be kept or as one that does not, and not yet told.
    fn client_of(keeps: bool) -> (Client, UnixStream) {
        let (sock, far) = UnixStream::pair().unwrap();
        sock.set_nonblocking(true).unwrap();
        let client = Client {
            sock,
            opening: opened(),
            decoder: PacketDecoder::new(),
            out: Vec::new(),
            sent: 0,
            attach: Some(Attach {
                next: 0,
                keep_after_end: keeps,
                every_byte: false,
                waited_since: None,
                name: None,
                picture: None,
            }),
            ended_queued: false,
            closing: false,
            dead: false,
            upgrading: false,
            asked: None,
            waiting: false,
            pings: VecDeque::new(),
            typed_to: 0,
            dropped_behind_at: None,
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
        assert_eq!(timer(&relay, now), None);

        relay.drive(0);
        relay.deliver_if_due(now);
        assert!(relay.delivered_sent, "told, so the session may go");
        assert_eq!(timer(&relay, now), None);
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
        let due = timer(&relay, now).unwrap();
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
        assert_eq!(timer(&relay, now), None, "it fired once");
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
        let due = timer(&relay, now).unwrap();
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
        let journal = Journal::create(MIN_CAPACITY).unwrap();
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
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(&rows(1, 40));
        let (mut relay, _slave) = relay_over(journal);
        let (mut client, _far) = client_of(false);
        client.attach = None;
        relay.clients.push(client);
        relay.feed_screen(Feed::Spent);
        let before = relay.core.lock().screen.text(10, false);
        relay.forget(0);
        assert_eq!(relay.journal.floor(), 0);
        assert_eq!(relay.core.lock().screen.text(10, false), before);
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
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(&rows(1, 40));
        journal.set_floor(journal.total());
        journal.append(&rows(41, 43));
        assert_eq!(replay_start(&journal, REPLAY_LIMIT), 0);
        let (mut relay, _slave) = relay_over(journal);
        relay.feed_screen(Feed::Spent);
        let text = relay.core.lock().screen.text(10, false);
        assert_eq!(relay.core.lock().forgotten, relay.journal.floor());
        assert_eq!(text.len(), 24 + 3, "{text:?}");
        assert_eq!(text.first().map(String::as_str), Some("row18"), "{text:?}");
        assert!(text.iter().any(|row| row == "row40"), "{text:?}");

        // The same journal without the floor keeps the lines before it.
        let plain = Journal::create(MIN_CAPACITY).unwrap();
        plain.append(&rows(1, 43));
        let (mut relay, _slave) = relay_over(plain);
        relay.feed_screen(Feed::Spent);
        assert_eq!(relay.core.lock().screen.text(10, false).len(), 24 + 10);
    }

    #[test]
    fn a_screen_that_begins_after_the_floor_has_nothing_from_before_it_to_forget() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(&rows(1, 10));
        journal.set_floor(journal.total());
        journal.append(&rows(11, 30));
        let (mut relay, _slave) = relay_over(journal);
        // The replay starts at a line past the floor (a long journal's limit).
        let start = relay.journal.line_start_from(relay.journal.floor() + 3);
        relay.core.lock().at = start;
        relay.feed_screen(Feed::Spent);
        assert_eq!(relay.core.lock().forgotten, relay.journal.floor());
        assert_eq!(relay.core.lock().at, relay.journal.total());
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
