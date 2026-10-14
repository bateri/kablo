//! The server side of a handshake for a reader that cannot block.
//!
//! [`kablo_proto::Protocol::server`] reads from a `Read`. A socket in
//! non-blocking mode hands over the bytes that happen to have arrived, so the
//! caller keeps them and asks again each time more come: a hello that is not
//! all there yet reads as an unexpected end, which here means "wait". Nothing
//! past the hello is consumed.

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
}
