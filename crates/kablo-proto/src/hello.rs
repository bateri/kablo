//! The handshake that opens a connection, before the first frame.
//!
//! ```text
//! client -> server   signature | min: u8 | max: u8
//! server -> client   signature | selected: u8 | min: u8 | max: u8
//! ```
//!
//! `min..=max` is the range of protocol versions the sender speaks; the
//! server picks the highest version both ends speak and says so in
//! `selected`, or says 0 when there is none (and then closes). The client
//! speaks first.
//!
//! A reader skips any bytes before the signature, up to [`NOISE_LIMIT`]: the
//! far end of an `ssh` command may print a banner or an rc file's output
//! before the program starts, and none of that is ours. The reader consumes
//! exactly the handshake and not a byte more, so frames can be read from the
//! same stream right after.

use std::collections::VecDeque;
use std::io::{Read, Write};

use crate::{Error, Result};

/// What the public protocol opens with: the name and a NUL, so the bytes are
/// unlikely to turn up by chance in a banner.
pub const SIGNATURE: &[u8] = b"KABLO\0";

/// How many bytes before the signature are skipped before giving up.
pub const NOISE_LIMIT: usize = 64 * 1024;

/// A protocol a connection can speak: how it announces itself and which
/// versions this build understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Protocol {
    /// The bytes that open a handshake.
    pub signature: &'static [u8],
    /// The lowest version understood (at least 1).
    pub min: u8,
    /// The highest version understood.
    pub max: u8,
}

impl Protocol {
    /// Describes a protocol. `min` must be at least 1 and `max` at least `min`.
    pub const fn new(signature: &'static [u8], min: u8, max: u8) -> Protocol {
        assert!(!signature.is_empty() && min >= 1 && min <= max);
        Protocol {
            signature,
            min,
            max,
        }
    }

    /// Runs the client side: sends the hello, reads the reply and returns the
    /// version the server selected.
    pub fn client<R: Read, W: Write>(&self, input: &mut R, output: &mut W) -> Result<u8> {
        self.write_hello(output)?;
        let reply = self.read_reply(input)?;
        match reply.selected {
            0 => Err(Error::NoCommonVersion {
                peer_min: reply.min,
                peer_max: reply.max,
            }),
            v if (self.min..=self.max).contains(&v) => Ok(v),
            _ => Err(Error::BadHello),
        }
    }

    /// Runs the server side: reads the hello (skipping noise), answers and
    /// returns the selected version. When there is none the answer says so
    /// and the result is [`Error::NoCommonVersion`].
    pub fn server<R: Read, W: Write>(&self, input: &mut R, output: &mut W) -> Result<u8> {
        let (peer_min, peer_max) = self.read_hello(input)?;
        let selected = negotiate((self.min, self.max), (peer_min, peer_max));
        self.write_reply(output, selected.unwrap_or(0))?;
        selected.ok_or(Error::NoCommonVersion { peer_min, peer_max })
    }

    /// Writes the client's hello.
    pub fn write_hello<W: Write>(&self, output: &mut W) -> Result<()> {
        let mut bytes = self.signature.to_vec();
        bytes.extend_from_slice(&[self.min, self.max]);
        output.write_all(&bytes)?;
        output.flush()?;
        Ok(())
    }

    /// Reads a client's hello: the version range it speaks.
    pub fn read_hello<R: Read>(&self, input: &mut R) -> Result<(u8, u8)> {
        skip_to_signature(input, self.signature)?;
        let [min, max] = read_array(input)?;
        check_range(min, max)?;
        Ok((min, max))
    }

    /// Writes the server's reply; `selected` is 0 for "no common version".
    pub fn write_reply<W: Write>(&self, output: &mut W, selected: u8) -> Result<()> {
        let mut bytes = self.signature.to_vec();
        bytes.extend_from_slice(&[selected, self.min, self.max]);
        output.write_all(&bytes)?;
        output.flush()?;
        Ok(())
    }

    /// Reads a server's reply.
    pub fn read_reply<R: Read>(&self, input: &mut R) -> Result<Reply> {
        skip_to_signature(input, self.signature)?;
        let [selected, min, max] = read_array(input)?;
        check_range(min, max)?;
        Ok(Reply { selected, min, max })
    }
}

/// What a server answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reply {
    /// The version chosen, or 0 when the ends share none.
    pub selected: u8,
    /// The lowest version the server speaks.
    pub min: u8,
    /// The highest version the server speaks.
    pub max: u8,
}

/// The highest version both ranges contain.
pub fn negotiate(ours: (u8, u8), theirs: (u8, u8)) -> Option<u8> {
    let high = ours.1.min(theirs.1);
    let low = ours.0.max(theirs.0);
    (low <= high && high >= 1).then_some(high)
}

fn check_range(min: u8, max: u8) -> Result<()> {
    if min == 0 || min > max {
        Err(Error::BadHello)
    } else {
        Ok(())
    }
}

fn read_array<R: Read, const N: usize>(input: &mut R) -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Reads until the signature has gone by, one byte at a time so nothing past
/// it is consumed.
fn skip_to_signature<R: Read>(input: &mut R, signature: &[u8]) -> Result<()> {
    let mut window = VecDeque::with_capacity(signature.len());
    for _ in 0..NOISE_LIMIT {
        let [byte] = read_array(input)?;
        if window.len() == signature.len() {
            window.pop_front();
        }
        window.push_back(byte);
        if window.iter().eq(signature) {
            return Ok(());
        }
    }
    Err(Error::NoSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: Protocol = Protocol::new(SIGNATURE, 1, 3);

    fn with_signature(noise: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut bytes = noise.to_vec();
        bytes.extend_from_slice(SIGNATURE);
        bytes.extend_from_slice(tail);
        bytes
    }

    #[test]
    fn the_highest_common_version_wins() {
        assert_eq!(negotiate((1, 3), (2, 5)), Some(3));
        assert_eq!(negotiate((1, 1), (1, 9)), Some(1));
        assert_eq!(negotiate((1, 2), (3, 4)), None);
        assert_eq!(negotiate((3, 4), (1, 2)), None);
    }

    #[test]
    fn the_server_skips_noise_and_stops_at_the_end_of_the_hello() {
        // Noise that nearly is the signature, the real one, then the first frame bytes.
        let noise = b"Welcome!\nKABLKABLO KABLO\x01KABLO";
        let wire = with_signature(noise, &[1, 2, 0xAA, 0xBB]);
        let mut input = wire.as_slice();
        let mut output = Vec::new();
        assert_eq!(P.server(&mut input, &mut output).unwrap(), 2);
        assert_eq!(input, [0xAA, 0xBB], "nothing past the hello was consumed");
        assert_eq!(output, with_signature(b"", &[2, 1, 3]));
    }

    #[test]
    fn the_client_skips_noise_before_the_reply() {
        let reply = with_signature(b"Last login: yesterday\r\nKABLO", &[2, 1, 3, 0x07]);
        let mut input = reply.as_slice();
        let mut output = Vec::new();
        assert_eq!(P.client(&mut input, &mut output).unwrap(), 2);
        assert_eq!(output, with_signature(b"", &[1, 3]));
        assert_eq!(input, [0x07]);
    }

    #[test]
    fn no_common_version_is_said_on_both_ends() {
        let hello = with_signature(b"", &[5, 6]);
        let mut output = Vec::new();
        let err = P.server(&mut hello.as_slice(), &mut output).unwrap_err();
        assert!(matches!(
            err,
            Error::NoCommonVersion {
                peer_min: 5,
                peer_max: 6
            }
        ));
        assert_eq!(output, with_signature(b"", &[0, 1, 3]));
        let err = P
            .client(&mut output.as_slice(), &mut Vec::new())
            .unwrap_err();
        assert!(matches!(
            err,
            Error::NoCommonVersion {
                peer_min: 1,
                peer_max: 3
            }
        ));
    }

    #[test]
    fn an_empty_range_or_a_selection_we_do_not_speak_is_refused() {
        for tail in [[0, 3], [3, 1]] {
            let err = P
                .server(&mut with_signature(b"", &tail).as_slice(), &mut Vec::new())
                .unwrap_err();
            assert!(matches!(err, Error::BadHello));
        }
        let reply = with_signature(b"", &[9, 1, 9]);
        assert!(matches!(
            P.client(&mut reply.as_slice(), &mut Vec::new()),
            Err(Error::BadHello)
        ));
    }

    #[test]
    fn a_stream_without_the_signature_ends_in_an_error_not_a_hang() {
        let noise = vec![b'x'; NOISE_LIMIT + 100];
        assert!(matches!(
            P.read_hello(&mut noise.as_slice()),
            Err(Error::NoSignature)
        ));
        let short = b"KABLO no";
        match P.read_hello(&mut short.as_slice()) {
            Err(Error::Io(e)) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
            other => panic!("{other:?}"),
        }
        // The signature right at the edge of the budget is still found.
        let mut edge = vec![b'x'; NOISE_LIMIT - SIGNATURE.len()];
        edge.extend_from_slice(SIGNATURE);
        edge.extend_from_slice(&[1, 1]);
        assert_eq!(P.read_hello(&mut edge.as_slice()).unwrap(), (1, 1));
    }

    #[test]
    fn another_protocols_signature_is_noise() {
        const OTHER: Protocol = Protocol::new(b"KABLOW\0", 1, 1);
        let wire = [OTHER.signature, &[1, 1][..], SIGNATURE, &[1, 1][..]].concat();
        assert_eq!(P.read_hello(&mut wire.as_slice()).unwrap(), (1, 1));
        assert!(
            OTHER
                .read_hello(&mut with_signature(b"", &[1, 1]).as_slice())
                .is_err()
        );
    }

    #[test]
    fn any_noise_before_the_hello_is_skipped() {
        // Fixed-seed pseudo-random bytes of every length up to a few KiB.
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut noise = Vec::new();
        for len in (0..3000).step_by(7) {
            while noise.len() < len {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                noise.push(x.to_be_bytes().first().copied().unwrap_or(0));
            }
            let wire = with_signature(&noise, &[1, 3, 0x55]);
            let mut input = wire.as_slice();
            assert_eq!(
                P.read_hello(&mut input).unwrap(),
                (1, 3),
                "{len} bytes of noise"
            );
            assert_eq!(input, [0x55]);
        }
    }
}
