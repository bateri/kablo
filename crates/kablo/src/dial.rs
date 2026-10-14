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
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
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

/// How a name becomes addresses: the system's resolver, or a test's own.
pub type Resolve<'a> = &'a dyn Fn(&str, u16) -> io::Result<Vec<SocketAddr>>;

/// The system's resolver (`getaddrinfo`, which has no timeout of its own).
fn system(host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
    (host, port)
        .to_socket_addrs()
        .map(|found| found.collect::<Vec<_>>())
}

/// Resolves the target's host and connects to the first address that takes the
/// connection, all before `deadline`. The addresses are tried in the order the
/// resolver gives (an IPv6 address that refuses is followed by the IPv4 one of
/// the same name); the refusal is the last one's.
pub fn connect(target: &Target, deadline: Instant) -> Result<TcpStream, Refusal> {
    connect_via(target, deadline, &system)
}

/// [`connect`] with `resolve` for the name. A resolver that fails, whatever the
/// way, or finds nothing is [`FailCode::UNRESOLVED`]: the error of
/// `getaddrinfo` has no kind that says "the name does not exist", and none is
/// needed, since nothing is connected to yet.
pub fn connect_via(
    target: &Target,
    deadline: Instant,
    resolve: Resolve<'_>,
) -> Result<TcpStream, Refusal> {
    let addresses = match resolve(&target.host, target.port) {
        Ok(found) if !found.is_empty() => found,
        _ => return Err(unresolved()),
    };
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

    /// An address that refuses connections: a port below the range the system
    /// hands out, which no listener of a test running side by side can be
    /// given, and nothing listens on. (A port that was bound and let go can be
    /// given to one of them before the connection is made.)
    fn refusing() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 1))
    }

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
        let address = refusing();
        let target = Target {
            host: "127.0.0.1".to_owned(),
            port: address.port(),
        };
        let why = connect(&target, soon()).unwrap_err();
        assert_eq!(why.code, FailCode::REFUSED);
        assert_eq!(why.text, "connection refused");
    }

    fn target(host: &str, port: u16) -> Target {
        Target {
            host: host.to_owned(),
            port,
        }
    }

    #[test]
    fn a_resolver_that_fails_or_finds_nothing_is_unresolved() {
        // The system's resolver is not asked: a lookup of a name that is not
        // there takes as long as the network says, and none of this is about
        // the network.
        let failing = |_: &str, _: u16| Err(io::Error::other("no such host"));
        let empty = |_: &str, _: u16| Ok(Vec::new());
        for (name, resolve) in [("failing", &failing as Resolve), ("empty", &empty)] {
            let why = connect_via(&target("nowhere.example", 80), soon(), resolve).unwrap_err();
            assert_eq!(why.code, FailCode::UNRESOLVED, "{name}");
            assert_eq!(why.text, "name not found", "{name}");
        }
    }

    #[test]
    fn the_resolver_is_given_the_name_and_the_port_as_they_came() {
        let asked = std::sync::Mutex::new(Vec::new());
        let resolve = |host: &str, port: u16| {
            asked.lock().unwrap().push((host.to_owned(), port));
            Err(io::Error::other("no such host"))
        };
        let _ = connect_via(&target("db.internal", 5432), soon(), &resolve);
        assert_eq!(*asked.lock().unwrap(), [("db.internal".to_owned(), 5432)]);
    }

    #[test]
    fn the_addresses_of_a_name_are_tried_in_order_until_one_takes_the_connection() {
        let closed = refusing();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let open = listener.local_addr().unwrap();
        let resolve = move |_: &str, _: u16| Ok(vec![closed, open]);
        let socket = connect_via(&target("two.example", 1), soon(), &resolve).unwrap();
        assert_eq!(socket.peer_addr().unwrap(), open);

        // When none does, the refusal is the last one's.
        let also = refusing();
        let resolve = move |_: &str, _: u16| Ok(vec![closed, also]);
        let why = connect_via(&target("two.example", 1), soon(), &resolve).unwrap_err();
        assert_eq!(why.code, FailCode::REFUSED);
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
