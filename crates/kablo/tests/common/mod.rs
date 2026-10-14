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

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use kablo_proto::v1::{ErrorCode, Exit, Message, Packet, PacketDecoder, Status};
use kablo_proto::{SessionId, WIRE};

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

    /// The pid of the `role` (`warden` or `relay`) of session `id`.
    pub fn find(&self, role: &str, id: &str) -> Option<u32> {
        let wanted = format!("kablo {role} --id {id}");
        self.processes()
            .into_iter()
            .find(|(_, _, command)| command.contains(&wanted))
            .map(|(pid, _, _)| pid)
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
        self.send(Message::Attach { offset });
        self.until("attached", |c| c.attached.is_some());
        self.attached.unwrap()
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
