//! Support for the tests that run real sessions.
//!
//! Every test gets a socket directory of its own under `/tmp` (short, because
//! a Unix socket path holds about a hundred bytes) and every `kablo` it starts
//! is told about it through `KABLO_DIR`: there is no way to build a [`Env`]
//! without one, so no test can reach the real `/tmp/kablo-<uid>`. When the
//! test ends, whatever it started is ended: first politely over the socket,
//! then by pid, and only pids that belong to its own sessions (found by the
//! unique session ids and directory on their command lines).

// Test support: a failure here is a failed test, so the panicking shortcuts
// are what it should use.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use kablo_proto::v1::stream::{self, FailCode, Incoming, IncomingDecoder};
use kablo_proto::v1::{
    ErrorCode, Exit, Features, Message, NewSession, Packet, PacketDecoder, Size, Status,
    TerminalState, View,
};
use kablo_proto::{SessionId, WIRE};
use kablo_screen::Screen;

/// How long a test waits for anything before it calls it failed.
pub const PATIENCE: Duration = Duration::from_secs(30);

/// Whether a read of a socket with a read timeout that failed with `error`
/// only means nothing has come yet, as opposed to the connection being over.
///
/// A signal is one of those: a read with a timeout is not restarted after one.
/// Linux ends it with `EINTR` when a child of this process ends, even though
/// nobody handles `SIGCHLD`, and the tests start and collect children all the
/// time from threads running side by side.
pub fn nothing_yet(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::Interrupted
    )
}

/// The file the process `pid` is running, as the system says it (a command
/// line says what the process was told, not what it is).
pub fn binary_of(pid: u32) -> Option<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStringExt;
        let mut buf = vec![0u8; 4096];
        // SAFETY: `buf` is a writable buffer of the length passed.
        let len = unsafe {
            libc::proc_pidpath(
                libc::c_int::try_from(pid).ok()?,
                buf.as_mut_ptr().cast(),
                buf.len() as u32,
            )
        };
        let len = usize::try_from(len).ok().filter(|&len| len > 0)?;
        buf.truncate(len);
        Some(PathBuf::from(std::ffi::OsString::from_vec(buf)))
    }
}

static SERIAL: AtomicUsize = AtomicUsize::new(0);

pub struct Env {
    pub dir: PathBuf,
    tag: String,
    sessions: Vec<String>,
}

impl Env {
    pub fn new() -> Env {
        let n = SERIAL.fetch_add(1, Ordering::Relaxed);
        let tag = format!("t{}n{n}", std::process::id());
        let dir = PathBuf::from(format!("/tmp/kt{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            !dir.to_string_lossy().starts_with("/tmp/kablo-"),
            "a test must never use the real socket directory"
        );
        Env {
            dir,
            tag,
            sessions: Vec::new(),
        }
    }

    /// A `kablo` command in this test's directory.
    pub fn kablo(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kablo"));
        command.env("KABLO_DIR", &self.dir);
        command
    }

    /// A session name unique to this test.
    pub fn name(&self, label: &str) -> String {
        format!("{}-{label}", self.tag)
    }

    /// Runs `kablo new --id <name> <args>` and returns what it printed or
    /// said.
    pub fn try_start(&mut self, label: &str, args: &[&str]) -> Result<String, String> {
        let id = self.name(label);
        self.sessions.push(id.clone());
        let output = self
            .kablo()
            .args(["new", "--id", &id])
            .args(args)
            .output()
            .unwrap();
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
        }
    }

    pub fn start(&mut self, label: &str, args: &[&str]) -> String {
        let id = self.name(label);
        let said = self
            .try_start(label, args)
            .unwrap_or_else(|why| panic!("kablo new failed: {why}"));
        assert_eq!(said, id);
        id
    }

    /// Has the end of this test clean up session `id`, which it did not start
    /// with [`Env::start`] (a bridge made it, say).
    pub fn track(&mut self, id: &str) {
        self.sessions.push(id.to_owned());
    }

    pub fn socket(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.sock"))
    }

    pub fn connect(&self, id: &str) -> Conn {
        Conn::open(self.socket(id), id)
    }

    /// The processes of this test's sessions: (pid, parent, command line).
    pub fn processes(&self) -> Vec<(u32, u32, String)> {
        let output = Command::new("ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut it = line.split_whitespace();
                let pid = it.next()?.parse().ok()?;
                let ppid = it.next()?.parse().ok()?;
                Some((pid, ppid, it.collect::<Vec<_>>().join(" ")))
            })
            .collect()
    }

    /// The children of `parent` that have ended and not been waited for.
    pub fn zombies_of(&self, parent: u32) -> Vec<u32> {
        let output = Command::new("ps")
            .args(["-axo", "pid=,ppid=,stat="])
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let mut it = line.split_whitespace();
                let pid: u32 = it.next()?.parse().ok()?;
                let ppid: u32 = it.next()?.parse().ok()?;
                (ppid == parent && it.next()?.starts_with('Z')).then_some(pid)
            })
            .collect()
    }

    /// The pid of the `role` (`warden` or `relay`) of session `id`.
    pub fn find(&self, role: &str, id: &str) -> Option<u32> {
        let wanted = format!("kablo {role} --id {id}");
        self.processes()
            .into_iter()
            .find(|(_, _, command)| command.contains(&wanted))
            .map(|(pid, _, _)| pid)
    }

    /// The command line of the `role` (`warden` or `relay`) of session `id`.
    pub fn command_of(&self, role: &str, id: &str) -> Option<String> {
        let wanted = format!("kablo {role} --id {id}");
        self.processes()
            .into_iter()
            .map(|(_, _, command)| command)
            .find(|command| command.contains(&wanted))
    }

    /// Waits until the warden of `id` runs `binary` and returns its pid: a
    /// warden that renews itself keeps its pid and changes the program it is.
    pub fn warden_running(&self, id: &str, binary: &Path) -> u32 {
        let wanted = std::fs::canonicalize(binary).unwrap();
        let start = Instant::now();
        loop {
            if let Some(pid) = self.find("warden", id) {
                let runs = binary_of(pid).and_then(|path| std::fs::canonicalize(path).ok());
                if runs.as_ref() == Some(&wanted) {
                    return pid;
                }
            }
            assert!(
                start.elapsed() < PATIENCE,
                "the warden of {id} never ran {}: {:?}",
                binary.display(),
                self.find("warden", id).and_then(binary_of)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The parent of process `pid`.
    pub fn parent_of(&self, pid: u32) -> Option<u32> {
        self.processes()
            .into_iter()
            .find(|(this, _, _)| *this == pid)
            .map(|(_, parent, _)| parent)
    }

    /// A copy of the `kablo` under test in a directory of its own, to upgrade
    /// to: a file somebody can run, named `kablo` so that the processes
    /// started from it are found by their command lines like any other.
    pub fn newer_kablo(&self, label: &str) -> PathBuf {
        let dir = self.dir.join(format!("newer-{label}"));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("kablo");
        std::fs::copy(env!("CARGO_BIN_EXE_kablo"), &path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// An executable file with this text in it, in this test's directory.
    pub fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.dir.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// Waits until session `id` has no process and no socket left.
    pub fn wait_gone(&self, id: &str) {
        let start = Instant::now();
        loop {
            let alive = self.find("warden", id).is_some() || self.find("relay", id).is_some();
            if !alive && !self.socket(id).exists() {
                return;
            }
            assert!(
                start.elapsed() < PATIENCE,
                "session {id} is still there: warden {:?} relay {:?} socket {}",
                self.find("warden", id),
                self.find("relay", id),
                self.socket(id).exists()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Politely first: a forced terminate over the socket ends the shell
        // and lets the warden clean up. A session that cannot be reached is
        // ended by pid below.
        for id in &self.sessions {
            if self.socket(id).exists() {
                if let Ok(mut conn) = Conn::try_open(self.socket(id), id, Duration::from_secs(2)) {
                    conn.send(Message::Terminate { force: true });
                    conn.pump(Duration::from_millis(300));
                }
            }
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && self
                .sessions
                .iter()
                .any(|id| self.find("warden", id).is_some())
        {
            std::thread::sleep(Duration::from_millis(50));
        }
        let processes = self.processes();
        for id in &self.sessions {
            let ours = [
                format!("kablo warden --id {id} "),
                format!("kablo relay --id {id}"),
            ];
            let wardens: Vec<u32> = processes
                .iter()
                .filter(|(_, _, c)| c.contains(&ours[0]))
                .map(|(pid, _, _)| *pid)
                .collect();
            let doomed = processes.iter().filter(|(pid, ppid, c)| {
                c.contains(&ours[0]) || c.contains(&ours[1]) || wardens.contains(ppid) && *pid > 1
            });
            for (pid, _, _) in doomed {
                // SAFETY: a signal to a pid found on one of this test's own
                // session command lines or a child of one.
                unsafe { libc::kill(*pid as libc::pid_t, libc::SIGKILL) };
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A client of one session.
/// A picture of a session's screen, as the pieces of it came.
#[derive(Clone, Debug, Default)]
pub struct Pic {
    /// The offset of the program's output the picture is of.
    pub offset: u64,
    pub cols: u16,
    pub rows: u16,
    pub bytes: Vec<u8>,
    pub pieces: usize,
    /// The last piece came.
    pub complete: bool,
}

impl Pic {
    /// Files a `picture` message among the pictures seen: a piece goes to the
    /// picture that is not yet complete, or begins another.
    fn file(pictures: &mut Vec<Pic>, offset: u64, cols: u16, rows: u16, more: bool, data: &[u8]) {
        if pictures.last().is_none_or(|last| last.complete) {
            pictures.push(Pic {
                offset,
                cols,
                rows,
                ..Pic::default()
            });
        }
        let last = pictures.last_mut().unwrap();
        assert_eq!(
            (last.offset, last.cols, last.rows),
            (offset, cols, rows),
            "the pieces of one picture agree"
        );
        last.bytes.extend_from_slice(data);
        last.pieces += 1;
        last.complete = !more;
    }
}

/// What a terminal shows once it has the picture and the output after it. It
/// keeps no scrollback, so what is compared is the screen and the state around
/// it (modes, cursor, scrolling region, the other screen).
pub fn rebuilt(picture: &Pic, after: &[u8]) -> Screen {
    let mut screen = Screen::new(picture.cols, picture.rows, 0);
    screen.feed(&picture.bytes);
    screen.feed(after);
    screen
}

/// What a terminal of the same kind shows that was given the whole stream.
pub fn whole(stream: &[u8], cols: u16, rows: u16) -> Screen {
    let mut screen = Screen::new(cols, rows, 0);
    screen.feed(stream);
    screen
}

/// Asserts that two screens are the same.
pub fn same(mut got: Screen, mut wanted: Screen) {
    let (got, wanted) = (got.image(), wanted.image());
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&wanted),
        "the screens differ"
    );
}

pub struct Conn {
    sock: UnixStream,
    decoder: PacketDecoder,
    pub id: SessionId,
    /// Every program byte received, from the offset the stream started at.
    pub data: Vec<u8>,
    /// The offset after the last byte received.
    pub next: u64,
    /// What `attached` said: where the stream starts, and whether it was cut.
    pub attached: Option<(u64, bool)>,
    /// How far `attached` said the questions were answered (0 when it does not
    /// say), from the last one.
    pub answered: u64,
    /// Every picture received, across attaches.
    pub pictures: Vec<Pic>,
    pub ended: Option<Exit>,
    pub errors: Vec<(ErrorCode, String)>,
    pub replies: Vec<Packet>,
    pub closed: bool,
    /// Every `state` received, across attaches, the last one last.
    pub states: Vec<TerminalState>,
    /// The kinds of message received in the order they came (`attached`,
    /// `state`, `picture`, `output`), a run of one kind written once.
    pub trace: Vec<&'static str>,
}

impl Conn {
    pub fn open(path: PathBuf, id: &str) -> Conn {
        Conn::try_open(path, id, PATIENCE).unwrap_or_else(|e| panic!("cannot connect to {id}: {e}"))
    }

    pub fn try_open(path: PathBuf, id: &str, patience: Duration) -> Result<Conn, String> {
        let mut sock = UnixStream::connect(path).map_err(|e| e.to_string())?;
        // A session that is going away (its end was heard, its keeper is
        // finishing) can take a connection and close it at once, and macOS
        // then refuses to set an option on the socket (`EINVAL`). That is a
        // connection that failed, for the caller to judge: the end of a test
        // asks every session to go this way and passes over one that is gone.
        sock.set_read_timeout(Some(patience))
            .map_err(|e| e.to_string())?;
        WIRE.client(&mut sock.try_clone().unwrap(), &mut sock)
            .map_err(|e| e.to_string())?;
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .map_err(|e| e.to_string())?;
        Ok(Conn {
            sock,
            decoder: PacketDecoder::new(),
            id: SessionId::new(id).unwrap(),
            data: Vec::new(),
            next: 0,
            attached: None,
            answered: 0,
            pictures: Vec::new(),
            ended: None,
            errors: Vec::new(),
            replies: Vec::new(),
            closed: false,
            states: Vec::new(),
            trace: Vec::new(),
        })
    }

    fn note(&mut self, kind: &'static str) {
        if self.trace.last() != Some(&kind) {
            self.trace.push(kind);
        }
    }

    pub fn send(&mut self, message: Message) {
        let packet = Packet::about(self.id.clone(), message);
        let mut bytes = Vec::new();
        packet.encode(&mut bytes).unwrap();
        // A write to a connection the server has dropped fails; the test
        // finds out by reading.
        let _ = self.sock.write_all(&bytes);
    }

    pub fn type_in(&mut self, text: &str) {
        self.send(Message::Input(text.as_bytes().to_vec()));
    }

    /// Reads for up to `wait` and files what arrived.
    pub fn pump(&mut self, wait: Duration) {
        let deadline = Instant::now() + wait;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match self.sock.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    break;
                }
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if nothing_yet(&e) => {}
                Err(_) => {
                    self.closed = true;
                    break;
                }
            }
            self.file();
            if Instant::now() >= deadline {
                break;
            }
        }
        self.file();
    }

    fn file(&mut self) {
        while let Some(packet) = self.decoder.next_packet().unwrap() {
            match packet.message {
                Message::Attached {
                    offset,
                    truncated,
                    answered,
                } => {
                    self.note("attached");
                    self.attached = Some((offset, truncated));
                    self.answered = answered;
                    self.next = offset;
                    self.data.clear();
                }
                Message::TerminalState(state) => {
                    self.note("state");
                    self.states.push(state);
                }
                Message::Picture {
                    offset,
                    cols,
                    rows,
                    more,
                    data,
                } => {
                    self.note("picture");
                    Pic::file(&mut self.pictures, offset, cols, rows, more, &data);
                }
                Message::Output { offset, data } => {
                    self.note("output");
                    assert_eq!(offset, self.next, "output is not contiguous");
                    self.next += data.len() as u64;
                    self.data.extend_from_slice(&data);
                }
                Message::Ended(exit) => self.ended = Some(exit),
                Message::Error { code, text } => self.errors.push((code, text)),
                _ => self.replies.push(packet),
            }
        }
    }

    /// Pumps until `done` holds of this connection or the patience is out.
    pub fn until(&mut self, what: &str, done: impl Fn(&Conn) -> bool) {
        let start = Instant::now();
        loop {
            self.pump(Duration::from_millis(20));
            if done(self) {
                return;
            }
            assert!(
                !self.closed,
                "the connection closed while waiting for {what}; got {:?}",
                String::from_utf8_lossy(&self.data)
            );
            assert!(
                start.elapsed() < PATIENCE,
                "timed out waiting for {what}; got {:?}",
                String::from_utf8_lossy(&self.data)
            );
        }
    }

    /// Pumps until the stream holds `needle` after the first `from` bytes.
    pub fn wait_for(&mut self, needle: &str, from: usize) {
        let needle = needle.as_bytes().to_vec();
        self.until(
            &format!("{:?}", String::from_utf8_lossy(&needle)),
            move |c| {
                c.data
                    .get(from..)
                    .is_some_and(|rest| rest.windows(needle.len()).any(|w| w == needle.as_slice()))
            },
        );
    }

    /// Attaches at `offset`; returns where the stream starts and whether it
    /// was cut.
    pub fn attach(&mut self, offset: u64) -> (u64, bool) {
        self.attached = None;
        self.send(Message::attach(offset));
        self.until("attached", |c| c.attached.is_some());
        self.attached.unwrap()
    }

    /// Attaches as a terminal of `cols` by `rows` that wants a picture of the
    /// screen; returns where the stream starts and whether it was cut, once the
    /// picture is whole.
    pub fn attach_with_picture(&mut self, offset: u64, cols: u16, rows: u16) -> (u64, bool) {
        self.attached = None;
        self.pictures.clear();
        self.send(Message::Attach {
            offset,
            view: Some(View {
                size: Size {
                    cols,
                    rows,
                    xpixel: 0,
                    ypixel: 0,
                },
                picture: true,
                picture_if_answered: false,
            }),
        });
        self.until("attached and its picture", |c| {
            c.attached.is_some() && c.pictures.last().is_some_and(|p| p.complete)
        });
        self.attached.unwrap()
    }

    /// Attaches as a terminal of `cols` by `rows` that answers what it is shown
    /// and cannot hold its answers back (as `kablo connect` is); returns where
    /// the stream starts and whether the session cut it, once the picture that
    /// takes the place of the bytes is whole.
    pub fn attach_if_answered(&mut self, offset: u64, cols: u16, rows: u16) -> (u64, bool) {
        self.attached = None;
        self.pictures.clear();
        self.send(Message::Attach {
            offset,
            view: Some(View {
                size: Size {
                    cols,
                    rows,
                    xpixel: 0,
                    ypixel: 0,
                },
                picture: false,
                picture_if_answered: true,
            }),
        });
        self.until("attached", |c| c.attached.is_some());
        if self.attached.is_some_and(|(_, truncated)| truncated) {
            self.until("its picture", |c| {
                c.pictures.last().is_some_and(|p| p.complete)
            });
        }
        self.attached.unwrap()
    }

    /// The picture last received.
    pub fn picture(&self) -> &Pic {
        self.pictures.last().expect("a picture")
    }

    pub fn status(&mut self) -> Status {
        self.replies.clear();
        self.send(Message::Status);
        self.until("a status", |c| {
            c.replies
                .iter()
                .any(|p| matches!(p.message, Message::StatusReply(_)))
        });
        for packet in self.replies.drain(..) {
            if let Message::StatusReply(status) = packet.message {
                return status;
            }
        }
        unreachable!()
    }

    /// Reads until the server closes the connection.
    pub fn until_closed(&mut self) {
        let start = Instant::now();
        while !self.closed {
            self.pump(Duration::from_millis(20));
            assert!(start.elapsed() < PATIENCE, "the server never closed it");
        }
    }
}

/// What a client of a bridge has seen of one session.
#[derive(Default)]
pub struct Seen {
    /// Every program byte received, across attaches.
    pub data: Vec<u8>,
    /// The offset after the last byte received.
    pub next: u64,
    /// Every `attached` received: where the stream started, and whether it
    /// was cut.
    pub attached: Vec<(u64, bool)>,
    /// Every picture received, across attaches.
    pub pictures: Vec<Pic>,
    pub ended: Option<Exit>,
    /// Every `state` received, across attaches.
    pub states: Vec<TerminalState>,
    /// Whether the program's bytes are left out of `data` (a test that lets a
    /// session run flat out has no use for them).
    pub discard: bool,
}

/// What a client of a bridge has seen of one stream.
#[derive(Default)]
pub struct Flow {
    /// The window the bridge offered in `opened`: what this end may send.
    pub opened: Option<u32>,
    /// Why the bridge could not open it.
    pub failed: Option<(FailCode, String)>,
    /// The bytes the target sent.
    pub data: Vec<u8>,
    /// How many bytes the target sent in all (kept when `data` is not).
    pub total: u64,
    /// The bridge said the target has no more to send.
    pub eof: bool,
    /// The bridge closed the stream.
    pub closed: bool,
    /// Bytes of credit received.
    pub credit: u64,
    /// Bytes sent to the target so far.
    pub sent: u64,
    /// Whether received bytes are held back from the target (no credit given
    /// until [`Bridge::stream_release`]); by default they are credited at once.
    pub hold: bool,
    /// Bytes received and held.
    pub owed: u64,
    /// Whether the bytes are left out of `data`.
    pub discard: bool,
    /// The bytes a command wrote as errors.
    pub stderr: Vec<u8>,
    /// How the command ended, as the bridge told it.
    pub exited: Option<Exit>,
    /// The `eof`, `exited` and `close` the bridge sent, in the order they came.
    pub ends: Vec<&'static str>,
}

/// A client of `kablo attach` (or of anything else that speaks the protocol
/// over a connected socket, such as `ssh host kablo attach`): many sessions
/// over one connection.
pub struct Bridge {
    sock: UnixStream,
    decoder: IncomingDecoder,
    child: Child,
    pub sessions: HashMap<String, Seen>,
    pub flows: HashMap<u32, Flow>,
    pub features: Vec<Features>,
    /// Every byte received after the handshake, once recording (see
    /// [`Bridge::record`]).
    pub raw: Option<Vec<u8>>,
    next_stream: u32,
    /// What the next stream opened is to do with the bytes it receives: held
    /// back from the target, and left out of its `data` (see [`Flow`]).
    pub next_flow: (bool, bool),
    pub listed: Vec<Vec<Status>>,
    pub created: Vec<Status>,
    pub statuses: Vec<Status>,
    pub pongs: Vec<Vec<u8>>,
    /// `(session, code, text)` of every `error`.
    pub errors: Vec<(Option<String>, ErrorCode, String)>,
    pub closed: bool,
    /// How the connection was found closed, for a failure message.
    lost: Option<String>,
}

impl Bridge {
    /// Starts `command` with a socket as its standard input and output and
    /// shakes hands over it. Returns what came before the signature.
    pub fn over(mut command: Command) -> (Bridge, Vec<u8>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let again = theirs.try_clone().unwrap();
        command
            .stdin(Stdio::from(std::os::fd::OwnedFd::from(again)))
            .stdout(Stdio::from(std::os::fd::OwnedFd::from(theirs)));
        let child = command.spawn().unwrap();
        let mut sock = ours;
        sock.set_read_timeout(Some(PATIENCE)).unwrap();
        // The handshake by hand, so that what precedes the reply is kept.
        let mut hello = Vec::new();
        WIRE.write_hello(&mut hello).unwrap();
        sock.write_all(&hello).unwrap();
        let mut seen = Vec::new();
        let mut byte = [0u8; 1];
        let signature = WIRE.signature;
        while !seen.ends_with(signature) {
            match sock.read(&mut byte) {
                Ok(0) => panic!("the bridge closed before it answered"),
                Ok(_) => seen.push(byte[0]),
                // Not an answer, and not the end of one (see `nothing_yet`).
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => panic!("the bridge answers the handshake: {e}"),
            }
        }
        let mut rest = [0u8; 3];
        sock.read_exact(&mut rest).unwrap();
        assert_eq!(rest[0], 1, "the bridge selects version 1");
        let noise = seen[..seen.len() - signature.len()].to_vec();
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        (
            Bridge {
                sock,
                decoder: IncomingDecoder::new(),
                child,
                sessions: HashMap::new(),
                flows: HashMap::new(),
                features: Vec::new(),
                raw: None,
                next_stream: 0,
                next_flow: (false, false),
                listed: Vec::new(),
                created: Vec::new(),
                statuses: Vec::new(),
                pongs: Vec::new(),
                errors: Vec::new(),
                closed: false,
                lost: None,
            },
            noise,
        )
    }

    /// `kablo attach` in this test's directory.
    pub fn start(env: &Env) -> Bridge {
        let mut command = env.kablo();
        command.arg("attach");
        Bridge::over(command).0
    }

    /// The pid of the `kablo attach` this is a client of.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    pub fn send(&mut self, session: Option<&str>, message: Message) {
        let session = session.map(|s| SessionId::new(s).unwrap());
        let mut bytes = Vec::new();
        Packet { session, message }.encode(&mut bytes).unwrap();
        let _ = self.sock.write_all(&bytes);
    }

    /// Reads for up to `wait` and files what arrived.
    pub fn pump(&mut self, wait: Duration) {
        let deadline = Instant::now() + wait;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match self.sock.read(&mut buf) {
                Ok(0) => {
                    self.closed = true;
                    self.lost = Some("the stream ended".to_owned());
                    break;
                }
                Ok(n) => {
                    if let Some(raw) = &mut self.raw {
                        raw.extend_from_slice(&buf[..n]);
                    }
                    self.decoder.push(&buf[..n]);
                }
                Err(e) if nothing_yet(&e) => {}
                Err(e) => {
                    self.closed = true;
                    self.lost = Some(format!("a read failed: {e}"));
                    break;
                }
            }
            self.file();
            if Instant::now() >= deadline {
                break;
            }
        }
        self.file();
    }

    fn file(&mut self) {
        let mut credits = Vec::new();
        while let Some(incoming) = self.decoder.next_incoming().unwrap() {
            let packet = match incoming {
                Incoming::Session(packet) => packet,
                Incoming::Stream { from, packet } => {
                    assert_eq!(
                        from,
                        stream::Side::Server,
                        "the bridge speaks as the server"
                    );
                    self.file_stream(packet, &mut credits);
                    continue;
                }
            };
            let name = packet.session.as_ref().map(|s| s.as_str().to_owned());
            match packet.message {
                Message::Attached {
                    offset, truncated, ..
                } => {
                    let seen = self.sessions.entry(name.unwrap()).or_default();
                    seen.attached.push((offset, truncated));
                    seen.next = offset;
                }
                Message::Picture {
                    offset,
                    cols,
                    rows,
                    more,
                    data,
                } => {
                    let seen = self.sessions.entry(name.unwrap()).or_default();
                    Pic::file(&mut seen.pictures, offset, cols, rows, more, &data);
                }
                Message::Output { offset, data } => {
                    let seen = self.sessions.entry(name.unwrap()).or_default();
                    assert_eq!(offset, seen.next, "output is not contiguous");
                    seen.next += data.len() as u64;
                    if !seen.discard {
                        seen.data.extend_from_slice(&data);
                    }
                }
                Message::Ended(exit) => {
                    self.sessions.entry(name.unwrap()).or_default().ended = Some(exit)
                }
                Message::TerminalState(state) => self
                    .sessions
                    .entry(name.unwrap())
                    .or_default()
                    .states
                    .push(state),
                Message::Error { code, text } => self.errors.push((name, code, text)),
                Message::ListReply(statuses) => self.listed.push(statuses),
                Message::Created(status) => self.created.push(status),
                Message::StatusReply(status) => self.statuses.push(status),
                Message::Pong(bytes) => self.pongs.push(bytes),
                Message::FeaturesReply(features) => self.features.push(features),
                other => panic!("the bridge sent {other:?}"),
            }
        }
        // Bytes taken are handed back after the whole read, the way a client
        // that reads a buffer's worth and then credits it does.
        for (id, bytes) in credits {
            self.stream_message(id, stream::Message::Credit(bytes));
        }
    }

    fn file_stream(&mut self, packet: stream::Packet, credits: &mut Vec<(u32, u32)>) {
        let id = packet.stream;
        let flow = self.flows.entry(id).or_default();
        match packet.message {
            stream::Message::Opened { window } => flow.opened = Some(window),
            stream::Message::Failed { code, text } => flow.failed = Some((code, text)),
            stream::Message::Data(bytes) => {
                flow.total += bytes.len() as u64;
                if !flow.discard {
                    flow.data.extend_from_slice(&bytes);
                }
                if flow.hold {
                    flow.owed += bytes.len() as u64;
                } else {
                    credits.push((id, bytes.len() as u32));
                }
            }
            stream::Message::Stderr(bytes) => {
                flow.total += bytes.len() as u64;
                flow.stderr.extend_from_slice(&bytes);
                if flow.hold {
                    flow.owed += bytes.len() as u64;
                } else {
                    credits.push((id, bytes.len() as u32));
                }
            }
            stream::Message::Credit(bytes) => flow.credit += u64::from(bytes),
            stream::Message::Eof => {
                flow.eof = true;
                flow.ends.push("eof");
            }
            stream::Message::Exited(exit) => {
                flow.exited = Some(exit);
                flow.ends.push("exited");
            }
            stream::Message::Close => {
                flow.closed = true;
                flow.ends.push("close");
            }
            other @ (stream::Message::Open { .. }
            | stream::Message::OpenCommand { .. }
            | stream::Message::OpenTicket { .. }) => {
                panic!("the bridge sent {other:?}")
            }
        }
    }

    /// Pumps until `done` holds or the patience is out.
    pub fn until(&mut self, what: &str, done: impl Fn(&Bridge) -> bool) {
        let start = Instant::now();
        loop {
            self.pump(Duration::from_millis(20));
            if done(self) {
                return;
            }
            if self.closed {
                let lost = self.lost.clone().unwrap_or_default();
                let end = self.ending();
                panic!("the bridge closed while waiting for {what} ({lost}); {end}");
            }
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {what}");
        }
    }

    /// How the process on the other end stands, for a failure message: it
    /// tells a process that was killed, or that left on its own, from one that
    /// is still there (the connection was lost on this side, then). A process
    /// that closed its end is given a moment to finish exiting.
    fn ending(&mut self) -> String {
        let start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return format!("its process ended with {status}"),
                Ok(None) if start.elapsed() < Duration::from_secs(2) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(None) => return "its process is still running".to_owned(),
                Err(e) => return format!("its process cannot be waited for: {e}"),
            }
        }
    }

    pub fn seen(&mut self, id: &str) -> &mut Seen {
        self.sessions.entry(id.to_owned()).or_default()
    }

    /// Starts a session through the bridge; the error is what it answered.
    pub fn try_new(
        &mut self,
        id: Option<&str>,
        argv: &[&str],
    ) -> Result<Status, (ErrorCode, String)> {
        let new = NewSession {
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            ..NewSession::default()
        };
        self.try_new_with(id, new)
    }

    /// Starts a session as `new` says, through the bridge.
    pub fn try_new_with(
        &mut self,
        id: Option<&str>,
        new: NewSession,
    ) -> Result<Status, (ErrorCode, String)> {
        let before = (self.created.len(), self.errors.len());
        self.send(id, Message::New(new));
        self.until("an answer to new", |b| {
            b.created.len() > before.0 || b.errors.len() > before.1
        });
        if self.created.len() > before.0 {
            Ok(self.created.pop().unwrap())
        } else {
            let (_, code, text) = self.errors.pop().unwrap();
            Err((code, text))
        }
    }

    pub fn attach(&mut self, id: &str, offset: u64) -> (u64, bool) {
        let before = self.seen(id).attached.len();
        self.send(Some(id), Message::attach(offset));
        self.until("attached", |b| {
            b.sessions
                .get(id)
                .is_some_and(|s| s.attached.len() > before)
        });
        *self.seen(id).attached.last().unwrap()
    }

    pub fn type_in(&mut self, id: &str, text: &str) {
        self.send(Some(id), Message::Input(text.as_bytes().to_vec()));
    }

    pub fn wait_for(&mut self, id: &str, needle: &str) {
        let needle = needle.as_bytes().to_vec();
        let id = id.to_owned();
        self.until(
            &format!("{:?}", String::from_utf8_lossy(&needle)),
            move |b| {
                b.sessions
                    .get(&id)
                    .is_some_and(|s| s.data.windows(needle.len()).any(|w| w == needle.as_slice()))
            },
        );
    }

    pub fn list(&mut self) -> Vec<Status> {
        let before = self.listed.len();
        self.send(None, Message::List);
        self.until("a list", |b| b.listed.len() > before);
        self.listed.pop().unwrap()
    }

    pub fn status(&mut self, id: &str) -> Status {
        self.statuses.clear();
        self.send(Some(id), Message::Status);
        self.until("a status", |b| !b.statuses.is_empty());
        self.statuses.pop().unwrap()
    }

    /// Pumps until an `error` arrives and returns it.
    pub fn next_error(&mut self) -> (Option<String>, ErrorCode, String) {
        self.until("an error", |b| !b.errors.is_empty());
        self.errors.remove(0)
    }

    /// Closes our end of the connection, as a client that goes away does.
    pub fn hang_up(&mut self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }

    /// Starts keeping every byte received from now on in `raw`.
    pub fn record(&mut self) {
        self.raw = Some(Vec::new());
    }

    /// Sends bytes as they are.
    pub fn send_bytes(&mut self, bytes: &[u8]) {
        let _ = self.sock.write_all(bytes);
    }

    /// Sends a stream frame as it is written here, with a payload that is
    /// `id` and then `rest`: for the frames a conforming client does not send.
    pub fn stream_raw(&mut self, kind: u8, id: u32, rest: &[u8]) {
        let mut payload = id.to_be_bytes().to_vec();
        payload.extend_from_slice(rest);
        let mut bytes = Vec::new();
        kablo_proto::frame::encode_frame(
            &mut bytes,
            &kablo_proto::frame::RawFrame {
                kind,
                channel: kablo_proto::frame::CHANNEL_STREAM,
                session: Vec::new(),
                payload,
            },
        )
        .unwrap();
        let _ = self.sock.write_all(&bytes);
    }

    /// Sends one stream message as the client.
    pub fn stream_message(&mut self, id: u32, message: stream::Message) {
        let mut bytes = Vec::new();
        stream::Packet::new(id, message)
            .encode(stream::Side::Client, &mut bytes)
            .unwrap();
        let _ = self.sock.write_all(&bytes);
    }

    /// Sends an `open` for the next stream number and returns it. The answer
    /// is in [`Bridge::flows`].
    pub fn open_stream(&mut self, host: &str, port: u16, window: u32) -> u32 {
        self.next_stream += 1;
        let id = self.next_stream;
        let (hold, discard) = std::mem::take(&mut self.next_flow);
        let flow = self.flow(id);
        flow.hold = hold;
        flow.discard = discard;
        self.stream_message(
            id,
            stream::Message::Open {
                window,
                port,
                host: host.to_owned(),
            },
        );
        id
    }

    /// Sends an `open` for a command, as the next stream number, and returns it.
    /// The answer is in [`Bridge::flows`].
    pub fn open_command(&mut self, command: stream::Command, window: u32) -> u32 {
        self.next_stream += 1;
        let id = self.next_stream;
        let (hold, discard) = std::mem::take(&mut self.next_flow);
        let flow = self.flow(id);
        flow.hold = hold;
        flow.discard = discard;
        self.stream_message(id, stream::Message::OpenCommand { window, command });
        id
    }

    /// Opens a command and waits for the answer: the stream and the window the
    /// bridge offered, or the stream, the code and the text of why it would
    /// not.
    pub fn run_command(
        &mut self,
        command: stream::Command,
        window: u32,
    ) -> Result<(u32, u32), (u32, FailCode, String)> {
        let id = self.open_command(command, window);
        self.until("the answer to an open", |b| {
            b.flows
                .get(&id)
                .is_some_and(|f| f.opened.is_some() || f.failed.is_some())
        });
        let flow = self.flow(id);
        match (flow.opened, flow.failed.clone()) {
            (Some(window), _) => Ok((id, window)),
            (None, Some((code, text))) => Err((id, code, text)),
            (None, None) => unreachable!(),
        }
    }

    /// Opens a stream and waits for the answer: the stream and the window the
    /// bridge offered, or the stream, the code and the text of why it would
    /// not.
    pub fn connect_stream(
        &mut self,
        host: &str,
        port: u16,
        window: u32,
    ) -> Result<(u32, u32), (u32, FailCode, String)> {
        let id = self.open_stream(host, port, window);
        self.until("the answer to an open", |b| {
            b.flows
                .get(&id)
                .is_some_and(|f| f.opened.is_some() || f.failed.is_some())
        });
        let flow = self.flow(id);
        match (flow.opened, flow.failed.clone()) {
            (Some(window), _) => Ok((id, window)),
            (None, Some((code, text))) => Err((id, code, text)),
            (None, None) => unreachable!(),
        }
    }

    /// Sends bytes on a stream, in `data` messages of at most `piece` bytes.
    /// The caller keeps within the window.
    pub fn stream_send(&mut self, id: u32, bytes: &[u8], piece: usize) {
        let mut wire = Vec::new();
        for chunk in bytes.chunks(piece) {
            stream::Packet::new(id, stream::Message::Data(chunk.to_vec()))
                .encode(stream::Side::Client, &mut wire)
                .unwrap();
        }
        let _ = self.sock.write_all(&wire);
        self.flow(id).sent += bytes.len() as u64;
    }

    /// The client has no more bytes for the target.
    pub fn stream_eof(&mut self, id: u32) {
        self.stream_message(id, stream::Message::Eof);
    }

    /// Ends a stream both ways.
    pub fn stream_close(&mut self, id: u32) {
        self.stream_message(id, stream::Message::Close);
    }

    /// What this client has seen of stream `id`.
    pub fn flow(&mut self, id: u32) -> &mut Flow {
        self.flows.entry(id).or_default()
    }

    /// Gives credit for what has been received and held.
    pub fn stream_release(&mut self, id: u32) {
        let owed = std::mem::take(&mut self.flow(id).owed);
        let mut left = owed;
        while left > 0 {
            let part = left.min(u64::from(u32::MAX)) as u32;
            self.stream_message(id, stream::Message::Credit(part));
            left -= u64::from(part);
        }
    }

    /// Waits for the process on the other end to exit.
    pub fn wait_exit(&mut self) -> std::process::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(start.elapsed() < PATIENCE, "the bridge never exited");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- TCP targets -------------------------------------------------------------

/// A listener of this test's own that runs `handler` on a thread of its own
/// for every connection it takes. Returns the port.
pub fn target(handler: impl Fn(TcpStream) + Send + Sync + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler = Arc::new(handler);
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            let Ok(connection) = connection else { continue };
            let handler = Arc::clone(&handler);
            std::thread::spawn(move || handler(connection));
        }
    });
    port
}

/// Gives back what it is sent, and its end when it hears the end.
pub fn echo(mut connection: TcpStream) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match connection.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if connection.write_all(&buf[..n]).is_err() {
                    return;
                }
            }
        }
    }
    let _ = connection.shutdown(Shutdown::Write);
}

/// A port that refuses connections. It is below the range the system hands
/// out, so no listener of a test running side by side is given it (a port that
/// was bound and let go can be, before the connection is made), and nothing
/// listens on it. (A socket that is bound and never listens is not a way to
/// hold a port that refuses: macOS drops the connection instead of refusing.)
pub fn closed_port() -> u16 {
    1
}

/// Bytes that show where they are in a stream: a swapped or dropped piece
/// changes them.
pub fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i % 251) as u8 ^ seed ^ (i / 251) as u8)
        .collect()
}

/// A connection to `address` that gives up reading after the patience, so a
/// test that is wrong fails instead of hanging.
pub fn client(address: SocketAddr) -> TcpStream {
    let socket = TcpStream::connect(address).unwrap();
    socket.set_read_timeout(Some(PATIENCE)).unwrap();
    socket
}

/// Whether a connection was ended on the other side, as a client sees it: an
/// end, or a reset. (A read that only ran out of patience is not an end.)
pub fn is_over(socket: &mut TcpStream) -> bool {
    let mut byte = [0u8; 1];
    match socket.read(&mut byte) {
        Ok(n) => n == 0,
        Err(e) => matches!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::ConnectionAborted
                | std::io::ErrorKind::BrokenPipe
        ),
    }
}

/// Reads exactly `len` bytes.
pub fn read_exactly(socket: &mut TcpStream, len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    socket.read_exact(&mut bytes).unwrap();
    bytes
}

// --- `kablo forward` -----------------------------------------------------------

/// A `kablo forward` in this test's directory (and so the carrier it starts:
/// the environment is inherited), with what it says on standard output and
/// standard error kept as lines.
pub struct Forward {
    child: Child,
    stdout: mpsc::Receiver<String>,
    stderr: mpsc::Receiver<String>,
    said: Vec<String>,
}

impl Forward {
    /// Starts `kablo forward <args>`.
    pub fn start(env: &Env, args: &[&str]) -> Forward {
        let mut command = env.kablo();
        command
            .arg("forward")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let lines = |source: Box<dyn Read + Send>| {
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(source).lines().map_while(Result::ok) {
                    if sender.send(line).is_err() {
                        break;
                    }
                }
            });
            receiver
        };
        let stdout = lines(Box::new(child.stdout.take().unwrap()));
        let stderr = lines(Box::new(child.stderr.take().unwrap()));
        Forward {
            child,
            stdout,
            stderr,
            said: Vec::new(),
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Waits for `count` lines of `forwarding ADDRESS to TARGET` and returns
    /// the addresses.
    pub fn announced(&mut self, count: usize) -> Vec<SocketAddr> {
        let mut found = Vec::new();
        while found.len() < count {
            let line = match self.stdout.recv_timeout(PATIENCE) {
                Ok(line) => line,
                Err(_) => panic!(
                    "no announcement (got {} of {count}); it said {:?}",
                    found.len(),
                    self.drain()
                ),
            };
            let rest = line
                .strip_prefix("forwarding ")
                .unwrap_or_else(|| panic!("not an announcement: {line:?}"));
            let (address, _) = rest.split_once(" to ").unwrap();
            found.push(address.parse().unwrap());
        }
        found
    }

    /// Asserts that nothing was announced and nothing will be: standard output
    /// has ended without a line.
    pub fn announced_nothing(&mut self) {
        match self.stdout.recv_timeout(PATIENCE) {
            Err(RecvTimeoutError::Disconnected) => {}
            other => panic!("expected no announcement, got {other:?}"),
        }
    }

    fn drain(&mut self) -> &[String] {
        while let Ok(line) = self.stderr.try_recv() {
            self.said.push(line);
        }
        &self.said
    }

    /// Waits for a line on standard error that has `needle` in it and returns
    /// it.
    pub fn said(&mut self, needle: &str) -> String {
        loop {
            if let Some(line) = self.said.iter().find(|line| line.contains(needle)) {
                return line.clone();
            }
            match self.stderr.recv_timeout(PATIENCE) {
                Ok(line) => self.said.push(line),
                Err(_) => panic!(
                    "never said {needle:?}; it said {:?}; the process: {:?}",
                    self.said,
                    self.child.try_wait()
                ),
            }
        }
    }

    /// Waits for the process to end.
    pub fn wait_exit(&mut self) -> ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(
                start.elapsed() < PATIENCE,
                "`kablo forward` never ended; it said {:?}",
                self.drain()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- kablo exec ------------------------------------------------------------------

/// A `kablo exec` of this test's directory that is running, with its standard
/// input, output and error as the test's ends of pipes. The carrier is this
/// build's own `kablo attach`.
pub struct Exec {
    pub child: Child,
    stdin: Option<std::process::ChildStdin>,
    /// None where the test holds the pipe itself.
    stdout: Option<mpsc::Receiver<Vec<u8>>>,
    stderr: mpsc::Receiver<Vec<u8>>,
}

/// What a `kablo exec` did: how it ended, and what it wrote.
#[derive(Debug)]
pub struct Ran {
    pub status: ExitStatus,
    pub out: Vec<u8>,
    pub err: Vec<u8>,
}

impl Ran {
    pub fn out(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    pub fn err(&self) -> String {
        String::from_utf8_lossy(&self.err).into_owned()
    }
}

impl Exec {
    /// Starts `kablo exec <options> -- <carrier> -- <program>`.
    pub fn start_over(env: &Env, options: &[&str], carrier: &[&str], program: &[&str]) -> Exec {
        Exec::spawn(env, options, carrier, program, true).0
    }

    /// Starts `kablo exec <options> -- kablo attach -- <program>` and leaves its
    /// standard output to the test: the pipe is the one returned.
    pub fn start_holding_output(
        env: &Env,
        options: &[&str],
        program: &[&str],
    ) -> (Exec, std::process::ChildStdout) {
        let carrier = [env!("CARGO_BIN_EXE_kablo"), "attach"];
        let (exec, held) = Exec::spawn(env, options, &carrier, program, false);
        (exec, held.unwrap())
    }

    fn spawn(
        env: &Env,
        options: &[&str],
        carrier: &[&str],
        program: &[&str],
        capture_output: bool,
    ) -> (Exec, Option<std::process::ChildStdout>) {
        let mut command = env.kablo();
        command
            .arg("exec")
            .args(options)
            .arg("--")
            .args(carrier)
            .arg("--")
            .args(program)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        let everything = |mut source: Box<dyn Read + Send>| {
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let mut all = Vec::new();
                let _ = source.read_to_end(&mut all);
                let _ = sender.send(all);
            });
            receiver
        };
        let (stdout, held) = if capture_output {
            (
                Some(everything(Box::new(child.stdout.take().unwrap()))),
                None,
            )
        } else {
            (None, child.stdout.take())
        };
        let exec = Exec {
            stdin: child.stdin.take(),
            stdout,
            stderr: everything(Box::new(child.stderr.take().unwrap())),
            child,
        };
        (exec, held)
    }

    /// Starts `kablo exec <options> -- kablo attach -- <program>`.
    pub fn start(env: &Env, options: &[&str], program: &[&str]) -> Exec {
        Exec::start_over(
            env,
            options,
            &[env!("CARGO_BIN_EXE_kablo"), "attach"],
            program,
        )
    }

    /// Writes to the program's input.
    pub fn type_in(&mut self, bytes: &[u8]) {
        self.stdin.as_mut().unwrap().write_all(bytes).unwrap();
    }

    /// Ends the program's input.
    pub fn end_input(&mut self) {
        self.stdin = None;
    }

    /// Waits for the end, and for what was written to the end of it.
    pub fn finish(mut self) -> Ran {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(start.elapsed() < PATIENCE, "`kablo exec` never ended");
            std::thread::sleep(Duration::from_millis(10));
        };
        self.stdin = None;
        let out = match &self.stdout {
            Some(stdout) => stdout.recv_timeout(PATIENCE).unwrap(),
            None => Vec::new(),
        };
        let err = self.stderr.recv_timeout(PATIENCE).unwrap();
        Ran { status, out, err }
    }
}

impl Drop for Exec {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs `kablo exec <options> -- kablo attach -- <program>` with `input` on
/// its standard input (then its end) and waits for it.
pub fn exec(env: &Env, options: &[&str], program: &[&str], input: &[u8]) -> Ran {
    let mut running = Exec::start(env, options, program);
    // A program that takes none of it must not hold the test up: the input is
    // written from a thread of its own.
    let stdin = running.stdin.take();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&input);
        }
    });
    let ran = running.finish();
    let _ = writer.join();
    ran
}
