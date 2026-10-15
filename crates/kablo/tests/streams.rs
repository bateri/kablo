//! Streams through a real bridge: TCP connections it makes for a client, on
//! channel 1, next to sessions.
//!
//! The tests are about order and about what each side is told, not about time:
//! nothing here asserts that something took long or short, only that it happened
//! (and the patience is long). The targets are listeners of the test's own on
//! `127.0.0.1:0`.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::net::Shutdown;
use std::sync::Mutex;
use std::sync::mpsc;

use common::{Bridge, Env, PATIENCE, closed_port, echo, pattern, target};
use kablo::streams::{self as engine, Verdict};
use kablo_proto::tlv;
use kablo_proto::v1::stream::{self, FailCode, MAX_STREAMS, MAX_WINDOW};
use kablo_proto::v1::{Message, Packet, PacketDecoder};

/// A session that echoes what is typed, attached from the start.
fn echo_session(env: &mut Env, bridge: &mut Bridge, label: &str) -> String {
    let id = env.name(label);
    env.track(&id);
    bridge.try_new(Some(&id), &["/bin/cat"]).unwrap();
    bridge.attach(&id, 0);
    id
}

#[test]
fn bytes_cross_in_order_both_ways_and_an_end_is_an_end_not_a_close() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let request = pattern(150_000, 1);
    let reply = pattern(100_000, 2);
    let (heard_tx, heard_rx) = mpsc::channel();
    let heard_tx = Mutex::new(heard_tx);
    let sent_back = reply.clone();
    let port = target(move |mut connection| {
        // Everything up to the end, then an answer on the half that is open.
        let mut heard = Vec::new();
        connection.read_to_end(&mut heard).unwrap();
        heard_tx.lock().unwrap().send(heard).unwrap();
        connection.write_all(&sent_back).unwrap();
        let _ = connection.shutdown(Shutdown::Write);
    });

    let (id, window) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
    // A stream of its own is given the largest window the bridge gives.
    assert_eq!(window, engine::WINDOW);
    bridge.stream_send(id, &request, 7_000);
    bridge.stream_eof(id);
    bridge.until("the answer and its end", |b| {
        b.flows[&id].eof && b.flows[&id].data.len() >= reply.len()
    });
    assert_eq!(bridge.flows[&id].data, reply);
    assert!(!bridge.flows[&id].closed, "an end is not a close");
    assert_eq!(heard_rx.recv_timeout(PATIENCE).unwrap(), request);
}

#[test]
fn a_close_ends_the_targets_socket() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let (ended_tx, ended_rx) = mpsc::channel();
    let ended_tx = Mutex::new(ended_tx);
    let port = target(move |mut connection| {
        let mut heard = Vec::new();
        // An end or a reset: either way the socket is over.
        let _ = connection.read_to_end(&mut heard);
        ended_tx.lock().unwrap().send(heard).unwrap();
    });
    let (id, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
    // The bytes written before a close may or may not have reached the target
    // (a close drops what is still on its way); the socket is over either way.
    bridge.stream_send(id, b"some bytes", 100);
    bridge.stream_close(id);
    let heard = ended_rx.recv_timeout(PATIENCE).unwrap();
    assert!(b"some bytes".starts_with(&heard), "{heard:?}");
}

#[test]
fn a_target_that_cannot_be_reached_is_failed_and_nothing_else_is_touched() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let port = target(echo);
    let (alive, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();

    let (_, code, text) = bridge
        .connect_stream("127.0.0.1", closed_port(), MAX_WINDOW)
        .unwrap_err();
    assert_eq!(
        (code, text.as_str()),
        (FailCode::REFUSED, "connection refused")
    );

    // (A name that does not resolve is not tried here: a real resolver's time
    // for a name that is not there is the network's to say. What a failed
    // resolution is called is tested in `dial` with a resolver of its own, and
    // how a refusal reaches the client in the engine's tests.)

    // What a conforming client cannot write: port 0 and no host, in an `open`
    // of a number above the others.
    let mut block = tlv::Writer::new();
    block.put_u16(1, 0);
    let mut rest = 1000u32.to_be_bytes().to_vec();
    rest.extend_from_slice(&block.finish());
    bridge.stream_raw(stream::kind::OPEN, 100, &rest);
    bridge.until("the answer to a port of 0", |b| {
        b.flows.get(&100).is_some_and(|f| f.failed.is_some())
    });
    assert_eq!(
        bridge.flows[&100].failed.as_ref().unwrap().0,
        FailCode::BAD_REQUEST
    );

    // The stream that was open before all this still carries bytes.
    bridge.stream_send(alive, b"still here", 100);
    bridge.until("the echo", |b| b.flows[&alive].data == b"still here");
    assert!(bridge.errors.is_empty());
}

#[test]
fn the_stream_after_the_last_that_fits_is_failed_with_the_limit() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let port = target(echo);
    // One at a time: a bridge also limits how many are being connected, and
    // the point here is how many are open.
    let mut open = Vec::new();
    for _ in 0..MAX_STREAMS {
        let (id, _) = bridge
            .connect_stream("127.0.0.1", port, MAX_WINDOW)
            .unwrap();
        open.push(id);
    }
    let (_, code, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap_err();
    assert_eq!(code, FailCode::LIMIT);

    // The streams that are open are not affected...
    let first = open[0];
    bridge.stream_send(first, b"hello", 100);
    bridge.until("the echo", |b| b.flows[&first].data == b"hello");
    // ...and a stream that is closed makes room.
    bridge.stream_close(open[1]);
    bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
}

#[test]
fn a_session_is_typed_into_and_answers_while_a_transfer_is_unfinished() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let total = 3 << 20;
    let flood = pattern(total, 5);
    let sending = flood.clone();
    let port = target(move |mut connection| {
        let _ = connection.write_all(&sending);
        let _ = connection.shutdown(Shutdown::Write);
    });
    let id = echo_session(&mut env, &mut bridge, "echo");

    // The client takes nothing of the transfer yet (no credit), so the bridge
    // sends at most the window the client gave, which is less than the
    // transfer. The session is typed into meanwhile.
    bridge.next_flow = (true, false);
    assert!(total > engine::WINDOW as usize);
    let (stream_id, window) = bridge
        .connect_stream("127.0.0.1", port, engine::WINDOW)
        .unwrap();
    assert_eq!(window, engine::WINDOW);
    bridge.until("the first of the transfer", |b| {
        b.flows[&stream_id].total > 0
    });
    bridge.type_in(&id, "typed during the transfer\n");
    bridge.wait_for(&id, "typed during the transfer");
    assert!(
        !bridge.flows[&stream_id].eof,
        "the transfer was over before the echo came"
    );

    // Taking the transfer lets it finish, all of it, in order.
    bridge.flow(stream_id).hold = false;
    bridge.stream_release(stream_id);
    bridge.until("the end of the transfer", |b| b.flows[&stream_id].eof);
    assert_eq!(bridge.flows[&stream_id].data, flood);
}

#[test]
fn a_session_that_never_stops_does_not_keep_a_transfer_from_moving() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let total = 2 << 20;
    let flood = pattern(total, 6);
    let sending = flood.clone();
    let port = target(move |mut connection| {
        let _ = connection.write_all(&sending);
        let _ = connection.shutdown(Shutdown::Write);
    });
    let id = env.name("flat-out");
    env.track(&id);
    bridge.try_new(Some(&id), &["/usr/bin/yes"]).unwrap();
    bridge.seen(&id).discard = true;
    bridge.attach(&id, 0);
    bridge.until("some output", |b| b.sessions[&id].next > 0);

    bridge.next_flow = (false, false);
    let (stream_id, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
    bridge.until("the whole transfer", |b| b.flows[&stream_id].eof);
    assert_eq!(bridge.flows[&stream_id].data, flood);
    // The session was still producing all along.
    let before = bridge.sessions[&id].next;
    bridge.until("more output", |b| b.sessions[&id].next > before);
}

#[test]
fn a_target_that_does_not_read_does_not_stop_the_bridge_reading_the_client() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let (total_tx, total_rx) = mpsc::channel();
    let total_tx = Mutex::new(total_tx);
    let port = target(move |mut connection| {
        // Takes nothing until it is told to.
        let _ = release_rx.lock().unwrap().recv();
        let mut heard = 0usize;
        let mut buf = [0u8; 16 * 1024];
        loop {
            match connection.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => heard += n,
            }
        }
        total_tx.lock().unwrap().send(heard).unwrap();
    });
    let id = echo_session(&mut env, &mut bridge, "echo");

    let (stream_id, window) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
    // As much as the bridge said it takes, all at once.
    let all = pattern(window as usize, 7);
    bridge.stream_send(stream_id, &all, 16 * 1024);
    // The session still answers, and the stream is still there.
    bridge.type_in(&id, "typed while a target is not reading\n");
    bridge.wait_for(&id, "typed while a target is not reading");
    assert!(!bridge.flows[&stream_id].closed);
    assert!(bridge.errors.is_empty());

    // The target wakes up and gets every byte.
    release_tx.send(()).unwrap();
    bridge.stream_eof(stream_id);
    assert_eq!(total_rx.recv_timeout(PATIENCE).unwrap(), all.len());
}

#[test]
fn a_stream_that_breaks_the_rules_loses_itself_and_nothing_else() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let port = target(echo);
    // A target that hears the end and keeps its own half open for good: a
    // stream to one that echoes would be over, and so no rule broken, once the
    // echo's end had come back.
    let held = target(|mut connection| {
        let mut buf = [0u8; 1024];
        while matches!(connection.read(&mut buf), Ok(n) if n > 0) {}
        loop {
            std::thread::park();
        }
    });
    let mut ids = Vec::new();
    for n in 0..5 {
        let (id, _) = bridge
            .connect_stream("127.0.0.1", if n == 2 { held } else { port }, MAX_WINDOW)
            .unwrap();
        ids.push(id);
    }
    // More in one `data` than a `data` holds.
    bridge.stream_raw(stream::kind::CLIENT_DATA, ids[0], &vec![0u8; 40_000]);
    // More credit than a window can be.
    bridge.stream_raw(stream::kind::CLIENT_CREDIT, ids[1], &u32::MAX.to_be_bytes());
    // Data after the end.
    bridge.stream_eof(ids[2]);
    bridge.stream_send(ids[2], b"too late", 100);
    bridge.until("the three closes", |b| {
        ids[..3].iter().all(|id| b.flows[id].closed)
    });
    assert!(!bridge.flows[&ids[3]].closed && !bridge.flows[&ids[4]].closed);
    // The others go on.
    for id in &ids[3..] {
        bridge.stream_send(*id, b"fine", 100);
    }
    bridge.until("the echoes", |b| {
        ids[3..].iter().all(|id| b.flows[id].data == b"fine")
    });
    assert!(bridge.errors.is_empty());
}

#[test]
fn frames_of_a_stream_that_is_over_are_ignored_and_so_is_an_old_number() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let port = target(echo);
    let (first, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
    bridge.stream_close(first);
    // What was still on its way when it was closed, and a stream that never
    // was.
    bridge.stream_send(first, b"late", 100);
    bridge.stream_message(first, stream::Message::Credit(10));
    bridge.stream_eof(first);
    bridge.stream_close(first);
    for ghost in [77, 78] {
        bridge.stream_send(ghost, b"nobody", 100);
        bridge.stream_message(ghost, stream::Message::Credit(10));
        bridge.stream_eof(ghost);
        bridge.stream_close(ghost);
    }
    // A number that is not above the last: not answered, whatever it asks.
    bridge.stream_message(
        first,
        stream::Message::Open {
            window: 10,
            port: closed_port(),
            host: "127.0.0.1".to_owned(),
        },
    );
    // Everything before this is behind us once the pong is in.
    bridge.send(None, Message::Ping(b"fence".to_vec()));
    bridge.until("the pong", |b| !b.pongs.is_empty());
    assert!(bridge.errors.is_empty());
    assert!(bridge.flows[&first].failed.is_none());
    for ghost in [77, 78] {
        let flow = &bridge.flows[&ghost];
        assert!(flow.opened.is_none() && flow.failed.is_none() && !flow.closed);
        assert!(flow.data.is_empty() && !flow.eof);
    }
    // A number above the last is fine, and nothing was disturbed.
    let (next, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
    bridge.stream_send(next, b"fresh", 100);
    bridge.until("the echo", |b| b.flows[&next].data == b"fresh");
}

#[test]
fn a_client_that_goes_away_closes_every_target_and_ends_the_bridge() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let (ended_tx, ended_rx) = mpsc::channel();
    let ended_tx = Mutex::new(ended_tx);
    let port = target(move |mut connection| {
        let mut heard = Vec::new();
        let _ = connection.read_to_end(&mut heard);
        ended_tx.lock().unwrap().send(heard).unwrap();
    });
    for _ in 0..3 {
        let (id, _) = bridge
            .connect_stream("127.0.0.1", port, MAX_WINDOW)
            .unwrap();
        bridge.stream_send(id, b"x", 10);
    }
    bridge.hang_up();
    for _ in 0..3 {
        assert_eq!(ended_rx.recv_timeout(PATIENCE).unwrap(), b"x");
    }
    assert!(bridge.wait_exit().success());
}

#[test]
fn a_client_asks_before_it_relies_on_streams() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    bridge.record();
    bridge.send(None, Message::Features);
    bridge.send(None, Message::Ping(b"after features".to_vec()));
    bridge.until("the pong", |b| !b.pongs.is_empty());
    let raw = bridge.raw.take().unwrap();

    // The answer comes before the pong, and says what the bridge does.
    let mut decoder = stream::IncomingDecoder::new();
    decoder.push(&raw);
    let mut messages = Vec::new();
    while let Some(incoming) = decoder.next_incoming().unwrap() {
        match incoming {
            stream::Incoming::Session(packet) => messages.push(packet.message),
            other => panic!("{other:?}"),
        }
    }
    assert!(matches!(
        messages.as_slice(),
        [Message::FeaturesReply(_), Message::Pong(_)]
    ));
    let Verdict::Streams(features) = engine::judge(&messages[0]) else {
        panic!("{:?}", messages[0]);
    };
    assert_eq!(features.max_streams, MAX_STREAMS);
    assert_eq!(features.max_window, MAX_WINDOW);
    assert_eq!(features.max_data as usize, stream::MAX_DATA);
}

#[test]
fn a_bridge_that_does_not_know_a_request_says_nothing_of_it_and_answers_the_ping() {
    // What a bridge from before streams does with `features` is what this one
    // does with a kind nobody has: it skips it. The client's reading of the
    // answer (nothing before the pong) is the same.
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    bridge.record();
    let unknown = kablo_proto::frame::RawFrame {
        kind: 0x7e,
        channel: kablo_proto::frame::CHANNEL_SESSION,
        session: Vec::new(),
        payload: Vec::new(),
    };
    let mut bytes = Vec::new();
    kablo_proto::frame::encode_frame(&mut bytes, &unknown).unwrap();
    bridge.send_bytes(&bytes);
    bridge.send(None, Message::Ping(b"after".to_vec()));
    bridge.until("the pong", |b| !b.pongs.is_empty());
    let raw = bridge.raw.take().unwrap();
    let mut decoder = PacketDecoder::new();
    decoder.push(&raw);
    let first = decoder.next_packet().unwrap().unwrap();
    assert!(matches!(first.message, Message::Pong(_)));
    assert_eq!(
        engine::judge(&first.message),
        Verdict::Without(None),
        "a pong with no answer before it means no streams"
    );
    assert!(bridge.errors.is_empty());
}

#[test]
fn a_reader_that_does_not_know_channel_one_reads_the_bridge_all_the_same() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    let port = target(echo);
    bridge.record();
    let (id, _) = bridge
        .connect_stream("127.0.0.1", port, MAX_WINDOW)
        .unwrap();
    let bytes = pattern(100_000, 8);
    bridge.stream_send(id, &bytes, 10_000);
    bridge.until("the echo", |b| b.flows[&id].data.len() == bytes.len());
    bridge.send(None, Message::Ping(b"end".to_vec()));
    bridge.until("the pong", |b| !b.pongs.is_empty());
    let raw = bridge.raw.take().unwrap();
    // The reader of sessions skips what is on channel 1 by its lengths and
    // reads what is on channel 0, the pong.
    let mut decoder = PacketDecoder::new();
    decoder.push(&raw);
    let mut seen: Vec<Packet> = Vec::new();
    while let Some(packet) = decoder.next_packet().unwrap() {
        seen.push(packet);
    }
    assert!(matches!(
        seen.as_slice(),
        [Packet {
            message: Message::Pong(_),
            ..
        }]
    ));
}
