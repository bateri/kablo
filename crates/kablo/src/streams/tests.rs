use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use kablo_proto::v1::stream::{Incoming, IncomingDecoder};

use super::*;

const PATIENCE: Duration = Duration::from_secs(30);

/// A connected pair of loopback sockets.
fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ours = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (theirs, _) = listener.accept().unwrap();
    (ours, theirs)
}

fn open(window: u32, port: u16, host: &str) -> Message {
    Message::Open {
        window,
        port,
        host: host.to_owned(),
    }
}

/// A server whose targets are the sockets `dial` hands out.
fn server_with(config: Config) -> Streams {
    Streams::new(Side::Server, config)
}

/// A dial that connects to nothing: it waits for a message on a channel, and
/// says what that message does.
type Gate = mpsc::Sender<Result<TcpStream, Refusal>>;

fn gated() -> (Config, Gate) {
    let (gate, wait) = mpsc::channel();
    let wait = Mutex::new(wait);
    let config = Config {
        dial: Arc::new(move |_, _| match wait.lock().unwrap().recv() {
            Ok(result) => result,
            Err(_) => Err(Refusal::new(FailCode::UNREACHABLE, "gone")),
        }),
        ..Config::default()
    };
    (config, gate)
}

/// Reports the connector has waiting, once the thread has got to it.
fn reports(streams: &mut Streams, out: &mut Vec<Packet>, until: impl Fn(&Streams) -> bool) {
    let start = Instant::now();
    while !until(streams) {
        let found = streams.interest();
        let mut set: Vec<libc::pollfd> = found.iter().map(|(fd, _)| *fd).collect();
        sys::poll(&mut set, Some(Duration::from_millis(20))).unwrap();
        for ((_, token), fd) in found.into_iter().zip(&set) {
            if fd.revents != 0 {
                streams.ready(token, fd.revents, out);
            }
        }
        assert!(start.elapsed() < PATIENCE, "the connector never reported");
    }
}

fn failed(out: &[Packet]) -> Vec<(u32, FailCode)> {
    out.iter()
        .filter_map(|packet| match &packet.message {
            Message::Failed { code, .. } => Some((packet.stream, *code)),
            _ => None,
        })
        .collect()
}

/// An open server-side stream whose target is `theirs`: opened with `window`
/// as the client's window, and the window this end offers is `config.window`.
fn opened(config: Config, client_window: u32) -> (Streams, TcpStream, Vec<Packet>) {
    let (server, theirs, _, out) = opened_keeping(config, client_window);
    (server, theirs, out)
}

/// [`opened`], and a second handle on the server's socket for the test to do
/// what only the owner of a socket can (shut its writing half, say).
fn opened_keeping(
    config: Config,
    client_window: u32,
) -> (Streams, TcpStream, TcpStream, Vec<Packet>) {
    let (ours, theirs) = pair();
    let handle = ours.try_clone().unwrap();
    let config = Config {
        dial: Arc::new({
            let ours = Mutex::new(Some(ours));
            move |_, _| Ok(ours.lock().unwrap().take().unwrap())
        }),
        ..config
    };
    let mut server = server_with(config);
    let mut out = Vec::new();
    server.handle(
        Side::Client,
        Packet::new(1, open(client_window, 80, "x")),
        &mut out,
    );
    reports(&mut server, &mut out, |s| s.streams.contains_key(&1));
    assert_eq!(out.len(), 1);
    assert!(matches!(
        out.first().map(|p| &p.message),
        Some(Message::Opened { .. })
    ));
    out.clear();
    (server, theirs, handle, out)
}

// --- both ends, over loopback ------------------------------------------------

/// A client engine and a server engine with the frames carried between them in
/// the order a bridge would: a turn's control packets, then its data frames.
struct Rig {
    client: Streams,
    server: Streams,
    c_out: Vec<Packet>,
    s_out: Vec<Packet>,
    to_client: IncomingDecoder,
    to_server: IncomingDecoder,
}

impl Rig {
    fn new(window: u32) -> Rig {
        let config = Config {
            window,
            ..Config::default()
        };
        Rig {
            client: Streams::new(Side::Client, config.clone()),
            server: Streams::new(Side::Server, config),
            c_out: Vec::new(),
            s_out: Vec::new(),
            to_client: IncomingDecoder::new(),
            to_server: IncomingDecoder::new(),
        }
    }

    /// One turn: sockets that are ready are read and written, then everything
    /// said is delivered.
    fn turn(&mut self) {
        for (streams, out) in [
            (&mut self.client, &mut self.c_out),
            (&mut self.server, &mut self.s_out),
        ] {
            let found = streams.interest();
            let mut set: Vec<libc::pollfd> = found.iter().map(|(fd, _)| *fd).collect();
            sys::poll(&mut set, Some(Duration::from_millis(5))).unwrap();
            for ((_, token), fd) in found.into_iter().zip(&set) {
                if fd.revents != 0 {
                    streams.ready(token, fd.revents, out);
                }
            }
            streams.settle(out);
        }
        let said = std::mem::take(&mut self.c_out);
        carry(
            Side::Client,
            said,
            &mut self.client,
            &mut self.server,
            &mut self.s_out,
            &mut self.to_server,
        );
        let said = std::mem::take(&mut self.s_out);
        carry(
            Side::Server,
            said,
            &mut self.server,
            &mut self.client,
            &mut self.c_out,
            &mut self.to_client,
        );
    }

    fn until(&mut self, what: &str, mut done: impl FnMut(&mut Rig) -> bool) {
        let start = Instant::now();
        loop {
            self.turn();
            if done(self) {
                return;
            }
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {what}");
        }
    }
}

/// Delivers what the end `from` said: its control packets, then its data
/// frames, which are decoded as the wire would.
fn carry(
    from: Side,
    packets: Vec<Packet>,
    frames: &mut Streams,
    into: &mut Streams,
    into_out: &mut Vec<Packet>,
    decoder: &mut IncomingDecoder,
) {
    for packet in packets {
        into.handle(from, packet, into_out);
    }
    while let Some(frame) = frames.take() {
        decoder.push(&frame);
        while let Some(incoming) = decoder.next_incoming().unwrap() {
            match incoming {
                Incoming::Stream { from, packet } => into.handle(from, packet, into_out),
                other => panic!("a session message: {other:?}"),
            }
        }
    }
}

/// Reads what a nonblocking socket has, returning false once it has ended.
fn take_all(socket: &mut TcpStream, into: &mut Vec<u8>) -> bool {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match socket.read(&mut buf) {
            Ok(0) => return false,
            Ok(n) => into.extend_from_slice(buf.get(..n).unwrap()),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => panic!("{e}"),
        }
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i % 251) as u8 ^ seed ^ (i / 251) as u8)
        .collect()
}

#[test]
fn bytes_cross_both_ways_and_each_half_closes_on_its_own() {
    // A window smaller than what is sent, so credit has to flow.
    let mut rig = Rig::new(64 * 1024);
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let port = target.local_addr().unwrap().port();
    let (mut app, local) = pair();
    app.set_nonblocking(true).unwrap();
    rig.client
        .open(local, "127.0.0.1", port, &mut rig.c_out)
        .unwrap();

    let mut accepted = None;
    rig.until("the target to be reached", |_| {
        if accepted.is_none() {
            accepted = target.accept().ok().map(|(socket, _)| socket);
        }
        accepted.is_some()
    });
    let mut accepted = accepted.unwrap();
    accepted.set_nonblocking(true).unwrap();

    // The app sends 300 KiB and says it is done; the target reads it all
    // before it hears the end, and answers with 200 KiB of its own.
    let request = pattern(300 * 1024, 1);
    let reply = pattern(200 * 1024, 2);
    let mut sent = 0;
    let mut heard = Vec::new();
    let mut answered = 0;
    let mut got = Vec::new();
    let mut ended = false;
    rig.until("both directions to finish", |_| {
        while sent < request.len() {
            match app.write(request.get(sent..).unwrap()) {
                Ok(n) => sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("{e}"),
            }
        }
        if sent == request.len() && !ended {
            app.shutdown(Shutdown::Write).unwrap();
            ended = true;
        }
        let open = take_all(&mut accepted, &mut heard);
        if !open {
            // The end of the request reached the target as an end; the other
            // direction is still open, and it answers now.
            while answered < reply.len() {
                match accepted.write(reply.get(answered..).unwrap()) {
                    Ok(n) => answered += n,
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => panic!("{e}"),
                }
            }
            if answered == reply.len() {
                let _ = accepted.shutdown(Shutdown::Write);
            }
        }
        let still = take_all(&mut app, &mut got);
        !still
    });
    assert_eq!(heard, request);
    assert_eq!(got, reply);
    rig.until("the streams to be forgotten", |rig| {
        rig.client.is_empty() && rig.server.is_empty()
    });
}

/// Rig with a client stream carried to a listener: the app's end of the local
/// connection and the target's end of the far one.
fn carried(rig: &mut Rig) -> (TcpStream, TcpStream) {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let port = target.local_addr().unwrap().port();
    let (app, local) = pair();
    rig.client
        .open(local, "127.0.0.1", port, &mut rig.c_out)
        .unwrap();
    let mut accepted = None;
    rig.until("the target to be reached", |_| {
        if accepted.is_none() {
            accepted = target.accept().ok().map(|(socket, _)| socket);
        }
        accepted.is_some()
    });
    let accepted = accepted.unwrap();
    accepted.set_nonblocking(true).unwrap();
    (app, accepted)
}

#[test]
fn an_app_that_leaves_ends_its_half_and_the_stream_ends_when_the_target_does_too() {
    let mut rig = Rig::new(64 * 1024);
    let (app, mut target) = carried(&mut rig);
    drop(app);
    rig.until("the target to hear the end", |_| {
        let mut buf = [0u8; 16];
        matches!(target.read(&mut buf), Ok(0))
    });
    // The target may still say something; the stream is open until it is done.
    assert!(!rig.client.is_empty());
    drop(target);
    rig.until("the streams to be forgotten", |rig| {
        rig.client.is_empty() && rig.server.is_empty()
    });
}

#[test]
fn a_connection_that_is_reset_closes_the_stream_at_both_ends() {
    let mut rig = Rig::new(64 * 1024);
    let (app, mut target) = carried(&mut rig);
    // A reset instead of an orderly end.
    let linger = libc::linger {
        l_onoff: 1,
        l_linger: 0,
    };
    // SAFETY: `linger` is a valid `struct linger` for the length given, on a
    // socket this test owns.
    let set = unsafe {
        libc::setsockopt(
            app.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_LINGER,
            (&raw const linger).cast(),
            size_of::<libc::linger>() as libc::socklen_t,
        )
    };
    assert_eq!(set, 0);
    drop(app);
    rig.until("the streams to be forgotten", |rig| {
        rig.client.is_empty() && rig.server.is_empty()
    });
    // The target's socket was closed with the stream: it hears an end (or a
    // reset), not silence.
    let start = Instant::now();
    loop {
        let mut buf = [0u8; 16];
        match target.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        assert!(start.elapsed() < PATIENCE);
    }
}

#[test]
fn a_dial_that_refuses_fails_the_stream_at_the_client() {
    let mut rig = Rig::new(MAX_WINDOW);
    // A port below the range the system hands out: nothing listens on it, and
    // none of the listeners of the tests running side by side can be given it
    // (a port that was bound and let go can be).
    let port = 1;
    let (_app, local) = pair();
    let id = rig
        .client
        .open(local, "127.0.0.1", port, &mut rig.c_out)
        .unwrap();
    let mut events = Vec::new();
    rig.until("the failure", |rig| {
        events.extend(rig.client.take_events());
        !events.is_empty()
    });
    assert!(matches!(
        events.first(),
        Some(Event::Failed { stream, code, .. }) if *stream == id && *code == FailCode::REFUSED
    ));
    assert!(rig.client.is_empty());
}

// --- the server alone ----------------------------------------------------------

#[test]
fn an_open_that_connects_is_opened_and_one_that_does_not_is_failed() {
    let (config, gate) = gated();
    let mut server = server_with(config);
    let mut out = Vec::new();
    server.handle(Side::Client, Packet::new(1, open(1000, 80, "a")), &mut out);
    server.handle(Side::Client, Packet::new(2, open(1000, 80, "b")), &mut out);
    assert!(
        out.is_empty(),
        "nothing is answered before a target is reached"
    );
    assert_eq!(server.len(), 2);
    let (ours, _theirs) = pair();
    gate.send(Ok(ours)).unwrap();
    gate.send(Err(Refusal::new(FailCode::REFUSED, "connection refused")))
        .unwrap();
    reports(&mut server, &mut out, |s| s.connecting.is_empty());
    // The threads may report in either order.
    let mut answers: Vec<(u32, &Message)> = out.iter().map(|p| (p.stream, &p.message)).collect();
    answers.sort_by_key(|(id, _)| *id);
    assert_eq!(answers.len(), 2);
    let opened = out
        .iter()
        .filter(|p| matches!(p.message, Message::Opened { window } if window == MAX_WINDOW))
        .count();
    let refused = failed(&out);
    assert_eq!(opened, 1);
    assert_eq!(refused.len(), 1);
    assert_eq!(
        refused.first().map(|(_, code)| *code),
        Some(FailCode::REFUSED)
    );
    assert_eq!(server.len(), 1);
}

#[test]
fn a_name_that_does_not_resolve_is_failed_as_unresolved() {
    // The path from a resolver's failure to the wire, without a network: the
    // resolver is a test's own.
    let config = Config {
        dial: Arc::new(|target, deadline| {
            dial::connect_via(target, deadline, &|_, _| {
                Err(io::Error::other("no such host"))
            })
        }),
        ..Config::default()
    };
    let mut server = server_with(config);
    let mut out = Vec::new();
    server.handle(
        Side::Client,
        Packet::new(1, open(1000, 80, "nowhere.example")),
        &mut out,
    );
    reports(&mut server, &mut out, |s| s.connecting.is_empty());
    assert_eq!(failed(&out), [(1, FailCode::UNRESOLVED)]);
    assert!(matches!(
        out.first().map(|p| &p.message),
        Some(Message::Failed { text, .. }) if text == "name not found"
    ));
}

#[test]
fn what_cannot_be_opened_is_failed_without_connecting() {
    let mut server = server_with(Config::default());
    let mut out = Vec::new();
    let bad = [
        (1, open(1000, 0, "x")),
        (2, open(1000, 80, "")),
        (3, open(1000, 80, &"x".repeat(MAX_HOST + 1))),
        (4, open(1000, 80, "a\0b")),
    ];
    for (id, message) in bad {
        server.handle(Side::Client, Packet::new(id, message), &mut out);
    }
    server.handle(
        Side::Client,
        Packet::new(5, open(MAX_WINDOW + 1, 80, "x")),
        &mut out,
    );
    assert_eq!(
        failed(&out),
        [
            (1, FailCode::BAD_REQUEST),
            (2, FailCode::BAD_REQUEST),
            (3, FailCode::BAD_REQUEST),
            (4, FailCode::BAD_REQUEST),
            (5, FailCode::LIMIT),
        ]
    );
    assert!(server.is_empty());
}

#[test]
fn too_many_streams_and_too_many_connecting_are_the_limit() {
    let (config, gate) = gated();
    let mut server = server_with(Config {
        max_connecting: 2,
        max_streams: 3,
        ..config
    });
    let mut out = Vec::new();
    for id in 1..=3 {
        server.handle(Side::Client, Packet::new(id, open(10, 80, "x")), &mut out);
    }
    // Two are connecting; the third is over that limit.
    assert_eq!(failed(&out), [(3, FailCode::LIMIT)]);
    out.clear();
    // Let both through: two streams are open, one more fits under three...
    let (a, _a) = pair();
    let (b, _b) = pair();
    gate.send(Ok(a)).unwrap();
    gate.send(Ok(b)).unwrap();
    reports(&mut server, &mut out, |s| s.connecting.is_empty());
    out.clear();
    server.handle(Side::Client, Packet::new(4, open(10, 80, "x")), &mut out);
    assert!(out.is_empty());
    // ...and the next is over the limit on streams though nothing else is
    // connecting beyond one.
    server.handle(Side::Client, Packet::new(5, open(10, 80, "x")), &mut out);
    assert_eq!(failed(&out), [(5, FailCode::LIMIT)]);
}

#[test]
fn a_target_that_is_too_slow_is_failed_once_and_its_late_answer_is_dropped() {
    let (config, gate) = gated();
    let mut server = server_with(config);
    let mut out = Vec::new();
    server.handle(Side::Client, Packet::new(1, open(10, 80, "x")), &mut out);
    let deadline = server.deadline().unwrap();
    assert!(deadline > Instant::now());
    // Before the deadline nothing happens; at it, the client is told.
    server.expire(Instant::now(), &mut out);
    assert!(out.is_empty());
    server.expire(deadline, &mut out);
    assert_eq!(failed(&out), [(1, FailCode::TIMED_OUT)]);
    assert_eq!(server.deadline(), None);
    // The thread is still there, so the stream still counts as connecting...
    assert_eq!(server.len(), 1);
    // ...until it reports, and its socket then goes nowhere.
    let (ours, theirs) = pair();
    gate.send(Ok(ours)).unwrap();
    out.clear();
    reports(&mut server, &mut out, |s| s.connecting.is_empty());
    assert!(out.is_empty());
    assert!(server.is_empty());
    theirs.set_nonblocking(false).unwrap();
    theirs.set_read_timeout(Some(PATIENCE)).unwrap();
    let mut buf = [0u8; 1];
    assert_eq!(
        (&theirs).read(&mut buf).unwrap(),
        0,
        "the socket was closed"
    );
}

#[test]
fn a_stream_closed_while_connecting_is_dropped_when_the_target_answers() {
    let (config, gate) = gated();
    let mut server = server_with(config);
    let mut out = Vec::new();
    server.handle(Side::Client, Packet::new(1, open(10, 80, "x")), &mut out);
    server.handle(Side::Client, Packet::new(1, Message::Close), &mut out);
    let (ours, theirs) = pair();
    gate.send(Ok(ours)).unwrap();
    reports(&mut server, &mut out, |s| s.connecting.is_empty());
    assert!(
        out.is_empty(),
        "nothing is said about a stream that was closed"
    );
    assert!(server.is_empty());
    theirs.set_read_timeout(Some(PATIENCE)).unwrap();
    let mut buf = [0u8; 1];
    assert_eq!((&theirs).read(&mut buf).unwrap(), 0);
}

#[test]
fn a_number_that_is_not_above_the_last_is_not_answered() {
    let (config, gate) = gated();
    let mut server = server_with(config);
    let mut out = Vec::new();
    server.handle(Side::Client, Packet::new(5, open(10, 0, "x")), &mut out);
    assert_eq!(failed(&out), [(5, FailCode::BAD_REQUEST)]);
    out.clear();
    for id in [5, 4, 1, 0] {
        server.handle(Side::Client, Packet::new(id, open(10, 80, "x")), &mut out);
    }
    assert!(out.is_empty());
    assert!(server.is_empty());
    drop(gate);
}

#[test]
fn an_administrator_can_turn_streams_off_and_on() {
    let mut server = server_with(Config::default());
    let mut out = Vec::new();
    server.set_closed(Some("turned off by the administrator".to_owned()));
    server.handle(Side::Client, Packet::new(1, open(10, 80, "x")), &mut out);
    assert_eq!(failed(&out), [(1, FailCode::FORBIDDEN)]);
    assert!(matches!(
        out.first().map(|p| &p.message),
        Some(Message::Failed { text, .. }) if text == "turned off by the administrator"
    ));
    assert!(server.is_empty());
}

#[test]
fn frames_of_streams_that_are_not_there_are_ignored() {
    let mut server = server_with(Config::default());
    let mut out = Vec::new();
    for message in [
        Message::Data(vec![1, 2, 3]),
        Message::Credit(10),
        Message::Eof,
        Message::Close,
    ] {
        server.handle(Side::Client, Packet::new(7, message), &mut out);
    }
    server.settle(&mut out);
    assert!(out.is_empty());
    assert!(server.is_empty());
    assert!(server.interest().is_empty());
}

#[test]
fn a_message_of_this_ends_own_kind_is_ignored() {
    let mut server = server_with(Config::default());
    let mut out = Vec::new();
    server.handle(Side::Server, Packet::new(1, open(10, 80, "x")), &mut out);
    assert!(out.is_empty());
    assert!(server.is_empty());
}

#[test]
fn a_stream_that_breaks_the_rules_is_closed_and_the_others_are_not() {
    let (mut server, _target, mut out) = opened(
        Config {
            window: 100,
            ..Config::default()
        },
        1000,
    );
    // A second stream, to see that it is not touched.
    let (ours, other) = pair();
    let ours = Mutex::new(Some(ours));
    server.config.dial = Arc::new(move |_, _| Ok(ours.lock().unwrap().take().unwrap()));
    server.handle(Side::Client, Packet::new(2, open(1000, 80, "y")), &mut out);
    reports(&mut server, &mut out, |s| s.streams.contains_key(&2));
    out.clear();

    // Credit past the largest window.
    server.handle(
        Side::Client,
        Packet::new(1, Message::Credit(MAX_WINDOW)),
        &mut out,
    );
    let closed = |out: &[Packet], id: u32| {
        out.iter()
            .any(|p| p.stream == id && matches!(p.message, Message::Close))
    };
    assert!(closed(&out, 1));
    assert!(!server.streams.contains_key(&1));
    assert!(server.streams.contains_key(&2));
    out.clear();

    // More data than a window holds, delivered before anything is written.
    for _ in 0..2 {
        server.handle(
            Side::Client,
            Packet::new(2, Message::Data(vec![0; 60])),
            &mut out,
        );
    }
    assert!(closed(&out, 2), "120 bytes into a window of 100: {out:?}");
    assert!(!server.streams.contains_key(&2));
    drop(other);
}

#[test]
fn data_that_is_too_big_or_comes_after_the_end_closes_the_stream() {
    let (mut server, _target, mut out) = opened(Config::default(), 1000);
    server.handle(
        Side::Client,
        Packet::new(1, Message::Data(vec![0; MAX_DATA + 1])),
        &mut out,
    );
    assert!(matches!(
        out.as_slice(),
        [Packet {
            stream: 1,
            message: Message::Close
        }]
    ));
    let (mut server, _target, mut out) = opened(Config::default(), 1000);
    server.handle(Side::Client, Packet::new(1, Message::Eof), &mut out);
    server.handle(
        Side::Client,
        Packet::new(1, Message::Data(vec![1])),
        &mut out,
    );
    assert!(
        out.iter()
            .any(|p| matches!(p.message, Message::Close) && p.stream == 1)
    );
}

#[test]
fn credit_is_given_for_what_was_written_in_steps_of_a_quarter_window() {
    let (mut server, mut target, mut out) = opened(
        Config {
            window: 400,
            ..Config::default()
        },
        1000,
    );
    // 90 bytes: less than a quarter of the window; no credit yet.
    server.handle(
        Side::Client,
        Packet::new(1, Message::Data(vec![7; 90])),
        &mut out,
    );
    server.settle(&mut out);
    assert!(out.is_empty());
    // 20 more: over a quarter, so all 110 are given back at once.
    server.handle(
        Side::Client,
        Packet::new(1, Message::Data(vec![8; 20])),
        &mut out,
    );
    server.settle(&mut out);
    assert!(matches!(
        out.as_slice(),
        [Packet {
            stream: 1,
            message: Message::Credit(110)
        }]
    ));
    let mut heard = vec![0u8; 110];
    target.read_exact(&mut heard).unwrap();
    assert_eq!(heard.iter().filter(|&&b| b == 7).count(), 90);
}

#[test]
fn the_end_of_the_client_shuts_the_targets_writing_half_after_everything_is_written() {
    let (mut server, mut target, mut out) = opened(Config::default(), 1000);
    server.handle(
        Side::Client,
        Packet::new(1, Message::Data(vec![5; 10])),
        &mut out,
    );
    server.handle(Side::Client, Packet::new(1, Message::Eof), &mut out);
    server.settle(&mut out);
    let mut all = Vec::new();
    target.read_to_end(&mut all).unwrap();
    assert_eq!(all, vec![5; 10]);
    // The other direction is still open: the stream is there.
    assert!(server.streams.contains_key(&1));
}

#[test]
fn the_targets_data_and_then_its_end_are_queued_in_that_order() {
    let (mut server, mut target, mut out) = opened(Config::default(), 1000);
    target.write_all(&[9; 3]).unwrap();
    target.shutdown(Shutdown::Write).unwrap();
    let mut frames = Vec::new();
    // Wait for the bytes and the end to arrive at the server's socket.
    let start = Instant::now();
    while frames.len() < 2 {
        server.ready(Token::Stream(1), libc::POLLIN, &mut out);
        while let Some(frame) = server.take() {
            frames.push(frame);
        }
        assert!(start.elapsed() < PATIENCE);
    }
    let mut decoder = IncomingDecoder::new();
    for frame in &frames {
        decoder.push(frame);
    }
    let mut seen = Vec::new();
    while let Some(Incoming::Stream { from, packet }) = decoder.next_incoming().unwrap() {
        assert_eq!(from, Side::Server);
        seen.push(packet.message);
    }
    assert_eq!(seen, [Message::Data(vec![9; 3]), Message::Eof]);
    assert!(out.is_empty());
}

#[test]
fn a_socket_that_takes_no_more_still_has_what_it_says_carried() {
    // The target answers and hangs up while the client is still sending: the
    // server can no longer write to it, and what it read is not lost to that.
    let config = Config {
        window: 1000,
        ..Config::default()
    };
    let (mut server, mut target, handle, mut out) = opened_keeping(config, 1000);
    target.write_all(b"the answer").unwrap();
    target.shutdown(Shutdown::Write).unwrap();
    let start = Instant::now();
    while server.data.len() < 2 {
        server.ready(Token::Stream(1), libc::POLLIN, &mut out);
        assert!(start.elapsed() < PATIENCE);
    }
    // Reading is done; the answer sits in the queue, not yet on the wire, as
    // it does when the wire is slow. Now writing fails.
    handle.shutdown(Shutdown::Write).unwrap();
    server.handle(
        Side::Client,
        Packet::new(1, Message::Data(vec![7; 600])),
        &mut out,
    );
    server.settle(&mut out);
    assert!(
        out.iter().all(|p| p.message != Message::Close),
        "a write that failed ended the stream: {out:?}"
    );
    // What was dropped counts as taken, so the client is not held up by it...
    assert!(
        out.iter()
            .any(|p| matches!(p.message, Message::Credit(600))),
        "{out:?}"
    );
    server.handle(
        Side::Client,
        Packet::new(1, Message::Data(vec![7; 100])),
        &mut out,
    );
    server.settle(&mut out);
    server.handle(
        Side::Client,
        Packet::new(1, Message::Data(vec![7; 400])),
        &mut out,
    );
    server.settle(&mut out);
    let credited: u32 = out
        .iter()
        .filter_map(|p| match p.message {
            Message::Credit(n) => Some(n),
            _ => None,
        })
        .sum();
    assert_eq!(credited, 1100);
    // ...and the answer and its end are still there to be sent, in order.
    let mut frames = Vec::new();
    while let Some(frame) = server.take() {
        frames.push(frame);
    }
    let mut decoder = IncomingDecoder::new();
    for frame in &frames {
        decoder.push(frame);
    }
    let mut seen = Vec::new();
    while let Some(Incoming::Stream { packet, .. }) = decoder.next_incoming().unwrap() {
        seen.push(packet.message);
    }
    assert_eq!(seen, [Message::Data(b"the answer".to_vec()), Message::Eof]);
    // The stream is over when the client has said its end too.
    assert!(!server.is_empty());
    server.handle(Side::Client, Packet::new(1, Message::Eof), &mut out);
    server.settle(&mut out);
    assert!(server.is_empty());
}

#[test]
fn a_socket_is_read_only_while_there_is_window_and_the_wire_is_not_far_behind() {
    let (mut server, mut target, mut out) = opened(Config::default(), 1000);
    let has_in = |server: &Streams| {
        server
            .interest()
            .iter()
            .any(|(fd, token)| *token == Token::Stream(1) && fd.events & libc::POLLIN != 0)
    };
    // 1000 bytes of window: that many are read and no more.
    assert!(has_in(&server));
    let writer = std::thread::spawn(move || {
        target.write_all(&pattern(5000, 3)).unwrap();
        target
    });
    let mut read = 0usize;
    let start = Instant::now();
    while read < 1000 {
        server.ready(Token::Stream(1), libc::POLLIN, &mut out);
        read = 1000 - server.streams.get(&1).unwrap().send_window as usize;
        assert!(start.elapsed() < PATIENCE);
    }
    assert!(!has_in(&server), "no window left");
    // Credit opens it again.
    server.handle(
        Side::Client,
        Packet::new(1, Message::Credit(MAX_WINDOW - 1000)),
        &mut out,
    );
    assert!(has_in(&server));
    drop(writer);
}

#[test]
fn reading_stops_while_the_wire_is_far_behind() {
    let (mut server, mut target, mut out) = opened(Config::default(), MAX_WINDOW);
    let writer = std::thread::spawn(move || {
        target.write_all(&pattern(1 << 20, 4)).unwrap();
        target
    });
    let start = Instant::now();
    while server.data_bytes < QUEUED_SOFT {
        server.ready(Token::Stream(1), libc::POLLIN, &mut out);
        assert!(start.elapsed() < PATIENCE);
    }
    // One more read than the limit allows would put the queue over it by a
    // frame: the limit is checked before each read.
    assert!(server.data_bytes < QUEUED_SOFT + MAX_DATA + 64);
    let before = server.data_bytes;
    server.ready(Token::Stream(1), libc::POLLIN, &mut out);
    assert_eq!(server.data_bytes, before, "read over the limit");
    assert!(
        server
            .interest()
            .iter()
            .all(|(fd, _)| fd.events & libc::POLLIN == 0),
        "polled for reading over the limit"
    );
    // Taking frames from the queue, until it is under the limit again, opens
    // it again.
    while server.data_bytes >= QUEUED_SOFT {
        server.take().unwrap();
    }
    assert!(
        server
            .interest()
            .iter()
            .any(|(fd, _)| fd.events & libc::POLLIN != 0)
    );
    drop(writer);
}

#[test]
fn a_closed_stream_takes_its_queued_frames_with_it() {
    let (mut server, mut target, mut out) = opened(Config::default(), MAX_WINDOW);
    target.write_all(&[1; 100]).unwrap();
    let start = Instant::now();
    while server.next_len().is_none() {
        server.ready(Token::Stream(1), libc::POLLIN, &mut out);
        assert!(start.elapsed() < PATIENCE);
    }
    server.handle(Side::Client, Packet::new(1, Message::Close), &mut out);
    assert_eq!(server.next_len(), None);
    assert_eq!(server.data_bytes, 0);
    assert!(out.is_empty(), "a close is not answered");
}
