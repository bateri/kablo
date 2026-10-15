//! `kablo connect`: a session on this terminal.
//!
//! The small client for a person, the way `dtach` or `abduco` is one: the
//! terminal goes raw, what is typed goes to the session as it is and what the
//! session writes goes to the screen as it is, and the control key
//! [`DETACH_KEY`] leaves the session running and gives the terminal back.
//! Resizing the window resizes the session's terminal.
//!
//! It attaches from the first byte the journal still holds, so a window that
//! was closed and comes back is filled again with what it missed. When the
//! journal no longer reaches back to the start, the session sends a picture of
//! the screen in its place: the terminal is reset and the picture is written to
//! it, then the program's own output goes on from where the picture ends.
//! `--fresh` attaches at the end instead and shows only what comes next.
//!
//! The connection is a [`Link`], so a relay that is replaced while a person is
//! connected does not show. Neither does a moment of crowding: when the link
//! has too many requests waiting for the session, typing and resizes are held
//! here and offered again after a short, growing pause ([`Held`]), and only a
//! session that stays unable to take them for [`BUSY_PATIENCE`] ends the
//! connection. A key the session refuses because its program is not reading
//! rings the terminal's bell and the connection stays: it is the person's
//! only way to say they have no more patience, and the way out (the detach
//! key) must go on working.

use std::collections::VecDeque;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::path::Path;
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{ErrorCode, Exit, Message, OFFSET_END, Packet, View};

use crate::link::{self, Link};
use crate::sys;

/// The byte that leaves the session: Ctrl-\.
pub const DETACH_KEY: u8 = 0x1c;

/// Resets the terminal (`RIS`) before a picture of the screen is written to it.
const RESET: &[u8] = b"\x1bc";

/// The exit status of a command that is used the wrong way (`EX_USAGE`).
const EXIT_USAGE: u8 = 64;

/// The pause before a request the link had no room for is offered again.
const BUSY_FIRST: Duration = Duration::from_millis(20);

/// The longest pause between two offers; the pause doubles up to it.
const BUSY_LONGEST: Duration = Duration::from_millis(320);

/// How long requests may wait for room, with the link taking none of them,
/// before the connection ends: a session that does not make room in this long
/// is stuck, not busy.
const BUSY_PATIENCE: Duration = Duration::from_secs(5);

/// Bytes of typing held while the link is full. Past it the terminal is no
/// longer read, so the rest waits in the terminal's own buffer and the
/// detach key, which is read from it, is not lost for longer than the
/// patience.
const HELD_LIMIT: usize = 1 << 20;

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
    let mut held = Held::default();
    let now = Instant::now();
    let offset = if fresh { OFFSET_END } else { 0 };
    // The terminal's size goes with the attach, so that a picture of the screen
    // is made for it; the resize after it is for a session that predates views.
    let view = sys::window_size(input)
        .ok()
        .filter(|size| size.cols > 0 && size.rows > 0)
        .map(|size| View {
            size,
            picture: false,
            picture_if_answered: false,
        });
    link.send(
        Packet::about(id.clone(), Message::Attach { offset, view }),
        now,
        &mut outgoing,
    );
    resize(&mut held, &mut link, &id, input, now, &mut outgoing);
    let mut buf = vec![0u8; 4096];
    let mut picturing = false;
    loop {
        if let Some(end) = show(&outgoing, output, &mut picturing) {
            return end;
        }
        outgoing.clear();
        if link.is_gone() {
            return End::Failed("the session is gone".to_owned());
        }
        let typing = if held.has_room() { libc::POLLIN } else { 0 };
        let mut set = vec![
            sys::want(input.as_raw_fd(), typing),
            sys::want(signals.as_raw_fd(), libc::POLLIN),
        ];
        let linked = link.pollfd(true);
        set.extend(linked);
        let wait = match (link.deadline(), held.deadline()) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
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
                    libc::SIGWINCH => resize(&mut held, &mut link, &id, input, now, &mut outgoing),
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
                        let keys = Packet::about(id.clone(), Message::Input(keys.to_vec()));
                        held.send(keys, now, |packet| link.offer(packet, now, &mut outgoing));
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
        if held.retry(now, |packet| link.offer(packet, now, &mut outgoing)) == Retry::Stuck {
            return End::Failed(link::BUSY.to_owned());
        }
    }
}

/// Tells the session the terminal's size.
fn resize(
    held: &mut Held,
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
        let resize = Packet::about(id.clone(), Message::Resize(size));
        held.send(resize, now, |packet| link.offer(packet, now, outgoing));
    }
}

/// How a try at the requests that are held went.
#[derive(Debug, PartialEq, Eq)]
enum Retry {
    /// Nothing is held, or it is not time to offer again yet.
    Idle,
    /// Requests are still held, and the patience is not out.
    Waiting,
    /// The link has taken none of them for [`BUSY_PATIENCE`].
    Stuck,
}

/// Requests the link had no room for, kept in the order they were made and
/// offered again with a growing pause. A key or a resize that finds the link
/// full is not lost and does not end the connection, as the link's own answer
/// to it would.
#[derive(Default)]
struct Held {
    packets: VecDeque<Packet>,
    /// The typed bytes among `packets`.
    bytes: usize,
    /// Set while something is held.
    stall: Option<Stall>,
}

struct Stall {
    /// When requests last began to wait with nothing taken: the patience runs
    /// from here, and any request the link takes starts it again.
    since: Instant,
    retry_at: Instant,
    pause: Duration,
}

impl Held {
    /// Whether more typing may be read from the terminal.
    fn has_room(&self) -> bool {
        self.bytes < HELD_LIMIT
    }

    /// When to offer again, while something is held.
    fn deadline(&self) -> Option<Instant> {
        self.stall.as_ref().map(|stall| stall.retry_at)
    }

    /// Offers `packet` through `offer`, which hands back what it has no room
    /// for. Behind requests that are held it waits its turn: bytes typed
    /// must not overtake bytes typed before them.
    fn send(
        &mut self,
        packet: Packet,
        now: Instant,
        mut offer: impl FnMut(Packet) -> Option<Packet>,
    ) {
        if self.packets.is_empty() {
            if let Some(refused) = offer(packet) {
                self.hold(refused, now);
            }
        } else {
            self.hold(packet, now);
        }
    }

    fn hold(&mut self, packet: Packet, now: Instant) {
        match &packet.message {
            Message::Input(bytes) => self.bytes += bytes.len(),
            // Only the latest size matters.
            Message::Resize(_) => self
                .packets
                .retain(|held| !matches!(held.message, Message::Resize(_))),
            _ => {}
        }
        self.packets.push_back(packet);
        if self.stall.is_none() {
            self.stall = Some(Stall {
                since: now,
                retry_at: now + BUSY_FIRST,
                pause: BUSY_FIRST,
            });
        }
    }

    /// Offers what is held, oldest first, when it is time; stops at the first
    /// request that is refused again.
    fn retry(&mut self, now: Instant, mut offer: impl FnMut(Packet) -> Option<Packet>) -> Retry {
        let Some(stall) = self.stall.as_mut() else {
            return Retry::Idle;
        };
        if now < stall.retry_at {
            return Retry::Waiting;
        }
        let mut taken = false;
        while let Some(packet) = self.packets.pop_front() {
            if let Message::Input(bytes) = &packet.message {
                self.bytes = self.bytes.saturating_sub(bytes.len());
            }
            if let Some(refused) = offer(packet) {
                if let Message::Input(bytes) = &refused.message {
                    self.bytes += bytes.len();
                }
                self.packets.push_front(refused);
                break;
            }
            taken = true;
        }
        if self.packets.is_empty() {
            self.stall = None;
            return Retry::Idle;
        }
        if taken {
            stall.since = now;
            stall.pause = BUSY_FIRST;
        } else if now.saturating_duration_since(stall.since) >= BUSY_PATIENCE {
            return Retry::Stuck;
        } else {
            stall.pause = (stall.pause * 2).min(BUSY_LONGEST);
        }
        stall.retry_at = now + stall.pause;
        Retry::Waiting
    }
}

/// Puts what the session sent on the screen. `Some` when it says the
/// connection is over.
fn show(packets: &[Packet], output: BorrowedFd<'_>, picturing: &mut bool) -> Option<End> {
    for packet in packets {
        match &packet.message {
            Message::Output { data, .. } => {
                if write_all(output, data).is_err() {
                    return Some(End::Stopped);
                }
            }
            // A new `attached` starts everything over, so a picture that was
            // coming is not finished by the one that follows.
            Message::Attached { .. } => *picturing = false,
            Message::Picture { more, data, .. } => {
                // The picture builds the screen on a terminal that has nothing
                // on it: this one is reset first, once per picture.
                let begun = if *picturing {
                    Ok(())
                } else {
                    write_all(output, RESET)
                };
                *picturing = *more;
                if begun.and_then(|()| write_all(output, data)).is_err() {
                    return Some(End::Stopped);
                }
            }
            Message::Ended(exit) => return Some(End::Ended(*exit)),
            // The program is not reading what was typed: say so with the bell
            // and let the person decide what to do.
            Message::Error { code, .. } if *code == ErrorCode::INPUT_FULL => {
                if write_all(output, b"\x07").is_err() {
                    return Some(End::Stopped);
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use kablo_proto::v1::Size;

    fn id() -> SessionId {
        SessionId::new("work").unwrap()
    }

    fn typed(text: &str) -> Packet {
        Packet::about(id(), Message::Input(text.as_bytes().to_vec()))
    }

    fn resized(cols: u16) -> Packet {
        Packet::about(
            id(),
            Message::Resize(Size {
                cols,
                rows: 24,
                xpixel: 0,
                ypixel: 0,
            }),
        )
    }

    /// A link that takes `room` more requests (`None`: any number) and keeps
    /// them, handing back the rest.
    struct Taker {
        room: Option<usize>,
        taken: Vec<Packet>,
    }

    impl Taker {
        fn new(room: Option<usize>) -> Taker {
            Taker {
                room,
                taken: Vec::new(),
            }
        }

        fn offer(&mut self, packet: Packet) -> Option<Packet> {
            match &mut self.room {
                Some(0) => Some(packet),
                room => {
                    if let Some(left) = room {
                        *left -= 1;
                    }
                    self.taken.push(packet);
                    None
                }
            }
        }
    }

    #[test]
    fn what_the_link_takes_is_not_held() {
        let mut held = Held::default();
        let mut link = Taker::new(None);
        let now = Instant::now();
        held.send(typed("a"), now, |p| link.offer(p));
        assert_eq!(link.taken, [typed("a")]);
        assert_eq!(held.deadline(), None);
        assert_eq!(held.retry(now, |p| link.offer(p)), Retry::Idle);
    }

    #[test]
    fn what_the_link_has_no_room_for_is_offered_again_in_order() {
        let mut held = Held::default();
        let mut link = Taker::new(Some(0));
        let start = Instant::now();
        for text in ["a", "b", "c"] {
            held.send(typed(text), start, |p| link.offer(p));
        }
        assert!(link.taken.is_empty());
        assert_eq!(held.deadline(), Some(start + BUSY_FIRST));

        // Not yet time: nothing is offered.
        link.room = None;
        assert_eq!(held.retry(start, |p| link.offer(p)), Retry::Waiting);
        assert!(link.taken.is_empty());

        assert_eq!(
            held.retry(start + BUSY_FIRST, |p| link.offer(p)),
            Retry::Idle
        );
        assert_eq!(link.taken, [typed("a"), typed("b"), typed("c")]);
        assert_eq!(held.deadline(), None);
        assert!(held.has_room());
    }

    #[test]
    fn typing_does_not_overtake_what_is_held() {
        let mut held = Held::default();
        let mut link = Taker::new(Some(0));
        let start = Instant::now();
        held.send(typed("first"), start, |p| link.offer(p));
        // Room again, but "first" is still waiting for its turn.
        link.room = None;
        held.send(typed("second"), start, |p| link.offer(p));
        assert!(link.taken.is_empty());
        held.retry(start + BUSY_FIRST, |p| link.offer(p));
        assert_eq!(link.taken, [typed("first"), typed("second")]);
    }

    #[test]
    fn only_the_latest_size_is_held() {
        let mut held = Held::default();
        let mut link = Taker::new(Some(0));
        let start = Instant::now();
        held.send(resized(80), start, |p| link.offer(p));
        held.send(typed("a"), start, |p| link.offer(p));
        held.send(resized(100), start, |p| link.offer(p));
        link.room = None;
        held.retry(start + BUSY_FIRST, |p| link.offer(p));
        assert_eq!(link.taken, [typed("a"), resized(100)]);
    }

    #[test]
    fn typing_is_read_only_while_there_is_room_to_hold_it() {
        let mut held = Held::default();
        let mut link = Taker::new(Some(0));
        let start = Instant::now();
        held.send(typed(&"x".repeat(HELD_LIMIT - 1)), start, |p| link.offer(p));
        assert!(held.has_room());
        held.send(typed("yy"), start, |p| link.offer(p));
        assert!(!held.has_room());
        link.room = None;
        held.retry(start + BUSY_FIRST, |p| link.offer(p));
        assert!(held.has_room());
    }

    #[test]
    fn the_pause_doubles_to_a_limit_and_the_patience_ends_it() {
        let mut held = Held::default();
        let mut link = Taker::new(Some(0));
        let start = Instant::now();
        held.send(typed("a"), start, |p| link.offer(p));
        let mut pauses = Vec::new();
        let mut now = start;
        let stuck_at = loop {
            let due = held.deadline().unwrap();
            pauses.push(due.duration_since(now));
            now = due;
            if held.retry(now, |p| link.offer(p)) == Retry::Stuck {
                break now.duration_since(start);
            }
            assert!(
                now.duration_since(start) < BUSY_PATIENCE + BUSY_LONGEST,
                "never gave up"
            );
        };
        let ms = |n| Duration::from_millis(n);
        assert_eq!(
            pauses[..6],
            [ms(20), ms(40), ms(80), ms(160), ms(320), ms(320)]
        );
        assert!(pauses.iter().all(|pause| *pause <= BUSY_LONGEST));
        // It gives up at the first attempt past the patience, no sooner.
        assert!(stuck_at >= BUSY_PATIENCE, "{stuck_at:?}");
        assert!(stuck_at < BUSY_PATIENCE + BUSY_LONGEST, "{stuck_at:?}");
        assert!(link.taken.is_empty());
    }

    #[test]
    fn any_progress_starts_the_patience_again() {
        let mut held = Held::default();
        let mut link = Taker::new(Some(0));
        let start = Instant::now();
        for text in ["a", "b"] {
            held.send(typed(text), start, |p| link.offer(p));
        }
        // Four and a half seconds of refusals, then one request goes through.
        let almost = start + BUSY_PATIENCE - Duration::from_millis(500);
        assert_eq!(held.retry(almost, |p| link.offer(p)), Retry::Waiting);
        link.room = Some(1);
        assert_eq!(
            held.retry(almost + BUSY_LONGEST, |p| link.offer(p)),
            Retry::Waiting
        );
        assert_eq!(link.taken, [typed("a")]);
        // Past the original patience, but not past the new one.
        link.room = Some(0);
        let later = start + BUSY_PATIENCE + Duration::from_secs(1);
        assert_eq!(held.retry(later, |p| link.offer(p)), Retry::Waiting);
    }

    /// What `show` wrote to the terminal for these packets.
    fn shown(packets: &[Packet], picturing: &mut bool) -> Vec<u8> {
        use std::io::Read;
        use std::os::fd::AsFd;
        let (ours, mut theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(show(packets, ours.as_fd(), picturing).is_none());
        drop(ours);
        let mut out = Vec::new();
        theirs.read_to_end(&mut out).unwrap();
        out
    }

    fn piece(more: bool, data: &str) -> Packet {
        Packet::about(
            id(),
            Message::Picture {
                offset: 100,
                cols: 80,
                rows: 24,
                more,
                data: data.as_bytes().to_vec(),
            },
        )
    }

    fn attached() -> Packet {
        Packet::about(
            id(),
            Message::Attached {
                offset: 100,
                truncated: true,
                answered: 0,
            },
        )
    }

    #[test]
    fn a_picture_resets_the_terminal_once_and_then_is_written_as_it_is() {
        let mut picturing = false;
        let out = shown(
            &[attached(), piece(true, "one "), piece(false, "two")],
            &mut picturing,
        );
        assert_eq!(out, b"\x1bcone two");
        assert!(!picturing, "the last piece ends it");
    }

    #[test]
    fn a_picture_cut_short_is_begun_again_by_the_next_attached() {
        let mut picturing = false;
        let first = shown(&[attached(), piece(true, "half")], &mut picturing);
        assert_eq!(first, b"\x1bchalf");
        assert!(picturing);
        // The connection was replaced: the session starts the picture over.
        let again = shown(&[attached(), piece(false, "whole")], &mut picturing);
        assert_eq!(again, b"\x1bcwhole");
    }

    #[test]
    fn output_after_a_picture_is_not_a_reset() {
        let mut picturing = false;
        let out = shown(
            &[
                piece(false, "screen"),
                Packet::about(
                    id(),
                    Message::Output {
                        offset: 100,
                        data: b"live".to_vec(),
                    },
                ),
                piece(false, "again"),
            ],
            &mut picturing,
        );
        assert_eq!(out, b"\x1bcscreenlive\x1bcagain");
    }
}
