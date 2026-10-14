//! Reaching the target of a stream: resolving a name and connecting, by a
//! deadline.
//!
//! Both steps block (a resolver has no timeout of its own), so they run on a
//! thread of their own, one per `open` (see [`crate::streams`]), and never on
//! the bridge's loop. What comes back is a connected socket or a
//! [`Refusal`]: the failure code of the wire and a few words of English that
//! read after a colon ("connection refused"), short enough for the line a
//! person sees ("Can't forward :8080 — connection refused").

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Instant;

use kablo_proto::v1::stream::FailCode;

/// Where a stream is to connect: what an `open` carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// A name or an address (IPv6 without brackets).
    pub host: String,
    /// The port; never 0.
    pub port: u16,
}

/// Why a target was not reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// What the wire calls it.
    pub code: FailCode,
    /// A few words of English.
    pub text: String,
}

impl Refusal {
    /// A refusal with this code and text.
    pub fn new(code: FailCode, text: &str) -> Refusal {
        Refusal {
            code,
            text: text.to_owned(),
        }
    }
}

/// Resolves the target's host and connects to the first address that takes the
/// connection, all before `deadline`. The addresses are tried in the order the
/// resolver gives (an IPv6 address that refuses is followed by the IPv4 one of
/// the same name); the refusal is the last one's.
pub fn connect(target: &Target, deadline: Instant) -> Result<TcpStream, Refusal> {
    let addresses = match (target.host.as_str(), target.port).to_socket_addrs() {
        Ok(found) => found.collect::<Vec<_>>(),
        Err(_) => return Err(unresolved()),
    };
    if addresses.is_empty() {
        return Err(unresolved());
    }
    let mut last = Refusal::new(FailCode::UNREACHABLE, "could not connect");
    for address in addresses {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(Refusal::new(FailCode::TIMED_OUT, "timed out"));
        }
        match TcpStream::connect_timeout(&address, left) {
            Ok(socket) => return Ok(socket),
            Err(error) => last = refusal(&error),
        }
    }
    Err(last)
}

fn unresolved() -> Refusal {
    Refusal::new(FailCode::UNRESOLVED, "name not found")
}

/// What a failed connect is called on the wire.
pub fn refusal(error: &io::Error) -> Refusal {
    use io::ErrorKind;
    let (code, text) = match error.kind() {
        ErrorKind::ConnectionRefused => (FailCode::REFUSED, "connection refused"),
        ErrorKind::TimedOut | ErrorKind::WouldBlock => (FailCode::TIMED_OUT, "timed out"),
        ErrorKind::HostUnreachable => (FailCode::UNREACHABLE, "no route to host"),
        ErrorKind::NetworkUnreachable => (FailCode::UNREACHABLE, "network is unreachable"),
        _ => (FailCode::UNREACHABLE, "could not connect"),
    };
    Refusal::new(code, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::time::Duration;

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[test]
    fn a_listening_port_is_reached_by_address_and_by_name() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        for host in ["127.0.0.1", "localhost"] {
            let target = Target {
                host: host.to_owned(),
                port,
            };
            connect(&target, soon()).unwrap_or_else(|why| panic!("{host}: {why:?}"));
        }
    }

    #[test]
    fn a_port_nobody_listens_on_is_refused() {
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let target = Target {
            host: "127.0.0.1".to_owned(),
            port,
        };
        let why = connect(&target, soon()).unwrap_err();
        assert_eq!(why.code, FailCode::REFUSED);
        assert_eq!(why.text, "connection refused");
    }

    #[test]
    fn a_name_that_does_not_exist_is_unresolved() {
        let target = Target {
            host: "kablo-test.invalid".to_owned(),
            port: 80,
        };
        assert_eq!(
            connect(&target, soon()).unwrap_err().code,
            FailCode::UNRESOLVED
        );
    }

    #[test]
    fn a_deadline_that_has_passed_is_a_timeout() {
        let target = Target {
            host: "127.0.0.1".to_owned(),
            port: 9,
        };
        let why = connect(&target, Instant::now()).unwrap_err();
        assert_eq!(why.code, FailCode::TIMED_OUT);
    }

    #[test]
    fn each_kind_of_failure_has_its_code() {
        for (kind, code) in [
            (io::ErrorKind::ConnectionRefused, FailCode::REFUSED),
            (io::ErrorKind::TimedOut, FailCode::TIMED_OUT),
            (io::ErrorKind::HostUnreachable, FailCode::UNREACHABLE),
            (io::ErrorKind::NetworkUnreachable, FailCode::UNREACHABLE),
            (io::ErrorKind::PermissionDenied, FailCode::UNREACHABLE),
        ] {
            assert_eq!(refusal(&io::Error::from(kind)).code, code, "{kind:?}");
        }
    }
}
