//! `kablo connect`: a session on this terminal.
//!
//! The small client for a person, the way `dtach` or `abduco` is one: the
//! terminal goes raw, what is typed goes to the session as it is and what the
//! session writes goes to the screen as it is, and the control key
//! [`DETACH_KEY`] leaves the session running and gives the terminal back.
//! Resizing the window resizes the session's terminal.
//!
//! It attaches from the first byte the journal still holds, so a window that
//! was closed and comes back is filled again with what it missed (kablo keeps
//! the bytes, not a picture of the screen, so a full-screen program is drawn
//! again from the start of the journal and the last frame wins); `--fresh`
//! attaches at the end instead and shows only what comes next.
//!
//! The connection is a [`Link`], so a relay that is replaced while a person is
//! connected does not show.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::time::Instant;

use kablo_proto::SessionId;
use kablo_proto::v1::{Exit, Message, OFFSET_END, Packet};

use crate::link::Link;
use crate::sys;

/// The byte that leaves the session: Ctrl-\.
pub const DETACH_KEY: u8 = 0x1c;

/// The exit status of a command that is used the wrong way (`EX_USAGE`).
const EXIT_USAGE: u8 = 64;

/// How a connection ended.
enum End {
    /// The person detached.
    Detached,
    /// The program ended.
    Ended(Exit),
    /// The terminal was closed or the process was told to stop.
    Stopped,
    /// Something went wrong, in words for the person.
    Failed(String),
}

/// Connects the terminal to session `id` in `dir`. Returns the exit status:
/// the program's own when it ends, 0 for a detach.
pub fn run(dir: &Path, id: SessionId, fresh: bool, err: &mut dyn Write) -> u8 {
    // SAFETY: descriptors 0 and 1 are open for the life of the process.
    let (input, output) = unsafe { (BorrowedFd::borrow_raw(0), BorrowedFd::borrow_raw(1)) };
    if !sys::is_tty(input) || !sys::is_tty(output) {
        let _ = writeln!(
            err,
            "kablo: connect needs a terminal on standard input and output"
        );
        return EXIT_USAGE;
    }
    let start = Instant::now();
    let link = match Link::open(dir, id.clone(), start) {
        Ok(link) => link,
        Err(packet) => {
            let _ = writeln!(err, "kablo: {}", describe(&packet));
            return 1;
        }
    };
    let signals = match sys::signal_pipe(&[libc::SIGWINCH, libc::SIGTERM, libc::SIGHUP]) {
        Ok(fd) => fd,
        Err(e) => {
            let _ = writeln!(err, "kablo: cannot watch for signals: {e}");
            return 1;
        }
    };
    // From here the terminal is raw, and the guard gives it back on every way
    // out of this block.
    let end = {
        let _raw = match sys::RawMode::enter(input) {
            Ok(raw) => raw,
            Err(e) => {
                let _ = writeln!(err, "kablo: cannot set up the terminal: {e}");
                return 1;
            }
        };
        serve(link, id, fresh, input, output, &signals)
    };
    match end {
        End::Detached => {
            let _ = writeln!(err, "\r\nkablo: detached; the session keeps running");
            0
        }
        End::Ended(exit) => {
            let (text, code) = match exit {
                Exit::Code(code) => (format!("exit status {code}"), (code & 0xff) as u8),
                Exit::Signal(signal) => (format!("signal {signal}"), (128 + signal) as u8),
            };
            let _ = writeln!(err, "\r\nkablo: the session ended ({text})");
            code
        }
        End::Stopped => 1,
        End::Failed(why) => {
            let _ = writeln!(err, "\r\nkablo: {why}");
            1
        }
    }
}

fn serve(
    mut link: Link,
    id: SessionId,
    fresh: bool,
    input: BorrowedFd<'_>,
    output: BorrowedFd<'_>,
    signals: &std::os::fd::OwnedFd,
) -> End {
    let mut outgoing = Vec::new();
    let now = Instant::now();
    let offset = if fresh { OFFSET_END } else { 0 };
    link.send(
        Packet::about(id.clone(), Message::Attach { offset }),
        now,
        &mut outgoing,
    );
    resize(&mut link, &id, input, now, &mut outgoing);
    let mut buf = vec![0u8; 4096];
    loop {
        if let Some(end) = show(&outgoing, output) {
            return end;
        }
        outgoing.clear();
        if link.is_gone() {
            return End::Failed("the session is gone".to_owned());
        }
        let mut set = vec![
            sys::want(input.as_raw_fd(), libc::POLLIN),
            sys::want(signals.as_raw_fd(), libc::POLLIN),
        ];
        let linked = link.pollfd(true);
        set.extend(linked);
        let wait = link
            .deadline()
            .map(|at| at.saturating_duration_since(Instant::now()));
        if sys::poll(&mut set, wait).is_err() {
            return End::Failed("cannot wait for input".to_owned());
        }
        let now = Instant::now();
        let ready = |index: usize| set.get(index).map_or(0, |fd| fd.revents);
        if ready(1) != 0 {
            let mut byte = [0u8; 16];
            let got = sys::read_fd(signals.as_fd(), &mut byte).unwrap_or(0);
            for &signal in byte.get(..got).unwrap_or_default() {
                match libc::c_int::from(signal) {
                    libc::SIGWINCH => resize(&mut link, &id, input, now, &mut outgoing),
                    _ => return End::Stopped,
                }
            }
        }
        if ready(0) != 0 {
            match sys::read_fd(input, &mut buf) {
                Ok(0) | Err(_) => return End::Stopped,
                Ok(n) => {
                    let typed = buf.get(..n).unwrap_or_default();
                    let (keys, detach) = match typed.iter().position(|&b| b == DETACH_KEY) {
                        Some(at) => (typed.get(..at).unwrap_or_default(), true),
                        None => (typed, false),
                    };
                    if !keys.is_empty() {
                        let input = Message::Input(keys.to_vec());
                        link.send(Packet::about(id.clone(), input), now, &mut outgoing);
                    }
                    if detach {
                        return End::Detached;
                    }
                }
            }
        }
        if set.len() > 2 && ready(2) != 0 {
            link.ready(ready(2), now, &mut outgoing);
        }
        link.tick(now, &mut outgoing);
    }
}

/// Tells the session the terminal's size.
fn resize(
    link: &mut Link,
    id: &SessionId,
    input: BorrowedFd<'_>,
    now: Instant,
    outgoing: &mut Vec<Packet>,
) {
    if let Ok(size) = sys::window_size(input)
        && size.cols > 0
        && size.rows > 0
    {
        link.send(
            Packet::about(id.clone(), Message::Resize(size)),
            now,
            outgoing,
        );
    }
}

/// Puts what the session sent on the screen. `Some` when it says the
/// connection is over.
fn show(packets: &[Packet], output: BorrowedFd<'_>) -> Option<End> {
    for packet in packets {
        match &packet.message {
            Message::Output { data, .. } => {
                if write_all(output, data).is_err() {
                    return Some(End::Stopped);
                }
            }
            Message::Ended(exit) => return Some(End::Ended(*exit)),
            Message::Error { .. } => return Some(End::Failed(describe(packet))),
            _ => {}
        }
    }
    None
}

/// What an `error` packet says, for a person.
fn describe(packet: &Packet) -> String {
    match &packet.message {
        Message::Error { text, .. } => crate::listing::printable(text.as_bytes()),
        _ => "unexpected answer".to_owned(),
    }
}

fn write_all(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> std::io::Result<()> {
    while !bytes.is_empty() {
        match sys::write_fd(fd, bytes) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let mut set = [sys::want(fd.as_raw_fd(), libc::POLLOUT)];
                sys::poll(&mut set, None)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}
