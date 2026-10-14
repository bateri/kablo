//! What an end of the connection owes the other end, in two queues that never
//! starve each other.
//!
//! The connection is one byte pipe. Session frames (output, replies) and the
//! control frames of streams go into the **main** queue, in the order they are
//! made. The `data` frames of streams wait in a queue of their own, held by
//! whoever makes them (anything that is [`Frames`]), and are moved behind the
//! main queue's bytes **at frame boundaries** only:
//!
//! - when the main queue has been written out, stream frames follow until
//!   about a [`SLICE`] of them is queued, so a session frame made meanwhile
//!   waits for one slice of stream bytes at most;
//! - at the end of every turn that `force`s it, at least one stream frame goes
//!   in behind the main queue however much is queued there, so a session that
//!   never stops producing (a program writing without end) cannot keep a
//!   stream from moving.
//!
//! The main queue goes first and the stream frames are few and small, which is
//! what keeps typing, replies and the credit that opens the other direction of
//! a transfer from waiting behind a bulk one. What the main queue holds is
//! counted by [`Outbound::pending`], and the callers' limits on it; the stream
//! frames waiting in the other queue are not part of it.

use std::io;

use kablo_proto::v1::stream::MAX_DATA;

/// How many bytes of stream frames follow a drained main queue before the
/// queue is looked at again: one full `data`.
pub const SLICE: usize = MAX_DATA;

/// A queue of encoded stream frames.
pub trait Frames {
    /// The size of the frame at the front, or `None` if there is none.
    fn next_len(&self) -> Option<usize>;
    /// Takes the frame at the front.
    fn take(&mut self) -> Option<Vec<u8>>;
}

impl Frames for std::collections::VecDeque<Vec<u8>> {
    fn next_len(&self) -> Option<usize> {
        self.front().map(Vec::len)
    }

    fn take(&mut self) -> Option<Vec<u8>> {
        self.pop_front()
    }
}

/// The bytes waiting to be written to the connection.
#[derive(Debug)]
pub struct Outbound {
    buf: Vec<u8>,
    sent: usize,
    /// The most bytes written in one call.
    chunk: usize,
}

impl Outbound {
    /// An empty queue that writes at most `chunk` bytes at a time.
    pub fn new(chunk: usize) -> Outbound {
        Outbound {
            buf: Vec::new(),
            sent: 0,
            chunk: chunk.max(1),
        }
    }

    /// Bytes queued and not yet written, stream frames that were moved in
    /// included.
    pub fn pending(&self) -> usize {
        self.buf.len().saturating_sub(self.sent)
    }

    /// Queues bytes that are whole frames (or a whole handshake reply).
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The queue itself, for an encoder that appends a frame to it.
    pub fn buffer(&mut self) -> &mut Vec<u8> {
        &mut self.buf
    }

    /// Moves stream frames in behind the queued bytes: while fewer than
    /// [`SLICE`] bytes are queued, and one frame regardless when `force`.
    pub fn admit(&mut self, frames: &mut impl Frames, force: bool) {
        let mut forced = force;
        while frames.next_len().is_some() {
            if !forced && self.pending() >= SLICE {
                break;
            }
            forced = false;
            match frames.take() {
                Some(frame) => self.buf.extend_from_slice(&frame),
                None => break,
            }
        }
    }

    /// Writes with `write` until it takes no more, moving stream frames in as
    /// the queue empties. `false` when `write` failed for good.
    pub fn flush(
        &mut self,
        frames: &mut impl Frames,
        write: &mut impl FnMut(&[u8]) -> io::Result<usize>,
    ) -> bool {
        loop {
            while self.pending() > 0 {
                let end = (self.sent + self.chunk).min(self.buf.len());
                let waiting = self.buf.get(self.sent..end).unwrap_or_default();
                match write(waiting) {
                    Ok(0) => {
                        self.compact();
                        return true;
                    }
                    Ok(n) => self.sent += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.compact();
                        return true;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => return false,
                }
            }
            self.compact();
            if frames.next_len().is_none() {
                return true;
            }
            self.admit(frames, false);
        }
    }

    fn compact(&mut self) {
        if self.sent >= self.buf.len() {
            self.buf.clear();
            self.sent = 0;
        } else if self.sent >= 256 * 1024 {
            self.buf.drain(..self.sent);
            self.sent = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A frame of `len` bytes, all `tag`.
    fn frame(tag: u8, len: usize) -> Vec<u8> {
        vec![tag; len]
    }

    /// A pipe that takes `room` bytes and then says `WouldBlock`; what it took
    /// is `taken`.
    struct Pipe {
        room: usize,
        taken: Vec<u8>,
    }

    impl Pipe {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.room == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let n = bytes.len().min(self.room);
            self.room -= n;
            self.taken.extend_from_slice(bytes.get(..n).unwrap());
            Ok(n)
        }
    }

    /// The order in which runs of different bytes were written.
    fn runs(bytes: &[u8]) -> Vec<(u8, usize)> {
        let mut found: Vec<(u8, usize)> = Vec::new();
        for &b in bytes {
            match found.last_mut() {
                Some((tag, len)) if *tag == b => *len += 1,
                _ => found.push((b, 1)),
            }
        }
        found
    }

    #[test]
    fn stream_frames_follow_the_queue_a_slice_at_a_time() {
        let mut out = Outbound::new(1 << 20);
        let mut streams: VecDeque<Vec<u8>> = (0..10).map(|_| frame(b's', SLICE)).collect();
        out.push(&frame(b'a', 100));
        out.admit(&mut streams, false);
        // The queue holds less than a slice, so one frame was moved in; the
        // queue then held a slice and no more were.
        assert_eq!(streams.len(), 9);
        assert_eq!(out.pending(), 100 + SLICE);
    }

    #[test]
    fn a_session_frame_waits_for_one_slice_of_a_transfer_at_most() {
        let mut out = Outbound::new(1 << 20);
        let mut streams: VecDeque<Vec<u8>> = (0..100).map(|_| frame(b's', SLICE)).collect();
        let mut pipe = Pipe {
            room: 10 * SLICE,
            taken: Vec::new(),
        };
        assert!(out.flush(&mut streams, &mut |b| pipe.write(b)));
        let before = pipe.taken.len();
        assert_eq!(before, 10 * SLICE);
        // A session frame is made while the transfer is under way, and the
        // pipe takes more.
        out.push(&frame(b'a', 50));
        out.admit(&mut streams, true);
        pipe.room = 10 * SLICE;
        assert!(out.flush(&mut streams, &mut |b| pipe.write(b)));
        let after = pipe.taken.get(before..).unwrap();
        let first = after.iter().position(|&b| b == b'a').unwrap();
        assert!(
            first <= SLICE,
            "the session frame was written after {first} bytes of stream frames"
        );
    }

    #[test]
    fn a_stream_moves_though_the_queue_never_empties() {
        let mut out = Outbound::new(1 << 20);
        let mut streams: VecDeque<Vec<u8>> = (0..5).map(|_| frame(b's', 10)).collect();
        let mut pipe = Pipe {
            room: 0,
            taken: Vec::new(),
        };
        // A session that makes a 100-byte frame every turn and a pipe that
        // takes 90: the queue is never empty at the end of a turn, and a
        // stream that waited for it to be would never move.
        for _ in 0..5 {
            out.push(&frame(b'y', 100));
            out.admit(&mut streams, true);
            pipe.room = 90;
            assert!(out.flush(&mut streams, &mut |b| pipe.write(b)));
            assert!(out.pending() > 0);
        }
        assert!(streams.is_empty(), "{} frames still waiting", streams.len());
        // Everything is written, in the order it was queued: each stream
        // frame behind the session frames queued before it.
        pipe.room = usize::MAX;
        assert!(out.flush(&mut streams, &mut |b| pipe.write(b)));
        let found = runs(&pipe.taken);
        let streamed: usize = found
            .iter()
            .filter(|(tag, _)| *tag == b's')
            .map(|(_, len)| len)
            .sum();
        assert_eq!(streamed, 50);
        let first = found.iter().position(|(tag, _)| *tag == b's').unwrap();
        assert!(first > 0, "a stream frame went ahead of the session frames");
    }

    #[test]
    fn without_force_a_stream_waits_for_a_busy_queue() {
        let mut out = Outbound::new(1 << 20);
        let mut streams: VecDeque<Vec<u8>> = VecDeque::from([frame(b's', 10)]);
        out.push(&frame(b'y', 2 * SLICE));
        out.admit(&mut streams, false);
        assert_eq!(streams.len(), 1);
        out.admit(&mut streams, true);
        assert!(streams.is_empty());
        assert_eq!(out.pending(), 2 * SLICE + 10);
    }

    #[test]
    fn a_failed_write_is_reported_and_a_blocked_one_is_not() {
        let mut out = Outbound::new(16);
        let mut streams: VecDeque<Vec<u8>> = VecDeque::new();
        out.push(&frame(b'a', 40));
        assert!(out.flush(&mut streams, &mut |_| Err(io::ErrorKind::WouldBlock.into())));
        assert_eq!(out.pending(), 40);
        assert!(!out.flush(&mut streams, &mut |_| Err(io::ErrorKind::BrokenPipe.into())));
    }

    #[test]
    fn writes_are_no_longer_than_the_chunk() {
        let mut out = Outbound::new(16);
        let mut streams: VecDeque<Vec<u8>> = VecDeque::new();
        out.push(&frame(b'a', 40));
        let mut sizes = Vec::new();
        assert!(out.flush(&mut streams, &mut |b| {
            sizes.push(b.len());
            Ok(b.len())
        }));
        assert_eq!(sizes, [16, 16, 8]);
        assert_eq!(out.pending(), 0);
    }
}
