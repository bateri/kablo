//! Key-length-value blocks: the shape of every payload that may grow.
//!
//! A block is a run of entries, each `key: u16`, `length: u32`, then `length`
//! bytes of value (big-endian). A reader skips a key it does not know; a key
//! never changes meaning or type, so growth is a new key. A key may repeat
//! (a list is one entry per element, in order); for a key that is not a list
//! the last entry wins.

use crate::{Error, Result};

/// The bytes before each value: key (2) and length (4).
pub const ENTRY_HEADER_LEN: usize = 6;

/// Builds a block.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    /// An empty block.
    pub fn new() -> Writer {
        Writer::default()
    }

    /// Appends an entry. A value too long for the length field writes a
    /// saturated length; such a value is longer than any frame may carry, so
    /// the frame layer rejects the block before it reaches a peer.
    pub fn put(&mut self, key: u16, value: &[u8]) -> &mut Writer {
        let len = u32::try_from(value.len()).unwrap_or(u32::MAX);
        self.buf.extend_from_slice(&key.to_be_bytes());
        self.buf.extend_from_slice(&len.to_be_bytes());
        self.buf.extend_from_slice(value);
        self
    }

    /// Appends a one-byte entry.
    pub fn put_u8(&mut self, key: u16, value: u8) -> &mut Writer {
        self.put(key, &[value])
    }

    /// Appends a four-byte entry.
    pub fn put_u32(&mut self, key: u16, value: u32) -> &mut Writer {
        self.put(key, &value.to_be_bytes())
    }

    /// Appends an eight-byte entry.
    pub fn put_u64(&mut self, key: u16, value: u64) -> &mut Writer {
        self.put(key, &value.to_be_bytes())
    }

    /// The finished block.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// The entries of a block, in order. A truncated entry yields one
/// `Err(Malformed)` and then the iterator ends.
#[derive(Debug, Clone)]
pub struct Entries<'a> {
    rest: &'a [u8],
}

/// Walks the entries of a block.
pub fn entries(block: &[u8]) -> Entries<'_> {
    Entries { rest: block }
}

impl<'a> Iterator for Entries<'a> {
    type Item = Result<(u16, &'a [u8])>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let rest = self.rest;
        let parsed = (|| {
            let (head, tail) = rest.split_first_chunk::<ENTRY_HEADER_LEN>()?;
            let [k0, k1, l0, l1, l2, l3] = *head;
            let len = usize::try_from(u32::from_be_bytes([l0, l1, l2, l3])).ok()?;
            let (value, tail) = tail.split_at_checked(len)?;
            Some((u16::from_be_bytes([k0, k1]), value, tail))
        })();
        match parsed {
            Some((key, value, tail)) => {
                self.rest = tail;
                Some(Ok((key, value)))
            }
            None => {
                self.rest = &[];
                Some(Err(Error::Malformed("tlv entry")))
            }
        }
    }
}

fn exact<const N: usize>(value: &[u8], what: &'static str) -> Result<[u8; N]> {
    <[u8; N]>::try_from(value).map_err(|_| Error::Malformed(what))
}

/// Reads a one-byte value; any other length is malformed.
pub fn read_u8(value: &[u8], what: &'static str) -> Result<u8> {
    Ok(u8::from_be_bytes(exact(value, what)?))
}

/// Reads a four-byte value; any other length is malformed.
pub fn read_u32(value: &[u8], what: &'static str) -> Result<u32> {
    Ok(u32::from_be_bytes(exact(value, what)?))
}

/// Reads an eight-byte value; any other length is malformed.
pub fn read_u64(value: &[u8], what: &'static str) -> Result<u64> {
    Ok(u64::from_be_bytes(exact(value, what)?))
}

/// Reads a UTF-8 text value.
pub fn read_text(value: &[u8], what: &'static str) -> Result<String> {
    String::from_utf8(value.to_vec()).map_err(|_| Error::Malformed(what))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_come_back_in_order_and_repeat() {
        let mut w = Writer::new();
        w.put(1, b"a").put(2, b"").put(1, b"bc").put_u32(3, 7);
        let block = w.finish();
        let got: Vec<_> = entries(&block).collect::<Result<_>>().unwrap();
        assert_eq!(
            got,
            [(1, &b"a"[..]), (2, b""), (1, b"bc"), (3, &[0, 0, 0, 7][..])]
        );
    }

    #[test]
    fn a_truncated_entry_is_one_error_and_the_end() {
        let mut w = Writer::new();
        w.put(1, b"abc");
        let mut block = w.finish();
        block.pop();
        let mut it = entries(&block);
        assert!(matches!(it.next(), Some(Err(Error::Malformed(_)))));
        assert!(it.next().is_none());
        // Half an entry header.
        assert!(matches!(entries(&[0, 1, 0]).next(), Some(Err(_))));
        // A length that points past the end must not allocate or panic.
        assert!(matches!(
            entries(&[0, 1, 0xff, 0xff, 0xff, 0xff]).next(),
            Some(Err(_))
        ));
    }

    #[test]
    fn scalars_must_have_their_exact_length() {
        assert_eq!(read_u32(&[0, 0, 1, 0], "x").unwrap(), 256);
        assert!(read_u32(&[0, 0, 1], "x").is_err());
        assert!(read_u32(&[0, 0, 0, 0, 1], "x").is_err());
        assert_eq!(read_u8(&[9], "x").unwrap(), 9);
        assert!(read_u8(&[], "x").is_err());
        assert!(read_u64(&[0; 7], "x").is_err());
        assert!(read_text(&[0xff], "x").is_err());
    }
}
