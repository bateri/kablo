//! A connection to a session's socket, from the client's side, for a loop
//! that must not block.
//!
//! The relay serves one session on one socket. Whoever wants to talk to it (the
//! bridge, `kablo connect`, `kablo list`) holds a [`Peer`]: a non-blocking
//! socket with an output buffer, the handshake and a frame decoder. It does
//! not decide when to read or write; the owner puts [`Peer::fd`] and
//! [`Peer::interest`] in its `poll` set and calls [`Peer::flush`] and
//! [`Peer::read`] when they are ready.
//!
//! The hello goes out in the same write as whatever follows it: the relay
//! reads the hello and then the frames behind it in whatever order they come,
//! so a request does not wait for the reply to the handshake.

use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use kablo_proto::WIRE;
use kablo_proto::v1::{Packet, PacketDecoder};

use crate::handshake::{self, ClientStep};
use crate::sys;

/// Reads of the socket one call of [`Peer::read`] makes at most, so one busy
/// session does not keep the rest of a loop waiting.
const READ_BUDGET: usize = 8;

/// Why a connection is of no more use.
#[derive(Debug)]
pub enum Gone {
    /// The other end closed it (or the socket failed).
    Closed,
    /// The other end speaks no version we do.
    Refused,
    /// The other end sent what no part of the protocol allows.
    Broken(String),
}

/// A connection to a relay.
#[derive(Debug)]
pub struct Peer {
    sock: UnixStream,
    out: Vec<u8>,
    sent: usize,
    /// Bytes received before the handshake is complete.
    hello: Vec<u8>,
    open: bool,
    decoder: PacketDecoder,
    /// Where reads land; kept so each read does not allocate.
    buf: Vec<u8>,
}

impl Peer {
    /// Connects to the socket at `path`. `Ok` says a listener took the
    /// connection, not that anybody is serving it yet: whether the relay
    /// answers is for [`Peer::is_open`] to say, later. The process on the
    /// other end must be ours.
    pub fn connect(path: &Path) -> io::Result<Peer> {
        let sock = UnixStream::from(sys::connect_nonblocking(path)?);
        let own = sys::effective_uid();
        let ours = sys::peer_uid(sock.as_fd()).is_ok_and(|peer| sys::uid_allowed(peer, own));
        if !ours {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the socket is served by somebody else",
            ));
        }
        let mut out = Vec::new();
        WIRE.write_hello(&mut out).map_err(io::Error::other)?;
        Ok(Peer {
            sock,
            out,
            sent: 0,
            hello: Vec::new(),
            open: false,
            decoder: PacketDecoder::new(),
            buf: vec![0u8; 64 * 1024],
        })
    }

    /// The socket, for a `poll` set.
    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    /// What to ask `poll` about: writability while bytes wait, readability
    /// when `reading`.
    pub fn interest(&self, reading: bool) -> libc::c_short {
        let mut events = 0;
        if reading {
            events |= libc::POLLIN;
        }
        if self.pending() > 0 {
            events |= libc::POLLOUT;
        }
        events
    }

    /// Whether the server has answered the handshake.
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Bytes queued and not yet written.
    pub fn pending(&self) -> usize {
        self.out.len().saturating_sub(self.sent)
    }

    /// Queues a packet. `false` when it cannot be encoded (nothing is queued).
    pub fn queue(&mut self, packet: &Packet) -> bool {
        let mark = self.out.len();
        if packet.encode(&mut self.out).is_err() {
            self.out.truncate(mark);
            return false;
        }
        true
    }

    /// Writes what the socket takes.
    pub fn flush(&mut self) -> Result<(), Gone> {
        while self.pending() > 0 {
            let waiting = self.out.get(self.sent..).unwrap_or_default();
            match sys::write_fd(self.sock.as_fd(), waiting) {
                Ok(0) => break,
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(_) => return Err(Gone::Closed),
            }
        }
        if self.sent >= self.out.len() {
            self.out.clear();
            self.sent = 0;
        } else if self.sent >= 64 * 1024 {
            self.out.drain(..self.sent);
            self.sent = 0;
        }
        Ok(())
    }

    /// Reads what has arrived and files the packets in `into`. An error does
    /// not lose the packets that came before it.
    pub fn read(&mut self, into: &mut Vec<Packet>) -> Result<(), Gone> {
        let mut buf = std::mem::take(&mut self.buf);
        let mut closed = false;
        for _ in 0..READ_BUDGET {
            match (&self.sock).read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => {
                    let bytes = buf.get(..n).unwrap_or_default();
                    if let Err(gone) = self.take(bytes) {
                        self.buf = buf;
                        return Err(gone);
                    }
                    if n < buf.len() {
                        break;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }
        self.buf = buf;
        loop {
            match self.decoder.next_packet() {
                Ok(Some(packet)) => into.push(packet),
                Ok(None) => break,
                Err(e) if e.is_recoverable() => {}
                Err(e) => return Err(Gone::Broken(e.to_string())),
            }
        }
        if closed { Err(Gone::Closed) } else { Ok(()) }
    }

    /// Bytes arrived: the handshake first, then packets.
    fn take(&mut self, bytes: &[u8]) -> Result<(), Gone> {
        if self.open {
            self.decoder.push(bytes);
            return Ok(());
        }
        self.hello.extend_from_slice(bytes);
        match handshake::reply(&WIRE, &self.hello) {
            Ok(ClientStep::Need) => {}
            Ok(ClientStep::Done { consumed, .. }) => {
                let rest = self.hello.get(consumed..).unwrap_or_default().to_vec();
                self.decoder.push(&rest);
                self.hello = Vec::new();
                self.open = true;
            }
            Ok(ClientStep::Refused) => return Err(Gone::Refused),
            Err(e) => return Err(Gone::Broken(e.to_string())),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kablo_proto::SessionId;
    use kablo_proto::v1::Message;
    use std::os::unix::net::UnixListener;
    use std::time::{Duration, Instant};

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!("/tmp/kt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    /// Reads from `peer` until `done` or a second has passed.
    fn pump(peer: &mut Peer, into: &mut Vec<Packet>, done: impl Fn(&[Packet]) -> bool) {
        let start = Instant::now();
        while !done(into) && start.elapsed() < Duration::from_secs(5) {
            let mut set = [sys::want(peer.fd(), peer.interest(true))];
            sys::poll(&mut set, Some(Duration::from_millis(50))).unwrap();
            peer.flush().unwrap();
            peer.read(into).unwrap();
        }
    }

    #[test]
    fn the_hello_and_a_request_go_out_together_and_the_answer_is_read_past_noise() {
        let dir = scratch("peer");
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let mut peer = Peer::connect(&path).unwrap();
        let id = SessionId::new("s").unwrap();
        assert!(peer.queue(&Packet::about(id.clone(), Message::Status)));
        peer.flush().unwrap();

        let (mut server, _) = listener.accept().unwrap();
        // The relay's side, by hand: the handshake and one frame behind it.
        let mut got = [0u8; 8];
        std::io::Read::read_exact(&mut server, &mut got).unwrap();
        assert_eq!(&got[..6], WIRE.signature);
        let mut reply = b"noise".to_vec();
        WIRE.write_reply(&mut reply, 1).unwrap();
        Packet::about(id.clone(), Message::Pong(b"x".to_vec()))
            .encode(&mut reply)
            .unwrap();
        // Noise, the whole reply, and three bytes of the frame behind it.
        let cut = 5 + 9 + 3;
        std::io::Write::write_all(&mut server, &reply[..cut]).unwrap();

        let mut packets = Vec::new();
        pump(&mut peer, &mut packets, |_| false);
        assert!(peer.is_open());
        assert!(packets.is_empty(), "half the way through a frame");
        std::io::Write::write_all(&mut server, &reply[cut..]).unwrap();
        pump(&mut peer, &mut packets, |p| !p.is_empty());
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].message, Message::Pong(b"x".to_vec()));

        drop(server);
        let mut more = Vec::new();
        let start = Instant::now();
        loop {
            match peer.read(&mut more) {
                Err(Gone::Closed) => break,
                Ok(()) => assert!(start.elapsed() < Duration::from_secs(5)),
                Err(other) => panic!("{other:?}"),
            }
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_path_nobody_listens_on_is_an_error_not_a_wait() {
        let dir = scratch("peer-none");
        let err = Peer::connect(&dir.join("none.sock")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_listener_that_never_accepts_takes_the_connection_and_answers_nothing() {
        // The shape of a session whose relay is gone for good: the keeper
        // still holds the socket.
        let dir = scratch("peer-mute");
        let path = dir.join("m.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let mut peer = Peer::connect(&path).unwrap();
        peer.flush().unwrap();
        let mut packets = Vec::new();
        let mut set = [sys::want(peer.fd(), libc::POLLIN)];
        assert_eq!(
            sys::poll(&mut set, Some(Duration::from_millis(100))).unwrap(),
            0
        );
        peer.read(&mut packets).unwrap();
        assert!(!peer.is_open() && packets.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
