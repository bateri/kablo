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
//! What the client asks of a session without attaching to it is kept the same
//! way, by what each asks for. A `read` and a `wait` are questions that may be
//! asked again, so the link remembers them until they are answered and asks again
//! of the next connection (a `wait` is how a client hears that a program ended,
//! and a relay that is replaced by `upgrade` must not make it wait for ever). A
//! `write` is not: typed twice, keys are pressed twice, so it goes on the
//! connection there is and never again. One that finds the link without a
//! connection is answered with an error and typed nowhere; one whose connection
//! is lost before the `ping` that follows it was answered leaves the client with an
//! error that says it cannot know ([`UNSURE`]). A `ping` is the link's business
//! only while a `write` waits for one ([`Link::awaits_confirmation`]); any other is
//! the owner's to answer, since a link that is replaced must not take a pulse for
//! typing.
//!
//! The link talks to its owner through packets: [`Link::send`] takes what the
//! client sent, and everything that is to go back to the client is pushed to
//! an `out` list, an `error` packet included when the link gives up. A link
//! that has given up, or whose session is over, says so with
//! [`Link::is_gone`] and the owner drops it.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{Capture, ErrorCode, Message, Packet, Size, View};

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

/// What the client is told when the connection to the session was lost while a
/// `ping` after a `write` was not answered: the `write` was either typed or not,
/// and sending it again would type it twice.
pub const UNSURE: &str =
    "the connection to the session was lost before it said whether what was written was typed";

/// What the client is told of a `write` that came when there was no connection
/// to the session to type it on.
pub const NOT_CONNECTED: &str =
    "the connection to the session is being made again; nothing was typed";

/// What the client asked to be attached to.
#[derive(Clone, Debug)]
struct Attach {
    /// The offset the client asked for.
    requested: u64,
    /// What the client said of its terminal, kept as current as its resizes
    /// make it: a relay that takes over does not know it. Taking the terminal
    /// (`View::take`) is something the attach does and not something the client
    /// is, so it is kept only until the first `attached` answers: a connection
    /// made again after that attaches without it, or the relay that was started
    /// anew would let go of whoever attached since.
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
    /// `read` requests sent whose answers have not come, in the order they were
    /// asked; asked again by the next connection.
    reads: VecDeque<Capture>,
    /// A `wait` was sent and the program's end has not come; asked again by the
    /// next connection.
    waiting: bool,
    /// `ping`s that name the session, sent and not answered: they confirm what
    /// was written before them, and a connection that ends before they come back
    /// cannot say whether it was.
    pings: u32,
    /// A `write` went on the connection and no `ping` has been sent after it.
    typed: bool,
    /// How many connections the link has made again.
    reconnects: u32,
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
    /// When the client last used the session: attached to it or sent it input.
    /// The link opens on the client's first request, so it starts as that.
    used: Instant,
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
                reads: VecDeque::new(),
                waiting: false,
                pings: 0,
                typed: false,
                reconnects: 0,
                parked: Vec::new(),
                parked_bytes: 0,
                give_up_at: Some(now + WAIT),
                retry_at: None,
                finished: false,
                gone: false,
                used: now,
            }),
            Err(e) => Err(Box::new(refusal(&id, &e))),
        }
    }

    /// What to tell a client for a session whose socket could not be connected to.
    pub fn refusal(id: &SessionId, error: &io::Error) -> Packet {
        refusal(id, error)
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

    /// Whether a `write` is waiting to be confirmed: one went on the connection
    /// and the `pong` of a `ping` after it has not come. A `ping` the client
    /// sends now is the answer to that, and goes to the session (its `pong`
    /// comes after the `write`'s `error`); any other is not the link's.
    pub fn awaits_confirmation(&self) -> bool {
        self.typed || self.pings > 0
    }

    /// How many times the link has connected again after losing a connection.
    /// A caller that put together an answer that came in pieces knows by it that
    /// a new connection has begun the answer again.
    pub fn reconnects(&self) -> u32 {
        self.reconnects
    }

    /// The client used the session at `now`.
    pub fn touch(&mut self, now: Instant) {
        self.used = now;
    }

    /// How long since the client last used the session.
    pub fn idle(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.used)
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
                    // A resize changes the view's size and nothing else the
                    // client asked for: its flags are about the bytes and the
                    // session's keeping, not the size, and the ones a later
                    // version adds are not ours to drop. (The picture is
                    // asked for again only while it is owed, below.)
                    let mut view = attach.view.unwrap_or_else(|| View::new(*size));
                    view.size = *size;
                    view.picture = false;
                    attach.view = Some(view);
                }
            }
            Message::Detach => self.attach = None,
            Message::Status => self.status_pending += 1,
            Message::Read(capture) => self.reads.push_back(*capture),
            Message::Wait => self.waiting = true,
            Message::Ping(_) => {
                self.pings += 1;
                self.typed = false;
            }
            _ => {}
        }
        let Some(peer) = self.peer.as_mut() else {
            // Typing is not put off until there is a connection: what is typed
            // late is typed twice if the first try went through.
            if matches!(packet.message, Message::Write(_)) {
                out.push(unavailable(&self.id, NOT_CONNECTED));
                return None;
            }
            // Attaching, detaching, asking for the status, reading and waiting
            // are remembered above and acted on when a connection is made; the
            // rest waits.
            if !remembered(&packet.message) {
                self.parked_bytes += parked_size(&packet);
                self.parked.push(packet);
            }
            return None;
        };
        peer.queue(&packet);
        let typing = matches!(packet.message, Message::Write(_));
        if typing {
            self.typed = true;
        }
        if peer.flush().is_err() {
            // A write on a connection that is found to be gone at once may or
            // may not have been typed, and no `pong` is going to say: the client
            // is told now, before the `pong` of the `ping` that follows (which
            // the owner would answer itself, with no link left to ask).
            if typing {
                self.typed = false;
                out.push(unavailable(&self.id, UNSURE));
            }
            self.lost(now, out);
        }
        None
    }

    /// Whether `packet` would go past what may wait for the session.
    fn is_full(&self, packet: &Packet) -> bool {
        match &self.peer {
            Some(peer) => peer.pending() > BACKLOG,
            None => {
                !remembered(&packet.message)
                    && !matches!(packet.message, Message::Write(_))
                    && self.parked_bytes + parked_size(packet) > BACKLOG
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
                Message::Attached {
                    offset, truncated, ..
                } => {
                    if self.swallow > 0 {
                        self.swallow -= 1;
                        if !truncated {
                            continue;
                        }
                    }
                    if let Some(attach) = self.attach.as_mut() {
                        attach.announced = true;
                        attach.delivered = Some(*offset);
                        // Taking the terminal was done: the next connection
                        // attaches as the client it is, not as one that takes
                        // again (see `Attach::view`).
                        if let Some(view) = attach.view.as_mut() {
                            view.take = false;
                        }
                        // A cut stream is followed by a picture; so is a
                        // stream that was asked to be.
                        attach.owed |= *truncated;
                    }
                }
                Message::Picture { more, .. } => {
                    // The picture of an attach, or the answer to a `read`: the
                    // attach's is the one that is owed.
                    let owed = self.attach.as_ref().is_some_and(|attach| attach.owed);
                    if let Some(attach) = self.attach.as_mut() {
                        attach.owed = *more;
                    }
                    if !more && !owed {
                        self.read_done(true);
                    }
                }
                Message::ScreenText { more, .. } => {
                    if !more {
                        self.read_done(false);
                    }
                }
                Message::Pong(_) => self.pings = self.pings.saturating_sub(1),
                // Another client took the terminal: this one is no longer
                // attached to it, and a new connection must not put it back (the
                // one that took would find it listed again after the relay
                // restarts). The client is told, whatever it does with it.
                Message::Error { code, .. } if *code == ErrorCode::TAKEN => {
                    self.attach = None;
                }
                Message::Output { offset, data } => {
                    if let Some(attach) = self.attach.as_mut() {
                        attach.delivered = Some(offset + data.len() as u64);
                        // The stream goes on: no picture is coming (a relay
                        // that sends none, or one that has sent it).
                        attach.owed = false;
                    }
                }
                Message::Ended(_) => {
                    self.finished = true;
                    self.waiting = false;
                }
                Message::StatusReply(_) => {
                    self.status_pending = self.status_pending.saturating_sub(1);
                }
                _ => {}
            }
            out.push(packet);
        }
        // A client that only waited has been told, and holds nothing else: the
        // connection is of no more use to it. (One that attached is kept as it
        // always was, until the session goes.)
        if self.finished && !self.owes() {
            self.peer = None;
            self.gone = true;
        }
    }

    /// A read was answered: the oldest asked of its kind is no longer owed.
    fn read_done(&mut self, picture: bool) {
        if let Some(at) = self.reads.iter().position(|c| c.picture == picture) {
            self.reads.remove(at);
        }
    }

    /// Whether the client is owed anything but the end of the program: it is
    /// attached, or asked something that has not been answered.
    fn owes(&self) -> bool {
        self.attach.is_some()
            || self.status_pending > 0
            || !self.reads.is_empty()
            || self.pings > 0
            || self.waiting
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
        // What was written and not confirmed may or may not have been typed, and
        // the confirmation was on the connection that is gone.
        if self.pings > 0 {
            self.pings = 0;
            self.typed = false;
            out.push(unavailable(&self.id, UNSURE));
        }
        // Nothing owed to this client any more: let go. The end of the program
        // does not let go of one that is still owed an answer it can have.
        if (self.finished && self.reads.is_empty()) || !self.owes() {
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
                self.reconnects += 1;
                // Whatever the old connection owed died with it.
                self.swallow = 0;
                if let Some(attach) = &self.attach {
                    let offset = attach.delivered.unwrap_or(attach.requested);
                    // The picture is asked for again only while it is owed; the
                    // size is the client's latest.
                    let view = match (attach.view, attach.owed) {
                        (Some(mut view), owed) => {
                            view.picture = owed;
                            Some(view)
                        }
                        (None, true) => {
                            let mut view = View::new(Size::default());
                            view.picture = true;
                            Some(view)
                        }
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
                for capture in &self.reads {
                    peer.queue(&Packet::about(self.id.clone(), Message::Read(*capture)));
                }
                if self.waiting {
                    peer.queue(&Packet::about(self.id.clone(), Message::Wait));
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

/// Whether the link keeps what a request asks for itself while there is no
/// connection, and asks the next connection for it, instead of holding the
/// request.
fn remembered(message: &Message) -> bool {
    matches!(
        message,
        Message::Attach { .. }
            | Message::Detach
            | Message::Status
            | Message::Read(_)
            | Message::Wait
    )
}

/// What a request counts for against [`BACKLOG`] while it waits.
fn parked_size(packet: &Packet) -> usize {
    match &packet.message {
        Message::Input(bytes) => bytes.len(),
        Message::Write(_) => 0,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::os::unix::net::UnixListener;

    use kablo_proto::v1::{ClientName, PacketDecoder, Stroke};
    use kablo_proto::{SessionId as Id, WIRE};

    /// A link whose connection has been lost and is waited for, to a session
    /// that is a listening socket in a directory of its own.
    fn link_between_connections(name: &str) -> (Link, UnixListener, PathBuf) {
        let dir = PathBuf::from(format!("/tmp/kl{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        sockdir::ensure_dir(&dir).unwrap();
        let id = Id::new("s").unwrap();
        let listener = UnixListener::bind(sockdir::socket_path(&dir, &id)).unwrap();
        let now = Instant::now();
        let mut link = Link::open(&dir, id, now).unwrap();
        link.peer = None;
        link.retry_at = Some(now);
        (link, listener, dir)
    }

    fn about(message: Message) -> Packet {
        Packet::about(Id::new("s").unwrap(), message)
    }

    #[test]
    fn a_ping_is_the_links_only_while_a_write_waits_for_one() {
        let dir = PathBuf::from(format!("/tmp/kl{}-confirm", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        sockdir::ensure_dir(&dir).unwrap();
        let id = Id::new("s").unwrap();
        let listener = UnixListener::bind(sockdir::socket_path(&dir, &id)).unwrap();
        let now = Instant::now();
        let mut link = Link::open(&dir, id.clone(), now).unwrap();
        let _held = listener.accept().unwrap();
        let mut out = Vec::new();
        assert!(!link.awaits_confirmation());

        link.send(
            about(Message::Write(vec![Stroke::text("x")])),
            now,
            &mut out,
        );
        assert!(link.awaits_confirmation());
        link.send(about(Message::Ping(b"p".to_vec())), now, &mut out);
        assert!(
            link.awaits_confirmation(),
            "the ping is still to be answered"
        );
        link.accept(vec![about(Message::Pong(b"p".to_vec()))], &mut out);
        assert!(!link.awaits_confirmation());
        assert_eq!(out.len(), 1, "only the pong, which is the client's");

        // A write and a ping lost with the connection: the client is told it
        // cannot know, once, instead of the `pong`.
        out.clear();
        link.send(
            about(Message::Write(vec![Stroke::text("y")])),
            now,
            &mut out,
        );
        link.send(about(Message::Ping(b"q".to_vec())), now, &mut out);
        link.lost(now, &mut out);
        assert_eq!(out, [unavailable(&id, UNSURE)]);
        assert!(!link.awaits_confirmation());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_a_client_asked_of_its_view_is_asked_again_of_the_next_connection_after_a_resize() {
        let (mut link, listener, dir) = link_between_connections("view");
        let now = Instant::now();
        let mut out = Vec::new();
        let size = |cols| Size {
            cols,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        };
        let mut view = View::new(size(80));
        view.picture_if_answered = true;
        view.keep_after_end = true;
        view.name = ClientName::new("MacBook");
        link.send(
            about(Message::Attach {
                offset: 0x10,
                view: Some(view),
            }),
            now,
            &mut out,
        );
        // The terminal is resized while the connection is down: the size is the
        // latest and the flags and the name are the ones that were asked for.
        link.send(about(Message::Resize(size(100))), now, &mut out);

        link.tick(now + RETRY, &mut out);
        assert!(link.peer.is_some());
        let _ = listener.accept().unwrap(); // the one that was lost
        let (mut sock, _) = listener.accept().unwrap();
        WIRE.server(&mut sock.try_clone().unwrap(), &mut sock)
            .unwrap();
        let mut decoder = PacketDecoder::new();
        let mut buf = [0u8; 1024];
        let heard = loop {
            let n = sock.read(&mut buf).unwrap();
            assert!(n > 0, "the attach never came");
            decoder.push(&buf[..n]);
            if let Some(packet) = decoder.next_packet().unwrap() {
                break packet.message;
            }
        };
        let Message::Attach {
            offset,
            view: Some(view),
        } = heard
        else {
            panic!("not an attach with a view: {heard:?}");
        };
        assert_eq!(offset, 0x10);
        assert_eq!(view.size, size(100));
        assert!(view.keep_after_end && view.picture_if_answered && !view.picture);
        assert_eq!(view.name, ClientName::new("MacBook"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The first message the link sends on the connection it makes next (the
    /// one after the connection that `link_between_connections` lost).
    fn first_sent_on_the_next_connection(
        link: &mut Link,
        listener: &UnixListener,
        now: Instant,
        out: &mut Vec<Packet>,
    ) -> Message {
        link.tick(now + RETRY, out);
        assert!(link.peer.is_some());
        let _ = listener.accept().unwrap(); // the one that was lost
        let (mut sock, _) = listener.accept().unwrap();
        WIRE.server(&mut sock.try_clone().unwrap(), &mut sock)
            .unwrap();
        let mut decoder = PacketDecoder::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = sock.read(&mut buf).unwrap();
            assert!(n > 0, "nothing was sent");
            decoder.push(&buf[..n]);
            if let Some(packet) = decoder.next_packet().unwrap() {
                return packet.message;
            }
        }
    }

    fn taking(name: &str) -> Message {
        let mut view = View::new(Size {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        });
        view.take = true;
        view.name = ClientName::new(name);
        Message::Attach {
            offset: 0,
            view: Some(view),
        }
    }

    fn attached() -> Packet {
        about(Message::Attached {
            offset: 0,
            truncated: false,
            answered: 0,
        })
    }

    #[test]
    fn taking_the_terminal_is_asked_again_only_until_the_session_has_answered() {
        // Answered: the take was done. A relay that starts anew, with clients
        // that attached since, is not made to let go of them by this client
        // coming back as the one it is.
        let (mut link, listener, dir) = link_between_connections("took");
        let now = Instant::now();
        let mut out = Vec::new();
        link.send(about(taking("MacBook")), now, &mut out);
        link.accept(vec![attached()], &mut out);
        let Message::Attach {
            view: Some(view), ..
        } = first_sent_on_the_next_connection(&mut link, &listener, now, &mut out)
        else {
            panic!("not an attach with a view");
        };
        assert!(!view.take);
        assert_eq!(view.name, ClientName::new("MacBook"));
        let _ = std::fs::remove_dir_all(&dir);

        // Not answered: a take that did not happen is still asked for, or a
        // connection that broke in the middle of it would turn it into a plain
        // attach without anyone saying.
        let (mut link, listener, dir) = link_between_connections("took-not-yet");
        link.send(about(taking("MacBook")), now, &mut out);
        let Message::Attach {
            view: Some(view), ..
        } = first_sent_on_the_next_connection(&mut link, &listener, now, &mut out)
        else {
            panic!("not an attach with a view");
        };
        assert!(view.take);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_client_that_was_taken_from_is_not_attached_again_by_a_new_connection() {
        let (mut link, listener, dir) = link_between_connections("taken");
        let now = Instant::now();
        let mut out = Vec::new();
        link.send(about(Message::attach(0)), now, &mut out);
        link.accept(vec![attached()], &mut out);
        assert!(link.is_attached());
        let taken = about(Message::Error {
            code: ErrorCode::TAKEN,
            text: "iMac".to_owned(),
        });
        out.clear();
        link.accept(vec![taken.clone()], &mut out);
        // The client is told, and the link no longer holds the attach.
        assert_eq!(out, vec![taken]);
        assert!(!link.is_attached());
        // What comes first on the connection made next is what was asked after:
        // not the attach that the other client took the terminal from.
        link.send(about(Message::Status), now, &mut out);
        let heard = first_sent_on_the_next_connection(&mut link, &listener, now, &mut out);
        assert!(matches!(heard, Message::Status), "{heard:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_that_cannot_be_flushed_is_said_to_be_unknown() {
        let dir = PathBuf::from(format!("/tmp/kl{}-flush", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        sockdir::ensure_dir(&dir).unwrap();
        let id = Id::new("s").unwrap();
        let listener = UnixListener::bind(sockdir::socket_path(&dir, &id)).unwrap();
        let now = Instant::now();
        let mut link = Link::open(&dir, id.clone(), now).unwrap();
        // The relay takes the connection and is gone before the write.
        drop(listener.accept().unwrap());
        let mut out = Vec::new();
        link.send(
            about(Message::Write(vec![Stroke::text("z")])),
            now,
            &mut out,
        );
        assert_eq!(out, [unavailable(&id, UNSURE)], "{out:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_between_connections_is_refused_and_the_rest_is_remembered() {
        let (mut link, listener, dir) = link_between_connections("between");
        let now = Instant::now();
        let mut out = Vec::new();
        let strokes = vec![Stroke::text("typed once")];
        link.send(about(Message::Write(strokes)), now, &mut out);
        // Typed nowhere, and said so: it is not kept for the next connection.
        assert_eq!(out, [unavailable(&Id::new("s").unwrap(), NOT_CONNECTED)]);
        assert!(link.parked.is_empty());
        assert!(!link.is_gone());

        let mut capture = Capture::default();
        capture.history = 3;
        for message in [
            Message::Read(capture),
            Message::Wait,
            Message::Status,
            Message::Ping(b"p".to_vec()),
        ] {
            link.send(about(message), now, &mut out);
        }
        assert_eq!(out.len(), 1, "nothing more was said");
        assert_eq!(
            (link.reads.len(), link.waiting, link.status_pending),
            (1, true, 1)
        );
        // Only the ping waits as a request does; the others are asked again.
        assert_eq!(link.parked.len(), 1);

        // The next connection is asked what the link remembers, and then what
        // waited.
        link.tick(now + RETRY, &mut out);
        assert!(link.peer.is_some());
        let _ = listener.accept().unwrap(); // the one that was lost
        let (mut sock, _) = listener.accept().unwrap();
        WIRE.server(&mut sock.try_clone().unwrap(), &mut sock)
            .unwrap();
        let mut decoder = PacketDecoder::new();
        let mut heard = Vec::new();
        let mut buf = [0u8; 1024];
        while heard.len() < 4 {
            let n = sock.read(&mut buf).unwrap();
            assert!(n > 0, "{heard:?}");
            decoder.push(&buf[..n]);
            while let Some(packet) = decoder.next_packet().unwrap() {
                heard.push(packet.message);
            }
        }
        assert_eq!(
            heard,
            [
                Message::Status,
                Message::Read(capture),
                Message::Wait,
                Message::Ping(b"p".to_vec())
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
