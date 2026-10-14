//! The wire between a session's warden and its relay.
//!
//! The warden is the shell's parent and never changes while the shell lives;
//! the relay is replaced by upgrades. They talk over a socket pair the warden
//! makes, in frames of the same shape as the public wire ([`kablo_proto::frame`])
//! but with a signature, version range and message set of their own. A relay
//! that is newer than its warden must still make itself understood, so this
//! wire follows the same rule as the public one: a build reads the current
//! version and the one before it, and the fixtures of every version stay.
//!
//! The relay opens the handshake and the warden, the frozen end, selects the
//! version. Messages travel in frames on the session channel with no session
//! id; every payload is empty or a [`kablo_proto::tlv`] block. One message
//! carries file descriptors: [`ToRelay::Adopt`] is sent with exactly
//! [`ADOPT_DESCRIPTORS`] descriptors in the ancillary data of the frame's
//! first byte, in this order: the terminal's master side, the journal's
//! shared memory, the listening socket.
//!
//! # The hand-over
//!
//! At most one process reads the terminal and writes the journal at any
//! moment (otherwise the bytes are split between two readers and "exactly the
//! missing bytes" means nothing). The warden reads it while no relay owns it;
//! a relay starts reading only on [`ToRelay::Go`]:
//!
//! ```text
//! warden -> relay    Adopt   (with the descriptors)
//! relay  -> warden   Ready   (everything is set up; nothing was read yet)
//! warden stops reading the terminal itself,
//!   or, when another relay owned it, tells that one:
//! warden -> old      Quit    (stop reading, finish writing the journal, exit)
//! old    -> warden   Released{offset}
//! warden -> relay    Go      (read the terminal, write the journal)
//! ```
//!
//! A new relay that dies before `Ready` leaves the old one serving.

use std::io::{Read, Write};
use std::marker::PhantomData;

use kablo_proto::frame::{self, CHANNEL_SESSION, FrameDecoder, RawFrame};
use kablo_proto::tlv;
pub use kablo_proto::v1::Exit;
use kablo_proto::{Error, Protocol, Result, SessionId};

/// What opens the handshake on this wire. It differs from the public
/// signature, so a connection mixed up between the two is noticed.
pub const SIGNATURE: &[u8] = b"KABLOW\0";
/// The lowest version of this wire that is read.
pub const MIN_VERSION: u8 = 1;
/// The highest version of this wire that is read.
pub const MAX_VERSION: u8 = 1;
/// This wire, as this build speaks it.
pub const INNER: Protocol = Protocol::new(SIGNATURE, MIN_VERSION, MAX_VERSION);

/// How many descriptors an [`ToRelay::Adopt`] carries.
pub const ADOPT_DESCRIPTORS: usize = 3;

/// A message in one direction of the wire.
pub trait Wire: Sized {
    /// The frame the message is.
    fn to_frame(&self) -> Result<RawFrame>;
    /// The message in a frame; `Ok(None)` for a kind or channel this version
    /// skips.
    fn from_frame(frame: RawFrame) -> Result<Option<Self>>;
}

/// Reads the next message, skipping frames this version does not know.
pub fn read<M: Wire, R: Read>(reader: &mut R) -> Result<Option<M>> {
    frame::read_with(reader, M::from_frame)
}

/// Writes one message.
pub fn write<M: Wire, W: Write>(writer: &mut W, message: &M) -> Result<()> {
    frame::write_frame(writer, &message.to_frame()?)
}

/// Turns bytes pushed in arbitrary pieces into messages, for a reader that
/// cannot block.
#[derive(Debug)]
pub struct Decoder<M> {
    frames: FrameDecoder,
    _message: PhantomData<fn() -> M>,
}

impl<M: Wire> Default for Decoder<M> {
    fn default() -> Self {
        Decoder::new()
    }
}

impl<M: Wire> Decoder<M> {
    /// An empty decoder.
    pub fn new() -> Decoder<M> {
        Decoder {
            frames: FrameDecoder::new(),
            _message: PhantomData,
        }
    }

    /// Adds bytes that arrived.
    pub fn push(&mut self, bytes: &[u8]) {
        self.frames.push(bytes);
    }

    /// The next message, or `None` when more bytes are needed.
    pub fn next_message(&mut self) -> Result<Option<M>> {
        self.frames.next_with(M::from_frame)
    }
}

/// What the warden tells the relay it adopts a session with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Adopt {
    /// The session.
    pub id: SessionId,
    /// The shell's process id.
    pub child_pid: u32,
    /// How the shell ended, if it already has.
    pub exit: Option<Exit>,
}

/// Warden to relay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToRelay {
    /// Take over the session. Sent with the descriptors
    /// ([`ADOPT_DESCRIPTORS`]).
    Adopt(Adopt),
    /// Nobody else reads the terminal now: start.
    Go,
    /// Stop reading, finish writing the journal, answer `Released` and exit.
    Quit,
    /// The shell has ended.
    ChildEnded(Exit),
    /// The upgrade this relay asked for did not happen.
    UpgradeFailed(String),
}

/// Relay to warden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ToWarden {
    /// Set up, nothing read yet: the warden may stop reading the terminal.
    Ready,
    /// Stopped reading; the journal ends at `offset`.
    Released {
        /// The total the journal holds.
        offset: u64,
    },
    /// A client asked for the relay to be replaced by the binary at `path`.
    Upgrade {
        /// The path, as bytes.
        path: Vec<u8>,
    },
    /// A client asked for the session to end.
    Terminate {
        /// Kill rather than hang up.
        force: bool,
    },
    /// The shell's end was delivered to a client: the session may be cleaned up.
    Delivered,
}

mod to_relay {
    pub const ADOPT: u8 = 0x01;
    pub const GO: u8 = 0x02;
    pub const QUIT: u8 = 0x03;
    pub const CHILD_ENDED: u8 = 0x04;
    pub const UPGRADE_FAILED: u8 = 0x05;
}

mod to_warden {
    pub const READY: u8 = 0x81;
    pub const RELEASED: u8 = 0x82;
    pub const UPGRADE: u8 = 0x83;
    pub const TERMINATE: u8 = 0x84;
    pub const DELIVERED: u8 = 0x85;
}

mod key {
    pub const ADOPT_ID: u16 = 1;
    pub const ADOPT_CHILD_PID: u16 = 2;
    pub const ADOPT_EXIT: u16 = 3;
    pub const EXIT: u16 = 1;
    pub const TEXT: u16 = 1;
    pub const OFFSET: u16 = 1;
    pub const PATH: u16 = 1;
    pub const FORCE: u16 = 1;
}

fn frame(kind: u8, payload: Vec<u8>) -> RawFrame {
    RawFrame {
        kind,
        channel: CHANNEL_SESSION,
        session: Vec::new(),
        payload,
    }
}

/// The value of the last entry with `key`.
fn last(block: &[u8], key: u16) -> Result<Option<&[u8]>> {
    let mut found = None;
    for entry in tlv::entries(block) {
        let (k, value) = entry?;
        if k == key {
            found = Some(value);
        }
    }
    Ok(found)
}

fn single(key: u16, value: &[u8]) -> Vec<u8> {
    let mut w = tlv::Writer::new();
    w.put(key, value);
    w.finish()
}

impl Wire for ToRelay {
    fn to_frame(&self) -> Result<RawFrame> {
        Ok(match self {
            ToRelay::Adopt(adopt) => {
                let mut w = tlv::Writer::new();
                w.put(key::ADOPT_ID, adopt.id.as_str().as_bytes());
                w.put_u32(key::ADOPT_CHILD_PID, adopt.child_pid);
                if let Some(exit) = adopt.exit {
                    w.put(key::ADOPT_EXIT, &exit.to_bytes());
                }
                frame(to_relay::ADOPT, w.finish())
            }
            ToRelay::Go => frame(to_relay::GO, Vec::new()),
            ToRelay::Quit => frame(to_relay::QUIT, Vec::new()),
            ToRelay::ChildEnded(exit) => {
                frame(to_relay::CHILD_ENDED, single(key::EXIT, &exit.to_bytes()))
            }
            ToRelay::UpgradeFailed(text) => {
                frame(to_relay::UPGRADE_FAILED, single(key::TEXT, text.as_bytes()))
            }
        })
    }

    fn from_frame(f: RawFrame) -> Result<Option<ToRelay>> {
        if f.channel != CHANNEL_SESSION {
            return Ok(None);
        }
        let p = f.payload.as_slice();
        Ok(Some(match f.kind {
            to_relay::ADOPT => {
                let mut id = None;
                let mut child_pid = 0;
                let mut exit = None;
                for entry in tlv::entries(p) {
                    let (k, value) = entry?;
                    match k {
                        key::ADOPT_ID => id = Some(SessionId::from_bytes(value)?),
                        key::ADOPT_CHILD_PID => {
                            child_pid = tlv::read_u32(value, "adopt child pid")?
                        }
                        key::ADOPT_EXIT => exit = Some(Exit::from_bytes(value)?),
                        _ => {}
                    }
                }
                let id = id.ok_or(Error::Malformed("adopt without an id"))?;
                ToRelay::Adopt(Adopt {
                    id,
                    child_pid,
                    exit,
                })
            }
            to_relay::GO => ToRelay::Go,
            to_relay::QUIT => ToRelay::Quit,
            to_relay::CHILD_ENDED => {
                let value =
                    last(p, key::EXIT)?.ok_or(Error::Malformed("child ended without a status"))?;
                ToRelay::ChildEnded(Exit::from_bytes(value)?)
            }
            to_relay::UPGRADE_FAILED => {
                let value = last(p, key::TEXT)?.unwrap_or_default();
                ToRelay::UpgradeFailed(String::from_utf8_lossy(value).into_owned())
            }
            _ => return Ok(None),
        }))
    }
}

impl Wire for ToWarden {
    fn to_frame(&self) -> Result<RawFrame> {
        Ok(match self {
            ToWarden::Ready => frame(to_warden::READY, Vec::new()),
            ToWarden::Released { offset } => frame(
                to_warden::RELEASED,
                single(key::OFFSET, &offset.to_be_bytes()),
            ),
            ToWarden::Upgrade { path } => frame(to_warden::UPGRADE, single(key::PATH, path)),
            ToWarden::Terminate { force } => frame(
                to_warden::TERMINATE,
                single(key::FORCE, &[u8::from(*force)]),
            ),
            ToWarden::Delivered => frame(to_warden::DELIVERED, Vec::new()),
        })
    }

    fn from_frame(f: RawFrame) -> Result<Option<ToWarden>> {
        if f.channel != CHANNEL_SESSION {
            return Ok(None);
        }
        let p = f.payload.as_slice();
        Ok(Some(match f.kind {
            to_warden::READY => ToWarden::Ready,
            to_warden::RELEASED => {
                let value =
                    last(p, key::OFFSET)?.ok_or(Error::Malformed("released without an offset"))?;
                ToWarden::Released {
                    offset: tlv::read_u64(value, "released offset")?,
                }
            }
            to_warden::UPGRADE => {
                let value =
                    last(p, key::PATH)?.ok_or(Error::Malformed("upgrade without a path"))?;
                ToWarden::Upgrade {
                    path: value.to_vec(),
                }
            }
            to_warden::TERMINATE => {
                let force = match last(p, key::FORCE)? {
                    Some(value) => tlv::read_u8(value, "terminate flag")? & 1 != 0,
                    None => false,
                };
                ToWarden::Terminate { force }
            }
            to_warden::DELIVERED => ToWarden::Delivered,
            _ => return Ok(None),
        }))
    }
}
