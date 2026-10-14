//! One client's connection to one session, which outlives the relay's
//! connections.
//!
//! A relay can die and be replaced, or be replaced on purpose, while a client
//! is attached. The keeper holds the session's socket across that, so the
//! next connection to the same path reaches the new relay. A [`Link`] makes
//! the client not notice: when its connection drops it connects again, attaches
//! again from the byte after the last one it handed to the client, and swallows
//! the `attached` that answers, because the client has had its own. What the
//! client sees is one continuous stream.
//!
//! The link keeps count itself: [`Attach::delivered`] moves only when an
//! `output` is handed to the owner, so the offset a new connection asks for
//! is exactly the first byte the client has not been given. If the journal
//! has moved past it by then, the new relay says so (`truncated`) and that
//! `attached` goes to the client, who sees the gap for what it is, and so does
//! the picture of the screen the relay sends after it. A picture the client is
//! owed (asked for, or announced by a `truncated` `attached`) and has not got
//! whole is asked for again by the next connection, whose `attached` is then
//! passed on too: the client starts the picture over.
//!
//! The link talks to its owner through packets: [`Link::send`] takes what the
//! client sent, and everything that is to go back to the client is pushed to
//! an `out` list, an `error` packet included when the link gives up. A link
//! that has given up, or whose session is over, says so with
//! [`Link::is_gone`] and the owner drops it.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{ErrorCode, Message, Packet, Size, View};

use crate::peer::{Gone, Peer};
use crate::{sockdir, sys};

/// How long a session may stay unreachable (from the moment a connection was
/// lost, or the first one made) before the link gives up and says
/// `unavailable`. A relay being replaced answers within a fraction of it.
pub const WAIT: Duration = Duration::from_secs(5);

/// How long to wait before connecting again after a connection that was never
/// answered, or one the listener would not take.
const RETRY: Duration = Duration::from_millis(100);

/// The most bytes of the client's requests that may wait for a connection
/// that is not answering.
const BACKLOG: usize = 1 << 20;

/// What the client is told when its request was dropped because more than
/// [`BACKLOG`] bytes of requests are waiting for the session. It passes: the
/// session takes them, or the link gives up on it in [`WAIT`].
pub const BUSY: &str = "too many requests are waiting";

/// What the client asked to be attached to.
#[derive(Clone, Debug)]
struct Attach {
    /// The offset the client asked for.
    requested: u64,
    /// What the client said of its terminal, kept as current as its resizes
    /// make it: a relay that takes over does not know it.
    view: Option<View>,
    /// The client is owed a picture of the screen it has not got whole.
    owed: bool,
    /// The offset after the last byte handed to the client; `None` until the
    /// first `attached` or `output`.
    delivered: Option<u64>,
    /// The client has been given its `attached`.
    announced: bool,
}

/// A client's connection to a session.
#[derive(Debug)]
pub struct Link {
    id: SessionId,
    path: PathBuf,
    peer: Option<Peer>,
    attach: Option<Attach>,
    /// `attached` replies still owed to attaches the link made itself.
    swallow: u32,
    /// `status` requests sent whose replies have not come.
    status_pending: u32,
    /// Requests that arrived while there was no connection to put them on.
    parked: Vec<Packet>,
    parked_bytes: usize,
    /// When the session is called unavailable, while no connection has been
    /// answered.
    give_up_at: Option<Instant>,
    /// When to try to connect again.
    retry_at: Option<Instant>,
    /// The program's end was handed to the client: nothing more will come.
    finished: bool,
    gone: bool,
}

impl Link {
    /// Connects to session `id` in `dir`. The error is the packet to give the
    /// client (boxed: it is large and rare).
    pub fn open(dir: &Path, id: SessionId, now: Instant) -> Result<Link, Box<Packet>> {
        let path = sockdir::socket_path(dir, &id);
        match Peer::connect(&path) {
            Ok(peer) => Ok(Link {
                id,
                path,
                peer: Some(peer),
                attach: None,
                swallow: 0,
                status_pending: 0,
                parked: Vec::new(),
                parked_bytes: 0,
                give_up_at: Some(now + WAIT),
                retry_at: None,
                finished: false,
                gone: false,
            }),
            Err(e) => Err(Box::new(refusal(&id, &e))),
        }
    }

    /// The session this is a link to.
    pub fn id(&self) -> &SessionId {
        &self.id
    }

    /// Whether the owner should drop this link.
    pub fn is_gone(&self) -> bool {
        self.gone
    }

    /// Whether the client is attached to the session.
    pub fn is_attached(&self) -> bool {
        self.attach.is_some()
    }

    /// The descriptor to wait on, if there is a connection. `reading` is
    /// false while the owner cannot take more output.
    pub fn pollfd(&self, reading: bool) -> Option<libc::pollfd> {
        let peer = self.peer.as_ref()?;
        Some(sys::want(peer.fd(), peer.interest(reading)))
    }

    /// The next time [`Link::tick`] has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        match (self.give_up_at, self.retry_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// A request from the client. One the link has no room for is answered
    /// with an `unavailable` error and dropped (see [`Link::offer`] for a
    /// caller that can wait instead).
    pub fn send(&mut self, packet: Packet, now: Instant, out: &mut Vec<Packet>) {
        if self.offer(packet, now, out).is_some() {
            out.push(unavailable(&self.id, BUSY));
        }
    }

    /// A request from the client, handed back untouched and with nothing about
    /// the link changed when too many requests are waiting for the session to
    /// take them: the owner may offer it again later. `None` means the link
    /// has it.
    pub fn offer(&mut self, packet: Packet, now: Instant, out: &mut Vec<Packet>) -> Option<Packet> {
        if self.is_full(&packet) {
            return Some(packet);
        }
        match &packet.message {
            Message::Attach { offset, view } => {
                self.attach = Some(Attach {
                    requested: *offset,
                    view: *view,
                    owed: view.is_some_and(|view| view.picture),
                    delivered: None,
                    announced: false,
                });
            }
            Message::Resize(size) => {
                if let Some(attach) = self.attach.as_mut() {
                    attach.view = Some(View {
                        size: *size,
                        picture: false,
                    });
                }
            }
            Message::Detach => self.attach = None,
            Message::Status => self.status_pending += 1,
            _ => {}
        }
        let Some(peer) = self.peer.as_mut() else {
            // Attaching, detaching and asking for the status are remembered
            // above and acted on when a connection is made; the rest waits.
            if !matches!(
                packet.message,
                Message::Attach { .. } | Message::Detach | Message::Status
            ) {
                self.parked_bytes += parked_size(&packet);
                self.parked.push(packet);
            }
            return None;
        };
        peer.queue(&packet);
        if peer.flush().is_err() {
            self.lost(now, out);
        }
        None
    }

    /// Whether `packet` would go past what may wait for the session.
    fn is_full(&self, packet: &Packet) -> bool {
        match &self.peer {
            Some(peer) => peer.pending() > BACKLOG,
            None => {
                !matches!(
                    packet.message,
                    Message::Attach { .. } | Message::Detach | Message::Status
                ) && self.parked_bytes + parked_size(packet) > BACKLOG
            }
        }
    }

    /// The connection reported `revents`.
    pub fn ready(&mut self, revents: libc::c_short, now: Instant, out: &mut Vec<Packet>) {
        let Some(peer) = self.peer.as_mut() else {
            return;
        };
        let mut lost = false;
        if revents & libc::POLLOUT != 0 && peer.flush().is_err() {
            lost = true;
        }
        let mut refused = None;
        if !lost && sys::is_ready(revents, libc::POLLIN) {
            let mut got = Vec::new();
            let read = peer.read(&mut got);
            let open = peer.is_open();
            self.accept(got, out);
            if open {
                self.give_up_at = None;
            }
            match read {
                Ok(()) => {}
                Err(Gone::Closed) => lost = true,
                Err(Gone::Refused) => refused = Some("the session speaks no version we do"),
                Err(Gone::Broken(_)) => refused = Some("the session sent what cannot be read"),
            }
        }
        if let Some(why) = refused {
            self.fail(ErrorCode::UNAVAILABLE, why, out);
        } else if lost {
            self.lost(now, out);
        }
    }

    /// Files what came from the session: keeps the count of what the client
    /// has been given, and hides what the link asked for on its own.
    fn accept(&mut self, packets: Vec<Packet>, out: &mut Vec<Packet>) {
        for packet in packets {
            match &packet.message {
                Message::Attached { offset, truncated } => {
                    if self.swallow > 0 {
                        self.swallow -= 1;
                        if !truncated {
                            continue;
                        }
                    }
                    if let Some(attach) = self.attach.as_mut() {
                        attach.announced = true;
                        attach.delivered = Some(*offset);
                        // A cut stream is followed by a picture; so is a
                        // stream that was asked to be.
                        attach.owed |= *truncated;
                    }
                }
                Message::Picture { more, .. } => {
                    if let Some(attach) = self.attach.as_mut() {
                        attach.owed = *more;
                    }
                }
                Message::Output { offset, data } => {
                    if let Some(attach) = self.attach.as_mut() {
                        attach.delivered = Some(offset + data.len() as u64);
                        // The stream goes on: no picture is coming (a relay
                        // that sends none, or one that has sent it).
                        attach.owed = false;
                    }
                }
                Message::Ended(_) => self.finished = true,
                Message::StatusReply(_) => {
                    self.status_pending = self.status_pending.saturating_sub(1);
                }
                _ => {}
            }
            out.push(packet);
        }
    }

    /// Time has passed.
    pub fn tick(&mut self, now: Instant, out: &mut Vec<Packet>) {
        if self.retry_at.is_some_and(|at| now >= at) {
            self.retry_at = None;
            self.reconnect(now, out);
        }
        if !self.gone && self.give_up_at.is_some_and(|at| now >= at) {
            self.fail(ErrorCode::UNAVAILABLE, "the session did not answer", out);
        }
    }

    /// The connection ended.
    fn lost(&mut self, now: Instant, out: &mut Vec<Packet>) {
        let answered = self.peer.as_ref().is_some_and(Peer::is_open);
        self.peer = None;
        // Nothing owed to this client any more: let go.
        if self.finished || (self.attach.is_none() && self.status_pending == 0) {
            self.gone = true;
            return;
        }
        if self.give_up_at.is_none() {
            self.give_up_at = Some(now + WAIT);
        }
        if answered {
            self.reconnect(now, out);
        } else {
            // It was never answered: do not spin on a session that takes the
            // connection and drops it.
            self.retry_at = Some(now + RETRY);
        }
    }

    /// Connects again and puts the client back where it was.
    fn reconnect(&mut self, now: Instant, out: &mut Vec<Packet>) {
        match Peer::connect(&self.path) {
            Ok(mut peer) => {
                // Whatever the old connection owed died with it.
                self.swallow = 0;
                if let Some(attach) = &self.attach {
                    let offset = attach.delivered.unwrap_or(attach.requested);
                    // The picture is asked for again only while it is owed; the
                    // size is the client's latest.
                    let view = match (attach.view, attach.owed) {
                        (Some(view), owed) => Some(View {
                            picture: owed,
                            ..view
                        }),
                        (None, true) => Some(View {
                            size: Size::default(),
                            picture: true,
                        }),
                        (None, false) => None,
                    };
                    peer.queue(&Packet::about(
                        self.id.clone(),
                        Message::Attach { offset, view },
                    ));
                    // The `attached` that answers is the client's own again
                    // unless the client is to see it: a picture starts over.
                    if attach.announced && !attach.owed {
                        self.swallow = 1;
                    }
                }
                for _ in 0..self.status_pending {
                    peer.queue(&Packet::about(self.id.clone(), Message::Status));
                }
                for packet in std::mem::take(&mut self.parked) {
                    peer.queue(&packet);
                }
                self.parked_bytes = 0;
                self.peer = Some(peer);
                if let Some(peer) = self.peer.as_mut()
                    && peer.flush().is_err()
                {
                    self.lost(now, out);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                self.retry_at = Some(now + RETRY);
            }
            Err(e) => {
                let packet = refusal(&self.id, &e);
                self.gone = true;
                self.peer = None;
                out.push(packet);
            }
        }
    }

    fn fail(&mut self, code: ErrorCode, text: &str, out: &mut Vec<Packet>) {
        self.gone = true;
        self.peer = None;
        out.push(Packet::about(
            self.id.clone(),
            Message::Error {
                code,
                text: text.to_owned(),
            },
        ));
    }
}

/// What to tell a client whose connection to a session could not be made.
fn refusal(id: &SessionId, error: &io::Error) -> Packet {
    match error.kind() {
        // Nothing listens: the file is stale or the session is over.
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused => Packet::about(
            id.clone(),
            Message::Error {
                code: ErrorCode::UNKNOWN_SESSION,
                text: "no such session".to_owned(),
            },
        ),
        _ => unavailable(id, &error.to_string()),
    }
}

/// What a request counts for against [`BACKLOG`] while it waits.
fn parked_size(packet: &Packet) -> usize {
    match &packet.message {
        Message::Input(bytes) => bytes.len(),
        _ => 64,
    }
}

fn unavailable(id: &SessionId, text: &str) -> Packet {
    Packet::about(
        id.clone(),
        Message::Error {
            code: ErrorCode::UNAVAILABLE,
            text: text.to_owned(),
        },
    )
}
