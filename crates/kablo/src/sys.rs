//! The libc calls the warden and the relay share, each behind a small safe
//! function.
//!
//! Everything `unsafe` in the process code is here or in `journal`, `pty` and
//! `procinfo`, so the loops that use them read as plain Rust. The functions
//! keep to what a loop needs: descriptor flags, `poll`, passing descriptors
//! over a Unix socket, the terminal's size, signals.

use std::ffi::{CString, OsString};
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, ExitStatus};
use std::time::Duration;

use kablo_proto::v1::Size;

/// Turns a `-1` result into the error `errno` holds.
pub fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// Marks `fd` close-on-exec: a copy that a child (the shell, the relay) kept
/// would hold a terminal or a journal open past everyone who meant to.
pub fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: `F_SETFD` changes only the descriptor's flags.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }).map(drop)
}

/// Takes close-on-exec off `fd`, so that a process this one replaces itself
/// with (`execve`) keeps it open under the same number.
pub fn clear_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: `F_SETFD` changes only the descriptor's flags.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) }).map(drop)
}

/// Whether `fd` is an open descriptor of this process.
pub fn is_open(fd: RawFd) -> bool {
    // SAFETY: `F_GETFD` only reads the descriptor's flags.
    unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
}

/// A pipe, as (read end, write end), both close-on-exec.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0 as libc::c_int; 2];
    // SAFETY: `ends` holds the two descriptors `pipe` writes.
    cvt(unsafe { libc::pipe(ends.as_mut_ptr()) })?;
    // SAFETY: both are fresh descriptors nobody else owns.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
    set_cloexec(read.as_raw_fd())?;
    set_cloexec(write.as_raw_fd())?;
    Ok((read, write))
}

/// Replaces this process with the program at `path`, run with `args` (the
/// first is the name it is told) and the environment this process has. Returns
/// only to say why it could not, with nothing of this process changed.
///
/// `std`'s own `exec` is not used because it sets `SIGPIPE` back to its
/// default before it tries and leaves it so when it fails: the next write to a
/// peer that has gone would then end the process that was meant to carry on.
pub fn exec(path: &Path, args: &[OsString]) -> io::Error {
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a name to run");
    let Ok(program) = CString::new(path.as_os_str().as_bytes()) else {
        return invalid();
    };
    let mut owned = Vec::with_capacity(args.len());
    for arg in args {
        match CString::new(arg.as_bytes()) {
            Ok(arg) => owned.push(arg),
            Err(_) => return invalid(),
        }
    }
    let mut argv: Vec<*const libc::c_char> = owned.iter().map(|arg| arg.as_ptr()).collect();
    argv.push(std::ptr::null());
    // SAFETY: `program` and the strings `argv` points into are NUL-terminated
    // and outlive the call, and `argv` ends with a null pointer. `execv`
    // returns only when it failed.
    unsafe { libc::execv(program.as_ptr(), argv.as_ptr()) };
    io::Error::last_os_error()
}

/// Puts `fd` in non-blocking mode.
pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `F_GETFL`/`F_SETFL` change only the open file's flags.
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    // SAFETY: as above.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }).map(drop)
}

/// What the passwd database says of the user this process acts as.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Passwd {
    /// The login name.
    pub name: Vec<u8>,
    /// The login shell, empty when the entry has none.
    pub shell: Vec<u8>,
}

/// The passwd entry of the effective user, if there is one.
pub fn passwd() -> Option<Passwd> {
    // SAFETY: an all-zero `passwd` is valid; `getpwuid_r` fills it.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: `entry`, `buf` and `found` belong to this frame; the buffer
    // length passed is its length.
    let status = unsafe {
        libc::getpwuid_r(
            effective_uid(),
            &raw mut entry,
            buf.as_mut_ptr(),
            buf.len(),
            &raw mut found,
        )
    };
    if status != 0 || found.is_null() {
        return None;
    }
    let text = |field: *const libc::c_char| {
        if field.is_null() {
            return Vec::new();
        }
        // SAFETY: a non-null string field of the entry points into `buf`,
        // NUL-terminated.
        unsafe { std::ffi::CStr::from_ptr(field) }
            .to_bytes()
            .to_vec()
    };
    Some(Passwd {
        name: text(entry.pw_name),
        shell: text(entry.pw_shell),
    })
}

/// The uid this process acts as.
pub fn effective_uid() -> u32 {
    // SAFETY: no arguments, cannot fail.
    unsafe { libc::geteuid() }
}

/// The uid of the process at the other end of a Unix socket.
#[cfg(target_os = "linux")]
pub fn peer_uid(fd: BorrowedFd<'_>) -> io::Result<u32> {
    // SAFETY: an all-zero `ucred` is valid; the kernel fills it.
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` belong to this frame and `len` is its size.
    cvt(unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&raw mut cred).cast(),
            &raw mut len,
        )
    })?;
    Ok(cred.uid)
}

/// The uid of the process at the other end of a Unix socket.
#[cfg(not(target_os = "linux"))]
pub fn peer_uid(fd: BorrowedFd<'_>) -> io::Result<u32> {
    let (mut uid, mut gid) = (0, 0);
    // SAFETY: both out-pointers belong to this frame.
    cvt(unsafe { libc::getpeereid(fd.as_raw_fd(), &raw mut uid, &raw mut gid) })?;
    Ok(uid)
}

/// Whether a peer with this uid may talk to a process that acts as `own`.
/// One rule, one place: only the same user is let in.
pub fn uid_allowed(peer: u32, own: u32) -> bool {
    peer == own
}

/// `poll(2)`. A signal that interrupts the wait is not an error: the call
/// returns `Ok(0)` and every `revents` is whatever it was, so a caller builds
/// its set afresh each turn and treats "nothing ready" as a turn to loop.
pub fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<usize> {
    let timeout = match timeout {
        None => -1,
        Some(wait) => {
            // Round up: waking a hair early would spin on a deadline that
            // has not quite come.
            let millis = wait.as_millis() + u128::from(wait.subsec_nanos() % 1_000_000 != 0);
            libc::c_int::try_from(millis).unwrap_or(libc::c_int::MAX)
        }
    };
    // SAFETY: `fds` is a valid slice of `pollfd`s for the whole call.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::Interrupted {
            for fd in fds.iter_mut() {
                fd.revents = 0;
            }
            Ok(0)
        } else {
            Err(error)
        };
    }
    Ok(ready as usize)
}

/// A `pollfd` that asks about `events` on `fd`.
pub fn want(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

/// Whether `revents` says something worth acting on: the events asked for,
/// or a hang-up or error (a read finds out which).
pub fn is_ready(revents: libc::c_short, events: libc::c_short) -> bool {
    revents & (events | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

/// How many descriptors one message may carry.
pub const MAX_FDS: usize = 8;

/// Sends `data` with `fds` attached (SCM_RIGHTS). The descriptors travel with
/// the first byte of `data`. Returns how many bytes were sent.
pub fn send_with_fds(sock: BorrowedFd<'_>, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    if fds.len() > MAX_FDS || data.is_empty() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let mut iov = libc::iovec {
        iov_base: data.as_ptr().cast_mut().cast(),
        iov_len: data.len(),
    };
    let payload = size_of_val(fds) as libc::c_uint;
    // `u64`s: the control buffer must be aligned for `cmsghdr`.
    let mut control = [0u64; 16];
    // SAFETY: a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(payload) } as usize;
    debug_assert!(space <= size_of_val(&control));
    // SAFETY: an all-zero `msghdr` is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: the control buffer is `space` bytes long and aligned, so the
    // first header and its data fit; the descriptors are copied in, not owned.
    unsafe {
        let header = libc::CMSG_FIRSTHDR(&raw const msg);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(payload) as _;
        let target = libc::CMSG_DATA(header).cast::<libc::c_int>();
        for (i, fd) in fds.iter().enumerate() {
            std::ptr::write_unaligned(target.add(i), *fd);
        }
    }
    loop {
        // SAFETY: `msg` points at this frame's buffers for the call.
        let sent = unsafe { libc::sendmsg(sock.as_raw_fd(), &raw const msg, 0) };
        if sent >= 0 {
            return Ok(sent as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Reads from a Unix socket into `buf` and collects every descriptor that
/// arrived with the bytes into `fds`, owned and close-on-exec. Returns how
/// many bytes were read (0 at the end of the stream). A message whose
/// ancillary data did not fit is an error, but the descriptors that did arrive
/// are in `fds` and close with it.
pub fn recv_with_fds(
    sock: BorrowedFd<'_>,
    buf: &mut [u8],
    fds: &mut Vec<OwnedFd>,
) -> io::Result<usize> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = [0u64; 16];
    // SAFETY: a pure size computation.
    let space =
        unsafe { libc::CMSG_SPACE((MAX_FDS * size_of::<libc::c_int>()) as libc::c_uint) } as usize;
    debug_assert!(space <= size_of_val(&control));
    // SAFETY: an all-zero `msghdr` is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // macOS has no `MSG_CMSG_CLOEXEC`: the flag is set after the fact there.
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let received = loop {
        // SAFETY: `msg` points at this frame's buffers for the call.
        let received = unsafe { libc::recvmsg(sock.as_raw_fd(), &raw mut msg, flags) };
        if received >= 0 {
            break received as usize;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    };
    // SAFETY: the kernel filled `control` up to `msg_controllen`; the `CMSG_*`
    // walk stays inside it, and each descriptor read is a fresh one this
    // process now owns.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&raw const msg);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(header).cast::<libc::c_int>();
                let bytes =
                    ((*header).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
                for i in 0..bytes / size_of::<libc::c_int>() {
                    let fd = OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i)));
                    // Best effort: a descriptor that cannot be marked is
                    // still ours to close.
                    let _ = set_cloexec(fd.as_raw_fd());
                    fds.push(fd);
                }
            }
            header = libc::CMSG_NXTHDR(&raw const msg, header);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("ancillary data was truncated"));
    }
    Ok(received)
}

/// Writes to a descriptor that may be non-blocking; `Interrupted` is retried.
pub fn write_fd(fd: BorrowedFd<'_>, buf: &[u8]) -> io::Result<usize> {
    loop {
        // SAFETY: the buffer is valid for its length for the call.
        let wrote = unsafe { libc::write(fd.as_raw_fd(), buf.as_ptr().cast(), buf.len()) };
        if wrote >= 0 {
            return Ok(wrote as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// The terminal's size.
pub fn window_size(fd: BorrowedFd<'_>) -> io::Result<Size> {
    // SAFETY: an all-zero `winsize` is valid; the kernel fills it.
    let mut window: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open for the call and `window` is a valid out-pointer.
    cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCGWINSZ as _, &raw mut window) })?;
    Ok(Size {
        cols: window.ws_col,
        rows: window.ws_row,
        xpixel: window.ws_xpixel,
        ypixel: window.ws_ypixel,
    })
}

/// Sets the terminal's size; the foreground process group gets `SIGWINCH`
/// when it changed.
pub fn set_window_size(fd: BorrowedFd<'_>, size: Size) -> io::Result<()> {
    let window = libc::winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: size.xpixel,
        ws_ypixel: size.ypixel,
    };
    // SAFETY: `fd` is open for the call and `window` is a valid input.
    cvt(unsafe { libc::ioctl(fd.as_raw_fd(), libc::TIOCSWINSZ as _, &raw const window) }).map(drop)
}

/// The foreground process group of the terminal `master` belongs to.
pub fn foreground_group(master: BorrowedFd<'_>) -> Option<libc::pid_t> {
    // SAFETY: `master` is open for the call.
    let group = unsafe { libc::tcgetpgrp(master.as_raw_fd()) };
    (group > 0).then_some(group)
}

/// Sends `signal` to the process `pid`. A process that is already gone is
/// not an error.
pub fn kill(pid: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: a signal to a pid; a bad one is an error, not a fault.
    match cvt(unsafe { libc::kill(pid, signal) }) {
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        other => other.map(drop),
    }
}

/// Sends `signal` to every process of the group `group`. A group that is
/// already gone is not an error.
pub fn kill_group(group: libc::pid_t, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: as in `kill`.
    match cvt(unsafe { libc::killpg(group, signal) }) {
        Err(e) if e.raw_os_error() == Some(libc::ESRCH) => Ok(()),
        other => other.map(drop),
    }
}

/// A child process, held by its number.
///
/// The warden keeps its shell and its relays like this instead of as
/// [`std::process::Child`]: a number outlives the warden replacing its own
/// program, a `Child` does not, and the new image takes the processes up again
/// by number ([`Proc::from_pid`]). It keeps what `Child` keeps, that the exit
/// status is collected once and that nothing is signalled after: a number
/// whose process was collected may belong to somebody else by then.
#[derive(Debug)]
pub struct Proc {
    pid: libc::pid_t,
    status: Option<ExitStatus>,
}

impl Proc {
    /// Takes over a child `std` started. Dropping the `Child` neither signals
    /// nor collects it, so from here the process is this one's to collect.
    pub fn new(child: Child) -> io::Result<Proc> {
        Proc::from_pid(child.id())
    }

    /// A child of this process that is known by its number only. The number
    /// must be that of a child: this process collects it.
    pub fn from_pid(pid: u32) -> io::Result<Proc> {
        match libc::pid_t::try_from(pid) {
            // 0 and negative numbers address groups, not a process.
            Ok(pid) if pid > 0 => Ok(Proc { pid, status: None }),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a process number",
            )),
        }
    }

    /// The process's number.
    pub fn id(&self) -> u32 {
        // Positive by construction.
        u32::try_from(self.pid).unwrap_or_default()
    }

    /// Kills the process; one that is already gone, or was collected, is not
    /// an error.
    pub fn kill(&mut self) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        kill(self.pid, libc::SIGKILL)
    }

    /// Waits for the process to end and collects it. Once collected, the
    /// same status comes back.
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.collect(0)?
            .ok_or_else(|| io::Error::other("a blocking wait returned without a status"))
    }

    /// Collects the process if it has ended; `None` if it still runs.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.collect(libc::WNOHANG)
    }

    fn collect(&mut self, flags: libc::c_int) -> io::Result<Option<ExitStatus>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        loop {
            let mut raw: libc::c_int = 0;
            // SAFETY: `raw` is a local that lives through the call.
            let got = unsafe { libc::waitpid(self.pid, &raw mut raw, flags) };
            if got == self.pid {
                let status = ExitStatus::from_raw(raw);
                self.status = Some(status);
                return Ok(Some(status));
            }
            if got == 0 {
                return Ok(None);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

/// A descriptor that becomes readable when the process `pid` (a child of this
/// one) exits. Nothing reads it: it stays readable, so a `poll` set holds it
/// until the exit has been collected. No signal handler is involved.
///
/// macOS: a kqueue holding an `EVFILT_PROC`/`NOTE_EXIT` filter (a kqueue
/// descriptor is itself pollable). Linux: a pidfd (kernel 5.3 or later).
pub fn exit_watch(pid: u32) -> io::Result<OwnedFd> {
    watch_exit(pid)
}

#[cfg(target_os = "macos")]
fn watch_exit(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: no arguments; the result is checked.
    let raw = cvt(unsafe { libc::kqueue() })?;
    // SAFETY: `raw` is a fresh descriptor nobody else owns. A kqueue is not
    // inherited across `fork` (kqueue(2)), so no close-on-exec is needed.
    let queue = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: an all-zero `kevent` is a valid value of a plain C struct.
    let mut change: libc::kevent = unsafe { std::mem::zeroed() };
    change.ident = pid as usize;
    change.filter = libc::EVFILT_PROC;
    change.flags = libc::EV_ADD;
    change.fflags = libc::NOTE_EXIT;
    // SAFETY: one change from this frame, no event buffer; `queue` is open.
    cvt(unsafe {
        libc::kevent(
            raw,
            &raw const change,
            1,
            std::ptr::null_mut(),
            0,
            std::ptr::null(),
        )
    })?;
    Ok(queue)
}

#[cfg(target_os = "linux")]
fn watch_exit(pid: u32) -> io::Result<OwnedFd> {
    let pid = libc::pid_t::try_from(pid).map_err(io::Error::other)?;
    // SAFETY: `pidfd_open(pid, 0)`; the result is checked. The descriptor is
    // close-on-exec by definition (pidfd_open(2)).
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    let raw = libc::c_int::try_from(raw).map_err(io::Error::other)?;
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nobody else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Connects to the Unix socket at `path` without ever blocking: a listener
/// whose queue is full is `WouldBlock` rather than a wait. (A connection
/// that is made sits in the listener's queue until it is accepted, so
/// "connected" says nothing about whether anybody is serving.)
pub fn connect_nonblocking(path: &std::path::Path) -> io::Result<OwnedFd> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: an all-zero `sockaddr_un` is a valid value of a plain C struct.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a socket path",
        ));
    }
    addr.sun_family = libc::AF_UNIX as _;
    for (slot, byte) in addr.sun_path.iter_mut().zip(bytes) {
        *slot = *byte as _;
    }
    // SAFETY: no pointers; the result is checked.
    let raw = cvt(unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) })?;
    // SAFETY: a fresh descriptor nobody else owns.
    let sock = unsafe { OwnedFd::from_raw_fd(raw) };
    set_cloexec(sock.as_raw_fd())?;
    set_nonblocking(sock.as_raw_fd())?;
    // SAFETY: `addr` is a valid address of the length given, for the call.
    cvt(unsafe {
        libc::connect(
            sock.as_raw_fd(),
            (&raw const addr).cast(),
            size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    })?;
    Ok(sock)
}

/// Reads from a descriptor that may be non-blocking; `Interrupted` is retried.
pub fn read_fd(fd: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<usize> {
    loop {
        // SAFETY: the buffer is valid for its length for the call.
        let got = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if got >= 0 {
            return Ok(got as usize);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Whether `fd` is a terminal.
pub fn is_tty(fd: BorrowedFd<'_>) -> bool {
    // SAFETY: `fd` is open for the call.
    unsafe { libc::isatty(fd.as_raw_fd()) == 1 }
}

/// A terminal put in raw mode; the settings it had come back when this is
/// dropped.
pub struct RawMode {
    fd: RawFd,
    saved: libc::termios,
}

impl RawMode {
    /// Puts the terminal `fd` in raw mode.
    pub fn enter(fd: BorrowedFd<'_>) -> io::Result<RawMode> {
        // SAFETY: an all-zero `termios` is a valid value; the kernel fills it.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open and `saved` is a valid out-pointer.
        cvt(unsafe { libc::tcgetattr(fd.as_raw_fd(), &raw mut saved) })?;
        let mut raw = saved;
        // SAFETY: `raw` is a valid `termios`.
        unsafe { libc::cfmakeraw(&raw mut raw) };
        // SAFETY: `fd` is open and `raw` is a valid `termios`.
        cvt(unsafe { libc::tcsetattr(fd.as_raw_fd(), libc::TCSAFLUSH, &raw const raw) })?;
        Ok(RawMode {
            fd: fd.as_raw_fd(),
            saved,
        })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // Now, not after the output has drained: a terminal that has stopped
        // reading (a window being closed, flow control) must not keep this
        // process from leaving.
        // SAFETY: the descriptor is the terminal this was made for, still
        // open (standard input outlives the guard) and `saved` is valid.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &raw const self.saved) };
    }
}

/// The write end of the pipe the signal handler uses.
static SIGNAL_PIPE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = SIGNAL_PIPE.load(std::sync::atomic::Ordering::Relaxed);
    if fd >= 0 {
        let byte = signal as u8;
        // SAFETY: `write` is async-signal-safe; one byte from this frame. A
        // full pipe drops the byte, which only means "already pending".
        unsafe { libc::write(fd, (&raw const byte).cast(), 1) };
    }
}

/// Makes each of `signals` readable as a byte (its number) from the returned
/// descriptor, so a `poll` loop can wait on them with everything else. At
/// most one such pipe exists per process: a second call replaces the first.
pub fn signal_pipe(signals: &[libc::c_int]) -> io::Result<OwnedFd> {
    let (read, write) = pipe()?;
    for end in [&read, &write] {
        set_nonblocking(end.as_raw_fd())?;
    }
    // The handler lives as long as the process, and so does its pipe.
    SIGNAL_PIPE.store(
        std::os::fd::IntoRawFd::into_raw_fd(write),
        std::sync::atomic::Ordering::Relaxed,
    );
    for &signal in signals {
        // SAFETY: an all-zero `sigaction` is valid.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_signal as extern "C" fn(libc::c_int) as usize;
        action.sa_flags = libc::SA_RESTART;
        // SAFETY: `action.sa_mask` is a valid out-pointer for the call.
        unsafe { libc::sigemptyset(&raw mut action.sa_mask) };
        // SAFETY: `action` is fully initialised; the handler is an
        // `extern "C"` function that only calls `write`.
        cvt(unsafe { libc::sigaction(signal, &raw const action, std::ptr::null_mut()) })?;
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    fn shell(script: &str) -> Proc {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .spawn()
            .unwrap();
        Proc::new(child).unwrap()
    }

    #[test]
    fn a_process_held_by_its_number_is_collected_once() {
        let mut proc = shell("exit 7");
        let pid = proc.id();
        let status = proc.wait().unwrap();
        assert_eq!(status.code(), Some(7));
        // The status stays; nothing is signalled to a number that may be
        // somebody else's by now, and the number is still the same.
        assert_eq!(proc.wait().unwrap(), status);
        assert_eq!(proc.try_wait().unwrap(), Some(status));
        assert!(proc.kill().is_ok());
        assert_eq!(proc.id(), pid);
    }

    #[test]
    fn a_process_that_runs_is_killed_and_collected_by_its_number() {
        let mut proc = shell("exec sleep 30");
        assert_eq!(proc.try_wait().unwrap(), None);
        proc.kill().unwrap();
        let status = proc.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert_eq!(proc.try_wait().unwrap(), Some(status));

        // A child known by its number only (the way a renewed warden has
        // its shell) is collected the same way.
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .spawn()
            .unwrap();
        let mut again = Proc::from_pid(child.id()).unwrap();
        drop(child);
        assert_eq!(again.wait().unwrap().code(), Some(3));
    }

    #[test]
    fn a_number_that_addresses_a_group_is_not_a_process() {
        assert!(Proc::from_pid(0).is_err());
        assert!(Proc::from_pid(u32::MAX).is_err());
    }

    #[test]
    fn descriptors_cross_a_socket_with_their_first_byte() {
        let (a, b) = UnixStream::pair().unwrap();
        let (x, y) = UnixStream::pair().unwrap();
        let sent = send_with_fds(a.as_fd(), b"hello", &[x.as_raw_fd(), y.as_raw_fd()]).unwrap();
        assert_eq!(sent, 5);
        let mut buf = [0u8; 16];
        let mut fds = Vec::new();
        let got = recv_with_fds(b.as_fd(), &mut buf, &mut fds).unwrap();
        assert_eq!(&buf[..got], b"hello");
        assert_eq!(fds.len(), 2);
        // Close-on-exec on every descriptor that arrived.
        for fd in &fds {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert!(flags & libc::FD_CLOEXEC != 0);
        }
        // The copies are the same sockets: what one writes the other reads.
        drop((x, y));
        let second = UnixStream::from(fds.remove(1));
        let first = UnixStream::from(fds.remove(0));
        use std::io::{Read, Write};
        (&second).write_all(b"x").unwrap();
        let mut one = [0u8];
        (&first).read_exact(&mut one).unwrap();
        assert_eq!(&one, b"x");
    }

    #[test]
    fn only_the_same_user_is_let_in() {
        assert!(uid_allowed(501, 501));
        assert!(!uid_allowed(0, 501));
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(a.as_fd()).unwrap(), effective_uid());
    }

    #[test]
    fn a_child_exit_makes_its_watch_readable() {
        let child = std::process::Command::new("sleep")
            .arg("0.2")
            .spawn()
            .unwrap();
        let mut child = child;
        let watch = exit_watch(child.id()).unwrap();
        let mut set = [want(watch.as_raw_fd(), libc::POLLIN)];
        let ready = poll(&mut set, Some(Duration::from_secs(10))).unwrap();
        assert_eq!(ready, 1);
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn a_descriptor_can_be_made_to_survive_an_exec_and_made_not_to() {
        let (read, write) = pipe().unwrap();
        let flags = |fd: &OwnedFd| unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
        assert!(flags(&read) & libc::FD_CLOEXEC != 0);
        assert!(flags(&write) & libc::FD_CLOEXEC != 0);
        clear_cloexec(read.as_raw_fd()).unwrap();
        assert_eq!(flags(&read) & libc::FD_CLOEXEC, 0);
        set_cloexec(read.as_raw_fd()).unwrap();
        assert!(flags(&read) & libc::FD_CLOEXEC != 0);
        assert!(is_open(read.as_raw_fd()));
        let number = write.as_raw_fd();
        drop(write);
        assert!(!is_open(number));
    }

    #[test]
    fn a_program_that_cannot_be_run_leaves_the_process_as_it_was() {
        let disposition = || {
            // SAFETY: an all-zero `sigaction` is valid; the call only reads
            // the current one into it.
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            // SAFETY: a null new action asks only for the old one.
            unsafe { libc::sigaction(libc::SIGPIPE, std::ptr::null(), &raw mut action) };
            action.sa_sigaction
        };
        let before = disposition();
        let why = exec(
            Path::new("/nonexistent/kablo"),
            &[OsString::from("kablo"), OsString::from("warden")],
        );
        assert_eq!(why.kind(), io::ErrorKind::NotFound);
        assert_eq!(disposition(), before, "SIGPIPE is as it was");
        let nul = exec(Path::new("/bin/sh"), &[OsString::from("a\0b")]);
        assert_eq!(nul.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn a_poll_deadline_rounds_up() {
        let (a, _b) = UnixStream::pair().unwrap();
        let mut set = [want(a.as_raw_fd(), libc::POLLIN)];
        let start = std::time::Instant::now();
        assert_eq!(
            poll(&mut set, Some(Duration::from_micros(1500))).unwrap(),
            0
        );
        assert!(start.elapsed() >= Duration::from_millis(2));
    }
}
