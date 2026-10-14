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
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kablo_proto::v1::{
    ErrorCode, Exit, Message, NewSession, Packet, PacketDecoder, Size, Status, View,
};
use kablo_proto::{SessionId, WIRE};
use kablo_screen::Screen;

/// How long a test waits for anything before it calls it failed.
pub const PATIENCE: Duration = Duration::from_secs(30);

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
    /// Every picture received, across attaches.
    pub pictures: Vec<Pic>,
    pub ended: Option<Exit>,
    pub errors: Vec<(ErrorCode, String)>,
    pub replies: Vec<Packet>,
    pub closed: bool,
}

impl Conn {
    pub fn open(path: PathBuf, id: &str) -> Conn {
        Conn::try_open(path, id, PATIENCE).unwrap_or_else(|e| panic!("cannot connect to {id}: {e}"))
    }

    pub fn try_open(path: PathBuf, id: &str, patience: Duration) -> Result<Conn, String> {
        let mut sock = UnixStream::connect(path).map_err(|e| e.to_string())?;
        sock.set_read_timeout(Some(patience)).unwrap();
        WIRE.client(&mut sock.try_clone().unwrap(), &mut sock)
            .map_err(|e| e.to_string())?;
        sock.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        Ok(Conn {
            sock,
            decoder: PacketDecoder::new(),
            id: SessionId::new(id).unwrap(),
            data: Vec::new(),
            next: 0,
            attached: None,
            pictures: Vec::new(),
            ended: None,
            errors: Vec::new(),
            replies: Vec::new(),
            closed: false,
        })
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
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
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
                Message::Attached { offset, truncated } => {
                    self.attached = Some((offset, truncated));
                    self.next = offset;
                    self.data.clear();
                }
                Message::Picture {
                    offset,
                    cols,
                    rows,
                    more,
                    data,
                } => Pic::file(&mut self.pictures, offset, cols, rows, more, &data),
                Message::Output { offset, data } => {
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
            }),
        });
        self.until("attached and its picture", |c| {
            c.attached.is_some() && c.pictures.last().is_some_and(|p| p.complete)
        });
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
}

/// A client of `kablo attach` (or of anything else that speaks the protocol
/// over a connected socket, such as `ssh host kablo attach`): many sessions
/// over one connection.
pub struct Bridge {
    sock: UnixStream,
    decoder: PacketDecoder,
    child: Child,
    pub sessions: HashMap<String, Seen>,
    pub listed: Vec<Vec<Status>>,
    pub created: Vec<Status>,
    pub statuses: Vec<Status>,
    pub pongs: Vec<Vec<u8>>,
    /// `(session, code, text)` of every `error`.
    pub errors: Vec<(Option<String>, ErrorCode, String)>,
    pub closed: bool,
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
            let got = sock
                .read(&mut byte)
                .expect("the bridge answers the handshake");
            assert_eq!(got, 1, "the bridge closed before it answered");
            seen.push(byte[0]);
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
                decoder: PacketDecoder::new(),
                child,
                sessions: HashMap::new(),
                listed: Vec::new(),
                created: Vec::new(),
                statuses: Vec::new(),
                pongs: Vec::new(),
                errors: Vec::new(),
                closed: false,
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
                    break;
                }
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
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
            let name = packet.session.as_ref().map(|s| s.as_str().to_owned());
            match packet.message {
                Message::Attached { offset, truncated } => {
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
                    seen.data.extend_from_slice(&data);
                }
                Message::Ended(exit) => {
                    self.sessions.entry(name.unwrap()).or_default().ended = Some(exit)
                }
                Message::Error { code, text } => self.errors.push((name, code, text)),
                Message::ListReply(statuses) => self.listed.push(statuses),
                Message::Created(status) => self.created.push(status),
                Message::StatusReply(status) => self.statuses.push(status),
                Message::Pong(bytes) => self.pongs.push(bytes),
                other => panic!("the bridge sent {other:?}"),
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
            assert!(!self.closed, "the bridge closed while waiting for {what}");
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {what}");
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
