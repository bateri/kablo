//! Both sides of a handshake for a reader that cannot block.
//!
//! [`kablo_proto::Protocol::server`] and [`kablo_proto::Protocol::read_reply`]
//! read from a `Read`. A socket in
//! non-blocking mode hands over the bytes that happen to have arrived, so the
//! caller keeps them and asks again each time more come: a hello that is not
//! all there yet reads as an unexpected end, which here means "wait". Nothing
//! past the hello is consumed.
//!
//! [`Opening`] is that keeping, for a connection that goes on to speak packets:
//! it holds the bytes of the hello and hands what follows it to the
//! connection's decoder.

use std::io::ErrorKind;

use kablo_proto::{Error, Protocol, Result};

/// Where a handshake stands.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// The hello is not all here yet.
    Need,
    /// The hello was read: answer with `reply`, speak `version`, and the
    /// bytes after the first `consumed` are the stream that follows.
    Done {
        /// The version selected.
        version: u8,
        /// The bytes to send back.
        reply: Vec<u8>,
        /// How many of the bytes given were the hello.
        consumed: usize,
    },
    /// The peer speaks no version we do: send `reply` (it says so) and close.
    Refused {
        /// The bytes to send back.
        reply: Vec<u8>,
    },
}

/// Tries to read a client's hello from `bytes`, everything received so far.
pub fn serve(protocol: &Protocol, bytes: &[u8]) -> Result<Step> {
    let mut input = bytes;
    let mut reply = Vec::new();
    match protocol.server(&mut input, &mut reply) {
        Ok(version) => Ok(Step::Done {
            version,
            reply,
            consumed: bytes.len().saturating_sub(input.len()),
        }),
        Err(Error::Io(e)) if e.kind() == ErrorKind::UnexpectedEof => Ok(Step::Need),
        Err(Error::NoCommonVersion { .. }) => Ok(Step::Refused { reply }),
        Err(e) => Err(e),
    }
}

/// Where the client side of a handshake stands.
#[derive(Debug, PartialEq, Eq)]
pub enum ClientStep {
    /// The server's reply is not all here yet.
    Need,
    /// The reply was read: speak `version`; the bytes after the first
    /// `consumed` are the stream that follows.
    Done {
        /// The version selected.
        version: u8,
        /// How many of the bytes given were the reply (and the noise before
        /// it).
        consumed: usize,
    },
    /// The server speaks no version we do.
    Refused,
}

/// Tries to read a server's reply from `bytes`, everything received so far.
/// Bytes before the signature (a banner, a startup file's output) are skipped.
pub fn reply(protocol: &Protocol, bytes: &[u8]) -> Result<ClientStep> {
    let mut input = bytes;
    match protocol.read_reply(&mut input) {
        Ok(reply) if reply.selected == 0 => Ok(ClientStep::Refused),
        Ok(reply) if (protocol.min..=protocol.max).contains(&reply.selected) => {
            Ok(ClientStep::Done {
                version: reply.selected,
                consumed: bytes.len().saturating_sub(input.len()),
            })
        }
        Ok(_) => Err(Error::BadHello),
        Err(Error::Io(e)) if e.kind() == ErrorKind::UnexpectedEof => Ok(ClientStep::Need),
        Err(e) => Err(e),
    }
}

/// The start of one connection: the bytes that arrive are the hello until it
/// is whole, and the stream after that.
#[derive(Debug, Default)]
pub struct Opening {
    /// Bytes received before the handshake is complete.
    hello: Vec<u8>,
    open: bool,
}

/// How the server end of an [`Opening`] stands.
#[derive(Debug, PartialEq, Eq)]
pub enum Served {
    /// The hello is not all here yet.
    Waiting,
    /// The handshake is over: send `reply` (empty after the first time), and
    /// the stream is in the decoder.
    Open(Vec<u8>),
    /// The peer speaks no version we do: send `reply` (it says so) and close.
    Refused(Vec<u8>),
}

/// How the client end of an [`Opening`] stands.
#[derive(Debug, PartialEq, Eq)]
pub enum Answered {
    /// The server's reply is not all here yet.
    Waiting,
    /// The handshake is over, and the stream is in the decoder.
    Open,
    /// The server speaks no version we do.
    Refused,
}

impl Opening {
    /// Whether the handshake is over.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Bytes arrived at the end that was connected to. While the hello is not
    /// whole they are kept; the bytes behind it, and every byte after, go to
    /// `stream`, whole or in part, as they come. An error is a hello that no
    /// version of the protocol starts with.
    pub fn serve(
        &mut self,
        protocol: &Protocol,
        bytes: &[u8],
        stream: impl FnOnce(&[u8]),
    ) -> Result<Served> {
        if self.open {
            stream(bytes);
            return Ok(Served::Open(Vec::new()));
        }
        self.hello.extend_from_slice(bytes);
        Ok(match serve(protocol, &self.hello)? {
            Step::Need => Served::Waiting,
            Step::Done {
                reply, consumed, ..
            } => {
                self.finish(consumed, stream);
                Served::Open(reply)
            }
            Step::Refused { reply } => Served::Refused(reply),
        })
    }

    /// Bytes arrived at the end that connected, as [`Opening::serve`] takes
    /// them at the end that was connected to.
    pub fn answer(
        &mut self,
        protocol: &Protocol,
        bytes: &[u8],
        stream: impl FnOnce(&[u8]),
    ) -> Result<Answered> {
        if self.open {
            stream(bytes);
            return Ok(Answered::Open);
        }
        self.hello.extend_from_slice(bytes);
        Ok(match reply(protocol, &self.hello)? {
            ClientStep::Need => Answered::Waiting,
            ClientStep::Done { consumed, .. } => {
                self.finish(consumed, stream);
                Answered::Open
            }
            ClientStep::Refused => Answered::Refused,
        })
    }

    /// The hello was the first `consumed` bytes: what follows is the stream.
    fn finish(&mut self, consumed: usize, stream: impl FnOnce(&[u8])) {
        stream(self.hello.get(consumed..).unwrap_or_default());
        self.hello = Vec::new();
        self.open = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kablo_proto::WIRE;

    fn hello() -> Vec<u8> {
        let mut bytes = Vec::new();
        WIRE.write_hello(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn a_hello_that_arrives_in_pieces_is_waited_for() {
        let bytes = hello();
        for cut in 0..bytes.len() {
            assert_eq!(serve(&WIRE, &bytes[..cut]).unwrap(), Step::Need, "{cut}");
        }
        let mut stream = b"banner\n".to_vec();
        stream.extend_from_slice(&bytes);
        stream.extend_from_slice(&[9, 9]);
        match serve(&WIRE, &stream).unwrap() {
            Step::Done {
                version,
                consumed,
                reply,
            } => {
                assert_eq!(version, 1);
                assert_eq!(consumed, stream.len() - 2, "the frames after it are left");
                assert!(!reply.is_empty());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_peer_with_no_common_version_is_told_so() {
        let mut bytes = WIRE.signature.to_vec();
        bytes.extend_from_slice(&[7, 9]);
        let Step::Refused { reply } = serve(&WIRE, &bytes).unwrap() else {
            panic!("expected a refusal");
        };
        assert!(reply.starts_with(WIRE.signature));
        assert_eq!(reply.get(WIRE.signature.len()), Some(&0));
    }

    #[test]
    fn noise_without_a_signature_is_an_error_once_it_is_too_long() {
        let noise = vec![b'x'; kablo_proto::NOISE_LIMIT + 8];
        assert!(serve(&WIRE, &noise).is_err());
        assert_eq!(serve(&WIRE, b"just a banner").unwrap(), Step::Need);
    }
    #[test]
    fn a_reply_that_arrives_in_pieces_is_waited_for() {
        let mut bytes = b"motd\n".to_vec();
        WIRE.write_reply(&mut bytes, 1).unwrap();
        for cut in 0..bytes.len() {
            assert_eq!(
                reply(&WIRE, &bytes[..cut]).unwrap(),
                ClientStep::Need,
                "{cut}"
            );
        }
        bytes.extend_from_slice(&[7, 7]);
        assert_eq!(
            reply(&WIRE, &bytes).unwrap(),
            ClientStep::Done {
                version: 1,
                consumed: bytes.len() - 2
            }
        );
    }

    #[test]
    fn a_server_with_nothing_in_common_is_refused() {
        let mut bytes = Vec::new();
        WIRE.write_reply(&mut bytes, 0).unwrap();
        assert_eq!(reply(&WIRE, &bytes).unwrap(), ClientStep::Refused);
    }

    #[test]
    fn a_server_opening_keeps_the_hello_and_hands_the_stream_on() {
        let bytes = hello();
        let mut opening = Opening::default();
        let mut stream = Vec::new();
        for byte in bytes.get(..bytes.len() - 1).unwrap() {
            let served = opening.serve(&WIRE, &[*byte], |b| stream.extend_from_slice(b));
            assert_eq!(served.unwrap(), Served::Waiting);
        }
        assert!(!opening.is_open());
        let last = [bytes.last().copied().unwrap(), 1, 2];
        let Served::Open(reply) = opening
            .serve(&WIRE, &last, |b| stream.extend_from_slice(b))
            .unwrap()
        else {
            panic!("expected the handshake to be over");
        };
        assert!(!reply.is_empty());
        assert!(opening.is_open());
        assert_eq!(stream, [1, 2], "what follows the hello, and not the hello");
        let served = opening.serve(&WIRE, &[3], |b| stream.extend_from_slice(b));
        assert_eq!(
            served.unwrap(),
            Served::Open(Vec::new()),
            "the reply goes out once"
        );
        assert_eq!(stream, [1, 2, 3]);
    }

    #[test]
    fn a_server_opening_refuses_a_peer_with_no_common_version_and_a_peer_that_is_noise() {
        let mut bytes = WIRE.signature.to_vec();
        bytes.extend_from_slice(&[7, 9]);
        let mut opening = Opening::default();
        let served = opening.serve(&WIRE, &bytes, |_| panic!("no stream"));
        let Served::Refused(reply) = served.unwrap() else {
            panic!("expected a refusal");
        };
        assert!(reply.starts_with(WIRE.signature));
        assert!(!opening.is_open());

        let noise = vec![b'x'; kablo_proto::NOISE_LIMIT + 8];
        let mut opening = Opening::default();
        assert!(
            opening
                .serve(&WIRE, &noise, |_| panic!("no stream"))
                .is_err()
        );
    }

    #[test]
    fn a_client_opening_skips_the_banner_and_hands_the_stream_on() {
        let mut bytes = b"motd\n".to_vec();
        WIRE.write_reply(&mut bytes, 1).unwrap();
        let whole = bytes.len();
        bytes.extend_from_slice(&[7, 7]);
        let mut opening = Opening::default();
        let mut stream = Vec::new();
        let answered = opening.answer(&WIRE, &bytes[..whole - 1], |b| stream.extend_from_slice(b));
        assert_eq!(answered.unwrap(), Answered::Waiting);
        assert!(!opening.is_open());
        let answered = opening.answer(&WIRE, &bytes[whole - 1..], |b| stream.extend_from_slice(b));
        assert_eq!(answered.unwrap(), Answered::Open);
        assert!(opening.is_open());
        assert_eq!(stream, [7, 7]);
        let answered = opening.answer(&WIRE, &[8], |b| stream.extend_from_slice(b));
        assert_eq!(answered.unwrap(), Answered::Open);
        assert_eq!(stream, [7, 7, 8]);
    }

    #[test]
    fn a_client_opening_is_refused_by_a_server_with_nothing_in_common() {
        let mut bytes = Vec::new();
        WIRE.write_reply(&mut bytes, 0).unwrap();
        let mut opening = Opening::default();
        let answered = opening.answer(&WIRE, &bytes, |_| panic!("no stream"));
        assert_eq!(answered.unwrap(), Answered::Refused);
        assert!(!opening.is_open());
    }
}
