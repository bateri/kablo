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
//!
//! After `Ready` a relay also says which renewal records its binary reads
//! ([`ToWarden::CanRenew`]); a warden that has none to give does not ask.
//!
//! # Replacing a relay
//!
//! A client's `upgrade` reaches the serving relay, which passes it on as
//! [`ToWarden::Upgrade`]. The warden starts the binary named as a second
//! relay beside the first and runs the same sequence above for it, which is
//! the whole of what an upgrade is: the old relay keeps serving until the new
//! one says `Ready`, and is then told `Quit`. Nothing in this wire is
//! specific to it except [`ToRelay::UpgradeFailed`], which tells the serving
//! relay that the second one did not take (the path was refused or the new
//! relay stopped first). The relay that leaves answers `Released` and exits,
//! and the warden does not wait for one that does not: past a limit it is
//! killed. What that can cost is the one read of the terminal it was in the
//! middle of (taken from the terminal, not yet published to the journal); a
//! relay that is stuck is not in the middle of one. A relay that is the second is not told `Go` while the
//! first still reads, and a relay serves no client before `Go`.
//!
//! # Renewing the warden
//!
//! A warden replaces its own program with a newer build of the same binary
//! (`execve`: same process, same shell, same descriptors) and hands the new
//! image what it knew as a [`Renewal`], one frame of its own kind written to a
//! pipe whose read end the new image gets by number. The record is this
//! wire's newest frozen surface and follows its rule: a build reads the
//! current record version and the one before it ([`RENEWAL_MIN_VERSION`] to
//! [`RENEWAL_VERSION`]), the fixtures of every version stay, and the warden
//! only renews into a binary whose relay said, with [`ToWarden::CanRenew`],
//! that it reads the version being written. The warden tells the relay it
//! serves that it was renewed with [`ToRelay::Renewed`], and every
//! [`ToRelay::Adopt`] says which build the warden is.

use std::io::{Read, Write};
use std::marker::PhantomData;
use std::os::fd::RawFd;

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

/// The lowest version of the [`Renewal`] record that is read.
pub const RENEWAL_MIN_VERSION: u8 = 1;
/// The highest version of the [`Renewal`] record that is read, and the one a
/// warden of this build writes.
pub const RENEWAL_VERSION: u8 = 1;

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
    /// The version of the build the warden is; `None` from a warden that did
    /// not say.
    pub warden_version: Option<String>,
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
    /// The warden replaced its program with a newer build: the version of it.
    Renewed(String),
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
    /// The binary this relay runs reads the [`Renewal`] records of versions
    /// `min` to `max`: a warden may renew into it when what it writes is in
    /// that range.
    CanRenew {
        /// The lowest version read.
        min: u8,
        /// The highest version read.
        max: u8,
    },
}

mod to_relay {
    pub const ADOPT: u8 = 0x01;
    pub const GO: u8 = 0x02;
    pub const QUIT: u8 = 0x03;
    pub const CHILD_ENDED: u8 = 0x04;
    pub const UPGRADE_FAILED: u8 = 0x05;
    pub const RENEWED: u8 = 0x06;
}

mod to_warden {
    pub const READY: u8 = 0x81;
    pub const RELEASED: u8 = 0x82;
    pub const UPGRADE: u8 = 0x83;
    pub const TERMINATE: u8 = 0x84;
    pub const DELIVERED: u8 = 0x85;
    pub const CAN_RENEW: u8 = 0x86;
}

mod record {
    /// The kind of the frame a [`super::Renewal`] is.
    pub const RENEWAL: u8 = 0x41;
}

mod key {
    pub const ADOPT_ID: u16 = 1;
    pub const ADOPT_CHILD_PID: u16 = 2;
    pub const ADOPT_EXIT: u16 = 3;
    pub const ADOPT_WARDEN_VERSION: u16 = 4;
    pub const EXIT: u16 = 1;
    pub const TEXT: u16 = 1;
    pub const OFFSET: u16 = 1;
    pub const PATH: u16 = 1;
    pub const FORCE: u16 = 1;
    pub const MIN: u16 = 1;
    pub const MAX: u16 = 2;
}

/// The entries of a [`super::Renewal`].
mod renewal_key {
    pub const VERSION: u16 = 1;
    pub const ID: u16 = 2;
    pub const DIR: u16 = 3;
    pub const SHELL_PID: u16 = 4;
    pub const MASTER_FD: u16 = 5;
    pub const JOURNAL_FD: u16 = 6;
    pub const LISTENER_FD: u16 = 7;
    pub const RELAY_FD: u16 = 8;
    pub const RELAY_PID: u16 = 9;
    pub const RELAY_SERIAL: u16 = 10;
    pub const RELAY_BINARY: u16 = 11;
    pub const RELAY_EXE: u16 = 12;
    pub const SERIALS: u16 = 13;
    pub const STRIKES: u16 = 14;
    pub const EVER_READY: u16 = 15;
    pub const CHUNK: u16 = 16;
    pub const DELIVERED: u16 = 17;
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
                if let Some(version) = &adopt.warden_version {
                    w.put(key::ADOPT_WARDEN_VERSION, version.as_bytes());
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
            ToRelay::Renewed(version) => {
                frame(to_relay::RENEWED, single(key::TEXT, version.as_bytes()))
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
                let mut warden_version = None;
                for entry in tlv::entries(p) {
                    let (k, value) = entry?;
                    match k {
                        key::ADOPT_ID => id = Some(SessionId::from_bytes(value)?),
                        key::ADOPT_CHILD_PID => {
                            child_pid = tlv::read_u32(value, "adopt child pid")?
                        }
                        key::ADOPT_EXIT => exit = Some(Exit::from_bytes(value)?),
                        key::ADOPT_WARDEN_VERSION => {
                            warden_version = Some(tlv::read_text(value, "adopt warden version")?)
                        }
                        _ => {}
                    }
                }
                let id = id.ok_or(Error::Malformed("adopt without an id"))?;
                ToRelay::Adopt(Adopt {
                    id,
                    child_pid,
                    exit,
                    warden_version,
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
            to_relay::RENEWED => {
                let value = last(p, key::TEXT)?.unwrap_or_default();
                ToRelay::Renewed(String::from_utf8_lossy(value).into_owned())
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
            ToWarden::CanRenew { min, max } => {
                let mut w = tlv::Writer::new();
                w.put_u8(key::MIN, *min);
                w.put_u8(key::MAX, *max);
                frame(to_warden::CAN_RENEW, w.finish())
            }
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
            to_warden::CAN_RENEW => {
                let min =
                    last(p, key::MIN)?.ok_or(Error::Malformed("can renew without a minimum"))?;
                let max =
                    last(p, key::MAX)?.ok_or(Error::Malformed("can renew without a maximum"))?;
                ToWarden::CanRenew {
                    min: tlv::read_u8(min, "can renew minimum")?,
                    max: tlv::read_u8(max, "can renew maximum")?,
                }
            }
            _ => return Ok(None),
        }))
    }
}

/// What a warden knew when it replaced its program, for the new image to
/// carry on with. It is the one frame of this wire that goes through a pipe
/// to a process of the same session rather than between a warden and a relay.
///
/// The descriptor numbers are those the new image inherits: the warden takes
/// close-on-exec off them before it replaces itself and the new image puts it
/// back. Only a warden whose shell is alive and whose relay is serving writes
/// one, so what it does not hold (an exit, a relay on its way in, the channel
/// to `kablo new`) is not in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Renewal {
    /// The session.
    pub id: SessionId,
    /// The directory of the session's socket, as bytes.
    pub dir: Vec<u8>,
    /// The shell's process id.
    pub shell_pid: u32,
    /// The terminal's master side.
    pub master: RawFd,
    /// The journal's shared memory.
    pub journal: RawFd,
    /// The listening socket.
    pub listener: RawFd,
    /// The channel to the relay that serves.
    pub relay_chan: RawFd,
    /// The serving relay's process id.
    pub relay_pid: u32,
    /// The number the serving relay has among the relays of this warden.
    pub relay_serial: u64,
    /// The binary the serving relay was started from, as bytes.
    pub relay_binary: Vec<u8>,
    /// The binary relays are started from, as bytes.
    pub relay_exe: Vec<u8>,
    /// How many relays this warden has started.
    pub serials: u64,
    /// How many relays in a row died young.
    pub strikes: u32,
    /// A relay has been set up at least once.
    pub ever_ready: bool,
    /// The most bytes one read of the terminal takes.
    pub chunk: u32,
    /// The shell's end was delivered to a client.
    pub delivered: bool,
}

fn need<'a>(block: &'a [u8], key: u16, what: &'static str) -> Result<&'a [u8]> {
    last(block, key)?.ok_or(Error::Malformed(what))
}

fn flag(block: &[u8], key: u16, what: &'static str) -> Result<bool> {
    match last(block, key)? {
        Some(value) => Ok(tlv::read_u8(value, what)? & 1 != 0),
        None => Ok(false),
    }
}

fn read_fd(block: &[u8], key: u16, what: &'static str) -> Result<RawFd> {
    let number = tlv::read_u32(need(block, key, what)?, what)?;
    RawFd::try_from(number).map_err(|_| Error::Malformed(what))
}

fn put_fd(w: &mut tlv::Writer, key: u16, fd: RawFd, what: &'static str) -> Result<()> {
    let number = u32::try_from(fd).map_err(|_| Error::Malformed(what))?;
    w.put_u32(key, number);
    Ok(())
}

impl Wire for Renewal {
    fn to_frame(&self) -> Result<RawFrame> {
        let mut w = tlv::Writer::new();
        w.put_u8(renewal_key::VERSION, RENEWAL_VERSION);
        w.put(renewal_key::ID, self.id.as_str().as_bytes());
        w.put(renewal_key::DIR, &self.dir);
        w.put_u32(renewal_key::SHELL_PID, self.shell_pid);
        put_fd(
            &mut w,
            renewal_key::MASTER_FD,
            self.master,
            "renewal master",
        )?;
        put_fd(
            &mut w,
            renewal_key::JOURNAL_FD,
            self.journal,
            "renewal journal",
        )?;
        put_fd(
            &mut w,
            renewal_key::LISTENER_FD,
            self.listener,
            "renewal listener",
        )?;
        put_fd(
            &mut w,
            renewal_key::RELAY_FD,
            self.relay_chan,
            "renewal relay channel",
        )?;
        w.put_u32(renewal_key::RELAY_PID, self.relay_pid);
        w.put_u64(renewal_key::RELAY_SERIAL, self.relay_serial);
        w.put(renewal_key::RELAY_BINARY, &self.relay_binary);
        w.put(renewal_key::RELAY_EXE, &self.relay_exe);
        w.put_u64(renewal_key::SERIALS, self.serials);
        w.put_u32(renewal_key::STRIKES, self.strikes);
        w.put_u8(renewal_key::EVER_READY, u8::from(self.ever_ready));
        w.put_u32(renewal_key::CHUNK, self.chunk);
        w.put_u8(renewal_key::DELIVERED, u8::from(self.delivered));
        Ok(frame(record::RENEWAL, w.finish()))
    }

    fn from_frame(f: RawFrame) -> Result<Option<Renewal>> {
        if f.channel != CHANNEL_SESSION || f.kind != record::RENEWAL {
            return Ok(None);
        }
        let p = f.payload.as_slice();
        let version = tlv::read_u8(
            need(p, renewal_key::VERSION, "renewal without a version")?,
            "renewal version",
        )?;
        if !(RENEWAL_MIN_VERSION..=RENEWAL_VERSION).contains(&version) {
            return Err(Error::Malformed(
                "renewal of a version this build does not read",
            ));
        }
        let id = SessionId::from_bytes(need(p, renewal_key::ID, "renewal without an id")?)?;
        let optional_u64 = |key, what| match last(p, key)? {
            Some(value) => tlv::read_u64(value, what),
            None => Ok(0),
        };
        Ok(Some(Renewal {
            id,
            dir: need(p, renewal_key::DIR, "renewal without a directory")?.to_vec(),
            shell_pid: tlv::read_u32(
                need(p, renewal_key::SHELL_PID, "renewal without a shell")?,
                "renewal shell pid",
            )?,
            master: read_fd(p, renewal_key::MASTER_FD, "renewal master")?,
            journal: read_fd(p, renewal_key::JOURNAL_FD, "renewal journal")?,
            listener: read_fd(p, renewal_key::LISTENER_FD, "renewal listener")?,
            relay_chan: read_fd(p, renewal_key::RELAY_FD, "renewal relay channel")?,
            relay_pid: tlv::read_u32(
                need(p, renewal_key::RELAY_PID, "renewal without a relay")?,
                "renewal relay pid",
            )?,
            relay_serial: tlv::read_u64(
                need(
                    p,
                    renewal_key::RELAY_SERIAL,
                    "renewal without a relay number",
                )?,
                "renewal relay serial",
            )?,
            relay_binary: need(
                p,
                renewal_key::RELAY_BINARY,
                "renewal without a relay binary",
            )?
            .to_vec(),
            relay_exe: need(
                p,
                renewal_key::RELAY_EXE,
                "renewal without a binary to start",
            )?
            .to_vec(),
            serials: optional_u64(renewal_key::SERIALS, "renewal serials")?,
            strikes: match last(p, renewal_key::STRIKES)? {
                Some(value) => tlv::read_u32(value, "renewal strikes")?,
                None => 0,
            },
            ever_ready: flag(p, renewal_key::EVER_READY, "renewal ever ready")?,
            chunk: tlv::read_u32(
                need(p, renewal_key::CHUNK, "renewal without a chunk")?,
                "renewal chunk",
            )?,
            delivered: flag(p, renewal_key::DELIVERED, "renewal delivered")?,
        }))
    }
}
