//! Streams: TCP connections the server makes for the client, on channel 1.
//!
//! A frame on [`CHANNEL_STREAM`] has no session id (a reader ignores whatever
//! one carries); the payload starts with the number of the stream it concerns,
//! a `u32` the client picked. As on the session channel, the kinds with the high
//! bit clear go client to server and the others server to client, and a kind
//! means what it means on this channel only. `data`, `credit`, `eof` and `close`
//! go both ways, so each has two kinds; whether a message is the client's or the
//! server's is told by [`Side`], which the one writing it names and the one
//! reading it gets back.
//!
//! Reading a stream message checks its structure and nothing else: the limits
//! ([`MAX_WINDOW`], [`MAX_STREAMS`]) and what an `open` asks for are the
//! receiver's to answer, one stream at a time, with a `failed` or a `close`,
//! which a codec that refused the frame could not do because it would not know
//! the stream. Writing is stricter: a message no conforming end would send
//! (an `open` for port 0, a `data` over [`MAX_DATA`]) is an error at the
//! sender.
//!
//! [`Packet`] is the session channel's counterpart and leaves channel 1 to
//! this module: a reader built on [`super::read_packet`] skips these frames,
//! which is what a build that predates streams does. [`Incoming`] reads both
//! channels.

use std::io::{Read, Write};
use std::time::Duration;

use super::Fields;
use crate::frame::{self, CHANNEL_SESSION, CHANNEL_STREAM, FrameDecoder, RawFrame};
use crate::tlv;
use crate::{Error, Result};

/// The most bytes one `data` message carries.
pub const MAX_DATA: usize = 32 * 1024;

/// The largest window an end offers for a stream: the bytes the other end may
/// send before it is given more.
pub const MAX_WINDOW: u32 = 256 * 1024;

/// The most streams a connection keeps open at once, counting the ones still
/// being connected.
pub const MAX_STREAMS: u32 = 128;

/// The most streams being connected at once on a connection.
pub const MAX_CONNECTING: u32 = 16;

/// How long the server tries to reach a stream's target before it gives up
/// with [`FailCode::TIMED_OUT`].
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The longest host name an `open` can carry: the longest a DNS name is.
pub const MAX_HOST: usize = 253;

/// The kind byte of each message. The messages that go both ways have the
/// client's kind and the server's.
pub mod kind {
    /// `open`: connect to a target and call the connection a stream.
    pub const OPEN: u8 = 0x01;
    /// `data`, client to server: bytes for the target.
    pub const CLIENT_DATA: u8 = 0x02;
    /// `credit`, client to server: the client has taken bytes and has room for more.
    pub const CLIENT_CREDIT: u8 = 0x03;
    /// `eof`, client to server: the client sends no more bytes on the stream.
    pub const CLIENT_EOF: u8 = 0x04;
    /// `close`, client to server: the stream is over, both ways.
    pub const CLIENT_CLOSE: u8 = 0x05;
    /// `opened`: the answer to an `open` that connected.
    pub const OPENED: u8 = 0x81;
    /// `data`, server to client: bytes from the target.
    pub const SERVER_DATA: u8 = 0x82;
    /// `credit`, server to client: the server has taken bytes and has room for more.
    pub const SERVER_CREDIT: u8 = 0x83;
    /// `eof`, server to client: the target sends no more bytes.
    pub const SERVER_EOF: u8 = 0x84;
    /// `close`, server to client: the stream is over, both ways.
    pub const SERVER_CLOSE: u8 = 0x85;
    /// `failed`: the answer to an `open` that did not connect.
    pub const FAILED: u8 = 0x86;
}

/// The name of a stream message kind, for diagnostics (`"unknown"` for a
/// kind this version does not have).
pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        kind::OPEN => "stream open",
        kind::OPENED => "stream opened",
        kind::FAILED => "stream failed",
        kind::CLIENT_DATA | kind::SERVER_DATA => "stream data",
        kind::CLIENT_CREDIT | kind::SERVER_CREDIT => "stream credit",
        kind::CLIENT_EOF | kind::SERVER_EOF => "stream eof",
        kind::CLIENT_CLOSE | kind::SERVER_CLOSE => "stream close",
        _ => "unknown",
    }
}

/// Which end of the connection a message is from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The client: the end that opens streams.
    Client,
    /// The server (the bridge): the end that connects them.
    Server,
}

/// Why an `open` did not connect. A code this version does not name still
/// arrives as itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FailCode(pub u16);

impl FailCode {
    /// The target refused the connection.
    pub const REFUSED: FailCode = FailCode(1);
    /// There is no way to the target, or connecting failed in a way no other
    /// code names.
    pub const UNREACHABLE: FailCode = FailCode(2);
    /// The host name could not be resolved.
    pub const UNRESOLVED: FailCode = FailCode(3);
    /// The target did not answer within [`CONNECT_TIMEOUT`].
    pub const TIMED_OUT: FailCode = FailCode(4);
    /// The connection has too many streams ([`MAX_STREAMS`], [`MAX_CONNECTING`])
    /// or the window offered is over [`MAX_WINDOW`].
    pub const LIMIT: FailCode = FailCode(5);
    /// The server has streams turned off.
    pub const FORBIDDEN: FailCode = FailCode(6);
    /// The `open` makes no sense: port 0, an empty or too long host.
    pub const BAD_REQUEST: FailCode = FailCode(7);
}

/// A stream message, without the stream it concerns (see [`Packet`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Client to server: connect to `host:port` and call the connection by
    /// this stream's number. `window` is how many bytes the client takes
    /// before the server has to wait for a `Credit`. Answered by `Opened` or
    /// `Failed`; the client sends no `Data` until the `Opened`.
    Open {
        /// The bytes the server may send before a `Credit`.
        window: u32,
        /// The target's port; never 0.
        port: u16,
        /// The target: a name or an address, 1 to [`MAX_HOST`] bytes, no NUL.
        host: String,
    },
    /// Server to client: the stream is connected. `window` is how many bytes
    /// the server takes before the client has to wait for a `Credit`.
    Opened {
        /// The bytes the client may send before a `Credit`.
        window: u32,
    },
    /// Server to client: the `open` did not connect, and the stream never was.
    Failed {
        /// Why.
        code: FailCode,
        /// For a log, and for a person once control characters are removed:
        /// short English that reads after a colon ("connection refused").
        text: String,
    },
    /// Either end: bytes of the connection, at most [`MAX_DATA`], and at most
    /// the window the other end has left.
    Data(Vec<u8>),
    /// Either end: the sender has taken this many bytes (from `Data` or into
    /// its socket) and the other end may send that many more.
    Credit(u32),
    /// Either end: the sender will send no more `Data` on the stream. The
    /// other direction goes on.
    Eof,
    /// Either end: the stream is over in both directions, bytes still on their
    /// way are dropped.
    Close,
}

/// The bit that marks the kinds that go server to client.
const FROM_SERVER: u8 = 0x80;

mod open_key {
    pub const PORT: u16 = 1;
    pub const HOST: u16 = 2;
}

/// What an `open` asks for must be something a server could connect to.
fn check_target(port: u16, host: &str) -> Result<()> {
    if port == 0 || host.is_empty() || host.len() > MAX_HOST || host.contains('\0') {
        return Err(Error::Malformed("stream target"));
    }
    Ok(())
}

/// The shapes of payload, which the (side, kind) pairs map to.
#[derive(Clone, Copy)]
enum Shape {
    Open,
    Opened,
    Failed,
    Data,
    Credit,
    Eof,
    Close,
}

/// `None` for a kind this version does not have.
fn shape(side: Side, kind: u8) -> Option<Shape> {
    Some(match (side, kind & !FROM_SERVER) {
        (Side::Client, 0x01) => Shape::Open,
        (Side::Server, 0x01) => Shape::Opened,
        (_, 0x02) => Shape::Data,
        (_, 0x03) => Shape::Credit,
        (_, 0x04) => Shape::Eof,
        (_, 0x05) => Shape::Close,
        (Side::Server, 0x06) => Shape::Failed,
        _ => return None,
    })
}

impl Message {
    /// The kind byte of the message when `from` sends it; an error for a
    /// message that end does not send (an `Open` from the server).
    pub fn kind(&self, from: Side) -> Result<u8> {
        Ok(match (self, from) {
            (Message::Open { .. }, Side::Client) => kind::OPEN,
            (Message::Opened { .. }, Side::Server) => kind::OPENED,
            (Message::Failed { .. }, Side::Server) => kind::FAILED,
            (Message::Data(_), Side::Client) => kind::CLIENT_DATA,
            (Message::Data(_), Side::Server) => kind::SERVER_DATA,
            (Message::Credit(_), Side::Client) => kind::CLIENT_CREDIT,
            (Message::Credit(_), Side::Server) => kind::SERVER_CREDIT,
            (Message::Eof, Side::Client) => kind::CLIENT_EOF,
            (Message::Eof, Side::Server) => kind::SERVER_EOF,
            (Message::Close, Side::Client) => kind::CLIENT_CLOSE,
            (Message::Close, Side::Server) => kind::SERVER_CLOSE,
            _ => return Err(Error::Malformed("stream message from the wrong end")),
        })
    }

    fn payload(&self, stream: u32) -> Result<Vec<u8>> {
        let mut out = stream.to_be_bytes().to_vec();
        match self {
            Message::Open { window, port, host } => {
                check_target(*port, host)?;
                out.extend_from_slice(&window.to_be_bytes());
                let mut target = tlv::Writer::new();
                target
                    .put_u16(open_key::PORT, *port)
                    .put(open_key::HOST, host.as_bytes());
                out.extend_from_slice(&target.finish());
            }
            Message::Opened { window } => out.extend_from_slice(&window.to_be_bytes()),
            Message::Failed { code, text } => {
                out.extend_from_slice(&code.0.to_be_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            Message::Data(bytes) => {
                if bytes.len() > MAX_DATA {
                    return Err(Error::Malformed("stream data"));
                }
                out.extend_from_slice(bytes);
            }
            Message::Credit(bytes) => out.extend_from_slice(&bytes.to_be_bytes()),
            Message::Eof | Message::Close => {}
        }
        Ok(out)
    }

    /// The target of an `open`. Both keys are optional and read as their zero
    /// (port 0, an empty host), which the server answers with `BAD_REQUEST`.
    fn read_target(block: &[u8]) -> Result<(u16, String)> {
        let mut port = 0;
        let mut host = String::new();
        for entry in tlv::entries(block) {
            let (key, value) = entry?;
            match key {
                open_key::PORT => port = tlv::read_u16(value, "stream open port")?,
                open_key::HOST => host = String::from_utf8_lossy(value).into_owned(),
                _ => {}
            }
        }
        Ok((port, host))
    }
}

/// A stream message and the stream it concerns: what one frame on channel 1
/// carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// The stream's number. The client picks it, counting up from 1, and never
    /// uses one twice on a connection.
    pub stream: u32,
    /// The message.
    pub message: Message,
}

impl Packet {
    /// A message about a stream.
    pub fn new(stream: u32, message: Message) -> Packet {
        Packet { stream, message }
    }

    /// The frame this packet is when `from` sends it.
    pub fn to_frame(&self, from: Side) -> Result<RawFrame> {
        Ok(RawFrame {
            kind: self.message.kind(from)?,
            channel: CHANNEL_STREAM,
            session: Vec::new(),
            payload: self.message.payload(self.stream)?,
        })
    }

    /// Reads a packet out of a frame, and says which end sent it. `Ok(None)`
    /// for a frame this version skips: a channel other than the stream
    /// channel, or a kind it does not know.
    pub fn from_frame(frame: RawFrame) -> Result<Option<(Side, Packet)>> {
        if frame.channel != CHANNEL_STREAM {
            return Ok(None);
        }
        let side = if frame.kind & FROM_SERVER != 0 {
            Side::Server
        } else {
            Side::Client
        };
        let Some(shape) = shape(side, frame.kind) else {
            return Ok(None);
        };
        let mut f = Fields::new(&frame.payload, kind_name(frame.kind));
        let stream = f.u32()?;
        let message = match shape {
            Shape::Open => {
                let window = f.u32()?;
                let (port, host) = Message::read_target(f.rest())?;
                Message::Open { window, port, host }
            }
            Shape::Opened => Message::Opened { window: f.u32()? },
            Shape::Failed => {
                let code = FailCode(f.u16()?);
                Message::Failed {
                    code,
                    text: String::from_utf8_lossy(f.rest()).into_owned(),
                }
            }
            Shape::Data => Message::Data(f.rest().to_vec()),
            Shape::Credit => Message::Credit(f.u32()?),
            Shape::Eof => Message::Eof,
            Shape::Close => Message::Close,
        };
        Ok(Some((side, Packet { stream, message })))
    }

    /// Appends the packet's bytes to `out` (for a writer that buffers).
    pub fn encode(&self, from: Side, out: &mut Vec<u8>) -> Result<()> {
        frame::encode_frame(out, &self.to_frame(from)?)
    }
}

/// Writes one stream packet.
pub fn write_packet<W: Write>(writer: &mut W, packet: &Packet, from: Side) -> Result<()> {
    frame::write_frame(writer, &packet.to_frame(from)?)
}

/// What one frame of either channel is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    /// A message about a session or the connection.
    Session(super::Packet),
    /// A message about a stream, from the end that sent it.
    Stream {
        /// Who sent it.
        from: Side,
        /// What it says.
        packet: Packet,
    },
}

impl Incoming {
    /// Reads a frame of either channel. `Ok(None)` for one this version
    /// skips: a channel it does not know, or a kind it does not know on a
    /// channel it does.
    pub fn from_frame(frame: RawFrame) -> Result<Option<Incoming>> {
        match frame.channel {
            CHANNEL_SESSION => Ok(super::Packet::from_frame(frame)?.map(Incoming::Session)),
            CHANNEL_STREAM => {
                Ok(Packet::from_frame(frame)?
                    .map(|(from, packet)| Incoming::Stream { from, packet }))
            }
            _ => Ok(None),
        }
    }
}

/// Reads the next message of either channel, skipping frames this version does
/// not know. `Ok(None)` is the stream ending between frames. After an
/// [`Error::is_recoverable`] error the next call reads the next frame.
pub fn read_incoming<R: Read>(reader: &mut R) -> Result<Option<Incoming>> {
    frame::read_with(reader, Incoming::from_frame)
}

/// Turns bytes pushed in arbitrary pieces into messages of either channel:
/// [`read_incoming`] for a reader that cannot block.
#[derive(Debug, Default)]
pub struct IncomingDecoder {
    frames: FrameDecoder,
}

impl IncomingDecoder {
    /// An empty decoder.
    pub fn new() -> IncomingDecoder {
        IncomingDecoder::default()
    }

    /// Adds bytes that arrived.
    pub fn push(&mut self, bytes: &[u8]) {
        self.frames.push(bytes);
    }

    /// How many pushed bytes are not yet part of a returned message.
    pub fn pending(&self) -> usize {
        self.frames.pending()
    }

    /// The next message, or `None` when more bytes are needed.
    pub fn next_incoming(&mut self) -> Result<Option<Incoming>> {
        self.frames.next_with(Incoming::from_frame)
    }
}
