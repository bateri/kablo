use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use kablo_proto::v1::Exit;
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
    // A connection with a stream or two gives each the largest window.
    let opened = out
        .iter()
        .filter(|p| matches!(p.message, Message::Opened { window } if window == WINDOW))
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

// --- programs ------------------------------------------------------------------

fn command(argv: &[&str]) -> Command {
    Command {
        argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
        ..Command::default()
    }
}

fn sh(script: &str) -> Command {
    command(&["/bin/sh", "-c", script])
}

/// Whether the process `pid` is not there: not even as a program that ended and
/// was not collected, which still answers a signal.
fn gone(pid: u32) -> bool {
    // SAFETY: signal 0 only checks.
    let found = unsafe { libc::kill(libc::pid_t::try_from(pid).unwrap(), 0) };
    found == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn wait_gone(what: &str, pids: &[u32]) {
    let start = Instant::now();
    while !pids.iter().all(|&pid| gone(pid)) {
        assert!(start.elapsed() < PATIENCE, "{what} are still there");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A server with programs, turned by hand: what it says is kept in the order
/// the wire would carry it (a turn's control packets, then its data frames).
struct Programs {
    server: Streams,
    out: Vec<Packet>,
    decoder: IncomingDecoder,
    said: Vec<Packet>,
    /// Whether credit is given back for what is received, as a client that
    /// takes everything does.
    credit: bool,
}

impl Programs {
    fn new(config: Config) -> Programs {
        Programs {
            server: server_with(config),
            out: Vec::new(),
            decoder: IncomingDecoder::new(),
            said: Vec::new(),
            credit: true,
        }
    }

    /// Opens stream `id` as a client would.
    fn open(&mut self, id: u32, window: u32, command: Command) {
        self.send(id, Message::OpenCommand { window, command });
    }

    fn send(&mut self, id: u32, message: Message) {
        self.server
            .handle(Side::Client, Packet::new(id, message), &mut self.out);
        self.server.settle(&mut self.out);
        self.collect();
    }

    fn turn(&mut self) {
        let found = self.server.interest();
        let mut set: Vec<libc::pollfd> = found.iter().map(|(fd, _)| *fd).collect();
        sys::poll(&mut set, Some(Duration::from_millis(5))).unwrap();
        for ((_, token), fd) in found.into_iter().zip(&set) {
            if fd.revents != 0 {
                self.server.ready(token, fd.revents, &mut self.out);
            }
        }
        self.server.settle(&mut self.out);
        self.server.expire(Instant::now(), &mut self.out);
        self.collect();
    }

    /// Takes what the server has said: its packets, then its frames.
    fn collect(&mut self) {
        let mut fresh = std::mem::take(&mut self.out);
        while let Some(frame) = self.server.take() {
            self.decoder.push(&frame);
            while let Some(incoming) = self.decoder.next_incoming().unwrap() {
                match incoming {
                    Incoming::Stream { packet, .. } => fresh.push(packet),
                    other => panic!("a session message: {other:?}"),
                }
            }
        }
        if self.credit {
            for packet in &fresh {
                if let Message::Data(bytes) | Message::Stderr(bytes) = &packet.message {
                    let credit = Message::Credit(u32::try_from(bytes.len()).unwrap());
                    let credit = Packet::new(packet.stream, credit);
                    self.server.handle(Side::Client, credit, &mut self.out);
                }
            }
        }
        self.said.extend(fresh);
    }

    fn until(&mut self, what: &str, done: impl Fn(&Programs) -> bool) {
        let start = Instant::now();
        while !done(self) {
            self.turn();
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {what}");
        }
    }

    /// Everything said about stream `id`, in order.
    fn about(&self, id: u32) -> Vec<&Message> {
        self.said
            .iter()
            .filter(|packet| packet.stream == id)
            .map(|packet| &packet.message)
            .collect()
    }

    fn bytes(&self, id: u32, errors: bool) -> Vec<u8> {
        let mut all = Vec::new();
        for message in self.about(id) {
            match message {
                Message::Data(bytes) if !errors => all.extend_from_slice(bytes),
                Message::Stderr(bytes) if errors => all.extend_from_slice(bytes),
                _ => {}
            }
        }
        all
    }

    /// The program's process number while the stream is open.
    fn pid(&self, id: u32) -> u32 {
        match &self.server.streams.get(&id).unwrap().end {
            End::Program(run) => run.program.id(),
            End::Socket(_) | End::Local(_) => panic!("not a program"),
        }
    }

    fn ended(&self, id: u32) -> bool {
        self.about(id).contains(&&Message::Close)
    }

    /// Turns until the stream is over and no program is waiting to end.
    fn finish(&mut self, id: u32) {
        self.until("the end of the stream", |p| {
            p.ended(id) && p.server.dying.is_empty()
        });
    }
}

fn opened_ids(said: &[Packet]) -> Vec<u32> {
    said.iter()
        .filter(|p| matches!(p.message, Message::Opened { .. }))
        .map(|p| p.stream)
        .collect()
}

#[test]
fn a_program_says_its_piece_then_eof_then_how_it_ended_then_close() {
    let mut p = Programs::new(Config::default());
    p.open(1, 1000, command(&["/bin/echo", "hello", "$HOME"]));
    assert_eq!(opened_ids(&p.said), [1]);
    // The client never says `eof` of its own, and the stream ends all the same.
    p.finish(1);
    assert_eq!(
        p.about(1),
        [
            &Message::Opened {
                window: Config::default().window
            },
            &Message::Data(b"hello $HOME\n".to_vec()),
            &Message::Eof,
            &Message::Exited(Exit::Code(0)),
            &Message::Close,
        ]
    );
    assert!(p.server.is_empty());
}

#[test]
fn standard_error_is_kept_apart_and_the_exit_code_is_the_programs() {
    let mut p = Programs::new(Config::default());
    p.open(1, 1000, sh("echo out; echo err >&2; exit 3"));
    p.finish(1);
    assert_eq!(p.bytes(1, false), b"out\n");
    assert_eq!(p.bytes(1, true), b"err\n");
    let tail: Vec<_> = p.about(1).into_iter().rev().take(3).collect();
    assert_eq!(
        tail,
        [
            &Message::Close,
            &Message::Exited(Exit::Code(3)),
            &Message::Eof
        ]
    );
}

#[test]
fn a_signal_that_ended_a_program_is_told_as_one() {
    let mut p = Programs::new(Config::default());
    p.open(1, 1000, sh("kill -9 $$"));
    p.finish(1);
    assert!(
        p.about(1)
            .contains(&&Message::Exited(Exit::Signal(libc::SIGKILL)))
    );
}

#[test]
fn the_end_comes_after_the_last_byte_even_when_the_pipes_close_before_the_program() {
    let mut p = Programs::new(Config::default());
    p.open(1, 1000, sh("echo last; exec >&- 2>&-; sleep 0.3; exit 4"));
    p.finish(1);
    let said = p.about(1);
    let eof = said.iter().position(|m| **m == Message::Eof).unwrap();
    let exited = said
        .iter()
        .position(|m| **m == Message::Exited(Exit::Code(4)))
        .unwrap();
    assert!(eof < exited, "{said:?}");
    assert_eq!(p.bytes(1, false), b"last\n");
}

#[test]
fn what_is_sent_in_goes_to_the_program_and_comes_back_within_the_windows() {
    // The window this end offers is 16 KiB; 200 KiB goes through `cat`, sent
    // only as credit comes.
    let window = 16 * 1024;
    let mut p = Programs::new(Config {
        window,
        ..Config::default()
    });
    p.open(1, MAX_WINDOW, command(&["/bin/cat"]));
    let input = pattern(200 * 1024, 7);
    let credited = |p: &Programs| -> usize {
        p.about(1)
            .iter()
            .map(|m| match m {
                Message::Credit(bytes) => *bytes as usize,
                _ => 0,
            })
            .sum()
    };
    let mut sent = 0;
    let start = Instant::now();
    while sent < input.len() {
        let room = (window as usize + credited(&p)).saturating_sub(sent);
        let piece = room.min(4096).min(input.len() - sent);
        if piece > 0 {
            p.send(1, Message::Data(input[sent..sent + piece].to_vec()));
            sent += piece;
        }
        p.turn();
        assert!(start.elapsed() < PATIENCE, "the input never went in");
    }
    p.send(1, Message::Eof);
    p.finish(1);
    assert_eq!(p.bytes(1, false), input);
    assert!(
        p.about(1).contains(&&Message::Exited(Exit::Code(0))),
        "{:?}",
        p.about(1).last()
    );
}

#[test]
fn a_stream_that_sends_past_its_window_loses_its_program() {
    let mut p = Programs::new(Config {
        window: 1000,
        ..Config::default()
    });
    p.open(1, 1000, command(&["/bin/cat"]));
    let pid = p.pid(1);
    p.send(1, Message::Data(vec![b'x'; 1001]));
    assert!(p.ended(1), "a close was expected");
    p.until("the program to be collected", |p| p.server.dying.is_empty());
    wait_gone("the program of a stream that broke the rules", &[pid]);
    assert!(p.server.is_empty());
}

#[test]
fn output_and_errors_are_paid_for_out_of_one_window() {
    let mut p = Programs::new(Config::default());
    p.credit = false;
    // Out and err as fast as they are taken, for ever. (How the window comes to
    // be split between them is up to which pipe is read first.)
    p.open(1, 1000, sh("while :; do echo out; echo err >&2; done"));
    let received = |p: &Programs| p.bytes(1, false).len() + p.bytes(1, true).len();
    p.until("the window to be used", |p| received(p) >= 1000);
    for _ in 0..40 {
        p.turn();
    }
    assert_eq!(
        received(&p),
        1000,
        "no more than the window, however it is split"
    );
    // Credit lets more through, and still within it.
    p.send(1, Message::Credit(500));
    p.until("more", |p| received(p) >= 1500);
    for _ in 0..40 {
        p.turn();
    }
    assert_eq!(received(&p), 1500);
    p.send(1, Message::Close);
    p.until("the program to go", |p| p.server.dying.is_empty());
}

#[test]
fn errors_alone_use_the_window_too() {
    let mut p = Programs::new(Config::default());
    p.credit = false;
    p.open(1, 1000, sh("while :; do echo err >&2; done"));
    p.until("the window to be used", |p| p.bytes(1, true).len() >= 1000);
    for _ in 0..40 {
        p.turn();
    }
    assert_eq!(p.bytes(1, true).len(), 1000);
    assert!(p.bytes(1, false).is_empty());
    p.send(1, Message::Credit(100));
    p.until("more", |p| p.bytes(1, true).len() >= 1100);
    p.send(1, Message::Close);
    p.until("the program to go", |p| p.server.dying.is_empty());
}

#[test]
fn a_command_that_is_not_valid_or_does_not_start_or_is_over_a_limit_is_failed() {
    let mut p = Programs::new(Config {
        max_streams: 2,
        ..Config::default()
    });
    let mut nul = command(&["/bin/echo"]);
    nul.argv.push(b"a\0b".to_vec());
    p.open(1, 1000, Command::default());
    p.open(2, 1000, nul);
    p.open(3, 1000, command(&["/no/such/program"]));
    let mut ask_home = command(&["/bin/true"]);
    ask_home.cwd = Some(b"/no/such/directory".to_vec());
    p.open(4, 1000, ask_home);
    p.open(5, MAX_WINDOW + 1, command(&["/bin/true"]));
    let failures: Vec<(u32, FailCode)> = failed(&p.said);
    assert_eq!(
        failures,
        [
            (1, FailCode::BAD_REQUEST),
            (2, FailCode::BAD_REQUEST),
            (3, FailCode::START_FAILED),
            (4, FailCode::START_FAILED),
            (5, FailCode::LIMIT),
        ]
    );
    let words: Vec<&str> = p
        .said
        .iter()
        .filter_map(|packet| match &packet.message {
            Message::Failed { code, text } if *code == FailCode::START_FAILED => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    // The program that is not there is said by the system; the directory that
    // is not there is named, since the system's answer would name the program.
    assert_eq!(
        words,
        [
            "No such file or directory",
            "cannot enter /no/such/directory: No such file or directory"
        ]
    );
    assert!(p.server.is_empty(), "nothing was started");

    // The streams limit counts programs.
    p.open(6, 1000, command(&["/bin/cat"]));
    p.open(7, 1000, command(&["/bin/cat"]));
    p.open(8, 1000, command(&["/bin/cat"]));
    assert_eq!(opened_ids(&p.said), [6, 7]);
    assert_eq!(failed(&p.said).last(), Some(&(8, FailCode::LIMIT)));
    // (Dropping the engine ends what is left.)
}

#[test]
fn an_open_that_was_numbered_before_is_not_answered_twice() {
    let mut p = Programs::new(Config::default());
    p.open(5, 1000, command(&["/bin/cat"]));
    p.open(5, 1000, command(&["/bin/cat"]));
    p.open(3, 1000, command(&["/bin/cat"]));
    assert_eq!(opened_ids(&p.said), [5]);
    assert_eq!(p.server.len(), 1);
}

#[test]
fn commands_run_when_connections_are_turned_off() {
    let mut p = Programs::new(Config::default());
    p.server.set_closed(Some("off".to_owned()));
    p.send(1, open(1000, 80, "example.test"));
    p.open(2, 1000, command(&["/bin/echo", "still runs"]));
    assert_eq!(failed(&p.said), [(1, FailCode::FORBIDDEN)]);
    p.finish(2);
    assert_eq!(p.bytes(2, false), b"still runs\n");
}

#[test]
fn a_close_ends_the_program_and_what_it_started_and_collects_them() {
    let mut p = Programs::new(Config::default());
    p.open(1, 1000, sh("sleep 60 & echo $!; echo $$; wait"));
    p.until("the program to say who it is", |p| {
        p.bytes(1, false).iter().filter(|&&b| b == b'\n').count() == 2
    });
    let said = String::from_utf8(p.bytes(1, false)).unwrap();
    let pids: Vec<u32> = said.lines().map(|l| l.parse().unwrap()).collect();
    assert_eq!(pids.len(), 2);
    assert_eq!(pids.get(1), Some(&p.pid(1)));
    assert!(!pids.iter().any(|&pid| gone(pid)));
    p.send(1, Message::Close);
    assert!(p.server.is_empty(), "the stream is gone at once");
    assert_eq!(p.server.dying.len(), 1);
    p.until("the program to be collected", |p| p.server.dying.is_empty());
    wait_gone("the program and what it started", &pids);
    assert_eq!(
        p.server.deadline(),
        None,
        "no timer without a program waiting"
    );
}

#[test]
fn a_program_that_ignores_the_signal_is_killed_when_the_time_is_up() {
    let wait = Duration::from_millis(300);
    let mut p = Programs::new(Config {
        kill_wait: wait,
        ..Config::default()
    });
    p.open(
        1,
        1000,
        sh("trap '' TERM; echo ready; while :; do sleep 1; done"),
    );
    p.until("the program to be ready", |p| !p.bytes(1, false).is_empty());
    let pid = p.pid(1);
    let closed = Instant::now();
    p.send(1, Message::Close);
    assert!(
        p.server.deadline().is_some(),
        "the time is up at some point"
    );
    p.until("the program to be collected", |p| p.server.dying.is_empty());
    assert!(closed.elapsed() >= wait, "it was killed before its time");
    wait_gone("the program that ignored the signal", &[pid]);
}

#[test]
fn shutting_down_ends_every_program_and_waits_for_them() {
    let wait = Duration::from_millis(300);
    let mut p = Programs::new(Config {
        kill_wait: wait,
        ..Config::default()
    });
    p.open(1, 1000, sh("echo ready; exec sleep 60"));
    p.open(
        2,
        1000,
        sh("trap '' TERM; echo ready; while :; do sleep 1; done"),
    );
    p.open(3, 1000, command(&["/bin/echo", "done already"]));
    p.until("the programs to be ready", |p| {
        !p.bytes(1, false).is_empty() && !p.bytes(2, false).is_empty() && p.ended(3)
    });
    let pids = [p.pid(1), p.pid(2)];
    let started = Instant::now();
    p.server.shutdown();
    assert!(started.elapsed() < wait + SHUTDOWN_REAP_WAIT, "it gave up");
    assert!(p.server.is_empty() && p.server.dying.is_empty());
    wait_gone("the programs of a connection that ended", &pids);
}

#[test]
fn a_program_that_is_dropped_with_the_engine_is_collected_too() {
    let mut p = Programs::new(Config::default());
    p.open(1, 1000, command(&["/bin/cat"]));
    let pid = p.pid(1);
    drop(p);
    assert!(gone(pid));
}

// --- commands, at the client ---------------------------------------------------

/// The test's ends of the descriptors a command is joined to.
struct Terminal {
    /// Written to give the program input; dropping it ends the input.
    typed: Option<OwnedFd>,
    /// Read for what the program wrote as output.
    shown: OwnedFd,
    /// Read for what it wrote as errors.
    warned: OwnedFd,
}

/// The descriptors a client gives a command, and the test's ends of them.
fn terminal() -> (Local, Terminal) {
    let (input, typed) = sys::pipe().unwrap();
    let (shown, output) = sys::pipe().unwrap();
    let (warned, errors) = sys::pipe().unwrap();
    for fd in [&shown, &warned] {
        sys::set_nonblocking(fd.as_raw_fd()).unwrap();
    }
    (
        Local {
            input,
            output,
            errors,
        },
        Terminal {
            typed: Some(typed),
            shown,
            warned,
        },
    )
}

/// What a pipe has now.
fn available(fd: &OwnedFd) -> Vec<u8> {
    let mut all = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        match sys::read_fd(fd.as_fd(), &mut buf) {
            Ok(0) => return all,
            Ok(n) => all.extend_from_slice(buf.get(..n).unwrap()),
            Err(e) if e.kind() == ErrorKind::WouldBlock => return all,
            Err(e) => panic!("{e}"),
        }
    }
}

/// The ends of a command, turned by hand against a server whose answers the
/// test makes up.
struct Alone {
    client: Streams,
    out: Vec<Packet>,
    events: Vec<Event>,
}

impl Alone {
    /// A client with a command open and answered with `window`.
    fn opened(window: u32, local: Local) -> Alone {
        let mut client = Streams::new(
            Side::Client,
            Config {
                window,
                ..Config::default()
            },
        );
        let mut out = Vec::new();
        client
            .open_command(local, command(&["/bin/cat"]), &mut out)
            .unwrap();
        assert!(matches!(
            out.first().map(|p| &p.message),
            Some(Message::OpenCommand { .. })
        ));
        out.clear();
        let mut alone = Alone {
            client,
            out,
            events: Vec::new(),
        };
        alone.hears(Message::Opened { window });
        assert_eq!(alone.events, [Event::Opened(1)]);
        alone
    }

    fn hears(&mut self, message: Message) {
        self.client
            .handle(Side::Server, Packet::new(1, message), &mut self.out);
        self.client.settle(&mut self.out);
        self.events.extend(self.client.take_events());
    }

    /// One look at the descriptors, as a loop would.
    fn turn(&mut self) {
        let found = self.client.interest();
        let mut set: Vec<libc::pollfd> = found.iter().map(|(fd, _)| *fd).collect();
        sys::poll(&mut set, Some(Duration::from_millis(5))).unwrap();
        for ((_, token), fd) in found.into_iter().zip(&set) {
            if fd.revents != 0 {
                self.client.ready(token, fd.revents, &mut self.out);
            }
        }
        self.client.settle(&mut self.out);
        self.events.extend(self.client.take_events());
    }

    fn closed(&self) -> bool {
        self.out.iter().any(|p| p.message == Message::Close)
    }
}

#[test]
fn a_command_run_for_a_client_keeps_output_and_errors_apart_and_ends_with_the_programs_status() {
    let mut rig = Rig::new(MAX_WINDOW);
    let (local, term) = terminal();
    // The input is never ended, and the command ends all the same.
    let id = rig
        .client
        .open_command(local, sh("echo out; echo err >&2; exit 3"), &mut rig.c_out)
        .unwrap();
    assert_eq!(id, 1);
    let mut events = Vec::new();
    rig.until("the end", |r| {
        events.extend(r.client.take_events());
        events.iter().any(|e| matches!(e, Event::Exited { .. }))
    });
    assert_eq!(
        events,
        [
            Event::Opened(1),
            Event::Exited {
                stream: 1,
                exit: Exit::Code(3)
            }
        ]
    );
    assert_eq!(available(&term.shown), b"out\n");
    assert_eq!(available(&term.warned), b"err\n");
    assert!(rig.client.is_empty(), "the stream is over");
    rig.until("the server to let the program go", |r| {
        r.server.is_empty() && r.server.dying.is_empty()
    });
}

#[test]
fn a_signal_that_ended_the_program_is_told_as_one() {
    let mut rig = Rig::new(MAX_WINDOW);
    let (local, _term) = terminal();
    rig.client
        .open_command(local, sh("kill -9 $$"), &mut rig.c_out)
        .unwrap();
    let mut events = Vec::new();
    rig.until("the end", |r| {
        events.extend(r.client.take_events());
        events.iter().any(|e| matches!(e, Event::Exited { .. }))
    });
    assert_eq!(
        events.last(),
        Some(&Event::Exited {
            stream: 1,
            exit: Exit::Signal(libc::SIGKILL)
        })
    );
}

#[test]
fn what_is_typed_goes_to_the_program_and_comes_back_though_it_is_more_than_a_window() {
    let window = 4000;
    let mut rig = Rig::new(window);
    let (local, mut term) = terminal();
    rig.client
        .open_command(local, command(&["/bin/cat"]), &mut rig.c_out)
        .unwrap();
    let sent = pattern(300_000, 5);
    let typed = term.typed.take().unwrap();
    let writer = {
        let sent = sent.clone();
        std::thread::spawn(move || {
            let mut file = std::fs::File::from(typed);
            file.write_all(&sent).unwrap();
            // Dropped: the end of the input.
        })
    };
    // The test's own end of the pipe, read by a thread that waits for it.
    // SAFETY: plain flag changes on a descriptor this test owns.
    unsafe {
        let flags = libc::fcntl(term.shown.as_raw_fd(), libc::F_GETFL);
        libc::fcntl(
            term.shown.as_raw_fd(),
            libc::F_SETFL,
            flags & !libc::O_NONBLOCK,
        );
    }
    let shown = std::fs::File::from(term.shown);
    let reader = std::thread::spawn(move || {
        let mut all = Vec::new();
        let mut shown = shown;
        // (The pipe is the test's end only: it ends when the engine lets go of
        // the other end.)
        shown.read_to_end(&mut all).unwrap();
        all
    });
    let mut events = Vec::new();
    rig.until("the end", |r| {
        events.extend(r.client.take_events());
        events.iter().any(|e| matches!(e, Event::Exited { .. }))
    });
    // The engine's side of the pipes goes with it.
    drop(rig.client);
    writer.join().unwrap();
    let back = reader.join().unwrap();
    assert_eq!(back.len(), sent.len());
    assert!(back == sent, "what came back is not what was typed");
}

#[test]
fn an_input_that_is_at_its_end_ends_the_programs_input_at_once() {
    let mut rig = Rig::new(MAX_WINDOW);
    let (mut local, term) = terminal();
    local.input = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    rig.client
        .open_command(local, sh("cat; echo done"), &mut rig.c_out)
        .unwrap();
    let mut events = Vec::new();
    rig.until("the end", |r| {
        events.extend(r.client.take_events());
        events.iter().any(|e| matches!(e, Event::Exited { .. }))
    });
    assert_eq!(available(&term.shown), b"done\n");
}

#[test]
fn a_program_that_does_not_start_is_a_failed_event_and_leaves_nothing() {
    let mut rig = Rig::new(MAX_WINDOW);
    let (local, _term) = terminal();
    rig.client
        .open_command(local, command(&["/no/such/program"]), &mut rig.c_out)
        .unwrap();
    let mut events = Vec::new();
    rig.until("the answer", |r| {
        events.extend(r.client.take_events());
        !events.is_empty()
    });
    assert!(
        matches!(&events[..], [Event::Failed { code, text, .. }]
            if *code == FailCode::START_FAILED && text == "No such file or directory"),
        "{events:?}"
    );
    assert!(rig.client.is_empty());
}

#[test]
fn a_command_that_is_not_valid_is_not_sent() {
    let mut client = Streams::new(Side::Client, Config::default());
    let (local, _term) = terminal();
    let mut out = Vec::new();
    assert!(
        client
            .open_command(local, Command::default(), &mut out)
            .is_err()
    );
    assert!(out.is_empty());
    assert!(client.is_empty());
}

#[test]
fn a_server_that_closes_a_command_early_closes_it_here_and_the_program_goes() {
    let mut rig = Rig::new(MAX_WINDOW);
    let (local, _term) = terminal();
    rig.client
        .open_command(local, command(&["/bin/cat"]), &mut rig.c_out)
        .unwrap();
    rig.until("the program to be running", |r| {
        r.server.streams.contains_key(&1)
    });
    let pid = {
        let End::Program(run) = &rig.server.streams.get(&1).unwrap().end else {
            panic!("not a program");
        };
        run.program.id()
    };
    let mut events = rig.client.take_events();
    rig.client
        .handle(Side::Server, Packet::new(1, Message::Close), &mut rig.c_out);
    events.extend(rig.client.take_events());
    assert_eq!(
        events,
        [
            Event::Opened(1),
            Event::Closed {
                stream: 1,
                cause: Cause::Server
            }
        ]
    );
    assert!(rig.client.is_empty());
    // (The server end of this test did not close it; the client does.)
    rig.server
        .handle(Side::Client, Packet::new(1, Message::Close), &mut rig.s_out);
    rig.until("the program to be collected", |r| r.server.dying.is_empty());
    wait_gone("the program of a stream the server closed", &[pid]);
}

#[test]
fn a_command_is_closed_from_here_with_a_close() {
    let mut rig = Rig::new(MAX_WINDOW);
    let (local, _term) = terminal();
    rig.client
        .open_command(local, command(&["/bin/cat"]), &mut rig.c_out)
        .unwrap();
    rig.until("the program to be running", |r| {
        r.server.streams.contains_key(&1)
    });
    let pid = {
        let End::Program(run) = &rig.server.streams.get(&1).unwrap().end else {
            panic!("not a program");
        };
        run.program.id()
    };
    rig.client.close(1, &mut rig.c_out);
    assert!(rig.client.is_empty());
    rig.until("the program to be collected", |r| {
        r.server.is_empty() && r.server.dying.is_empty()
    });
    wait_gone("the program of a stream closed here", &[pid]);
    // A stream that is not there is nothing to close.
    let mut none = Vec::new();
    rig.client.close(1, &mut none);
    assert!(none.is_empty());
}

#[test]
fn a_reader_that_has_gone_away_ends_the_command() {
    let mut rig = Rig::new(MAX_WINDOW);
    let (local, term) = terminal();
    drop(term.shown);
    rig.client
        .open_command(local, command(&["/usr/bin/yes"]), &mut rig.c_out)
        .unwrap();
    let mut events = Vec::new();
    rig.until("the command to give up", |r| {
        events.extend(r.client.take_events());
        events.iter().any(|e| matches!(e, Event::Closed { .. }))
    });
    assert!(
        events.contains(&Event::Closed {
            stream: 1,
            cause: Cause::Local(ErrorKind::BrokenPipe)
        }),
        "{events:?}"
    );
    assert!(rig.client.is_empty());
    // The close reaches the server and the program is let go.
    rig.until("the program to go", |r| {
        r.server.is_empty() && r.server.dying.is_empty()
    });
}

#[test]
fn what_the_program_wrote_is_not_lost_to_the_close_that_follows_its_end() {
    // A window far larger than a pipe holds, and a reader that takes its time.
    let (local, term) = terminal();
    let mut alone = Alone::opened(MAX_WINDOW, local);
    let sent = pattern(200_000, 9);
    for piece in sent.chunks(MAX_DATA) {
        alone.hears(Message::Data(piece.to_vec()));
    }
    alone.hears(Message::Eof);
    alone.hears(Message::Exited(Exit::Code(0)));
    alone.hears(Message::Close);
    assert!(
        !alone
            .events
            .iter()
            .any(|e| matches!(e, Event::Exited { .. })),
        "all of it has not been written yet: {:?}",
        alone.events
    );
    assert!(!alone.client.is_empty());
    let mut got = Vec::new();
    let start = Instant::now();
    while !alone
        .events
        .iter()
        .any(|e| matches!(e, Event::Exited { .. }))
    {
        got.extend(available(&term.shown));
        alone.turn();
        assert!(start.elapsed() < PATIENCE, "the end never came");
    }
    got.extend(available(&term.shown));
    assert_eq!(got.len(), sent.len());
    assert!(got == sent);
    assert!(
        !alone
            .events
            .iter()
            .any(|e| matches!(e, Event::Closed { .. }))
    );
    assert!(alone.client.is_empty());
}

#[test]
fn errors_are_paid_for_out_of_the_window_and_given_back_as_they_are_written() {
    let (local, term) = terminal();
    let mut alone = Alone::opened(1000, local);
    alone.hears(Message::Stderr(vec![b'e'; 400]));
    alone.hears(Message::Data(vec![b'o'; 300]));
    // Written at once (the pipes are empty), and credit given for both once a
    // quarter of the window is.
    assert_eq!(available(&term.warned), vec![b'e'; 400]);
    assert_eq!(available(&term.shown), vec![b'o'; 300]);
    let credited: u32 = alone
        .out
        .iter()
        .filter_map(|p| match p.message {
            Message::Credit(n) => Some(n),
            _ => None,
        })
        .sum();
    assert_eq!(credited, 700);
    // More than the window it gave is not allowed, the errors included.
    alone.hears(Message::Stderr(vec![b'e'; 1001]));
    assert!(alone.closed());
    assert_eq!(
        alone.events.last(),
        Some(&Event::Closed {
            stream: 1,
            cause: Cause::Server
        })
    );
}

#[test]
fn an_end_that_is_told_out_of_turn_closes_the_stream() {
    // `exited` before the `eof`.
    let (local, _term) = terminal();
    let mut alone = Alone::opened(1000, local);
    alone.hears(Message::Exited(Exit::Code(0)));
    assert!(alone.closed());
    assert!(matches!(
        alone.events.last(),
        Some(Event::Closed {
            cause: Cause::Server,
            ..
        })
    ));

    // `stderr` after the `eof`.
    let (local, _term) = terminal();
    let mut alone = Alone::opened(1000, local);
    alone.hears(Message::Eof);
    alone.hears(Message::Stderr(b"late".to_vec()));
    assert!(alone.closed());

    // `data` before `opened`.
    let (local, _term) = terminal();
    let mut client = Streams::new(Side::Client, Config::default());
    let mut out = Vec::new();
    client
        .open_command(local, command(&["/bin/cat"]), &mut out)
        .unwrap();
    out.clear();
    client.handle(
        Side::Server,
        Packet::new(1, Message::Data(b"early".to_vec())),
        &mut out,
    );
    assert_eq!(out, [Packet::new(1, Message::Close)]);
}

#[test]
fn errors_and_ends_belong_to_commands_and_not_to_connections() {
    for message in [
        Message::Stderr(b"no".to_vec()),
        Message::Exited(Exit::Code(0)),
    ] {
        let (ours, _theirs) = pair();
        let mut client = Streams::new(Side::Client, Config::default());
        let mut out = Vec::new();
        client.open(ours, "x", 80, &mut out).unwrap();
        out.clear();
        client.handle(
            Side::Server,
            Packet::new(1, Message::Opened { window: 1000 }),
            &mut out,
        );
        client.handle(Side::Server, Packet::new(1, Message::Eof), &mut out);
        client.handle(Side::Server, Packet::new(1, message), &mut out);
        assert_eq!(out, [Packet::new(1, Message::Close)]);
        assert!(client.is_empty());
        assert!(
            !client
                .take_events()
                .iter()
                .any(|e| matches!(e, Event::Closed { .. })),
            "a connection is not a command"
        );
    }
}

#[test]
fn only_a_bridge_that_says_it_runs_commands_is_asked_to() {
    let with = Features::default().with_commands();
    assert_eq!(
        judge_commands(&SessionMessage::FeaturesReply(with.clone())),
        Some(with)
    );
    // Streams off, commands on: the administrator's switch is not theirs.
    let off = Features::streams_off("no".to_owned()).with_commands();
    assert!(judge_commands(&SessionMessage::FeaturesReply(off)).is_some());
    assert_eq!(
        judge_commands(&SessionMessage::FeaturesReply(Features::default())),
        None
    );
    assert_eq!(judge_commands(&SessionMessage::Pong(Vec::new())), None);
}

#[test]
fn errors_that_cannot_be_written_are_dropped_with_credit_and_the_program_goes_on() {
    let (local, term) = terminal();
    // Nobody reads the errors.
    drop(term.warned);
    let mut alone = Alone::opened(1000, local);
    alone.hears(Message::Stderr(vec![b'e'; 400]));
    alone.hears(Message::Data(vec![b'o'; 100]));
    alone.hears(Message::Stderr(vec![b'e'; 300]));
    assert_eq!(available(&term.shown), vec![b'o'; 100]);
    let credited: u32 = alone
        .out
        .iter()
        .filter_map(|p| match p.message {
            Message::Credit(n) => Some(n),
            _ => None,
        })
        .sum();
    assert_eq!(credited, 800, "dropped bytes count as taken");
    assert!(!alone.closed(), "the program was not ended");
    alone.hears(Message::Eof);
    alone.hears(Message::Exited(Exit::Code(4)));
    assert_eq!(
        alone.events.last(),
        Some(&Event::Exited {
            stream: 1,
            exit: Exit::Code(4)
        })
    );
}

#[test]
fn an_output_with_no_reader_ends_the_program_unless_it_has_ended_already() {
    // The reader goes away while the program runs: it is ended.
    let (local, term) = terminal();
    drop(term.shown);
    let mut alone = Alone::opened(1000, local);
    alone.hears(Message::Data(vec![b'o'; 10]));
    assert!(alone.closed());
    assert_eq!(
        alone.events.last(),
        Some(&Event::Closed {
            stream: 1,
            cause: Cause::Local(ErrorKind::BrokenPipe)
        })
    );

    // The program has ended and said how, and its last words have nowhere to
    // go: how it ended is the result. (`data`, `eof` and `exited` come in one
    // breath, as a server says them.)
    let (local, term) = terminal();
    drop(term.shown);
    let mut alone = Alone::opened(1000, local);
    for message in [
        Message::Data(b"x".to_vec()),
        Message::Eof,
        Message::Exited(Exit::Code(7)),
    ] {
        alone
            .client
            .handle(Side::Server, Packet::new(1, message), &mut alone.out);
    }
    alone.client.settle(&mut alone.out);
    alone.events.extend(alone.client.take_events());
    assert_eq!(
        alone.events.last(),
        Some(&Event::Exited {
            stream: 1,
            exit: Exit::Code(7)
        })
    );
    assert!(!alone.closed());
    assert!(alone.client.is_empty());
}

#[test]
fn a_program_that_has_ended_reads_no_more_input() {
    // The output is full, so what the program wrote waits and the stream is
    // not over; the input is not read meanwhile.
    let (input, _typed) = sys::pipe().unwrap();
    let (_shown, output) = sys::pipe().unwrap();
    let (_warned, errors) = sys::pipe().unwrap();
    let mut alone = Alone::opened(
        MAX_WINDOW,
        Local {
            input,
            output,
            errors,
        },
    );
    for piece in pattern(200_000, 1).chunks(MAX_DATA) {
        alone.hears(Message::Data(piece.to_vec()));
    }
    alone.hears(Message::Eof);
    alone.hears(Message::Exited(Exit::Code(0)));
    assert!(alone.client.ended(1));
    let reads = |alone: &Alone| {
        alone
            .client
            .interest()
            .iter()
            .any(|(_, token)| matches!(token, Token::LocalInput(_)))
    };
    assert!(!reads(&alone), "the input is still being read");
}

#[test]
fn a_regular_file_takes_what_it_is_given_in_one_write() {
    let (local, _term) = terminal();
    let dir = std::env::temp_dir().join(format!("kablo-streams-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("out");
    let file = std::fs::File::create(&path).unwrap();
    let local = Local {
        output: OwnedFd::from(file),
        ..local
    };
    let mut alone = Alone::opened(MAX_WINDOW, local);
    let sent = pattern(MAX_DATA, 2);
    alone.hears(Message::Data(sent.clone()));
    // Written at once, all of it, though a pipe is given 512 bytes at a time.
    assert_eq!(std::fs::read(&path).unwrap(), sent);
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir(&dir);
}

// --- the window the streams share ------------------------------------------------

/// The windows the server gave, in the order it gave them.
fn given(said: &[Packet]) -> Vec<u32> {
    said.iter()
        .filter_map(|p| match p.message {
            Message::Opened { window } => Some(window),
            _ => None,
        })
        .collect()
}

/// The credit the server gave stream `id`, in order.
fn credits(p: &Programs, id: u32) -> Vec<u32> {
    p.about(id)
        .into_iter()
        .filter_map(|m| match m {
            Message::Credit(bytes) => Some(*bytes),
            _ => None,
        })
        .collect()
}

/// A program that takes its input and says nothing.
fn sink() -> Command {
    sh("cat >/dev/null")
}

#[test]
fn the_streams_of_a_connection_share_the_window_within_the_least_and_the_largest() {
    let mut p = Programs::new(Config {
        window: 1000,
        budget: 2000,
        least: 100,
        ..Config::default()
    });
    for id in 1..=25 {
        p.open(id, 1000, sink());
    }
    let given = given(&p.said);
    assert_eq!(given.len(), 25);
    // 2000 shared by one, two, three, four: the largest twice, then a part.
    assert_eq!(given[..4], [1000, 1000, 666, 500]);
    // By twenty streams a part is the least, and it goes no lower.
    assert_eq!(given[19], 100);
    assert!(given[19..].iter().all(|&w| w == 100), "{given:?}");
    for id in 1..=25 {
        p.send(id, Message::Eof);
    }
    for id in 1..=25 {
        p.finish(id);
    }
}

#[test]
fn credit_brings_a_stream_to_its_part_as_other_streams_come_and_go() {
    let mut p = Programs::new(Config {
        window: 1000,
        budget: 1000,
        least: 100,
        ..Config::default()
    });
    p.open(1, 1000, sink());
    p.open(2, 1000, sink());
    // The first had the whole budget; the second is given half of it, and half
    // is the first one's part from now on.
    assert_eq!(given(&p.said), [1000, 500]);
    // 400 bytes taken: the other end may still send 600, more than the part,
    // so nothing is given back.
    p.send(1, Message::Data(vec![b'x'; 400]));
    assert_eq!(credits(&p, 1), []);
    // 400 more: 200 left, and 300 of credit makes it the part.
    p.send(1, Message::Data(vec![b'x'; 400]));
    assert_eq!(credits(&p, 1), [300]);
    // The second ends, and the first's part is the whole budget again: the
    // next credit gives what it took and the difference.
    p.send(2, Message::Eof);
    p.finish(2);
    p.send(1, Message::Data(vec![b'x'; 100]));
    assert_eq!(credits(&p, 1), [300, 600]);
    p.send(1, Message::Eof);
    p.finish(1);
}

#[test]
fn credit_never_takes_the_other_end_past_the_largest_window() {
    // However much has been taken, what the other end may send stays within the
    // part, which stays within the protocol's ceiling.
    assert!(Config::default().window <= MAX_WINDOW);
    let mut p = Programs::new(Config::default());
    p.open(1, 1000, sink());
    assert_eq!(given(&p.said), [WINDOW]);
    // Twice the window, sent as a client would: only into the window it has.
    let start = Instant::now();
    let mut sent = 0;
    while sent < 2 * WINDOW as usize {
        let room = p.server.streams.get(&1).unwrap().recv_window as usize;
        if room >= MAX_DATA {
            p.send(1, Message::Data(vec![b'x'; MAX_DATA]));
            sent += MAX_DATA;
        } else {
            p.turn();
        }
        let stream = p.server.streams.get(&1).unwrap();
        assert!(stream.recv_window + stream.held() <= WINDOW);
        assert!(
            start.elapsed() < PATIENCE,
            "the program stopped taking input"
        );
    }
    assert!(!credits(&p, 1).is_empty());
    p.send(1, Message::Eof);
    p.finish(1);
}
