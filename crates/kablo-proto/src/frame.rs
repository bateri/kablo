//! Frames: the unit both directions of the wire are made of.
//!
//! ```text
//!  0       1        2              3               7
//!  +-------+--------+--------------+---------------+------------+---------+
//!  | kind  | channel| session len  | payload len   | session id | payload |
//!  | u8    | u8     | u8 (<= 64)   | u32 (<= 1 MiB)| bytes      | bytes   |
//!  +-------+--------+--------------+---------------+------------+---------+
//! ```
//!
//! Integers are big-endian. The layer knows nothing about what a kind means:
//! it moves bytes, bounds them and hands them up as a [`RawFrame`]. Both
//! lengths are checked against their limits before a byte of the body is read
//! or any memory is reserved for it, so a hostile length costs nothing.

use std::io::{self, Read, Write};

use crate::{Error, Result};

/// The bytes before the session id.
pub const HEADER_LEN: usize = 7;
/// The longest session id a frame may carry.
pub const MAX_SESSION_LEN: usize = 64;
/// The longest payload a frame may carry: 1 MiB.
pub const MAX_PAYLOAD: usize = 1 << 20;
/// The channel of terminal sessions, the only one v1 uses. A frame on any
/// other channel is skipped (the field is reserved for port forwarding).
pub const CHANNEL_SESSION: u8 = 0;

/// How much a payload read reserves before bytes have actually arrived.
const FIRST_CHUNK: usize = 16 * 1024;

/// One frame, as it is on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawFrame {
    /// What the payload means. The high bit marks the server-to-client half.
    pub kind: u8,
    /// [`CHANNEL_SESSION`], or a channel this version does not know.
    pub channel: u8,
    /// The session id bytes (empty for a connection-level frame).
    pub session: Vec<u8>,
    /// The payload.
    pub payload: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Header {
    kind: u8,
    channel: u8,
    session_len: usize,
    payload_len: usize,
}

impl Header {
    fn parse(bytes: &[u8; HEADER_LEN]) -> Result<Header> {
        let [kind, channel, session_len, a, b, c, d] = *bytes;
        let session_len = usize::from(session_len);
        if session_len > MAX_SESSION_LEN {
            return Err(Error::SessionTooLong { len: session_len });
        }
        let payload_len = usize::try_from(u32::from_be_bytes([a, b, c, d])).unwrap_or(usize::MAX);
        if payload_len > MAX_PAYLOAD {
            return Err(Error::PayloadTooLarge { len: payload_len });
        }
        Ok(Header {
            kind,
            channel,
            session_len,
            payload_len,
        })
    }

    fn body_len(&self) -> usize {
        self.session_len + self.payload_len
    }
}

/// Appends a frame to `out`. On error `out` is left as it was.
pub fn encode_frame(out: &mut Vec<u8>, frame: &RawFrame) -> Result<()> {
    if frame.session.len() > MAX_SESSION_LEN {
        return Err(Error::SessionTooLong {
            len: frame.session.len(),
        });
    }
    if frame.payload.len() > MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge {
            len: frame.payload.len(),
        });
    }
    let session_len = u8::try_from(frame.session.len()).map_err(|_| Error::SessionTooLong {
        len: frame.session.len(),
    })?;
    let payload_len = u32::try_from(frame.payload.len()).map_err(|_| Error::PayloadTooLarge {
        len: frame.payload.len(),
    })?;
    out.reserve(HEADER_LEN + frame.session.len() + frame.payload.len());
    out.extend_from_slice(&[frame.kind, frame.channel, session_len]);
    out.extend_from_slice(&payload_len.to_be_bytes());
    out.extend_from_slice(&frame.session);
    out.extend_from_slice(&frame.payload);
    Ok(())
}

/// Writes one frame with a single `write_all`, so two frames never interleave
/// on a writer that is not shared and a short write is the writer's to retry.
pub fn write_frame<W: Write>(writer: &mut W, frame: &RawFrame) -> Result<()> {
    let mut buf = Vec::new();
    encode_frame(&mut buf, frame)?;
    writer.write_all(&buf)?;
    Ok(())
}

/// Reads one frame. `Ok(None)` is the stream ending exactly between frames;
/// ending inside one is an [`Error::Io`] of kind `UnexpectedEof`.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<RawFrame>> {
    let mut head = [0u8; HEADER_LEN];
    let Some((first, rest)) = head.split_first_mut() else {
        return Ok(None);
    };
    loop {
        match reader.read(std::slice::from_mut(first)) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    reader.read_exact(rest)?;
    let header = Header::parse(&head)?;
    let session = read_bytes(reader, header.session_len)?;
    let payload = read_bytes(reader, header.payload_len)?;
    Ok(Some(RawFrame {
        kind: header.kind,
        channel: header.channel,
        session,
        payload,
    }))
}

/// Reads frames until `convert` accepts one: the others (kinds this version
/// does not know) are consumed whole and skipped. `Ok(None)` is the stream
/// ending between frames. A `convert` error leaves the stream aligned on the
/// next frame.
pub fn read_with<R, M>(
    reader: &mut R,
    mut convert: impl FnMut(RawFrame) -> Result<Option<M>>,
) -> Result<Option<M>>
where
    R: Read,
{
    while let Some(frame) = read_frame(reader)? {
        if let Some(message) = convert(frame)? {
            return Ok(Some(message));
        }
    }
    Ok(None)
}

/// Reads exactly `len` bytes, reserving memory as they arrive rather than up
/// front, so a peer that promises a lot and sends nothing holds nothing.
fn read_bytes<R: Read>(reader: &mut R, len: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(len.min(FIRST_CHUNK));
    let limit = u64::try_from(len).unwrap_or(u64::MAX);
    reader.by_ref().take(limit).read_to_end(&mut bytes)?;
    if bytes.len() == len {
        Ok(bytes)
    } else {
        Err(io::Error::from(io::ErrorKind::UnexpectedEof).into())
    }
}

/// Turns bytes pushed in arbitrary pieces into frames: the counterpart of
/// [`read_frame`] for a reader that cannot block (a `poll` loop). After an
/// error other than [`Error::Malformed`] from a converter, the decoder is
/// stuck on the bad header: drop it with the connection.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    pos: usize,
}

impl FrameDecoder {
    /// An empty decoder.
    pub fn new() -> FrameDecoder {
        FrameDecoder::default()
    }

    /// Adds bytes that arrived.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// How many pushed bytes are not yet part of a returned frame.
    pub fn pending(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }

    /// The next whole frame, or `None` when more bytes are needed.
    pub fn next_frame(&mut self) -> Result<Option<RawFrame>> {
        let avail = self.buf.get(self.pos..).unwrap_or_default();
        let Some(head) = avail.first_chunk::<HEADER_LEN>() else {
            return Ok(None);
        };
        let header = Header::parse(head)?;
        let total = HEADER_LEN + header.body_len();
        let Some(body) = avail.get(HEADER_LEN..total) else {
            return Ok(None);
        };
        let Some((session, payload)) = body.split_at_checked(header.session_len) else {
            return Ok(None);
        };
        let frame = RawFrame {
            kind: header.kind,
            channel: header.channel,
            session: session.to_vec(),
            payload: payload.to_vec(),
        };
        self.pos += total;
        self.compact();
        Ok(Some(frame))
    }

    /// The next frame `convert` accepts, skipping the others, as
    /// [`read_with`] does.
    pub fn next_with<M>(
        &mut self,
        mut convert: impl FnMut(RawFrame) -> Result<Option<M>>,
    ) -> Result<Option<M>> {
        while let Some(frame) = self.next_frame()? {
            if let Some(message) = convert(frame)? {
                return Ok(Some(message));
            }
        }
        Ok(None)
    }

    fn compact(&mut self) {
        if self.pos >= self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        } else if self.pos >= FIRST_CHUNK && self.pos * 2 >= self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: u8, session: &str, payload: &[u8]) -> RawFrame {
        RawFrame {
            kind,
            channel: 0,
            session: session.as_bytes().to_vec(),
            payload: payload.to_vec(),
        }
    }

    #[test]
    fn the_header_layout_is_pinned() {
        let mut out = Vec::new();
        encode_frame(&mut out, &frame(0x07, "ab", b"xyz")).unwrap();
        assert_eq!(
            out,
            [0x07, 0x00, 0x02, 0, 0, 0, 3, b'a', b'b', b'x', b'y', b'z']
        );
    }

    #[test]
    fn a_length_over_the_limit_is_refused_before_the_body_is_read() {
        // Only the header is available: asking for the body would hit EOF, so
        // the length error proves the body was never touched.
        for len in [MAX_PAYLOAD as u32 + 1, u32::MAX] {
            let mut bytes = vec![0x07, 0x00, 0x00];
            bytes.extend_from_slice(&len.to_be_bytes());
            let err = read_frame(&mut bytes.as_slice()).unwrap_err();
            assert!(matches!(err, Error::PayloadTooLarge { .. }), "{err}");
            let mut decoder = FrameDecoder::new();
            decoder.push(&bytes);
            assert!(matches!(
                decoder.next_frame(),
                Err(Error::PayloadTooLarge { .. })
            ));
        }
        let err = read_frame(&mut [0x07, 0x00, 65, 0, 0, 0, 0].as_slice()).unwrap_err();
        assert!(matches!(err, Error::SessionTooLong { len: 65 }));
    }

    #[test]
    fn the_largest_payload_goes_through_and_one_more_does_not() {
        let big = frame(0x07, "s", &vec![7u8; MAX_PAYLOAD]);
        let mut wire = Vec::new();
        encode_frame(&mut wire, &big).unwrap();
        assert_eq!(read_frame(&mut wire.as_slice()).unwrap().unwrap(), big);
        let mut out = vec![1, 2, 3];
        let over = frame(0x07, "s", &vec![7u8; MAX_PAYLOAD + 1]);
        assert!(matches!(
            encode_frame(&mut out, &over),
            Err(Error::PayloadTooLarge { .. })
        ));
        assert_eq!(out, [1, 2, 3], "a failed encode leaves the buffer alone");
        let long_id = frame(0x07, &"a".repeat(65), b"");
        assert!(matches!(
            encode_frame(&mut out, &long_id),
            Err(Error::SessionTooLong { .. })
        ));
    }

    #[test]
    fn the_end_of_the_stream_is_clean_between_frames_and_an_error_inside_one() {
        let mut wire = Vec::new();
        encode_frame(&mut wire, &frame(0x07, "s", b"hello")).unwrap();
        let mut reader = wire.as_slice();
        assert!(read_frame(&mut reader).unwrap().is_some());
        assert!(read_frame(&mut reader).unwrap().is_none());
        for cut in 1..wire.len() {
            match read_frame(&mut wire.get(..cut).unwrap()) {
                Err(Error::Io(e)) => {
                    assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof, "cut at {cut}")
                }
                other => panic!("cut at {cut}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_decoder_agrees_with_the_reader_however_the_bytes_are_cut() {
        let frames: Vec<RawFrame> = (0..40u8)
            .map(|i| {
                frame(
                    i,
                    if i % 3 == 0 { "" } else { "session-1" },
                    &vec![i; usize::from(i) * 37],
                )
            })
            .collect();
        let mut wire = Vec::new();
        for f in &frames {
            encode_frame(&mut wire, f).unwrap();
        }
        let mut reader = wire.as_slice();
        for f in &frames {
            assert_eq!(&read_frame(&mut reader).unwrap().unwrap(), f);
        }
        for piece in [1, 2, 3, 7, 64, 1000, wire.len()] {
            let mut decoder = FrameDecoder::new();
            let mut got = Vec::new();
            for chunk in wire.chunks(piece) {
                decoder.push(chunk);
                while let Some(f) = decoder.next_frame().unwrap() {
                    got.push(f);
                }
            }
            assert_eq!(got, frames, "pieces of {piece}");
            assert_eq!(decoder.pending(), 0);
        }
    }

    #[test]
    fn frames_the_converter_does_not_accept_are_skipped_whole() {
        let mut wire = Vec::new();
        encode_frame(&mut wire, &frame(0x7e, "x", &[1; 300])).unwrap();
        encode_frame(&mut wire, &frame(0x01, "", b"")).unwrap();
        encode_frame(&mut wire, &frame(0x7f, "", b"zz")).unwrap();
        let convert = |f: RawFrame| Ok((f.kind == 0x01).then_some(f.kind));
        let mut reader = wire.as_slice();
        assert_eq!(read_with(&mut reader, convert).unwrap(), Some(0x01));
        assert_eq!(
            read_with(&mut reader, convert).unwrap(),
            None,
            "the trailing unknown frame is skipped to the end"
        );
        let mut decoder = FrameDecoder::new();
        decoder.push(&wire);
        assert_eq!(decoder.next_with(convert).unwrap(), Some(0x01));
        assert_eq!(decoder.next_with(convert).unwrap(), None);
    }
}
