//! The warden: the shell's parent, which outlives everything else.
//!
//! One warden per session. It opens the terminal, starts the shell as its
//! child, creates the journal and binds the session's socket, then starts a
//! *relay* and hands it all three. From then on it does little: it keeps the
//! shell's terminal read while no relay is doing so, restarts a relay that
//! dies, passes a client's `terminate` on to the shell, and cleans up when
//! the shell's end has reached a client.
//!
//! **It is frozen.** While the shell lives the warden's code does not change:
//! a new release's relay is started by the old release's warden, and an
//! update never touches the shell's parent. So this file is std and `libc`
//! only (through `sys`), panics nowhere (`unwrap`, `expect` and indexing are
//! denied in the workspace), speaks only the inner wire, and holds no logic
//! that could be wrong in a way a relay could not work around. Behaviour
//! belongs in the relay.
//!
//! It waits on file descriptors only: the shell's exit watch, the terminal
//! while it reads it, the relay's channel. The one timer is the end of an
//! ended session that no client has heard of: [`ENDED_HOLD`] after the shell
//! exits the warden gives up waiting and cleans up.

use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{Exit, Message, NewSession};

use crate::inner::{self, Adopt, Decoder, INNER, ToRelay, ToWarden};
use crate::journal::{self, Journal};
use crate::pty;
use crate::sockdir;
use crate::sys;

/// How long the end of a session waits for a client to hear of it: a day.
pub const ENDED_HOLD: Duration = Duration::from_secs(24 * 60 * 60);

/// How many relays in a row may die young before the warden stops starting
/// them.
pub const RELAY_STRIKES: u32 = 5;

/// A relay that dies sooner than this after it was started counts as a strike;
/// one that served longer than this was working, and the count starts over.
pub const RELAY_YOUTH: Duration = Duration::from_secs(10);

/// The most program bytes one read of the terminal takes.
const READ_CHUNK: u64 = 64 * 1024;

/// How many reads of the terminal one turn of the loop may make before it
/// looks at the other descriptors.
const READ_BUDGET: usize = 16;

/// Runs a warden for session `id` in socket directory `dir`; the request to
/// start it and the channel to answer on are standard input, a socket that
/// `kablo new` made. Returns the process's exit status.
pub fn run(id: SessionId, dir: &Path) -> u8 {
    // A new session: no controlling terminal, no process group of the
    // session that started us.
    // SAFETY: no arguments; failing (already a leader) changes nothing here.
    unsafe { libc::setsid() };
    let mut control = match take_control() {
        Ok(control) => control,
        Err(e) => {
            // Nobody to tell; there is no standard error to speak of either.
            let _ = e;
            return 1;
        }
    };
    let request = match read_request(&mut control, &id) {
        Ok(request) => request,
        Err(e) => return refuse(&mut control, &format!("the request was unreadable: {e}")),
    };
    if let Err(e) = sockdir::ensure_dir(dir) {
        return refuse(&mut control, &e.to_string());
    }
    let path = sockdir::socket_path(dir, &id);
    let listener = match sockdir::listen(&path) {
        Ok(listener) => listener,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            return refuse(&mut control, &crate::launch::exists_text(&id));
        }
        Err(e) => return refuse(&mut control, &e.to_string()),
    };
    let warden = match Warden::start(id, &request, listener, &control) {
        Ok(warden) => warden,
        Err(why) => {
            let _ = std::fs::remove_file(&path);
            return refuse(&mut control, &why);
        }
    };
    // `kablo new` waits for the end of this channel: the warden holds its own
    // copy, and the first relay's `ready` is what it says on it.
    drop(control);
    let code = warden.serve();
    // The socket is ours from the bind on: whatever ended the warden, it goes.
    let _ = std::fs::remove_file(&path);
    code
}

/// Takes standard input (the socket `kablo new` made) for our own and puts
/// `/dev/null` in its place, so that no later `open` can return descriptor 0.
fn take_control() -> io::Result<UnixStream> {
    // SAFETY: a plain `dup` of descriptor 0.
    let copy = sys::cvt(unsafe { libc::dup(0) })?;
    // SAFETY: a descriptor just returned to us, owned from here.
    let copy = unsafe { OwnedFd::from_raw_fd(copy) };
    sys::set_cloexec(copy.as_raw_fd())?;
    close_inherited(copy.as_raw_fd());
    // SAFETY: a C string for the call.
    let null = sys::cvt(unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) })?;
    // SAFETY: a descriptor just returned to us, owned from here.
    let null = unsafe { OwnedFd::from_raw_fd(null) };
    // SAFETY: both descriptors are open; this replaces what 0 refers to.
    sys::cvt(unsafe { libc::dup2(null.as_raw_fd(), 0) })?;
    Ok(UnixStream::from(copy))
}

/// Closes every descriptor above 2 but `keep`: whatever the process that
/// started `kablo new` left open must not be held by a session that outlives
/// it (an ssh channel kept open here would keep a login alive).
fn close_inherited(keep: std::os::fd::RawFd) {
    let open: Vec<std::os::fd::RawFd> = std::fs::read_dir("/dev/fd")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    for fd in open {
        if fd > 2 && fd != keep {
            // SAFETY: closing a descriptor number found open; the listing's
            // own descriptor is among them and is already closed by now,
            // which `close` reports and nobody minds.
            unsafe { libc::close(fd) };
        }
    }
}

/// Reads the one packet `kablo new` sends: this session's `new`.
fn read_request(control: &mut UnixStream, id: &SessionId) -> io::Result<NewSession> {
    let packet = kablo_proto::v1::read_packet(control)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
        .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
    match (packet.session, packet.message) {
        (Some(session), Message::New(new)) if session == *id => Ok(new),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a request for this session",
        )),
    }
}

/// Tells `kablo new` why there is no session, and ends.
fn refuse(control: &mut UnixStream, why: &str) -> u8 {
    let _ = control.write_all(format!("error: {why}\n").as_bytes());
    1
}

/// A relay as the warden sees it.
struct RelayLink {
    child: Child,
    chan: UnixStream,
    stage: Stage,
    /// Bytes received before the handshake is complete.
    hello: Vec<u8>,
    decoder: Decoder<ToWarden>,
    /// Bytes waiting to be written to the channel.
    out: Vec<u8>,
    started: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Started; its hello has not been read.
    Hello,
    /// It has the session and has not said it is set up.
    AwaitReady,
    /// It reads the terminal and writes the journal.
    Active,
}

struct Warden {
    id: SessionId,
    master: OwnedFd,
    journal: Journal,
    listener: UnixListener,
    child: Child,
    /// Readable when the shell exits; gone once the exit is collected.
    watch: Option<OwnedFd>,
    exit: Option<Exit>,
    ended_at: Option<Instant>,
    delivered: bool,
    master_open: bool,
    relay: Option<RelayLink>,
    relay_exe: PathBuf,
    strikes: u32,
    ever_ready: bool,
    quitting: bool,
    /// The channel to `kablo new` until the first relay is ready.
    control: Option<UnixStream>,
    chunk: usize,
}

impl Warden {
    /// Opens the terminal, starts the shell, makes the journal. The error is
    /// what `kablo new` is told.
    fn start(
        id: SessionId,
        request: &NewSession,
        listener: UnixListener,
        control: &UnixStream,
    ) -> Result<Warden, String> {
        let capacity = request.journal_size.unwrap_or(journal::DEFAULT_CAPACITY);
        let journal = Journal::create(capacity).map_err(|e| e.to_string())?;
        let size = match request.size {
            Some(size) if size.cols != 0 && size.rows != 0 => size,
            _ => pty::DEFAULT_SIZE,
        };
        let pty = pty::open(size).map_err(|e| format!("cannot open a terminal: {e}"))?;
        let mut child = pty::spawn(&pty.slave, request)
            .map_err(|e| format!("cannot start the program: {e}"))?;
        let pty::Pty { master, slave } = pty;
        // Our copy of the slave must go: while it is open the master never
        // sees the program's end.
        drop(slave);
        let started = (|| {
            let watch = sys::exit_watch(child.id())
                .map_err(|e| format!("cannot watch the program: {e}"))?;
            let relay_exe =
                std::env::current_exe().map_err(|e| format!("cannot find myself: {e}"))?;
            let control = control.try_clone().map_err(|e| e.to_string())?;
            Ok::<_, String>((watch, relay_exe, control))
        })();
        let (watch, relay_exe, control) = match started {
            Ok(started) => started,
            Err(why) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(why);
            }
        };
        // Do not hold the directory `kablo new` was run from.
        // SAFETY: a C string for the call.
        unsafe { libc::chdir(c"/".as_ptr()) };
        let chunk = usize::try_from(READ_CHUNK.min(journal.capacity() / 4)).unwrap_or(4096);
        Ok(Warden {
            id,
            master,
            journal,
            listener,
            child,
            watch: Some(watch),
            exit: None,
            ended_at: None,
            delivered: false,
            master_open: true,
            relay: None,
            relay_exe,
            strikes: 0,
            ever_ready: false,
            quitting: false,
            control: Some(control),
            chunk,
        })
    }

    /// The loop. Ends when the session is over and cleaned up (status 0) or
    /// when no relay could ever be started (status 1, after the shell was
    /// ended).
    fn serve(mut self) -> u8 {
        self.start_relay();
        loop {
            if self.give_up_on_start() {
                self.kill_shell();
                return 1;
            }
            if self.delivered {
                self.finish();
                return 0;
            }
            let deadline = self.ended_at.map(|at| at + ENDED_HOLD);
            if deadline.is_some_and(|at| Instant::now() >= at) {
                self.finish();
                return 0;
            }
            let timeout = deadline.map(|at| at.saturating_duration_since(Instant::now()));

            let mut set = Vec::with_capacity(3);
            let mut sources = Vec::with_capacity(3);
            if let Some(watch) = &self.watch {
                set.push(sys::want(watch.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Child);
            }
            if self.master_open && !self.relay_owns_terminal() {
                set.push(sys::want(self.master.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Terminal);
            }
            if let Some(relay) = &self.relay {
                let events = libc::POLLIN
                    | if relay.out.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    };
                set.push(sys::want(relay.chan.as_raw_fd(), events));
                sources.push(Source::Relay);
            }
            if sys::poll(&mut set, timeout).is_err() {
                // `poll` fails only for a bad argument or no memory; neither
                // gets better by trying again at once.
                self.kill_shell();
                return 1;
            }
            for (fd, source) in set.iter().zip(&sources) {
                if fd.revents == 0 {
                    continue;
                }
                match source {
                    Source::Child => self.on_child_exit(),
                    Source::Terminal => self.read_terminal(),
                    Source::Relay => self.on_relay(fd.revents),
                }
            }
        }
    }

    /// Whether a relay is the one reading the terminal right now.
    fn relay_owns_terminal(&self) -> bool {
        self.relay
            .as_ref()
            .is_some_and(|relay| relay.stage == Stage::Active)
    }

    /// No relay has ever got going and none will: the session cannot be
    /// served, and `kablo new` is told so.
    fn give_up_on_start(&mut self) -> bool {
        if self.ever_ready || self.relay.is_some() || self.strikes < RELAY_STRIKES {
            return false;
        }
        if let Some(mut control) = self.control.take() {
            let _ = control.write_all(b"error: the relay does not start\n");
        }
        true
    }

    /// Reads the terminal into the journal because no relay does.
    fn read_terminal(&mut self) {
        for _ in 0..READ_BUDGET {
            match self.journal.fill_from(self.master.as_fd(), self.chunk) {
                Ok(0) => {
                    self.master_open = false;
                    return;
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // `EIO` is how Linux says the program's side is closed.
                Err(_) => {
                    self.master_open = false;
                    return;
                }
            }
        }
    }

    /// The shell exited: collect it and tell the relay.
    fn on_child_exit(&mut self) {
        self.watch = None;
        let exit = match self.child.wait() {
            Ok(status) => pty::exit_of(status),
            Err(_) => Exit::Code(-1),
        };
        self.exit = Some(exit);
        self.ended_at = Some(Instant::now());
        if self
            .relay
            .as_ref()
            .is_some_and(|relay| relay.stage != Stage::Hello)
        {
            self.send(&ToRelay::ChildEnded(exit));
        }
    }

    /// Hangs up on the shell and the foreground group of its terminal
    /// (`force`: kills them).
    fn terminate(&mut self, force: bool) {
        if self.exit.is_some() {
            return;
        }
        let signal = if force { libc::SIGKILL } else { libc::SIGHUP };
        if let Some(group) = sys::foreground_group(self.master.as_fd()) {
            let _ = sys::kill_group(group, signal);
        }
        if let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            let _ = sys::kill(pid, signal);
        }
    }

    /// The session cannot start: end the shell that was started.
    fn kill_shell(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.watch = None;
        self.quitting = true;
        if let Some(relay) = self.relay.take() {
            let mut child = relay.child;
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    // --- the relay --------------------------------------------------------

    /// Starts a relay, or counts the failure to.
    fn start_relay(&mut self) {
        while self.relay.is_none() && self.strikes < RELAY_STRIKES && !self.quitting {
            match self.spawn_relay() {
                Ok(link) => self.relay = Some(link),
                Err(_) => self.strikes += 1,
            }
        }
    }

    fn spawn_relay(&self) -> io::Result<RelayLink> {
        let (ours, theirs) = UnixStream::pair()?;
        ours.set_nonblocking(true)?;
        let child = Command::new(&self.relay_exe)
            .arg("relay")
            .arg("--id")
            .arg(self.id.as_str())
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(RelayLink {
            child,
            chan: ours,
            stage: Stage::Hello,
            hello: Vec::new(),
            decoder: Decoder::new(),
            out: Vec::new(),
            started: Instant::now(),
        })
    }

    /// The relay's channel is readable or writable, or hung up.
    fn on_relay(&mut self, revents: libc::c_short) {
        if revents & libc::POLLOUT != 0 {
            self.flush_relay();
        }
        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0 {
            return;
        }
        let mut buf = [0u8; 4096];
        let mut unexpected = Vec::new();
        loop {
            let Some(relay) = self.relay.as_mut() else {
                return;
            };
            match sys::recv_with_fds(relay.chan.as_fd(), &mut buf, &mut unexpected) {
                Ok(0) => return self.relay_gone(),
                Ok(n) => {
                    // The relay never sends descriptors; any that came are
                    // closed with `unexpected` at the end of this call.
                    unexpected.clear();
                    let bytes = buf.get(..n).unwrap_or_default();
                    if !self.relay_bytes(bytes) {
                        return self.relay_gone();
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => return self.relay_gone(),
            }
        }
    }

    /// Bytes from the relay. `false` when the relay misbehaved.
    fn relay_bytes(&mut self, bytes: &[u8]) -> bool {
        let Some(relay) = self.relay.as_mut() else {
            return false;
        };
        let rest = if relay.stage == Stage::Hello {
            relay.hello.extend_from_slice(bytes);
            let step = match crate::handshake::serve(&INNER, &relay.hello) {
                Ok(step) => step,
                Err(_) => return false,
            };
            match step {
                crate::handshake::Step::Need => return true,
                crate::handshake::Step::Refused { .. } => return false,
                crate::handshake::Step::Done {
                    reply, consumed, ..
                } => {
                    let leftover = relay.hello.get(consumed..).unwrap_or_default().to_vec();
                    relay.hello.clear();
                    if !self.adopt(&reply) {
                        return false;
                    }
                    leftover
                }
            }
        } else {
            bytes.to_vec()
        };
        if let Some(relay) = self.relay.as_mut() {
            relay.decoder.push(&rest);
        }
        loop {
            let message = match self.relay.as_mut() {
                Some(relay) => relay.decoder.next_message(),
                None => return false,
            };
            match message {
                Ok(Some(message)) => self.on_message(message),
                Ok(None) => return true,
                Err(e) if e.is_recoverable() => {}
                Err(_) => return false,
            }
            if self.relay.is_none() {
                return false;
            }
        }
    }

    /// Answers the relay's hello and gives it the session.
    fn adopt(&mut self, reply: &[u8]) -> bool {
        let Some(relay) = self.relay.as_mut() else {
            return false;
        };
        // The reply is a few bytes into an empty socket buffer: it goes whole
        // or the relay is not one to rely on.
        match sys::write_fd(relay.chan.as_fd(), reply) {
            Ok(n) if n == reply.len() => {}
            _ => return false,
        }
        let adopt = ToRelay::Adopt(Adopt {
            id: self.id.clone(),
            child_pid: self.child.id(),
            exit: self.exit,
        });
        let mut bytes = Vec::new();
        if inner::write(&mut bytes, &adopt).is_err() {
            return false;
        }
        let fds = [
            self.master.as_raw_fd(),
            self.journal.as_raw_fd(),
            self.listener.as_raw_fd(),
        ];
        match sys::send_with_fds(relay.chan.as_fd(), &bytes, &fds) {
            Ok(n) if n == bytes.len() => {}
            _ => return false,
        }
        relay.stage = Stage::AwaitReady;
        true
    }

    fn on_message(&mut self, message: ToWarden) {
        match message {
            ToWarden::Ready => {
                let ready = self
                    .relay
                    .as_mut()
                    .is_some_and(|relay| relay.stage == Stage::AwaitReady);
                if !ready {
                    return;
                }
                // From here the relay reads the terminal and the warden does
                // not: the exit from the poll set is the next turn's, and
                // `Go` is what lets the relay begin.
                if let Some(relay) = self.relay.as_mut() {
                    relay.stage = Stage::Active;
                }
                self.ever_ready = true;
                self.send(&ToRelay::Go);
                if let Some(mut control) = self.control.take() {
                    let _ = control.write_all(b"ok\n");
                }
            }
            ToWarden::Terminate { force } => self.terminate(force),
            ToWarden::Delivered => {
                if self.exit.is_some() {
                    self.delivered = true;
                }
            }
            ToWarden::Upgrade { .. } => {
                self.send(&ToRelay::UpgradeFailed(
                    "this warden cannot start another relay".to_owned(),
                ));
            }
            ToWarden::Released { .. } => {}
        }
    }

    /// Queues a message for the relay and writes what the channel takes.
    fn send(&mut self, message: &ToRelay) {
        let Some(relay) = self.relay.as_mut() else {
            return;
        };
        if inner::write(&mut relay.out, message).is_err() {
            return;
        }
        self.flush_relay();
    }

    fn flush_relay(&mut self) {
        let Some(relay) = self.relay.as_mut() else {
            return;
        };
        while !relay.out.is_empty() {
            match sys::write_fd(relay.chan.as_fd(), &relay.out) {
                Ok(n) if n > 0 => {
                    relay.out.drain(..n.min(relay.out.len()));
                }
                Ok(_) => return,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // A dead channel is reported by the read side.
                Err(_) => return,
            }
        }
    }

    /// The relay's end of the channel closed: it died. Reap it and start the
    /// next, unless it keeps dying young.
    fn relay_gone(&mut self) {
        let Some(relay) = self.relay.take() else {
            return;
        };
        let mut child = relay.child;
        // A relay that closed its channel and lives on is of no use.
        let _ = child.kill();
        let _ = child.wait();
        if relay.started.elapsed() < RELAY_YOUTH {
            self.strikes += 1;
        } else {
            self.strikes = 0;
        }
        self.start_relay();
    }

    /// Ends the session: the relay told to go and waited for. The socket path
    /// is the caller's to remove.
    fn finish(&mut self) {
        self.quitting = true;
        let Some(relay) = self.relay.as_ref() else {
            return;
        };
        if relay.stage != Stage::Hello {
            self.send(&ToRelay::Quit);
            // The relay answers and closes its end; wait for that, discarding
            // what it says.
            let mut buf = [0u8; 256];
            let mut fds = Vec::new();
            loop {
                let Some(relay) = self.relay.as_ref() else {
                    return;
                };
                let mut set = [sys::want(relay.chan.as_raw_fd(), libc::POLLIN)];
                if sys::poll(&mut set, None).is_err() {
                    break;
                }
                match sys::recv_with_fds(relay.chan.as_fd(), &mut buf, &mut fds) {
                    Ok(0) => break,
                    Ok(_) => fds.clear(),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => break,
                }
            }
        }
        if let Some(relay) = self.relay.take() {
            let mut child = relay.child;
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// What a descriptor in the poll set stands for.
enum Source {
    Child,
    Terminal,
    Relay,
}
