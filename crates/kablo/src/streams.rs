//! Streams: TCP connections and programs carried over the connection to a
//! bridge.
//!
//! This is the one engine of both ends. The bridge (the [`Side::Server`]) makes
//! the connection to the target an `open` names, or starts the program it
//! names; a client (the [`Side::Client`]) hands it a socket it has accepted and
//! has the bridge open the other half, or asks it to run a program and hands it
//! the descriptors that are the program's input, output and errors. Once a
//! stream is open the two ends are alike, and so is the code: bytes move
//! between a socket (or a program's pipes, or the client's own) and the wire
//! under a **window** each way: see **Programs**.
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
//! **How large.** A window has to hold what is on its way while the credit for
//! it comes back, or a transfer waits a round trip for every window, and a
//! window larger than that only fills the carrier's queues, which typing on
//! the same connection then waits behind. So a stream's window is what the
//! link needs, measured by this end from its own credit: how long it takes to
//! come back as bytes, and the rate they come at ([`crate::tune`]; a new
//! stream starts at what the last one learned, or [`Config::start`]). That is
//! within a part of [`Config::budget`],
//! which the streams of a connection share equally, at most [`Config::window`]
//! (ssh's own window for a channel) and at least [`Config::least`], so that a
//! hundred streams keep the memory of a hundred small windows. Both are looked
//! at whenever credit is due: credit is what keeps the window where it should
//! be, given until what the other end may still send and what this end holds
//! unwritten come to it, more of it when the window grew and less when it
//! shrank.
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
//! stream's queued frames with it. The end of a program's stream is the one
//! control packet pair that is queued with the data: its `exited` and the
//! `close` after it say what happened *after* the last byte, so they wait
//! behind the `eof`, and the stream is let go without taking the frames with it.
//! (The client end of a command takes the same pair the other way: bytes of the
//! program's output that have been received and not yet written here are not
//! lost to the `close` that follows `exited`.)
//!
//! **A stream that is not valid** (data past its window, past its end or
//! before it was opened, a credit past [`MAX_WINDOW`]) is ended with a
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
//!
//! **Programs** (server end). An `open` that names a command starts it on the
//! spot ([`crate::process`]: a fork and an `exec`, which is as quick as the
//! system makes them) and answers `opened` or `failed` at once; nothing
//! connects, so nothing is waiting. The stream's end is then the program's
//! three pipes instead of a socket. The client's `data` is written to its
//! input, and shut when the client says `eof`; its output and its errors are
//! read like a socket is, out of the same window and under the same rule on
//! what waits for the wire, and told apart on the wire by the kind of message
//! (`data`, `stderr`). The stream's own `eof` is sent when both have ended, and
//! its `exited`, then `close`, when the program has ended too ([`Token::Exit`]
//! is how that is learned); the stream is over for the client without waiting
//! for its `eof`. A program that leaves another holding its pipes keeps the
//! stream open until that one lets go or the client closes it.
//!
//! **A command, at the client** ([`Streams::open_command`]). The program is on
//! the other side; here its standard input is read from a descriptor the owner
//! gives ([`Local`]) and its standard output and errors are written to two
//! more, which the owner may share with the whole system (a terminal, a pipe in
//! a pipeline) and so are never made non-blocking: each is written only when a
//! look says it takes bytes, and in pieces a pipe takes whole, so that nothing
//! here waits on a reader that is slow. Standard output and errors are two
//! queues paid for out of the one window, like the server's two pipes. The
//! stream is over for the owner ([`Event::Exited`]) when the server has said how
//! the program ended and everything it wrote has been written; the server's
//! `close` that follows is not an abort. A stream that is closed before that
//! (the server closed it, or it broke the rules, or what the program wrote
//! cannot be written here) is [`Event::Closed`], and is told to the server. What
//! the program wrote as errors and cannot be written is dropped, with credit
//! for it, and the program goes on: a reader of errors that went away is not a
//! reason to end it; one of its output is, since a program nobody hears is not
//! worth running.
//!
//! **A conversation of mail, at the client** ([`Streams::open_mail`]) rides the
//! same end: a program on the server hands bytes to the app at the other end of
//! a bridge's mailbox. It has the input and output of [`Local`] and nothing
//! else: no errors, no program, no exit status (what arrives for either ends
//! the stream). It is over ([`Event::Over`]) when both ends have sent `eof`
//! and everything received has been written, or when the app closes it, once
//! what it said before has been written; the app's `close` is not an abort.
//!
//! A program lasts as long as its stream. A `close` (and a stream that breaks
//! the rules) tells the program's whole process group to end and keeps the
//! program for [`Config::kill_wait`]: it is collected the moment the group is
//! gone, or killed when the time is up, so that nothing is left running or
//! waiting to be collected. The wait is a timer with that stop condition and is
//! on the loop's deadline ([`Streams::deadline`]) only while there are such
//! programs. When the connection ends the owner calls [`Streams::shutdown`],
//! which does the same for every program that is still running and waits for it
//! (the one place that blocks, and it is on the way out). A bridge that is
//! killed outright leaves its programs to the system, as it does anything.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use kablo_proto::v1::stream::{
    CONNECT_TIMEOUT, Command, FailCode, MAX_CONNECTING, MAX_DATA, MAX_HOST, MAX_STREAMS,
    MAX_WINDOW, Message, Packet, Side,
};
use kablo_proto::v1::{Exit, Features, Message as SessionMessage};

use crate::dial::{self, Refusal, Target};
use crate::outbound::Frames;
use crate::process::{self, COMMAND_KILL_WAIT, Pipes, Program};
use crate::sys;
use crate::tune::{self, Rtt, Tuner};

/// Bytes of `data` frames that may wait for the wire before sockets are not
/// read.
pub const QUEUED_SOFT: usize = 4 * MAX_DATA;

/// The most a pipe is sure to take in one write once it has said it takes
/// bytes (POSIX's `PIPE_BUF`, which `libc` has for Linux only).
#[cfg(target_os = "linux")]
const PIPE_BUF: usize = libc::PIPE_BUF;
#[cfg(not(target_os = "linux"))]
const PIPE_BUF: usize = 512;

/// How a target is reached: its address and the time by which it must be.
/// Tests put their own here.
pub type Dial = Arc<dyn Fn(&Target, Instant) -> Result<TcpStream, Refusal> + Send + Sync>;

/// The largest window a stream is given: ssh's own window for a channel, which
/// is what a transfer carried over ssh gets in any case.
pub const WINDOW: u32 = 2 * 1024 * 1024;

/// The smallest window a stream is given when many share the budget.
pub const LEAST_WINDOW: u32 = 256 * 1024;

/// The window the streams of a connection share: a least window for every
/// stream a connection may have.
pub const WINDOW_BUDGET: u32 = MAX_STREAMS * LEAST_WINDOW;

const _: () = assert!(WINDOW <= MAX_WINDOW && LEAST_WINDOW <= WINDOW);

/// What an end is willing to do.
#[derive(Clone)]
pub struct Config {
    /// The largest window given to a stream: what the other end may send
    /// before it is given credit, when few streams share [`Config::budget`].
    pub window: u32,
    /// The window all streams share; each is given an equal part, within
    /// [`Config::least`] and [`Config::window`].
    pub budget: u32,
    /// The smallest window given to a stream, however many share the budget.
    pub least: u32,
    /// The window a stream starts with before the connection has learned what
    /// the link needs.
    pub start: u32,
    /// The most streams open at once, the ones still connecting included.
    pub max_streams: usize,
    /// The most being connected at once.
    pub max_connecting: usize,
    /// How long a target has to answer.
    pub connect_timeout: Duration,
    /// How targets are reached.
    pub dial: Dial,
    /// How long a program told to end has before it is killed.
    pub kill_wait: Duration,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            window: WINDOW,
            budget: WINDOW_BUDGET,
            least: LEAST_WINDOW,
            start: tune::START,
            max_streams: MAX_STREAMS as usize,
            max_connecting: MAX_CONNECTING as usize,
            connect_timeout: CONNECT_TIMEOUT,
            dial: Arc::new(dial::connect),
            kill_wait: COMMAND_KILL_WAIT,
        }
    }
}

/// What a descriptor in the owner's poll set stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Token {
    /// The socket of this stream.
    Stream(u32),
    /// The input of the program of this stream, which has bytes to take.
    Stdin(u32),
    /// The output of the program of this stream.
    Stdout(u32),
    /// The errors of the program of this stream.
    Stderr(u32),
    /// Client end of a command: the descriptor the program's input is read
    /// from has bytes.
    LocalInput(u32),
    /// Client end of a command: the descriptor its output is written to takes
    /// bytes.
    LocalOutput(u32),
    /// Client end of a command: the descriptor its errors are written to takes
    /// bytes.
    LocalErrors(u32),
    /// The end of the program of this stream.
    Exit(u32),
    /// The end of a program (this process number) whose stream is over.
    Dying(u32),
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
    /// A command's program ended as `exit` says, and everything it wrote has
    /// been written to the descriptors of [`Local`] (or has nowhere to go: the
    /// program had ended all the same). The stream is gone.
    Exited {
        /// The stream.
        stream: u32,
        /// How the program ended.
        exit: Exit,
    },
    /// A conversation of mail is over: the app closed it, or both ends have
    /// said all they had, and everything the app said has been written to the
    /// descriptors of [`Local`]. The stream is gone.
    Over {
        /// The stream.
        stream: u32,
    },
    /// A command's (or a conversation's) stream was ended before its end was
    /// told. The stream is gone.
    Closed {
        /// The stream.
        stream: u32,
        /// Who ended it.
        cause: Cause,
    },
}

/// Who ended a command's stream before its program's end was told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The server closed it, or broke the rules of the protocol.
    Server,
    /// This end did: the program's output could not be written here, for this
    /// reason (`BrokenPipe` is a reader that went away).
    Local(io::ErrorKind),
}

/// The descriptors of a command that is run for this end (client): where its
/// standard input comes from and where its standard output and errors go. They
/// are the owner's to give, and none is made non-blocking; the same descriptor
/// may be given for the output and the errors.
#[derive(Debug)]
pub struct Local {
    /// Read to give the program input; at its end the program's input ends.
    pub input: OwnedFd,
    /// Written with what the program writes.
    pub output: OwnedFd,
    /// Written with what the program writes as errors.
    pub errors: OwnedFd,
}

/// What the other end of a stream is.
enum End {
    Socket(TcpStream),
    Program(Run),
    /// This end's own descriptors, for a command run on the server.
    Local(Held),
}

/// [`Local`], with the input gone once it is over.
struct Held {
    input: Option<OwnedFd>,
    output: OwnedFd,
    errors: OwnedFd,
    /// Whether the output and the errors are regular files, which take what
    /// they are given without being asked.
    files: [bool; 2],
    /// A conversation of mail, not a command: the other end is an app that a
    /// program on the server talks to. It has no errors and no exit status, and
    /// is over when both ends have said all they had or the app closes it.
    mail: bool,
}

impl Held {
    /// Writes to the output or the errors, if it takes bytes now (and
    /// `WouldBlock` if not): the descriptor may be shared, so it is asked
    /// rather than made non-blocking, and no more is given than a pipe takes
    /// whole. (A regular file is never asked: it takes what it is given, and a
    /// transfer into one is not made of pieces of a pipe's size.)
    fn write(&self, channel: Channel, bytes: &[u8]) -> io::Result<usize> {
        let (fd, file) = match channel {
            Channel::Out => (&self.output, self.files[0]),
            Channel::Err => (&self.errors, self.files[1]),
        };
        if file {
            return sys::write_fd(fd.as_fd(), bytes);
        }
        let mut asked = [sys::want(fd.as_raw_fd(), libc::POLLOUT)];
        if sys::poll(&mut asked, Some(Duration::ZERO))? == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        sys::write_fd(
            fd.as_fd(),
            bytes.get(..bytes.len().min(PIPE_BUF)).unwrap_or(bytes),
        )
    }
}

/// A program, and the pipes this end holds to it. A pipe is gone once it is
/// over: the output and the errors when they have ended, the input when it is
/// shut. That is also what leaves it out of the descriptors waited for.
struct Run {
    program: Program,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
}

/// Which of a program's two outputs is read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Channel {
    Out,
    Err,
}

impl End {
    /// Whether this is a command run for this end: the only stream that has
    /// errors and an exit status.
    fn is_command(&self) -> bool {
        matches!(self, End::Local(held) if !held.mail)
    }

    /// Whether this is a conversation of mail.
    fn is_mail(&self) -> bool {
        matches!(self, End::Local(held) if held.mail)
    }

    /// Writes toward the other end: the socket, the program's input (`channel`
    /// says which of the two outputs a client's own descriptors are written
    /// for). A program that has closed its input is a broken pipe, as a socket
    /// that is reset.
    fn write(&mut self, channel: Channel, bytes: &[u8]) -> io::Result<usize> {
        match self {
            End::Socket(socket) => socket.write(bytes),
            End::Program(run) => match &run.stdin {
                Some(pipe) => sys::write_fd(pipe.as_fd(), bytes),
                None => Err(io::ErrorKind::BrokenPipe.into()),
            },
            End::Local(local) => local.write(channel, bytes),
        }
    }

    /// Reads what the other end says; a program's output or its errors.
    fn read(&mut self, channel: Channel, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            End::Socket(socket) => socket.read(buf),
            End::Program(run) => {
                let pipe = match channel {
                    Channel::Out => &run.stdout,
                    Channel::Err => &run.stderr,
                };
                match pipe {
                    Some(pipe) => sys::read_fd(pipe.as_fd(), buf),
                    None => Err(io::ErrorKind::WouldBlock.into()),
                }
            }
            End::Local(local) => match &local.input {
                Some(input) => sys::read_fd(input.as_fd(), buf),
                None => Err(io::ErrorKind::WouldBlock.into()),
            },
        }
    }

    /// The writing half is over: the socket is shut, the program's input is
    /// closed.
    fn shut(&mut self) {
        match self {
            End::Socket(socket) => {
                let _ = socket.shutdown(Shutdown::Write);
            }
            End::Program(run) => run.stdin = None,
            // Nothing to shut: the descriptors are the owner's.
            End::Local(_) => {}
        }
    }

    /// `channel` has nothing more. Whether nothing is left to read.
    fn end_reading(&mut self, channel: Channel) -> bool {
        match self {
            End::Socket(_) => true,
            End::Program(run) => {
                match channel {
                    Channel::Out => run.stdout = None,
                    Channel::Err => run.stderr = None,
                }
                run.stdout.is_none() && run.stderr.is_none()
            }
            End::Local(local) => {
                local.input = None;
                true
            }
        }
    }
}

/// One open stream.
struct Stream {
    end: End,
    /// Client end: `open` was sent and `opened` has not come.
    opening: bool,
    /// Bytes the other end still lets this end send.
    send_window: u32,
    /// Bytes this end still lets the other end send. With what is held
    /// unwritten (`inbox`, `errors`) it is the window this stream has now;
    /// credit brings that to what the tuner says, within the stream's part
    /// ([`Streams::part`]).
    recv_window: u32,
    /// Bytes received in all.
    received: u64,
    /// How large this end's window should be.
    tuner: Tuner,
    /// Bytes received and not yet written to the socket.
    inbox: VecDeque<u8>,
    /// Client end of a command: the program's errors received and not yet
    /// written. They are paid for out of the same window as the output.
    errors: VecDeque<u8>,
    /// Client end of a command: how the server said the program ended. After
    /// that nothing more arrives but the `close`.
    exited: Option<Exit>,
    /// Client end of a command: the errors cannot be written any more. What
    /// arrives for them is dropped, with credit for it, and the program goes
    /// on.
    errors_dead: bool,
    /// This end has sent `eof`: the socket said it had nothing more.
    sent_eof: bool,
    /// The other end has sent `eof`.
    got_eof: bool,
    /// Mail: the app closed the conversation. What it said before is still to
    /// be written here, and nothing more is sent to it.
    peer_closed: bool,
    /// The writing half of the socket (the program's input) is shut.
    shut: bool,
    /// Writing to the socket failed for good (the other end of it is gone):
    /// what arrives for it is dropped, with credit given for it so the other
    /// end is not held up, and the reading half goes on, so that what the
    /// socket said before and after is still carried.
    write_dead: bool,
}

impl Stream {
    fn new(end: End, send_window: u32, recv_window: u32, opening: bool) -> Stream {
        Stream {
            end,
            opening,
            send_window,
            recv_window,
            received: 0,
            tuner: Tuner::new(recv_window),
            inbox: VecDeque::new(),
            errors: VecDeque::new(),
            exited: None,
            errors_dead: false,
            sent_eof: false,
            got_eof: false,
            peer_closed: false,
            shut: false,
            write_dead: false,
        }
    }

    /// Bytes received and not written yet: the part of the window this end
    /// holds.
    fn held(&self) -> u32 {
        u32::try_from(self.inbox.len().saturating_add(self.errors.len())).unwrap_or(u32::MAX)
    }

    /// Whether the socket may be read: the stream is open, has window to send
    /// into, and its socket has not said it has nothing more.
    fn can_send(&self) -> bool {
        // (A command's program that has ended reads no more.)
        !self.opening
            && !self.sent_eof
            && self.send_window > 0
            && self.exited.is_none()
            && !self.peer_closed
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

/// Judges, for a client that wants a program run, the first message that came
/// back after `features` and `ping` were sent: the bridge's features if it runs
/// commands. A bridge that does not (one from before commands, or one that
/// answered something else) gives `None`; the client waits for nothing and
/// never guesses. Commands do not depend on streams: a bridge whose
/// administrator turned those off still runs them, and says so here.
pub fn judge_commands(first: &SessionMessage) -> Option<Features> {
    match first {
        SessionMessage::FeaturesReply(features) if features.commands => Some(features.clone()),
        _ => None,
    }
}

/// A program whose stream is over and that has been told to end.
struct Dying {
    program: Program,
    /// When its group is killed if anything of it is still there; none once it
    /// has been.
    kill_at: Option<Instant>,
}

/// How long [`Streams::shutdown`] waits for a program it has killed to be
/// collected before it leaves it to the system: a program the system will not
/// let go of (it is waiting on a disk) is not worth holding up an exit for.
const SHUTDOWN_REAP_WAIT: Duration = Duration::from_secs(1);

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
    /// Server end: streams of the connection that something other than these
    /// keeps (the conversations of mail), which count against the limit.
    held: usize,
    connecting: BTreeMap<u32, Connecting>,
    connector: Option<Connector>,
    /// Programs whose streams are over, by process number. Not streams: the
    /// client has been told, and the number may be used again.
    dying: BTreeMap<u32, Dying>,
    /// Server end: why streams are off, while they are.
    closed: Option<String>,
    data: VecDeque<Queued>,
    data_bytes: usize,
    /// Streams that have been given bytes (or an `eof`) to write.
    dirty: BTreeSet<u32>,
    events: Vec<Event>,
    scratch: Vec<u8>,
    /// The connection's round trip, as the streams' credit measures it.
    rtt: Rtt,
    /// The window the last stream to measure the link settled on: where the
    /// next one starts.
    learned: Option<u32>,
}

/// The least a quarter of `window` and one byte: how many written bytes are
/// kept from the other end before they are given back.
fn credit_step(window: u32) -> u32 {
    (window / 4).max(1)
}

/// Answers an `open` that cannot be taken.
fn refuse(out: &mut Vec<Packet>, id: u32, code: FailCode, text: &str) {
    out.push(Packet::new(
        id,
        Message::Failed {
            code,
            text: text.to_owned(),
        },
    ));
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
            held: 0,
            connecting: BTreeMap::new(),
            connector: None,
            dying: BTreeMap::new(),
            closed: None,
            data: VecDeque::new(),
            data_bytes: 0,
            dirty: BTreeSet::new(),
            events: Vec::new(),
            scratch: vec![0; MAX_DATA],
            rtt: Rtt::new(),
            learned: None,
        }
    }

    /// Server end: takes the number of an `open` that something else answers (a
    /// ticket's), so that the numbers stay as strictly rising as they are for
    /// the `open`s these streams answer. `false` when the number is not above
    /// every one seen: a leftover of a stream that is over, or a mistake, which
    /// is not answered.
    pub fn reserve(&mut self, id: u32) -> bool {
        if id <= self.highest {
            return false;
        }
        self.highest = id;
        true
    }

    /// Server end: how many streams of the connection something else holds,
    /// which an `open` is judged against along with the ones here.
    pub fn hold(&mut self, held: usize) {
        self.held = held;
    }

    /// The side this end speaks as: the one to encode the packets it makes with.
    pub fn side(&self) -> Side {
        self.side
    }

    /// How many streams there are, the ones still connecting included.
    pub fn len(&self) -> usize {
        self.streams.len() + self.connecting.len()
    }

    /// The window a stream is given while `sharing` streams (it among them)
    /// share the budget: an equal part, within the least and the largest.
    fn part(&self, sharing: usize) -> u32 {
        let config = &self.config;
        let least = config.least.min(config.window);
        let sharing = u32::try_from(sharing.max(1)).unwrap_or(u32::MAX);
        (config.budget / sharing).clamp(least, config.window)
    }

    /// The window a stream about to be added starts with: what the link was
    /// last found to need (or the start), within the part it has when the
    /// ones there are and it share the budget.
    fn first_window(&self) -> u32 {
        let part = self.part(self.len().saturating_add(1));
        self.learned.unwrap_or(self.config.start).min(part)
    }

    /// Whether there are no streams.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Client end: how many streams were opened and not yet answered. A server
    /// connects only so many at once and fails the rest, so a client that
    /// would open more waits for answers instead.
    pub fn opening(&self) -> usize {
        self.streams
            .values()
            .filter(|stream| stream.opening)
            .count()
    }

    /// Server end: turns streams off, with the reason an `open` of a connection
    /// is answered with (or on again with `None`). Open streams are not
    /// touched, and neither are commands: whoever may log in may run a program.
    pub fn set_closed(&mut self, why: Option<String>) {
        self.closed = why;
    }

    /// Client end of a command: whether the server has said how the program
    /// ended; of a conversation of mail, whether the app has closed it. The
    /// stream is then over but for what was said and is still to be written
    /// here ([`Event::Exited`], [`Event::Over`] tell when it is all written).
    pub fn ended(&self, id: u32) -> bool {
        self.streams
            .get(&id)
            .is_some_and(|s| s.exited.is_some() || s.peer_closed)
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
        let window = self.first_window();
        let packet = Packet::new(
            id,
            Message::Open {
                window,
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
            .insert(id, Stream::new(End::Socket(socket), 0, window, true));
        out.push(packet);
        Ok(id)
    }

    /// Client end: opens a stream that runs `command` on the server, with the
    /// descriptors of `local` for its input, output and errors. The input is not
    /// read until the server has said `opened`. Returns the stream's number;
    /// the `open` is in `out`.
    pub fn open_command(
        &mut self,
        local: Local,
        command: Command,
        out: &mut Vec<Packet>,
    ) -> io::Result<u32> {
        let window = self.first_window();
        self.open_local(
            local,
            window,
            Message::OpenCommand { window, command },
            false,
            out,
        )
    }

    /// Client end: opens a stream that hands bytes to the app at the other end
    /// of a mailbox (a bridge's), with `local`'s input and output for what is
    /// said each way; `local.errors` is not used (a conversation has none: what
    /// arrives for it ends the stream). The input is not read until the bridge
    /// has said `opened`, which it does when the app has taken the conversation.
    /// Returns the stream's number; the `open` is in `out`.
    pub fn open_mail(&mut self, local: Local, out: &mut Vec<Packet>) -> io::Result<u32> {
        let window = self.first_window();
        self.open_local(local, window, Message::open_mail(window), true, out)
    }

    /// The part of [`Streams::open_command`] and [`Streams::open_mail`] that is
    /// the same: `open` is the message that asks (it gives `window`), and `mail`
    /// says what the stream is.
    fn open_local(
        &mut self,
        local: Local,
        window: u32,
        open: Message,
        mail: bool,
        out: &mut Vec<Packet>,
    ) -> io::Result<u32> {
        if self.streams.len() >= self.config.max_streams {
            return Err(io::Error::other("too many streams"));
        }
        let id = self
            .last_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("out of stream numbers"))?;
        let packet = Packet::new(id, open);
        // The writer refuses a command no server could run.
        packet
            .encode(self.side, &mut Vec::new())
            .map_err(io::Error::other)?;
        self.last_id = id;
        let Local {
            input,
            output,
            errors,
        } = local;
        let files = [
            sys::is_regular_file(output.as_fd()),
            sys::is_regular_file(errors.as_fd()),
        ];
        let held = Held {
            input: Some(input),
            output,
            errors,
            files,
            mail,
        };
        self.streams
            .insert(id, Stream::new(End::Local(held), 0, window, true));
        out.push(packet);
        Ok(id)
    }

    /// Client end: ends a stream from here. The server is told with a `close`,
    /// which ends a command's program; what has not been written yet is
    /// dropped. Nothing happens for a stream that is not there.
    pub fn close(&mut self, id: u32, out: &mut Vec<Packet>) {
        if self.streams.contains_key(&id) {
            self.reset(id, out);
        }
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
        let mut fail = |code: FailCode, text: &str| refuse(out, id, code, text);
        if let Some(why) = &self.closed {
            return fail(FailCode::FORBIDDEN, why);
        }
        if port == 0 || host.is_empty() || host.len() > MAX_HOST || host.contains('\0') {
            return fail(FailCode::BAD_REQUEST, "not a place to connect to");
        }
        if window > MAX_WINDOW {
            return fail(FailCode::LIMIT, "window too large");
        }
        if self.len().saturating_add(self.held) >= self.config.max_streams {
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

    /// Server end: an `open` of a command. It is not a connection, so the switch
    /// that turns those off is not asked, and nothing is connecting: the program
    /// is started and the answer is given.
    fn command_requested(&mut self, id: u32, window: u32, command: Command, out: &mut Vec<Packet>) {
        // As for any `open`: a number not above every one seen is not answered.
        if id <= self.highest {
            return;
        }
        self.highest = id;
        if command.check().is_err() {
            return refuse(out, id, FailCode::BAD_REQUEST, "not a command to run");
        }
        if window > MAX_WINDOW {
            return refuse(out, id, FailCode::LIMIT, "window too large");
        }
        if self.len().saturating_add(self.held) >= self.config.max_streams {
            return refuse(out, id, FailCode::LIMIT, "too many streams");
        }
        match process::start(&command) {
            Ok((
                program,
                Pipes {
                    stdin,
                    stdout,
                    stderr,
                },
            )) => {
                let run = Run {
                    program,
                    stdin: Some(stdin),
                    stdout: Some(stdout),
                    stderr: Some(stderr),
                };
                let given = self.first_window();
                self.streams
                    .insert(id, Stream::new(End::Program(run), window, given, false));
                out.push(Packet::new(id, Message::Opened { window: given }));
            }
            Err(e) => refuse(out, id, FailCode::START_FAILED, &process::system_words(&e)),
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
                    let given = self.first_window();
                    self.streams.insert(
                        id,
                        Stream::new(End::Socket(socket), entry.window, given, false),
                    );
                    out.push(Packet::new(id, Message::Opened { window: given }));
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
    /// time, or a program that was told to end is killed.
    pub fn deadline(&self) -> Option<Instant> {
        self.connecting
            .values()
            .filter(|c| c.phase == Phase::Pending)
            .map(|c| c.deadline)
            .chain(self.dying.values().filter_map(|d| d.kill_at))
            .min()
    }

    /// Server end: answers `failed` (timed out) for every stream that has
    /// been connecting past its deadline, and kills the programs that were told
    /// to end and have not.
    pub fn expire(&mut self, now: Instant, out: &mut Vec<Packet>) {
        self.expire_programs(now);
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
            (Side::Server, Message::OpenCommand { window, command }) => {
                self.command_requested(id, window, command, out);
            }
            // A ticket answers mail, which this end does not take: an `open`
            // with no target to connect to, as it was read before tickets.
            (Side::Server, Message::OpenTicket { window, .. }) => {
                self.open_requested(id, window, 0, String::new(), out);
            }
            (Side::Client, Message::Opened { window }) => self.opened(id, window, out),
            (Side::Client, Message::Failed { code, text }) => self.failed(id, code, text),
            (Side::Client, Message::Stderr(bytes)) => {
                self.data_in(id, bytes, Channel::Err, out);
            }
            (Side::Client, Message::Exited(exit)) => self.exited_in(id, exit, out),
            (_, Message::Data(bytes)) => self.data_in(id, bytes, Channel::Out, out),
            (_, Message::Credit(bytes)) => self.credit_in(id, bytes, out),
            (_, Message::Eof) => self.eof_in(id, out),
            (_, Message::Close) => self.close_in(id),
            // `open` going to a client, `opened` or `failed` to a server. (`stderr`
            // and `exited` go to a client only.)
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

    /// `data` or (`Channel::Err`) `stderr` arrived.
    fn data_in(&mut self, id: u32, bytes: Vec<u8>, channel: Channel, out: &mut Vec<Packet>) {
        let part = self.part(self.len());
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        let size = bytes.len();
        // A connection has no errors of its own to speak of, and nor has a
        // conversation of mail.
        let errors_without_a_program = channel == Channel::Err && !stream.end.is_command();
        if stream.opening
            || errors_without_a_program
            || stream.got_eof
            || size > MAX_DATA
            || u64::try_from(size).unwrap_or(u64::MAX) > u64::from(stream.recv_window)
        {
            self.reset(id, out);
            return;
        }
        let size = u32::try_from(size).unwrap_or(0);
        stream.recv_window -= size;
        stream.received = stream.received.saturating_add(u64::from(size));
        stream
            .tuner
            .received(stream.received, Instant::now(), &mut self.rtt, part);
        self.learned = Some(stream.tuner.measured());
        if stream.write_dead || (channel == Channel::Err && stream.errors_dead) {
            // Nowhere to write it: it counts as taken, and since it is not
            // held, credit for it is due like for what was written.
        } else if channel == Channel::Err {
            stream.errors.extend(bytes);
        } else {
            stream.inbox.extend(bytes);
        }
        self.dirty.insert(id);
    }

    /// `exited` arrived: how a command's program ended. It comes once, after
    /// the server's `eof`, and only on a command's stream.
    fn exited_in(&mut self, id: u32, exit: Exit, out: &mut Vec<Packet>) {
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        if !stream.end.is_command() || stream.opening || !stream.got_eof || stream.exited.is_some()
        {
            self.reset(id, out);
            return;
        }
        stream.exited = Some(exit);
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
        // The `close` that follows `exited` ends nothing that is not over: what
        // the program wrote is still being written here.
        if self.streams.get(&id).is_some_and(|s| s.exited.is_some()) {
            return;
        }
        // The app closing a conversation ends it for both, but what it said
        // before is still being written here: the conversation is over once it
        // has been ([`Event::Over`]), and nothing more is sent to the app.
        if let Some(stream) = self.streams.get_mut(&id)
            && stream.end.is_mail()
            && !stream.opening
        {
            stream.peer_closed = true;
            stream.got_eof = true;
            self.dirty.insert(id);
            return;
        }
        if self.forget(id) {
            self.events.push(Event::Closed {
                stream: id,
                cause: Cause::Server,
            });
        }
    }

    // --- sockets ------------------------------------------------------------

    /// The descriptors to wait for, and what each stands for.
    pub fn interest(&self) -> Vec<(libc::pollfd, Token)> {
        let mut found = Vec::new();
        for (&id, stream) in &self.streams {
            let reading = self.may_read(stream);
            let writing = !stream.inbox.is_empty();
            match &stream.end {
                End::Socket(socket) => {
                    let mut events = 0;
                    if writing {
                        events |= libc::POLLOUT;
                    }
                    if reading {
                        events |= libc::POLLIN;
                    }
                    if events != 0 {
                        found.push((sys::want(socket.as_raw_fd(), events), Token::Stream(id)));
                    }
                }
                End::Program(run) => {
                    if let Some(pipe) = run.stdin.as_ref().filter(|_| writing) {
                        found.push((sys::want(pipe.as_raw_fd(), libc::POLLOUT), Token::Stdin(id)));
                    }
                    if let Some(pipe) = run.stdout.as_ref().filter(|_| reading) {
                        found.push((sys::want(pipe.as_raw_fd(), libc::POLLIN), Token::Stdout(id)));
                    }
                    if let Some(pipe) = run.stderr.as_ref().filter(|_| reading) {
                        found.push((sys::want(pipe.as_raw_fd(), libc::POLLIN), Token::Stderr(id)));
                    }
                    if let Some(watch) = run.program.watch_fd() {
                        found.push((sys::want(watch, libc::POLLIN), Token::Exit(id)));
                    }
                }
                End::Local(local) => {
                    if let Some(pipe) = local.input.as_ref().filter(|_| reading) {
                        found.push((
                            sys::want(pipe.as_raw_fd(), libc::POLLIN),
                            Token::LocalInput(id),
                        ));
                    }
                    if writing {
                        found.push((
                            sys::want(local.output.as_raw_fd(), libc::POLLOUT),
                            Token::LocalOutput(id),
                        ));
                    }
                    if !stream.errors.is_empty() {
                        found.push((
                            sys::want(local.errors.as_raw_fd(), libc::POLLOUT),
                            Token::LocalErrors(id),
                        ));
                    }
                }
            }
        }
        found.extend(self.dying_interest());
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

    /// The ends of the programs that were told to end, while they have not
    /// been collected.
    fn dying_interest(&self) -> Vec<(libc::pollfd, Token)> {
        self.dying
            .iter()
            .filter_map(|(&pid, dying)| {
                let watch = dying.program.watch_fd()?;
                Some((sys::want(watch, libc::POLLIN), Token::Dying(pid)))
            })
            .collect()
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
                    self.fill(id, Channel::Out, out);
                }
            }
            // Writing is found out by trying: a closed input is an error on the
            // next write, which `drain` knows what to do with.
            Token::Stdin(id) => self.drain(id, out),
            Token::Stdout(id) => self.fill(id, Channel::Out, out),
            Token::Stderr(id) => self.fill(id, Channel::Err, out),
            Token::LocalInput(id) => self.fill(id, Channel::Out, out),
            Token::LocalOutput(id) | Token::LocalErrors(id) => self.drain(id, out),
            Token::Exit(id) => self.program_ended(id),
            Token::Dying(pid) => self.collect(pid),
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
    /// has said it is done and everything is written. (A command's output and
    /// errors are two inboxes, written to two descriptors.)
    fn drain(&mut self, id: u32, out: &mut Vec<Packet>) {
        let part = self.part(self.len());
        let now = Instant::now();
        self.rtt.look(now);
        let Some(stream) = self.streams.get_mut(&id) else {
            return;
        };
        let command = matches!(stream.end, End::Local(_));
        let mut gave_up = None;
        for channel in [Channel::Out, Channel::Err] {
            loop {
                let inbox = match channel {
                    Channel::Out => &mut stream.inbox,
                    Channel::Err => &mut stream.errors,
                };
                let (front, _) = inbox.as_slices();
                if front.is_empty() {
                    break;
                }
                match stream.end.write(channel, front) {
                    Ok(0) => break,
                    Ok(n) => {
                        inbox.drain(..n);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    // What a command wrote has nowhere to go. Its errors are
                    // dropped and it goes on; if it is the output the reader of
                    // went away, it is over, since a program that cannot be
                    // heard is not worth running.
                    Err(e) if command => {
                        if channel == Channel::Err {
                            inbox.clear();
                            stream.errors_dead = true;
                        } else {
                            gave_up = Some(e.kind());
                        }
                        break;
                    }
                    // The socket will take no more. That is the end of what can
                    // be written to it, not of what it says: the stream goes on
                    // as far as its reading half does (a target that answered and
                    // hung up has its answer carried; a reset socket fails the
                    // read too, and that ends the stream).
                    Err(_) => {
                        inbox.clear();
                        stream.write_dead = true;
                        break;
                    }
                }
            }
        }
        if let Some(kind) = gave_up {
            if stream.exited.is_some() {
                // The program is gone and has been heard of: what it wrote has
                // nowhere to go, and how it ended is the result all the same.
                stream.inbox.clear();
                stream.errors.clear();
                self.finish(id);
            } else {
                self.reset_by(id, Cause::Local(kind), out);
            }
            return;
        }
        // What the other end may still send and what is held unwritten come
        // to the window the stream should have: credit for what was taken
        // (written or dropped) makes up the difference, more of it when the
        // window grew and less when it shrank, and none until a quarter of the
        // window is due. The credit is what times the link.
        let window = stream.tuner.window(&self.rtt, part);
        let due = window
            .saturating_sub(stream.held())
            .saturating_sub(stream.recv_window);
        // (An app that has closed its conversation has nothing to send.)
        if due >= credit_step(window) && !stream.peer_closed {
            let limit = stream
                .received
                .saturating_add(u64::from(stream.recv_window));
            stream.tuner.crediting(limit, now, &self.rtt);
            stream.recv_window = stream.recv_window.saturating_add(due);
            out.push(Packet::new(id, Message::Credit(due)));
        }
        let written = stream.inbox.is_empty() && stream.errors.is_empty();
        if ((stream.got_eof && written) || stream.write_dead) && !stream.shut {
            stream.end.shut();
            stream.shut = true;
        }
        self.finish(id);
    }

    /// Reads a stream's socket (or one of a program's outputs) once, if the
    /// stream has window to send into and the wire is not far behind, and
    /// queues what it said. The window is the stream's, whichever output the
    /// bytes were on.
    fn fill(&mut self, id: u32, channel: Channel, out: &mut Vec<Packet>) {
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
            match stream.end.read(channel, buf) {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                other => break other,
            }
        };
        let over = match read {
            Ok(0) => true,
            Ok(n) => {
                stream.send_window -= u32::try_from(n).unwrap_or(0);
                let bytes = buf.get(..n).unwrap_or_default().to_vec();
                let message = match channel {
                    Channel::Out => Message::Data(bytes),
                    Channel::Err => Message::Stderr(bytes),
                };
                if !self.queue(id, message) {
                    self.reset(id, out);
                }
                return;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
            // A socket that fails to be read is over, and so is the stream. A
            // pipe that does is a program that has nothing more to say: what
            // it did say is carried and the end is told as any end.
            Err(_) if matches!(stream.end, End::Socket(_)) => {
                self.reset(id, out);
                return;
            }
            Err(_) => true,
        };
        if over && stream.end.end_reading(channel) {
            stream.sent_eof = true;
            if !self.queue(id, Message::Eof) {
                self.reset(id, out);
                return;
            }
            self.finish(id);
        }
    }

    /// The program of a stream has ended: it is collected, and the stream is
    /// over if everything it said has been carried.
    fn program_ended(&mut self, id: u32) {
        if let Some(Stream {
            end: End::Program(run),
            ..
        }) = self.streams.get_mut(&id)
        {
            run.program.reap();
        }
        self.finish(id);
    }

    /// A stream both ends have said all they have to say on, and whose socket
    /// has been told so, is over; so is a program's once it has said all it had
    /// (the client's `eof` does not matter: the program is gone), and so is a
    /// conversation of mail once both ends have, or the app has closed it.
    fn finish(&mut self, id: u32) {
        let over = self.streams.get(&id).is_some_and(|s| match &s.end {
            End::Socket(_) => s.sent_eof && s.got_eof && s.shut,
            End::Program(run) => s.sent_eof && run.program.exit().is_some(),
            // Over once the server has said how the program ended and all it
            // wrote has been written; this end's own `eof` does not matter,
            // the program is gone.
            End::Local(held) if held.mail => {
                (s.peer_closed || (s.sent_eof && s.got_eof)) && s.inbox.is_empty()
            }
            End::Local(_) => s.exited.is_some() && s.inbox.is_empty() && s.errors.is_empty(),
        });
        if !over {
            return;
        }
        // Its `eof` may still be queued; that is the point of keeping the
        // frames.
        let Some(stream) = self.streams.remove(&id) else {
            return;
        };
        self.dirty.remove(&id);
        match stream.end {
            End::Program(run) => {
                // How it ended, and the end of the stream, go behind the `eof`.
                if let Some(exit) = run.program.exit() {
                    self.queue(id, Message::Exited(exit));
                    self.queue(id, Message::Close);
                }
            }
            End::Local(held) if held.mail => self.events.push(Event::Over { stream: id }),
            End::Local(_) => {
                if let Some(exit) = stream.exited {
                    self.events.push(Event::Exited { stream: id, exit });
                }
            }
            End::Socket(_) => {}
        }
    }

    /// Ends a stream that cannot go on, and says so.
    fn reset(&mut self, id: u32, out: &mut Vec<Packet>) {
        self.reset_by(id, Cause::Server, out);
    }

    /// [`Streams::reset`], for a stream that the other end broke or that this
    /// end gave up.
    fn reset_by(&mut self, id: u32, cause: Cause, out: &mut Vec<Packet>) {
        if self.forget(id) {
            self.events.push(Event::Closed { stream: id, cause });
        }
        out.push(Packet::new(id, Message::Close));
    }

    /// Drops a stream and everything queued for it. The program of a stream, if
    /// it has one, is told to end. Whether it was a command run for this end.
    fn forget(&mut self, id: u32) -> bool {
        let mut command = false;
        if let Some(stream) = self.streams.remove(&id) {
            match stream.end {
                End::Program(Run { program, .. }) => self.dismiss(program),
                End::Local(_) => command = true,
                End::Socket(_) => {}
            }
        }
        self.dirty.remove(&id);
        self.data.retain(|queued| queued.stream != id);
        self.data_bytes = self.data.iter().map(|queued| queued.frame.len()).sum();
        command
    }

    /// Tells a program whose stream is over to end: its group is signalled and
    /// it is kept until it has been collected and the group is gone, or the
    /// time is up and the group is killed.
    fn dismiss(&mut self, mut program: Program) {
        program.terminate();
        // (A program that has been collected and left nothing is already done.)
        if program.reap().is_some() && !program.group_exists() {
            return;
        }
        let kill_at = Some(Instant::now() + self.config.kill_wait);
        self.dying.insert(program.id(), Dying { program, kill_at });
    }

    /// A program that was told to end has ended: it is collected, and let go if
    /// nothing of its group is left or its group has been killed already.
    fn collect(&mut self, pid: u32) {
        let Some(dying) = self.dying.get_mut(&pid) else {
            return;
        };
        // (A wake-up before the system will say so is looked at again.)
        if dying.program.reap().is_none() {
            return;
        }
        if dying.kill_at.is_none() || !dying.program.group_exists() {
            self.dying.remove(&pid);
        }
    }

    /// Kills the groups of the programs whose time is up.
    fn expire_programs(&mut self, now: Instant) {
        let due: Vec<u32> = self
            .dying
            .iter()
            .filter(|(_, d)| d.kill_at.is_some_and(|at| at <= now))
            .map(|(&pid, _)| pid)
            .collect();
        for pid in due {
            if let Some(dying) = self.dying.get_mut(&pid) {
                dying.program.kill();
                dying.kill_at = None;
            }
            // Collected at once if it has gone; otherwise its end is waited
            // for with the descriptor that says so.
            if self
                .dying
                .get_mut(&pid)
                .is_some_and(|d| d.program.reap().is_some())
            {
                self.dying.remove(&pid);
            }
        }
    }

    /// The connection is ending: every program that is running is told to end,
    /// and this waits until they have, killing what has not when its time is up
    /// ([`Config::kill_wait`]) and giving up on what cannot be collected a
    /// moment after that ([`SHUTDOWN_REAP_WAIT`]). The one wait in the engine;
    /// the owner is on its way out.
    pub fn shutdown(&mut self) {
        let programs: Vec<u32> = self
            .streams
            .iter()
            .filter(|(_, stream)| matches!(stream.end, End::Program(_)))
            .map(|(&id, _)| id)
            .collect();
        for id in programs {
            self.forget(id);
        }
        let limit = Instant::now() + self.config.kill_wait + SHUTDOWN_REAP_WAIT;
        let mut unheard = Vec::new();
        while !self.dying.is_empty() {
            let now = Instant::now();
            if now >= limit {
                break;
            }
            self.expire_programs(now);
            let found = self.dying_interest();
            let mut set: Vec<libc::pollfd> = found.iter().map(|(fd, _)| *fd).collect();
            let until = self
                .dying
                .values()
                .filter_map(|d| d.kill_at)
                .min()
                .map_or(limit, |at| at.min(limit));
            if sys::poll(&mut set, Some(until.saturating_duration_since(now))).is_err() {
                break;
            }
            for ((_, token), fd) in found.into_iter().zip(&set) {
                if fd.revents != 0 {
                    self.ready(token, fd.revents, &mut unheard);
                }
            }
        }
        // What is left cannot be collected: the system will, when this exits.
        // (Dropping a program waits for it.)
        for dying in std::mem::take(&mut self.dying).into_values() {
            std::mem::forget(dying.program);
        }
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
