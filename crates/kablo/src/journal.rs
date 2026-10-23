//! The journal: everything the program has written, in a ring of shared memory.
//!
//! The ring lives in an anonymous shared mapping (`memfd_create` on Linux, an
//! unlinked `shm_open` object on macOS) that the warden creates and hands to
//! each relay as a descriptor, so it outlives any relay. Its first 64 bytes
//! are a header; the rest is the ring:
//!
//! ```text
//!   0   magic     b"KABLOJNL"
//!   8   version   u32 (1), little-endian (it is read by the machine that wrote it)
//!  12   reserved
//!  16   capacity u64: the ring's size in bytes
//!  24   total    u64: bytes ever appended (an atomic; the commit point)
//!  32   history  u32: scrollback rows the relay's screen keeps, plus one (0: not said)
//!  40   answered u64: the offset up to which the terminal's questions were answered,
//!                plus one (0: nobody keeps track)
//!  48   floor    u64: the offset a client was told it may not go back past (0: none),
//!                see below
//!  56   reserved up to 64
//!  64   the ring: byte `n` of the stream is at `64 + n % capacity`
//! ```
//!
//! The stream's offset of a byte is its count since the session began, so an
//! offset names the same byte for the life of the session and a client that
//! says "I have everything below N" is understood by whichever relay serves
//! it. `total` lives in the mapping and not in a process, which is what lets a
//! relay die and the next one go on from the same number.
//!
//! **One writer at a time**, handed over by the owner protocol of the inner
//! wire. The writer fills the ring and only then publishes the new `total`
//! (release); a reader copies and re-reads `total` afterwards (acquire) to see
//! that the bytes it copied have not been overwritten in the meantime. The
//! layout is part of what a frozen warden hands to a newer relay, so it is
//! versioned and a header that does not match is refused. A field that was
//! reserved is read as zero by a journal an older warden made, and zero is what
//! every such field says "not said" with.
//!
//! **Two starts.** The ring holds the last `capacity` bytes ([`Journal::ring_from`]), and a
//! client may ask to forget the past (`forget`): the journal then keeps a *floor*, the
//! offset it was asked at, and [`Journal::retained_from`] is the later of the two. The floor
//! decides what may be replayed to a client that comes (back): one that wants bytes before it
//! is told they are gone and gets a picture of the screen. It does not take bytes away from
//! a client that is attached and is owed some: they are in the ring, and what a client was
//! owed it gets, so the one that asks to forget is not dropped for the output that was on its
//! way. The floor lives in the header so that a relay that comes after (a restart, an
//! upgrade) forgets the same past; a journal an older warden made reads it as zero.
//!
//! The header is also where a session's settings live that every relay of it
//! needs and no relay can be told by anyone else (the offset the terminal's
//! questions were answered up to is such a thing: what the warden reads while no
//! relay does, the next relay answers): the warden is frozen and a
//! relay may be replaced by a newer one, so what the person asked for at the
//! start (the scrollback of the screen) is written once, here, by whoever makes
//! the journal.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::sys;

const MAGIC: [u8; 8] = *b"KABLOJNL";
const VERSION: u32 = 1;
const HEADER_LEN: usize = 64;
const OFFSET_VERSION: usize = 8;
const OFFSET_CAPACITY: usize = 16;
const OFFSET_TOTAL: usize = 24;
const OFFSET_HISTORY: usize = 32;
const OFFSET_ANSWERED: usize = 40;
const OFFSET_FLOOR: usize = 48;

/// The smallest ring a session may ask for.
pub const MIN_CAPACITY: u64 = 16 * 1024;
/// The largest ring a session may ask for.
pub const MAX_CAPACITY: u64 = 1 << 30;
/// The ring of a session that does not ask for one: 8 MiB.
pub const DEFAULT_CAPACITY: u64 = 8 << 20;

/// A mapped journal.
#[derive(Debug)]
pub struct Journal {
    fd: OwnedFd,
    base: NonNull<u8>,
    map_len: usize,
    capacity: u64,
}

impl Journal {
    /// A new, empty journal of `capacity` bytes
    /// (`MIN_CAPACITY..=MAX_CAPACITY`).
    pub fn create(capacity: u64) -> io::Result<Journal> {
        if !(MIN_CAPACITY..=MAX_CAPACITY).contains(&capacity) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the journal size is outside 16 KiB to 1 GiB",
            ));
        }
        let fd = anonymous_memory()?;
        let wanted = HEADER_LEN as u64 + capacity;
        let map_len = usize::try_from(wanted).map_err(io::Error::other)?;
        // SAFETY: `fd` is a fresh object nobody else has mapped.
        sys::cvt(unsafe { libc::ftruncate(fd.as_raw_fd(), wanted as libc::off_t) })?;
        let base = map(fd.as_fd(), map_len)?;
        let journal = Journal {
            fd,
            base,
            map_len,
            capacity,
        };
        // SAFETY: the mapping is `map_len` bytes, the header is inside it, and
        // nobody else has the descriptor yet.
        unsafe {
            std::ptr::copy_nonoverlapping(MAGIC.as_ptr(), journal.base.as_ptr(), MAGIC.len());
            journal
                .base
                .as_ptr()
                .add(OFFSET_VERSION)
                .cast::<u32>()
                .write_unaligned(VERSION);
            journal
                .base
                .as_ptr()
                .add(OFFSET_CAPACITY)
                .cast::<u64>()
                .write_unaligned(capacity);
        }
        journal.total_cell().store(0, Ordering::Release);
        // Nobody has answered anything yet, and this journal says so.
        journal.answered_cell().store(1, Ordering::Release);
        Ok(journal)
    }

    /// Maps a journal another process created, after checking its header.
    pub fn open(fd: OwnedFd) -> io::Result<Journal> {
        // SAFETY: an all-zero `stat` is valid; the kernel fills it.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is open for the call and `stat` a valid out-pointer.
        sys::cvt(unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) })?;
        let map_len = usize::try_from(stat.st_size).map_err(io::Error::other)?;
        if map_len < HEADER_LEN {
            return Err(bad("the journal is smaller than its header"));
        }
        let base = map(fd.as_fd(), map_len)?;
        let mut journal = Journal {
            fd,
            base,
            map_len,
            capacity: 0,
        };
        // SAFETY: the mapping is `map_len >= HEADER_LEN` bytes.
        let (magic, version, capacity) = unsafe {
            let mut magic = [0u8; 8];
            std::ptr::copy_nonoverlapping(journal.base.as_ptr(), magic.as_mut_ptr(), 8);
            (
                magic,
                journal
                    .base
                    .as_ptr()
                    .add(OFFSET_VERSION)
                    .cast::<u32>()
                    .read_unaligned(),
                journal
                    .base
                    .as_ptr()
                    .add(OFFSET_CAPACITY)
                    .cast::<u64>()
                    .read_unaligned(),
            )
        };
        if magic != MAGIC {
            return Err(bad("not a journal"));
        }
        if version != VERSION {
            return Err(bad("a journal of an unknown version"));
        }
        let fits = (MIN_CAPACITY..=MAX_CAPACITY).contains(&capacity)
            && (HEADER_LEN as u64).saturating_add(capacity) <= map_len as u64;
        if !fits {
            return Err(bad("a journal whose ring does not fit its memory"));
        }
        journal.capacity = capacity;
        Ok(journal)
    }

    /// The descriptor, for handing the journal to another process.
    pub fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The ring's size in bytes.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// The rows of scrollback the session was started with, `None` when it
    /// was not asked for any number (including a journal an older warden made).
    pub fn history(&self) -> Option<u32> {
        // SAFETY: the header is inside the mapping and the offset is 4-byte
        // aligned within a page-aligned base; a value that nobody writes
        // concurrently after `set_history`.
        let stored = unsafe {
            self.base
                .as_ptr()
                .add(OFFSET_HISTORY)
                .cast::<u32>()
                .read_unaligned()
        };
        stored.checked_sub(1)
    }

    /// The offset up to which the terminal's questions have been answered (or
    /// need no answer): what a relay that takes over must answer starts here.
    /// `None` for a journal that does not keep track (one an older warden made),
    /// of which nothing is answered late.
    pub fn answered(&self) -> Option<u64> {
        self.answered_cell().load(Ordering::Acquire).checked_sub(1)
    }

    /// Records that the questions up to `offset` are answered (the single writer
    /// that reads the terminal).
    pub fn set_answered(&mut self, offset: u64) {
        self.answered_cell()
            .store(offset.saturating_add(1), Ordering::Release);
    }

    /// Makes the journal one that keeps no track of answers, as one an older
    /// warden made (for a test).
    #[cfg(test)]
    pub fn forget_answers(&mut self) {
        self.answered_cell().store(0, Ordering::Release);
    }

    /// Records the scrollback rows the session was started with (the single
    /// writer, before any relay reads it).
    pub fn set_history(&mut self, rows: u32) {
        // SAFETY: as in `history`; `&mut self` makes this the only writer.
        unsafe {
            self.base
                .as_ptr()
                .add(OFFSET_HISTORY)
                .cast::<u32>()
                .write_unaligned(rows.saturating_add(1));
        }
    }

    /// How many bytes the program has written so far: the offset the next
    /// byte will have.
    pub fn total(&self) -> u64 {
        self.total_cell().load(Ordering::Acquire)
    }

    /// The oldest offset the ring still holds: the bytes before it were written over.
    pub fn ring_from(&self) -> u64 {
        self.total().saturating_sub(self.capacity)
    }

    /// The oldest offset a client is given without a picture: the ring's start, or the
    /// floor if a client asked to forget the past after that. Bytes between the two are
    /// still in the ring ([`Journal::copy_out`] reads them, for a client that is owed
    /// them) but are not replayed to one that comes for them.
    pub fn retained_from(&self) -> u64 {
        self.ring_from().max(self.floor())
    }

    /// The offset the past was last forgotten at (0: it never was).
    pub fn floor(&self) -> u64 {
        self.floor_cell().load(Ordering::Acquire)
    }

    /// Forgets the past before `offset`: from now on [`Journal::retained_from`] is no
    /// earlier. A floor only moves forward, and not past what has been written (the single
    /// writer).
    pub fn set_floor(&mut self, offset: u64) {
        let offset = offset.min(self.total());
        if offset > self.floor() {
            self.floor_cell().store(offset, Ordering::Release);
        }
    }

    /// Appends `data` (the single writer).
    pub fn append(&mut self, data: &[u8]) {
        let mut total = self.total_cell().load(Ordering::Relaxed);
        let mut rest = data;
        while !rest.is_empty() {
            let (head, room) = self.head_and_room(total);
            let take = rest.len().min(room);
            let (now, later) = rest.split_at(take);
            // SAFETY: `head + take <= capacity`, inside the mapped ring.
            unsafe {
                std::ptr::copy_nonoverlapping(now.as_ptr(), self.ring_at(head), take);
            }
            total += take as u64;
            // Publish after every piece: a reader never sees a total whose
            // bytes are not in place.
            self.total_cell().store(total, Ordering::Release);
            rest = later;
        }
    }

    /// Reads from `source` (a terminal's master side, non-blocking) straight
    /// into the ring, at most `max` bytes, and publishes what arrived. The
    /// number of bytes is returned; 0 is the end of the stream, and the
    /// error is whatever `read(2)` said (`WouldBlock` among them).
    ///
    /// Reading into the ring and not through a buffer is the only place a
    /// byte is in this process and not yet in the journal, and it is a single
    /// instruction wide.
    pub fn fill_from(&mut self, source: BorrowedFd<'_>, max: usize) -> io::Result<usize> {
        let total = self.total_cell().load(Ordering::Relaxed);
        let (head, room) = self.head_and_room(total);
        let want = room.min(max);
        loop {
            // SAFETY: `head + want <= capacity`: the destination is inside the
            // mapped ring, and the single writer is the only one touching the
            // bytes past `total`.
            let got = unsafe { libc::read(source.as_raw_fd(), self.ring_at(head).cast(), want) };
            if got >= 0 {
                let got = got as usize;
                if got > 0 {
                    self.total_cell()
                        .store(total + got as u64, Ordering::Release);
                }
                return Ok(got);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    /// Copies up to `out.len()` bytes starting at stream offset `offset`.
    /// Returns how many (0 at the end of the stream), or `None` when the
    /// bytes asked for are not there: before the ring's start, past the end,
    /// or overwritten while they were being copied.
    pub fn copy_out(&self, offset: u64, out: &mut [u8]) -> Option<usize> {
        let total = self.total();
        if offset > total || offset < total.saturating_sub(self.capacity) {
            return None;
        }
        let available = usize::try_from(total - offset).unwrap_or(usize::MAX);
        let wanted = out.len().min(available);
        let mut done = 0;
        while done < wanted {
            let at = offset + done as u64;
            let head = (at % self.capacity) as usize;
            let room = (self.capacity as usize).saturating_sub(head);
            let take = (wanted - done).min(room);
            let target = out.get_mut(done..done + take)?;
            // SAFETY: `head + take <= capacity`, inside the mapped ring; the
            // target slice is `take` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.ring_at(head).cast_const(),
                    target.as_mut_ptr(),
                    take,
                );
            }
            done += take;
        }
        // A writer that lapped the reader while it copied has overwritten
        // the start of what it copied.
        if offset < self.ring_from() {
            return None;
        }
        Some(wanted)
    }

    /// The offset of the byte after the first newline in the ring: where a
    /// stream may start when the beginning of it is gone, so that it does not
    /// start inside a line (an escape sequence is rarely longer than one).
    /// With no newline in the ring, the ring's start. (The ring's, not the
    /// floor's: what a screen is built from is whatever is still written.)
    pub fn line_start(&self) -> u64 {
        self.line_start_from(0)
    }

    /// Like [`Journal::line_start`], looking from `from` on (or from the
    /// ring's start, if that is later). With no newline after it, the offset
    /// looked from.
    pub fn line_start_from(&self, from: u64) -> u64 {
        let first = |journal: &Journal| from.max(journal.ring_from());
        let mut at = first(self);
        let mut lapped = 0;
        let mut chunk = [0u8; 4096];
        while at < self.total() {
            match self.copy_out(at, &mut chunk) {
                Some(0) => break,
                Some(n) => {
                    if let Some(found) = chunk
                        .get(..n)
                        .and_then(|c| c.iter().position(|&b| b == b'\n'))
                    {
                        return at + found as u64 + 1;
                    }
                    at += n as u64;
                }
                // A writer in another process lapped the scan: start over
                // from the ring's new start, but not for ever.
                None => {
                    lapped += 1;
                    if lapped > 8 {
                        break;
                    }
                    at = first(self);
                }
            }
        }
        first(self)
    }

    fn answered_cell(&self) -> &AtomicU64 {
        // SAFETY: as in `total_cell`; `OFFSET_ANSWERED` is 8-byte aligned too.
        unsafe { &*self.base.as_ptr().add(OFFSET_ANSWERED).cast::<AtomicU64>() }
    }

    fn floor_cell(&self) -> &AtomicU64 {
        // SAFETY: as in `total_cell`; `OFFSET_FLOOR` is 8-byte aligned too.
        unsafe { &*self.base.as_ptr().add(OFFSET_FLOOR).cast::<AtomicU64>() }
    }

    fn total_cell(&self) -> &AtomicU64 {
        // SAFETY: the header is inside the mapping, `OFFSET_TOTAL` is 8-byte
        // aligned within a page-aligned base, and the mapping lives as long as
        // `self`. Every access to it is atomic.
        unsafe { &*self.base.as_ptr().add(OFFSET_TOTAL).cast::<AtomicU64>() }
    }

    /// Where the byte at stream offset `total` lands in the ring, and how many
    /// bytes fit from there before the ring wraps.
    fn head_and_room(&self, total: u64) -> (usize, usize) {
        let head = (total % self.capacity) as usize;
        (head, (self.capacity as usize).saturating_sub(head))
    }

    /// A pointer to ring position `head`.
    fn ring_at(&self, head: usize) -> *mut u8 {
        // SAFETY: callers keep `head < capacity`, and the mapping is
        // `HEADER_LEN + capacity` bytes.
        unsafe { self.base.as_ptr().add(HEADER_LEN + head) }
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
        // SAFETY: the mapping was made by `map` with this length and nothing
        // outlives the journal.
        unsafe { libc::munmap(self.base.as_ptr().cast(), self.map_len) };
    }
}

fn bad(what: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

/// Maps `len` bytes of `fd`, shared, readable and writable.
fn map(fd: BorrowedFd<'_>, len: usize) -> io::Result<NonNull<u8>> {
    // SAFETY: a fresh mapping chosen by the kernel; the descriptor is open.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    NonNull::new(map.cast()).ok_or_else(|| io::Error::other("mmap returned null"))
}

/// Shared memory with no name: Linux' `memfd_create`, macOS' `shm_open`
/// unlinked at once. Close-on-exec either way, set again explicitly: a copy
/// in a child (the shell) would keep the region past everyone.
fn anonymous_memory() -> io::Result<OwnedFd> {
    #[cfg(target_os = "linux")]
    let fd = {
        // SAFETY: a static name; the flags ask for close-on-exec.
        let fd = unsafe { libc::memfd_create(c"kablo-journal".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a descriptor just returned to us, owned from here.
        unsafe { OwnedFd::from_raw_fd(fd) }
    };
    #[cfg(not(target_os = "linux"))]
    let fd = {
        use std::sync::atomic::AtomicU32;
        static SERIAL: AtomicU32 = AtomicU32::new(0);
        let mut tries = 0;
        loop {
            // macOS keeps a name to 31 bytes.
            let name = format!(
                "/kb.{:x}.{:x}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            );
            let name = std::ffi::CString::new(name).map_err(|_| io::ErrorKind::InvalidInput)?;
            // SAFETY: `name` is a C string for the call; the mode is passed
            // promoted, as the variadic declaration requires.
            let fd = unsafe {
                libc::shm_open(
                    name.as_ptr(),
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    0o600 as libc::c_uint,
                )
            };
            if fd >= 0 {
                // SAFETY: the name just created; the object lives while a
                // descriptor or a mapping does.
                unsafe { libc::shm_unlink(name.as_ptr()) };
                // SAFETY: a descriptor just returned to us, owned from here.
                break unsafe { OwnedFd::from_raw_fd(fd) };
            }
            let error = io::Error::last_os_error();
            tries += 1;
            if error.kind() != io::ErrorKind::AlreadyExists || tries > 8 {
                return Err(error);
            }
        }
    };
    sys::set_cloexec(fd.as_raw_fd())?;
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn pattern(from: u64, len: usize) -> Vec<u8> {
        (from..from + len as u64).map(|n| (n % 251) as u8).collect()
    }

    #[test]
    fn what_is_appended_is_read_back_by_offset() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"hello ");
        journal.append(b"world");
        assert_eq!(journal.total(), 11);
        assert_eq!(journal.retained_from(), 0);
        let mut out = [0u8; 32];
        assert_eq!(journal.copy_out(0, &mut out), Some(11));
        assert_eq!(&out[..11], b"hello world");
        assert_eq!(journal.copy_out(6, &mut out[..3]), Some(3));
        assert_eq!(&out[..3], b"wor");
        assert_eq!(journal.copy_out(11, &mut out), Some(0));
        assert_eq!(journal.copy_out(12, &mut out), None, "past the end");
    }

    #[test]
    fn the_ring_wraps_and_drops_the_oldest() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        let cap = MIN_CAPACITY as usize;
        let data = pattern(0, cap + 1000);
        // In pieces that do not divide the capacity, so a piece straddles the end.
        for piece in data.chunks(1500) {
            journal.append(piece);
        }
        assert_eq!(journal.total(), data.len() as u64);
        assert_eq!(journal.retained_from(), 1000);
        let mut out = vec![0u8; cap];
        assert_eq!(journal.copy_out(0, &mut out), None, "dropped");
        assert_eq!(journal.copy_out(999, &mut out), None);
        assert_eq!(journal.copy_out(1000, &mut out), Some(cap));
        assert_eq!(out, data.get(1000..).unwrap());
    }

    #[test]
    fn a_second_mapping_sees_the_same_stream() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        journal.append(b"shared");
        let copy = unsafe { OwnedFd::from_raw_fd(libc::dup(journal.as_raw_fd())) };
        let other = Journal::open(copy).unwrap();
        assert_eq!(other.capacity(), MIN_CAPACITY);
        assert_eq!(other.total(), 6);
        journal.append(b"!");
        let mut out = [0u8; 8];
        assert_eq!(other.copy_out(0, &mut out), Some(7));
        assert_eq!(&out[..7], b"shared!");
    }

    #[test]
    fn a_header_that_does_not_match_is_refused() {
        let journal = Journal::create(MIN_CAPACITY).unwrap();
        // Scribble over the magic.
        unsafe { journal.base.as_ptr().write(b'X') };
        let copy = unsafe { OwnedFd::from_raw_fd(libc::dup(journal.as_raw_fd())) };
        assert!(Journal::open(copy).is_err());
        assert!(Journal::create(MIN_CAPACITY - 1).is_err());
        assert!(Journal::create(MAX_CAPACITY + 1).is_err());
    }

    #[test]
    fn a_stream_may_start_after_the_first_line_break() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        let cap = MIN_CAPACITY as usize;
        let mut data = Vec::new();
        while data.len() < cap + 100 {
            data.extend_from_slice(b"0123456789 line\n");
        }
        journal.append(&data);
        let start = journal.line_start();
        assert!(start > journal.retained_from());
        let mut byte = [0u8];
        assert_eq!(journal.copy_out(start - 1, &mut byte), Some(1));
        assert_eq!(byte, *b"\n");

        let mut flat = Journal::create(MIN_CAPACITY).unwrap();
        flat.append(&vec![b'a'; cap + 5]);
        assert_eq!(
            flat.line_start(),
            flat.retained_from(),
            "no newline: the start"
        );
    }

    #[test]
    fn a_terminal_is_read_straight_into_the_ring() {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        sys::set_nonblocking(reader.as_raw_fd()).unwrap();
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        use std::io::Write;
        (&writer).write_all(b"from the pty").unwrap();
        let n = journal.fill_from(reader.as_fd(), 4096).unwrap();
        assert_eq!(n, 12);
        assert_eq!(journal.total(), 12);
        let err = journal.fill_from(reader.as_fd(), 4096).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(writer);
        assert_eq!(journal.fill_from(reader.as_fd(), 4096).unwrap(), 0, "end");
        let mut out = [0u8; 12];
        assert_eq!(journal.copy_out(0, &mut out), Some(12));
        assert_eq!(&out, b"from the pty");
    }

    #[test]
    fn what_was_answered_is_in_the_header() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        assert_eq!(journal.answered(), Some(0), "a new journal keeps track");
        journal.set_answered(1234);
        let again = Journal::open(dup_of(&journal)).unwrap();
        assert_eq!(again.answered(), Some(1234));
        // A journal an older warden made has a zero there: nobody keeps track.
        // SAFETY: the header is inside the mapping; the single writer is this test.
        unsafe {
            journal
                .base
                .as_ptr()
                .add(OFFSET_ANSWERED)
                .cast::<u64>()
                .write_unaligned(0);
        }
        assert_eq!(journal.answered(), None);
    }

    #[test]
    fn the_scrollback_a_session_started_with_is_in_the_header() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        assert_eq!(journal.history(), None, "a journal nobody said it for");
        journal.set_history(0);
        assert_eq!(journal.history(), Some(0), "none is something to say");
        journal.set_history(5000);
        assert_eq!(journal.history(), Some(5000));
        // Another process maps the same memory and reads the same number.
        let again = Journal::open(dup_of(&journal)).unwrap();
        assert_eq!(again.history(), Some(5000));
        assert_eq!(again.total(), 0, "and the rest of the header is untouched");
    }

    #[test]
    fn the_past_that_was_forgotten_is_in_the_header_and_the_ring_still_holds_it() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        assert_eq!(journal.floor(), 0, "nothing was forgotten");
        journal.append(b"before");
        journal.set_floor(6);
        journal.append(b" after");
        assert_eq!(journal.floor(), 6);
        assert_eq!(
            journal.retained_from(),
            6,
            "a client that comes is given from here"
        );
        assert_eq!(journal.ring_from(), 0, "and the ring has all of it");
        // A client that is owed bytes before the floor can still be given them.
        let mut out = [0u8; 16];
        assert_eq!(journal.copy_out(0, &mut out), Some(12));
        assert_eq!(&out[..12], b"before after");
        assert_eq!(
            journal.line_start_from(0),
            0,
            "no break: from the ring's start"
        );

        // It moves forward only, and not past what was written.
        journal.set_floor(3);
        assert_eq!(journal.floor(), 6);
        journal.set_floor(1 << 40);
        assert_eq!(journal.floor(), 12);

        // Another mapping of the same memory (a relay that comes after) reads it.
        let again = Journal::open(dup_of(&journal)).unwrap();
        assert_eq!(again.floor(), 12);
        assert_eq!(
            again.history(),
            None,
            "and the rest of the header is untouched"
        );

        // A journal an older warden made has zero there: no floor.
        // SAFETY: the header is inside the mapping; the single writer is this test.
        unsafe {
            journal
                .base
                .as_ptr()
                .add(OFFSET_FLOOR)
                .cast::<u64>()
                .write_unaligned(0);
        }
        assert_eq!(journal.floor(), 0);
        assert_eq!(journal.retained_from(), 0);
    }

    #[test]
    fn the_ring_passing_the_floor_is_the_start_again() {
        let mut journal = Journal::create(MIN_CAPACITY).unwrap();
        let cap = MIN_CAPACITY as usize;
        journal.append(&pattern(0, 100));
        journal.set_floor(100);
        journal.append(&pattern(100, cap + 50));
        // The ring wrote over the floor: it is the later of the two that counts.
        assert_eq!(journal.ring_from(), 150);
        assert_eq!(journal.retained_from(), 150);
        let mut out = [0u8; 8];
        assert_eq!(journal.copy_out(120, &mut out), None, "written over");
    }

    fn dup_of(journal: &Journal) -> OwnedFd {
        // SAFETY: `dup` of a descriptor that is open for the call.
        let fd = unsafe { libc::dup(journal.as_raw_fd()) };
        assert!(fd >= 0);
        // SAFETY: a descriptor just made, owned from here.
        unsafe { OwnedFd::from_raw_fd(fd) }
    }
}
