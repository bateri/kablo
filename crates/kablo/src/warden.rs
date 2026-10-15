//! The warden: the shell's parent, which outlives everything else.
//!
//! One warden per session. It opens the terminal, starts the shell as its
//! child, creates the journal and binds the session's socket, then starts a
//! *relay* and hands it all three. From then on it does little: it keeps the
//! shell's terminal read while no relay is doing so, restarts a relay that
//! dies, passes a client's `terminate` on to the shell, and cleans up when
//! the shell's end has reached a client.
//!
//! **Its code reaches a running session only by renewal.** Nothing starts a
//! new warden for a session that has a shell: a new release's relay is
//! started by the old release's warden, and an update never touches the
//! shell's parent. So this file is std and `libc` only (through `sys`), panics
//! nowhere (`unwrap`, `expect` and indexing are denied in the workspace),
//! speaks only the inner wire, and holds no logic that could be wrong in a way
//! a relay could not work around. Behaviour belongs in the relay. What cannot
//! change is the way out: a warden of any release must be able to renew into
//! a later one and a later one must be able to take over from it (see
//! *Renewing*), because a build that lost either would leave every warden
//! started from the builds before it where it is.
//!
//! **Replacing the relay.** A relay that was asked to (`upgrade`) hands the
//! request on; the warden checks the binary named, starts it as a *second*
//! relay beside the first and gives it the session. Only when that one says it
//! is set up (`Ready`) does the first get `Quit`; when the first has let go of
//! the terminal (`Released`, or it is simply gone) the second is told `Go` and
//! is the relay. At every moment one process at most reads the terminal, which
//! is what keeps "exactly the bytes it missed" true across the change. A second
//! relay that fails before `Ready` costs nothing: the first never stopped, and
//! is told so (`UpgradeFailed`). After the change the warden starts relays
//! from the new binary.
//!
//! **Renewing.** The relay an upgrade brought in also says which warden
//! records its binary reads ([`ToWarden::CanRenew`]). Once it serves and the
//! warden is at rest (the shell lives, nothing is being handed over, nothing
//! is half-sent or half-read on the relay's channel) the warden writes what it
//! knows as a [`Renewal`] into a pipe and replaces its own program with that
//! binary (`execve`): the same process, so the shell's parent and the pid
//! stay, and the descriptors it keeps (terminal, journal, listening socket,
//! the relay's channel) stay open under the same numbers. The new image
//! ([`resume`]) reads the record, takes the processes up again by number and
//! tells the relay it was renewed. If the `execve` fails the old image goes
//! on, and no second try is made for that upgrade. If the new image cannot
//! take the session up it ends, and the relay serves on without a warden: the
//! shell lives, but nobody restarts a relay, collects its exit code or hangs
//! it up on `terminate`.
//!
//! It waits on file descriptors only: the shell's exit watch, the terminal
//! while it reads it, the channels of its relays. Its timers are named and
//! end: [`ENDED_HOLD`] after the shell exits and no client has heard of it,
//! the warden gives up waiting and cleans up; [`QUIT_WAIT`] is how long a relay
//! that was told to quit has to answer.

use std::ffi::OsStr;
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{Exit, Message, NewSession};

use crate::inner::{self, Adopt, Decoder, INNER, Renewal, ToRelay, ToWarden};
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

/// How long a relay that was told to quit has to answer, and let go of the
/// terminal, before it is killed: neither an upgrade nor the end of a session
/// waits for one that is stuck.
pub const QUIT_WAIT: Duration = Duration::from_secs(5);

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
    let warden = match Warden::start(id, dir, &request, listener, &control) {
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

/// Carries on as the warden of a session that an earlier image of this very
/// process was the warden of: it replaced its program with this one
/// ([`Warden::renew`]) and left what it knew in the pipe whose read end is
/// descriptor `record`. Nothing of a first start is done again (no new
/// session, no `setsid`, descriptors are not closed), and a record that
/// cannot be used ends the process with the socket left alone: the relay
/// keeps serving the session without a warden.
pub fn resume(id: SessionId, record: RawFd) -> u8 {
    if record <= 2 || !sys::is_open(record) {
        return 1;
    }
    // SAFETY: the earlier image passed this descriptor, the read end of a
    // pipe, by number; it was just seen to be open and is owned from here.
    let pipe = unsafe { OwnedFd::from_raw_fd(record) };
    let mut file = std::fs::File::from(pipe);
    let renewal = match inner::read::<Renewal, _>(&mut file) {
        Ok(Some(renewal)) if renewal.id == id => renewal,
        _ => return 1,
    };
    drop(file);
    let warden = match Warden::resume(renewal) {
        Ok(warden) => warden,
        Err(_) => return 1,
    };
    let path = sockdir::socket_path(&warden.dir, &warden.id);
    let code = warden.serve();
    // As after a first start: the socket is this warden's to remove.
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
    /// Which relay this is, for the life of the warden: the places a relay is
    /// kept change when it takes over, the number does not.
    serial: u64,
    child: sys::Proc,
    chan: UnixStream,
    stage: Stage,
    /// Bytes received before the handshake is complete.
    hello: Vec<u8>,
    decoder: Decoder<ToWarden>,
    /// Bytes waiting to be written to the channel.
    out: Vec<u8>,
    started: Instant,
    /// The binary it was started from.
    exe: PathBuf,
    /// The renewal record versions its binary says it reads, once it has said.
    renews: Option<(u8, u8)>,
    /// An upgrade asked for this relay: the warden renews into its binary once
    /// it serves.
    for_upgrade: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    /// Started; its hello has not been read.
    Hello,
    /// It has the session and has not said it is set up.
    AwaitReady,
    /// Set up and waiting for the relay that has the terminal to let go.
    Standby,
    /// It reads the terminal and writes the journal.
    Active,
    /// It was told to quit and has not yet let go of the terminal.
    Retiring,
}

/// Where a relay is kept.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role {
    /// The one that serves: `Active`, or `Retiring` while it hands over.
    Serving,
    /// One being brought up, to take over.
    Next,
}

struct Warden {
    id: SessionId,
    /// The directory of the session's socket.
    dir: PathBuf,
    master: OwnedFd,
    journal: Journal,
    listener: UnixListener,
    child: sys::Proc,
    /// Readable when the shell exits; gone once the exit is collected.
    watch: Option<OwnedFd>,
    exit: Option<Exit>,
    ended_at: Option<Instant>,
    delivered: bool,
    master_open: bool,
    relay: Option<RelayLink>,
    /// A relay on its way to taking over: the first of a session, a restart,
    /// or the replacement an upgrade asked for.
    next: Option<RelayLink>,
    /// When a relay that was told to quit is killed for not answering.
    retire_by: Option<Instant>,
    /// The binary relays are started from.
    relay_exe: PathBuf,
    serials: u64,
    strikes: u32,
    ever_ready: bool,
    quitting: bool,
    /// An upgrade has brought in a relay whose binary this warden is to renew
    /// into, as soon as the session is at rest.
    renew_pending: bool,
    /// The channel to `kablo new` until the first relay is ready.
    control: Option<UnixStream>,
    chunk: usize,
}

impl Warden {
    /// Opens the terminal, starts the shell, makes the journal. The error is
    /// what `kablo new` is told.
    fn start(
        id: SessionId,
        dir: &Path,
        request: &NewSession,
        listener: UnixListener,
        control: &UnixStream,
    ) -> Result<Warden, String> {
        let capacity = request.journal_size.unwrap_or(journal::DEFAULT_CAPACITY);
        let mut journal = Journal::create(capacity).map_err(|e| e.to_string())?;
        if let Some(rows) = request.history {
            journal.set_history(rows);
        }
        let size = match request.size {
            Some(size) if size.cols != 0 && size.rows != 0 => size,
            _ => pty::DEFAULT_SIZE,
        };
        let pty = pty::open(size).map_err(|e| format!("cannot open a terminal: {e}"))?;
        let mut child = pty::spawn(&pty.slave, request)
            .and_then(sys::Proc::new)
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
            dir: dir.to_path_buf(),
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
            next: None,
            retire_by: None,
            relay_exe,
            serials: 0,
            strikes: 0,
            ever_ready: false,
            quitting: false,
            renew_pending: false,
            control: Some(control),
            chunk,
        })
    }

    /// Takes the session up from the record an earlier image left. The
    /// processes are known by number, the descriptors by the numbers they had,
    /// and the relay is the one that served, `Active`.
    fn resume(renewal: Renewal) -> Result<Warden, String> {
        let Renewal {
            id,
            dir,
            shell_pid,
            master,
            journal,
            listener,
            relay_chan,
            relay_pid,
            relay_serial,
            relay_binary,
            relay_exe,
            serials,
            strikes,
            ever_ready,
            chunk,
            delivered,
        } = renewal;
        let numbers = [master, journal, listener, relay_chan];
        let mut sorted = numbers;
        sorted.sort_unstable();
        if numbers.iter().any(|&fd| fd <= 2 || !sys::is_open(fd))
            || sorted.windows(2).any(|pair| pair.first() == pair.get(1))
        {
            return Err("the record names descriptors that are not open".to_owned());
        }
        // SAFETY: each number is a descriptor the earlier image left open for
        // this one; checked open and distinct above, owned from here.
        let [master, journal, listener, chan] =
            numbers.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
        for fd in [&master, &journal, &listener, &chan] {
            sys::set_cloexec(fd.as_raw_fd()).map_err(|e| e.to_string())?;
        }
        let journal =
            Journal::open(journal).map_err(|e| format!("cannot open the journal: {e}"))?;
        let listener = UnixListener::from(listener);
        let chan = UnixStream::from(chan);
        chan.set_nonblocking(true).map_err(|e| e.to_string())?;
        let mut child = sys::Proc::from_pid(shell_pid).map_err(|e| e.to_string())?;
        let relay_child = sys::Proc::from_pid(relay_pid).map_err(|e| e.to_string())?;
        // The watch first and the look after it: a shell that ended while
        // nobody was watching waits as a zombie, and its exit is not lost.
        let watch = sys::exit_watch(child.id()).ok();
        let ended = child
            .try_wait()
            .map_err(|e| format!("cannot look at the program: {e}"))?
            .is_some();
        if watch.is_none() && !ended {
            return Err("cannot watch the program".to_owned());
        }
        let path = |bytes: Vec<u8>| PathBuf::from(std::ffi::OsString::from_vec(bytes));
        // A relay that has been serving is not a young one: if it dies soon
        // after the renewal, that is not a strike.
        let started = Instant::now()
            .checked_sub(RELAY_YOUTH)
            .unwrap_or_else(Instant::now);
        let relay = RelayLink {
            serial: relay_serial,
            child: relay_child,
            chan,
            stage: Stage::Active,
            hello: Vec::new(),
            decoder: Decoder::new(),
            out: Vec::new(),
            started,
            exe: path(relay_binary),
            renews: None,
            for_upgrade: false,
        };
        let mut warden = Warden {
            id,
            dir: path(dir),
            master,
            journal,
            listener,
            child,
            watch,
            exit: None,
            ended_at: None,
            delivered,
            master_open: true,
            relay: Some(relay),
            next: None,
            retire_by: None,
            relay_exe: path(relay_exe),
            serials,
            strikes,
            ever_ready,
            quitting: false,
            renew_pending: false,
            control: None,
            chunk: usize::try_from(chunk).unwrap_or(4096),
        };
        warden.send(relay_serial, &ToRelay::Renewed(crate::release()));
        if ended {
            warden.on_child_exit();
        }
        Ok(warden)
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
            let now = Instant::now();
            let ended_by = self.ended_at.map(|at| at + ENDED_HOLD);
            if ended_by.is_some_and(|at| now >= at) {
                self.finish();
                return 0;
            }
            if self.retire_by.is_some_and(|at| now >= at) {
                // The relay that was told to quit did not: it is not waited
                // for. The journal is whole up to what it published, so the
                // next relay can take over from there.
                self.promote();
                continue;
            }
            // Every change to what this looks at comes with an event that
            // ends a turn, so the look comes before each wait.
            self.renew_if_at_rest();
            let deadline = match (ended_by, self.retire_by) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            let timeout = deadline.map(|at| at.saturating_duration_since(now));

            let mut set = Vec::with_capacity(4);
            let mut sources = Vec::with_capacity(4);
            if let Some(watch) = &self.watch {
                set.push(sys::want(watch.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Child);
            }
            if self.master_open && !self.relay_owns_terminal() {
                set.push(sys::want(self.master.as_raw_fd(), libc::POLLIN));
                sources.push(Source::Terminal);
            }
            for link in self.relay.iter().chain(self.next.iter()) {
                let events = libc::POLLIN
                    | if link.out.is_empty() {
                        0
                    } else {
                        libc::POLLOUT
                    };
                set.push(sys::want(link.chan.as_raw_fd(), events));
                sources.push(Source::Relay(link.serial));
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
                    // A relay that took over or went in the meantime is
                    // looked for by its number, not its place.
                    Source::Relay(serial) => self.on_relay(*serial, fd.revents),
                }
            }
        }
    }

    /// Whether a relay is the one reading the terminal right now.
    fn relay_owns_terminal(&self) -> bool {
        self.relay
            .as_ref()
            .is_some_and(|relay| matches!(relay.stage, Stage::Active | Stage::Retiring))
    }

    /// No relay has ever got going and none will: the session cannot be
    /// served, and `kablo new` is told so.
    fn give_up_on_start(&mut self) -> bool {
        if self.ever_ready
            || self.relay.is_some()
            || self.next.is_some()
            || self.strikes < RELAY_STRIKES
        {
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

    /// The shell exited: collect it and tell the relays that have the session.
    fn on_child_exit(&mut self) {
        self.watch = None;
        // A session whose shell has ended is left as it is.
        self.renew_pending = false;
        let exit = match self.child.wait() {
            Ok(status) => pty::exit_of(status),
            Err(_) => Exit::Code(-1),
        };
        self.exit = Some(exit);
        self.ended_at = Some(Instant::now());
        // A relay still shaking hands is told in what it adopts.
        let told: Vec<u64> = self
            .relay
            .iter()
            .chain(self.next.iter())
            .filter(|link| link.stage != Stage::Hello)
            .map(|link| link.serial)
            .collect();
        for serial in told {
            self.send(serial, &ToRelay::ChildEnded(exit));
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
            reap(relay);
        }
        if let Some(relay) = self.next.take() {
            reap(relay);
        }
    }

    // --- the relays -------------------------------------------------------

    /// Which of the two places the relay called `serial` is in.
    fn role(&self, serial: u64) -> Option<Role> {
        if self
            .relay
            .as_ref()
            .is_some_and(|link| link.serial == serial)
        {
            Some(Role::Serving)
        } else if self.next.as_ref().is_some_and(|link| link.serial == serial) {
            Some(Role::Next)
        } else {
            None
        }
    }

    fn link(&mut self, serial: u64) -> Option<&mut RelayLink> {
        self.relay
            .iter_mut()
            .chain(self.next.iter_mut())
            .find(|link| link.serial == serial)
    }

    /// Takes the relay called `serial` out of the place it is in.
    fn take(&mut self, serial: u64) -> Option<RelayLink> {
        match self.role(serial)? {
            Role::Serving => self.relay.take(),
            Role::Next => self.next.take(),
        }
    }

    /// Starts a relay from the current binary, or counts the failure to.
    fn start_relay(&mut self) {
        while self.relay.is_none()
            && self.next.is_none()
            && self.strikes < RELAY_STRIKES
            && !self.quitting
        {
            let exe = self.relay_exe.clone();
            match self.spawn_relay(&exe) {
                Ok(link) => self.next = Some(link),
                Err(_) => self.strikes += 1,
            }
        }
    }

    fn spawn_relay(&mut self, exe: &Path) -> io::Result<RelayLink> {
        let (ours, theirs) = UnixStream::pair()?;
        ours.set_nonblocking(true)?;
        let child = sys::Proc::new(
            Command::new(exe)
                .arg("relay")
                .arg("--id")
                .arg(self.id.as_str())
                .stdin(Stdio::from(OwnedFd::from(theirs)))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        )?;
        self.serials = self.serials.wrapping_add(1);
        Ok(RelayLink {
            serial: self.serials,
            child,
            chan: ours,
            stage: Stage::Hello,
            hello: Vec::new(),
            decoder: Decoder::new(),
            out: Vec::new(),
            started: Instant::now(),
            exe: exe.to_path_buf(),
            renews: None,
            for_upgrade: false,
        })
    }

    /// The channel of relay `serial` is readable or writable, or hung up.
    fn on_relay(&mut self, serial: u64, revents: libc::c_short) {
        if revents & libc::POLLOUT != 0 {
            self.flush_relay(serial);
        }
        if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0 {
            return;
        }
        let mut buf = [0u8; 4096];
        let mut unexpected = Vec::new();
        loop {
            let Some(link) = self.link(serial) else {
                return;
            };
            match sys::recv_with_fds(link.chan.as_fd(), &mut buf, &mut unexpected) {
                Ok(0) => return self.relay_gone(serial),
                Ok(n) => {
                    // The relay never sends descriptors; any that came are
                    // closed with `unexpected` at the end of this call.
                    unexpected.clear();
                    let bytes = buf.get(..n).unwrap_or_default();
                    if !self.relay_bytes(serial, bytes) {
                        return self.relay_gone(serial);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(_) => return self.relay_gone(serial),
            }
        }
    }

    /// Bytes from a relay. `false` when the relay misbehaved.
    fn relay_bytes(&mut self, serial: u64, bytes: &[u8]) -> bool {
        let Some(link) = self.link(serial) else {
            return false;
        };
        let rest = if link.stage == Stage::Hello {
            link.hello.extend_from_slice(bytes);
            let step = match crate::handshake::serve(&INNER, &link.hello) {
                Ok(step) => step,
                Err(_) => return false,
            };
            match step {
                crate::handshake::Step::Need => return true,
                crate::handshake::Step::Refused { .. } => return false,
                crate::handshake::Step::Done {
                    reply, consumed, ..
                } => {
                    let leftover = link.hello.get(consumed..).unwrap_or_default().to_vec();
                    link.hello.clear();
                    if !self.adopt(serial, &reply) {
                        return false;
                    }
                    leftover
                }
            }
        } else {
            bytes.to_vec()
        };
        // Everything the bytes hold is read before any of it is acted on: what
        // a message does can move the relay it came from.
        let mut messages = Vec::new();
        {
            let Some(link) = self.link(serial) else {
                return false;
            };
            link.decoder.push(&rest);
            loop {
                match link.decoder.next_message() {
                    Ok(Some(message)) => messages.push(message),
                    Ok(None) => break,
                    Err(e) if e.is_recoverable() => {}
                    Err(_) => return false,
                }
            }
        }
        for message in messages {
            self.on_message(serial, message);
        }
        true
    }

    /// Answers the relay's hello and gives it the session.
    fn adopt(&mut self, serial: u64, reply: &[u8]) -> bool {
        let adopt = ToRelay::Adopt(Adopt {
            id: self.id.clone(),
            child_pid: self.child.id(),
            exit: self.exit,
            warden_version: Some(crate::release()),
        });
        let fds = [
            self.master.as_raw_fd(),
            self.journal.as_raw_fd(),
            self.listener.as_raw_fd(),
        ];
        let Some(link) = self.link(serial) else {
            return false;
        };
        // The reply is a few bytes into an empty socket buffer: it goes whole
        // or the relay is not one to rely on.
        match sys::write_fd(link.chan.as_fd(), reply) {
            Ok(n) if n == reply.len() => {}
            _ => return false,
        }
        let mut bytes = Vec::new();
        if inner::write(&mut bytes, &adopt).is_err() {
            return false;
        }
        match sys::send_with_fds(link.chan.as_fd(), &bytes, &fds) {
            Ok(n) if n == bytes.len() => {}
            _ => return false,
        }
        link.stage = Stage::AwaitReady;
        true
    }

    fn on_message(&mut self, serial: u64, message: ToWarden) {
        let Some(role) = self.role(serial) else {
            return;
        };
        let stage = self.link(serial).map(|link| link.stage);
        match message {
            ToWarden::Ready => {
                if role == Role::Next && stage == Some(Stage::AwaitReady) {
                    self.on_next_ready();
                }
            }
            ToWarden::Released { .. } => {
                if role == Role::Serving && stage == Some(Stage::Retiring) {
                    self.promote();
                }
            }
            ToWarden::Terminate { force } => {
                if role == Role::Serving {
                    self.terminate(force);
                }
            }
            ToWarden::Delivered => {
                if role == Role::Serving && self.exit.is_some() {
                    self.delivered = true;
                }
            }
            ToWarden::Upgrade { path } => {
                if role == Role::Serving && stage == Some(Stage::Active) {
                    self.upgrade(&path);
                }
            }
            ToWarden::CanRenew { min, max } => {
                if let Some(link) = self.link(serial) {
                    link.renews = Some((min, max));
                }
            }
        }
    }

    /// The relay being brought up is set up. If another relay has the
    /// terminal it is asked to let go; if none does, this one takes over.
    fn on_next_ready(&mut self) {
        if let Some(next) = self.next.as_mut() {
            next.stage = Stage::Standby;
        }
        let serving = self.relay.as_mut().map(|link| (link.serial, link.stage));
        match serving {
            Some((serial, Stage::Active)) => {
                if let Some(link) = self.relay.as_mut() {
                    link.stage = Stage::Retiring;
                }
                self.retire_by = Some(Instant::now() + QUIT_WAIT);
                self.send(serial, &ToRelay::Quit);
            }
            // Told to quit already: the change finishes when it lets go.
            Some(_) => {}
            None => self.promote(),
        }
    }

    /// The relay that served has let go of the terminal (or is gone): the one
    /// that was brought up takes over. If it is not set up yet it will, when
    /// it is; until then nobody has the terminal and the warden reads it.
    fn promote(&mut self) {
        self.retire_by = None;
        if let Some(old) = self.relay.take() {
            reap(old);
        }
        let Some(mut next) = self.next.take() else {
            self.start_relay();
            return;
        };
        if next.stage != Stage::Standby {
            self.next = Some(next);
            return;
        }
        next.stage = Stage::Active;
        self.relay_exe.clone_from(&next.exe);
        self.ever_ready = true;
        // Whether the binary can be renewed into is looked at when the
        // warden is at rest: what it says about itself is a frame of its own.
        self.renew_pending |= next.for_upgrade;
        let serial = next.serial;
        self.relay = Some(next);
        self.send(serial, &ToRelay::Go);
        if let Some(mut control) = self.control.take() {
            let _ = control.write_all(b"ok\n");
        }
    }

    /// A relay asked for itself to be replaced by the binary at `path`.
    fn upgrade(&mut self, path: &[u8]) {
        if self.quitting {
            return;
        }
        if self.exit.is_some() {
            return self.upgrade_failed("the program has ended; there is nothing to move");
        }
        let exe = match check_binary(path) {
            Ok(exe) => exe,
            Err(why) => return self.upgrade_failed(&why),
        };
        if let Some(waiting) = self.next.take() {
            if waiting.stage == Stage::Standby {
                self.next = Some(waiting);
                return self.upgrade_failed("an upgrade is finishing; try again in a moment");
            }
            // One that has not got ready by now is not waited for: a binary
            // that never speaks must not make upgrades impossible.
            reap(waiting);
        }
        match self.spawn_relay(&exe) {
            Ok(mut link) => {
                link.for_upgrade = true;
                self.next = Some(link);
            }
            Err(e) => self.upgrade_failed(&format!("cannot start {}: {e}", exe.display())),
        }
    }

    /// Tells the relay that asked that the upgrade did not happen.
    fn upgrade_failed(&mut self, why: &str) {
        let serving = self
            .relay
            .as_ref()
            .filter(|link| link.stage == Stage::Active)
            .map(|link| link.serial);
        if let Some(serial) = serving {
            self.send(serial, &ToRelay::UpgradeFailed(why.to_owned()));
        }
    }

    /// Renews this warden if an upgrade asked for it and the session is at
    /// rest: the shell lives, no relay is being brought in or let go of, and
    /// the relay's channel holds nothing half-sent or half-read, which a new
    /// image could not take up. Called before every wait, so it runs again
    /// whenever something it looks at has changed.
    fn renew_if_at_rest(&mut self) {
        if !self.renew_pending {
            return;
        }
        if self.exit.is_some() || self.quitting {
            self.renew_pending = false;
            return;
        }
        if self.control.is_some() || self.next.is_some() || self.retire_by.is_some() {
            return;
        }
        let Some(link) = self.relay.as_ref() else {
            return;
        };
        if link.stage != Stage::Active || !link.out.is_empty() || link.decoder.pending() != 0 {
            return;
        }
        match link.renews {
            // It has not said yet; it does, right after it is set up.
            None => return,
            Some(range) if !reads_renewal(range) => {
                self.renew_pending = false;
                return;
            }
            Some(_) => {}
        }
        // One try for an upgrade: whatever comes of it, not again.
        self.renew_pending = false;
        self.renew();
    }

    /// Replaces this program with the binary of the relay that serves, and
    /// hands the new image what this one knows. Returns only if that did not
    /// happen, with everything as it was.
    fn renew(&mut self) {
        let Some(link) = self.relay.as_ref() else {
            return;
        };
        let Ok(chunk) = u32::try_from(self.chunk) else {
            return;
        };
        // What made the binary fit to run was looked at when the relay was
        // started from it; the file may have been changed since.
        if check_binary(link.exe.as_os_str().as_bytes()).is_err() {
            return;
        }
        let kept = [
            self.master.as_raw_fd(),
            self.journal.as_raw_fd(),
            self.listener.as_raw_fd(),
            link.chan.as_raw_fd(),
        ];
        let exe = link.exe.clone();
        let renewal = Renewal {
            id: self.id.clone(),
            dir: self.dir.as_os_str().as_bytes().to_vec(),
            shell_pid: self.child.id(),
            master: self.master.as_raw_fd(),
            journal: self.journal.as_raw_fd(),
            listener: self.listener.as_raw_fd(),
            relay_chan: link.chan.as_raw_fd(),
            relay_pid: link.child.id(),
            relay_serial: link.serial,
            relay_binary: exe.as_os_str().as_bytes().to_vec(),
            relay_exe: self.relay_exe.as_os_str().as_bytes().to_vec(),
            serials: self.serials,
            strikes: self.strikes,
            ever_ready: self.ever_ready,
            chunk,
            delivered: self.delivered,
        };
        let mut record = Vec::new();
        if inner::write(&mut record, &renewal).is_err() {
            return;
        }
        let Ok((read, write)) = sys::pipe() else {
            return;
        };
        // A record the pipe cannot take whole is not sent: this warden never
        // waits on a pipe of its own.
        if sys::set_nonblocking(write.as_raw_fd()).is_err() {
            return;
        }
        match sys::write_fd(write.as_fd(), &record) {
            Ok(n) if n == record.len() => {}
            _ => return,
        }
        drop(write);
        let carried: Vec<RawFd> = kept.iter().copied().chain([read.as_raw_fd()]).collect();
        if carried.iter().any(|&fd| sys::clear_cloexec(fd).is_err()) {
            for fd in kept {
                let _ = sys::set_cloexec(fd);
            }
            return;
        }
        // A watch on the shell is made anew by the new image; this one must
        // not leak into it (a kqueue is not closed by an exec on its own).
        if let Some(watch) = &self.watch {
            let _ = sys::set_cloexec(watch.as_raw_fd());
        }
        // `--id` and `--dir` stay on the command line as they were: they are
        // how this process is found among others.
        let args = [
            exe.as_os_str().to_owned(),
            "warden".into(),
            "--id".into(),
            self.id.as_str().into(),
            "--dir".into(),
            self.dir.as_os_str().to_owned(),
            "--resume".into(),
            read.as_raw_fd().to_string().into(),
        ];
        // It returns only to say it did not happen.
        let _ = sys::exec(&exe, &args);
        for fd in kept {
            let _ = sys::set_cloexec(fd);
        }
    }

    /// Queues a message for relay `serial` and writes what the channel takes.
    fn send(&mut self, serial: u64, message: &ToRelay) {
        let Some(link) = self.link(serial) else {
            return;
        };
        if inner::write(&mut link.out, message).is_err() {
            return;
        }
        self.flush_relay(serial);
    }

    fn flush_relay(&mut self, serial: u64) {
        let Some(link) = self.link(serial) else {
            return;
        };
        while !link.out.is_empty() {
            match sys::write_fd(link.chan.as_fd(), &link.out) {
                Ok(n) if n > 0 => {
                    link.out.drain(..n.min(link.out.len()));
                }
                Ok(_) => return,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                // A dead channel is reported by the read side.
                Err(_) => return,
            }
        }
    }

    /// A relay's end of the channel closed: it died (or let go and left).
    /// Reap it and carry on: another is waiting to take over, or the next is
    /// started, unless relays keep dying young.
    fn relay_gone(&mut self, serial: u64) {
        let Some(role) = self.role(serial) else {
            return;
        };
        let Some(link) = self.take(serial) else {
            return;
        };
        let (stage, young) = (link.stage, link.started.elapsed() < RELAY_YOUTH);
        reap(link);
        // A relay that was told to quit is not a relay that died.
        let strike = |warden: &mut Warden| {
            if young {
                warden.strikes += 1;
            } else {
                warden.strikes = 0;
            }
        };
        match role {
            Role::Serving => {
                if stage != Stage::Retiring {
                    strike(self);
                }
                self.promote();
            }
            Role::Next if self.relay.is_none() => {
                // A start that did not take.
                strike(self);
                self.start_relay();
            }
            Role::Next => {
                // An upgrade that did not take: the relay that serves never
                // stopped (or, if it was told to quit, will go and a relay
                // from the old binary is started).
                self.upgrade_failed("the new relay stopped before it took over");
            }
        }
    }

    /// Ends the session: the relay told to go and waited for, but not for
    /// long. The socket path is the caller's to remove.
    fn finish(&mut self) {
        self.quitting = true;
        if let Some(waiting) = self.next.take() {
            reap(waiting);
        }
        let Some(link) = self.relay.as_ref() else {
            return;
        };
        let (serial, stage) = (link.serial, link.stage);
        if stage == Stage::Active {
            self.send(serial, &ToRelay::Quit);
        }
        // The relay answers and closes its end; wait for that, but only as
        // long as it takes a relay that is working.
        if let Some(link) = self.relay.as_ref() {
            await_close(&link.chan, QUIT_WAIT);
        }
        if let Some(link) = self.relay.take() {
            reap(link);
        }
    }
}

/// Reads and discards what comes on `chan` until the other end closes it or
/// `within` has passed; whether it closed.
fn await_close(chan: &UnixStream, within: Duration) -> bool {
    let by = Instant::now() + within;
    let mut buf = [0u8; 256];
    let mut fds = Vec::new();
    loop {
        let left = by.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        let mut set = [sys::want(chan.as_raw_fd(), libc::POLLIN)];
        if sys::poll(&mut set, Some(left)).is_err() {
            return false;
        }
        if set.iter().all(|fd| fd.revents == 0) {
            continue;
        }
        match sys::recv_with_fds(chan.as_fd(), &mut buf, &mut fds) {
            Ok(0) => return true,
            Ok(_) => fds.clear(),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(_) => return false,
        }
    }
}

/// Ends a relay's process, whatever it is doing, and collects it.
fn reap(link: RelayLink) {
    let mut child = link.child;
    let _ = child.kill();
    let _ = child.wait();
}

/// The binary an upgrade names, if it is one to start: an absolute path to a
/// regular file somebody can run, that is ours or the superuser's (what an
/// administrator installs) and that nobody else can write to.
fn check_binary(path: &[u8]) -> Result<PathBuf, String> {
    let path = PathBuf::from(OsStr::from_bytes(path));
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    let meta =
        std::fs::metadata(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    let owner = meta.uid();
    if owner != sys::effective_uid() && owner != 0 {
        return Err(format!("{} belongs to somebody else", path.display()));
    }
    let mode = meta.permissions().mode();
    if mode & 0o022 != 0 {
        return Err(format!(
            "{} can be written by others; it is not run",
            path.display()
        ));
    }
    if mode & 0o111 == 0 {
        return Err(format!("{} is not executable", path.display()));
    }
    Ok(path)
}

/// Whether a relay's binary, which says it reads the renewal records from
/// `range.0` to `range.1`, reads the one this build writes.
fn reads_renewal(range: (u8, u8)) -> bool {
    (range.0..=range.1).contains(&inner::RENEWAL_VERSION)
}

/// What a descriptor in the poll set stands for.
enum Source {
    Child,
    Terminal,
    /// The channel of the relay with this number.
    Relay(u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/kt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn file(dir: &Path, name: &str, mode: u32) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    #[test]
    fn only_a_binary_that_can_be_run_and_not_changed_by_others_is_started() {
        let dir = scratch("check-binary");
        let good = file(&dir, "good", 0o755);
        assert_eq!(check_binary(good.as_os_str().as_bytes()), Ok(good.clone()));
        assert!(check_binary(file(&dir, "mine", 0o700).as_os_str().as_bytes()).is_ok());

        let refused = |path: &Path| check_binary(path.as_os_str().as_bytes()).unwrap_err();
        assert!(refused(Path::new("kablo")).contains("not an absolute path"));
        assert!(refused(&dir.join("nothing")).contains("cannot read"));
        assert!(refused(&dir).contains("not a regular file"));
        assert!(refused(&file(&dir, "plain", 0o644)).contains("not executable"));
        assert!(refused(&file(&dir, "group", 0o775)).contains("written by others"));
        assert!(refused(&file(&dir, "world", 0o757)).contains("written by others"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_warden_renews_only_into_a_binary_that_reads_what_it_writes() {
        let version = inner::RENEWAL_VERSION;
        assert!(reads_renewal((inner::RENEWAL_MIN_VERSION, version)));
        assert!(reads_renewal((version, version.saturating_add(1))));
        assert!(!reads_renewal((
            version.saturating_add(1),
            version.saturating_add(2)
        )));
        assert!(!reads_renewal((0, version.saturating_sub(1))));
        // A range written the wrong way round reads nothing.
        assert!(!reads_renewal((version.saturating_add(1), version)));
    }

    #[test]
    fn a_relay_that_never_answers_is_waited_for_a_while_and_no_longer() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let started = Instant::now();
        assert!(!await_close(&ours, Duration::from_millis(200)));
        assert!(started.elapsed() >= Duration::from_millis(200));
        assert!(started.elapsed() < Duration::from_secs(5));

        // What it says on the way out is read and thrown away; the end is the
        // close.
        (&theirs).write_all(b"released").unwrap();
        drop(theirs);
        assert!(await_close(&ours, Duration::from_secs(30)));
    }
}
