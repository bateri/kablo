//! The wire format of kablo, a transparent terminal session keeper.
//!
//! This crate is the whole protocol and nothing else: it reads and writes
//! bytes through [`std::io::Read`] and [`std::io::Write`] (or a buffer for a
//! reader that cannot block) and does no I/O of its own, so a client in any
//! program, on any transport that carries bytes, can speak to a kablo server
//! with it. `docs/PROTOCOL.md` in the repository describes the bytes.
//!
//! Beside the wire, [`install`] holds the rules a client shares for putting a
//! kablo binary on a server: which target a server is, what the binary of a
//! release is called and where it goes.
//!
//! A connection is a [`hello`] handshake, then [`frame`]s. A frame is a header
//! (kind, channel, session id, payload length) and a payload; the payload of
//! the frames in use is one of the [`v1`] messages: a [`v1::Packet`] on the
//! session channel, a [`v1::stream::Packet`] on the channel of streams (TCP
//! connections the server makes for the client, and programs it runs). Nothing
//! about the terminal's contents is in the protocol: the program's bytes travel
//! inside `output` and `input` payloads untouched.
//!
//! # Compatibility
//!
//! Every end speaks a range of versions and the server picks the highest both
//! speak. A build reads **the current version and the one before it**, so a
//! client and a server one release apart still understand each other. The
//! bytes of each message of each version are kept as fixtures that a test
//! reads and compares; a fixture is never edited, a change of format is a new
//! version with new fixtures. Inside a version the format still grows: a frame
//! of a kind or on a channel the reader does not know is skipped by its length,
//! a key it does not know in a [`tlv`] block is skipped, and a fixed layout may
//! gain fields at its end.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod error;
pub mod frame;
pub mod hello;
pub mod install;
pub mod session;
pub mod tlv;
pub mod v1;

pub use error::{Error, Result};
pub use frame::{CHANNEL_SESSION, CHANNEL_STREAM, MAX_PAYLOAD, MAX_SESSION_LEN};
pub use hello::{NOISE_LIMIT, Protocol, SIGNATURE};
pub use session::SessionId;

/// The lowest wire version this build reads.
pub const MIN_VERSION: u8 = 1;
/// The highest wire version this build reads.
pub const MAX_VERSION: u8 = 1;

/// The public protocol, as this build speaks it.
pub const WIRE: Protocol = Protocol::new(SIGNATURE, MIN_VERSION, MAX_VERSION);
