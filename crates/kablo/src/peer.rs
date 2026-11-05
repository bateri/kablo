//! A connection to a session's socket, from the client's side, for a loop
//! that must not block.
//!
//! The relay serves one session on one socket. Whoever wants to talk to it (the
//! bridge, `kablo list`) holds a [`Peer`]: a non-blocking
//! socket with an output buffer, the handshake and a frame decoder. It does
//! not decide when to read or write; the owner puts [`Peer::fd`] and
//! [`Peer::interest`] in its `poll` set and calls [`Peer::flush`] and
//! [`Peer::read`] when they are ready.
//!
//! The hello goes out in the same write as whatever follows it: the relay
//! reads the hello and then the frames behind it in whatever order they come,
//! so a request does not wait for the reply to the handshake.

use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::Path;

use kablo_proto::WIRE;
use kablo_proto::v1::{Packet, PacketDecoder};

use crate::handshake::{Answered, Opening};
use crate::outbound::{self, Outbound};
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

/// Connects to the socket at `path` without blocking, and refuses it unless the
/// process that listens on it is ours. Every connection this program makes to a
/// socket of a session or of a bridge's mailbox goes through here, so that the
/// check is made in one place.
pub fn connect_ours(path: &Path) -> io::Result<OwnedFd> {
    let sock = sys::connect_nonblocking(path)?;
    let own = sys::effective_uid();
    if !sys::peer_uid(sock.as_fd()).is_ok_and(|peer| sys::uid_allowed(peer, own)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the socket is served by somebody else",
        ));
    }
    Ok(sock)
}

/// A connection to a relay.
#[derive(Debug)]
pub struct Peer {
    sock: UnixStream,
    out: Outbound,
    opening: Opening,
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
        let sock = UnixStream::from(connect_ours(path)?);
        let mut out = Outbound::new(outbound::CHUNK);
        WIRE.write_hello(out.buffer()).map_err(io::Error::other)?;
        Ok(Peer {
            sock,
            out,
            opening: Opening::default(),
            decoder: PacketDecoder::new(),
            buf: vec![0u8; 64 * 1024],
        })
    }

    /// The socket, for a `poll` set.
    pub fn fd(&self) -> RawFd {
        self.sock.as_raw_fd()
    }

    /// What to ask `poll` about: writability while bytes wait, readability
    /// when `reading`, and always while the handshake is not answered: the
    /// owner can take no more output, but the answer is not output, and a
    /// connection that cannot hear it would give up on a session that is
    /// there.
    pub fn interest(&self, reading: bool) -> libc::c_short {
        let mut events = 0;
        if reading || !self.opening.is_open() {
            events |= libc::POLLIN;
        }
        if self.pending() > 0 {
            events |= libc::POLLOUT;
        }
        events
    }

    /// Whether the server has answered the handshake.
    pub fn is_open(&self) -> bool {
        self.opening.is_open()
    }

    /// Bytes queued and not yet written.
    pub fn pending(&self) -> usize {
        self.out.pending()
    }

    /// Queues a packet. `false` when it cannot be encoded (nothing is queued).
    pub fn queue(&mut self, packet: &Packet) -> bool {
        let queue = self.out.buffer();
        let mark = queue.len();
        if packet.encode(queue).is_err() {
            queue.truncate(mark);
            return false;
        }
        true
    }

    /// Writes what the socket takes.
    pub fn flush(&mut self) -> Result<(), Gone> {
        let sock = self.sock.as_fd();
        if self.out.drain(&mut |bytes| sys::write_fd(sock, bytes)) {
            Ok(())
        } else {
            Err(Gone::Closed)
        }
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
        match self
            .opening
            .answer(&WIRE, bytes, |stream| self.decoder.push(stream))
        {
            Ok(Answered::Waiting | Answered::Open) => Ok(()),
            Ok(Answered::Refused) => Err(Gone::Refused),
            Err(e) => Err(Gone::Broken(e.to_string())),
        }
    }
}
