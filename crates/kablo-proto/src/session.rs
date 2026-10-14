//! Session identifiers.

use std::{fmt, str::FromStr};

use crate::{Error, Result};

/// The name of a session: 1 to [`SessionId::MAX_LEN`] bytes of
/// `[A-Za-z0-9._-]`. The relay's socket is `<id>.sock` in the socket
/// directory, so the alphabet has no separator and no way to name a parent
/// directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(String);

impl SessionId {
    /// The longest id, in bytes.
    pub const MAX_LEN: usize = 64;

    /// Validates an id.
    pub fn new(id: &str) -> Result<SessionId> {
        let valid = !id.is_empty()
            && id.len() <= Self::MAX_LEN
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
        if valid {
            Ok(SessionId(id.to_owned()))
        } else {
            Err(Error::Malformed("session id"))
        }
    }

    /// Validates an id read off the wire.
    pub fn from_bytes(id: &[u8]) -> Result<SessionId> {
        match std::str::from_utf8(id) {
            Ok(id) => SessionId::new(id),
            Err(_) => Err(Error::Malformed("session id")),
        }
    }

    /// The id as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for SessionId {
    type Err = Error;

    fn from_str(s: &str) -> Result<SessionId> {
        SessionId::new(s)
    }
}

impl AsRef<str> for SessionId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_a_small_alphabet() {
        for ok in ["a", "work", "Work_1.x-y", ".hidden", &"a".repeat(64)] {
            assert_eq!(SessionId::new(ok).unwrap().as_str(), ok);
        }
        for bad in ["", "a/b", "a b", "ü", "a\0b", "a\n", &"a".repeat(65)] {
            assert!(SessionId::new(bad).is_err(), "{bad:?}");
        }
        assert!(SessionId::from_bytes(&[0xff, 0xfe]).is_err());
    }
}
