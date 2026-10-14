//! The error type of every layer of the codec.

use std::{fmt, io};

/// What can go wrong while reading or writing the wire.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The underlying reader or writer failed. A stream that ends in the
    /// middle of a frame is [`io::ErrorKind::UnexpectedEof`] here; one that
    /// ends between frames is not an error (`Ok(None)`).
    Io(io::Error),
    /// A frame arrived whole but its contents do not parse. The stream is still
    /// aligned on a frame boundary, so the caller may answer with an `error`
    /// message and carry on ([`Error::is_recoverable`]).
    Malformed(&'static str),
    /// A header declared a payload over [`crate::MAX_PAYLOAD`], or a payload
    /// to be written is over it. Nothing was allocated for it. The stream
    /// cannot be trusted past this point: drop the connection.
    PayloadTooLarge {
        /// The declared (or attempted) payload length.
        len: usize,
    },
    /// A header declared a session id over [`crate::MAX_SESSION_LEN`] bytes,
    /// or one to be written is. Drop the connection.
    SessionTooLong {
        /// The declared (or attempted) id length.
        len: usize,
    },
    /// The signature did not show up within the noise budget
    /// ([`crate::NOISE_LIMIT`] bytes): the other end is not speaking this
    /// protocol.
    NoSignature,
    /// The handshake carried a version range that is empty or starts at 0.
    BadHello,
    /// The two ends share no protocol version.
    NoCommonVersion {
        /// The lowest version the peer speaks.
        peer_min: u8,
        /// The highest version the peer speaks.
        peer_max: u8,
    },
}

/// The codec's result type.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Whether the stream is still aligned on a frame boundary after this
    /// error, so reading can go on. Only [`Error::Malformed`] is: every other
    /// error means the framing itself is broken or the peer is not speaking
    /// the protocol.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Error::Malformed(_))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Malformed(what) => write!(f, "malformed {what}"),
            Error::PayloadTooLarge { len } => write!(f, "payload of {len} bytes is over the limit"),
            Error::SessionTooLong { len } => {
                write!(f, "session id of {len} bytes is over the limit")
            }
            Error::NoSignature => write!(f, "no protocol signature from the peer"),
            Error::BadHello => write!(f, "handshake with an empty version range"),
            Error::NoCommonVersion { peer_min, peer_max } => {
                write!(
                    f,
                    "no common protocol version (peer speaks {peer_min}..={peer_max})"
                )
            }
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}
