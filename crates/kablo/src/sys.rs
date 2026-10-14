//! The libc calls the warden and the relay share, each behind a small safe
//! function.
//!
//! Everything `unsafe` in the process code is here or in `journal`, `pty` and
//! `procinfo`, so the loops that use them read as plain Rust. The functions
//! keep to what a loop needs: descriptor flags, `poll`, passing descriptors
//! over a Unix socket, the terminal's size, signals.

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
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

/// Puts `fd` in non-blocking mode.
pub fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: `F_GETFL`/`F_SETFL` change only the open file's flags.
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    // SAFETY: as above.
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }).map(drop)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

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
