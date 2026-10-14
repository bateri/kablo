//! `kablo forward`, end to end on this machine: the command that listens, with
//! a real `kablo attach` as the carrier, and the engine of it in-process
//! against bridges that are not this build's.
//!
//! Like the stream tests these are about order and about what each side is
//! told, not about time: nothing asserts that something took long or short,
//! only that it happened, and the patience is long. The targets are listeners
//! of the test's own on `127.0.0.1:0`, and so are the ports `kablo forward`
//! takes (`0:` lets the system choose and the announcement says which).

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;

use common::{
    Env, Forward, PATIENCE, client, closed_port, echo, is_over, pattern, read_exactly, target,
};
use kablo::forward::{self, End, Wire};
use kablo_proto::WIRE;
use kablo_proto::v1::stream::IncomingDecoder;
use kablo_proto::v1::{ErrorCode, Features, Message, Packet, PacketDecoder};

const KABLO: &str = env!("CARGO_BIN_EXE_kablo");

/// The arguments of a forwarding over this build's own bridge: loopback only,
/// the system's choice of port, then the carrier.
fn over_attach(port: u16) -> Vec<String> {
    vec![
        "--bind".to_owned(),
        "127.0.0.1".to_owned(),
        format!("0:127.0.0.1:{port}"),
        "--".to_owned(),
        KABLO.to_owned(),
        "attach".to_owned(),
    ]
}

fn start(env: &Env, args: &[String]) -> Forward {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    Forward::start(env, &args)
}

#[test]
fn a_connection_to_a_local_port_reaches_the_target_and_comes_back() {
    let env = Env::new();
    let port = target(echo);
    let mut forward = start(&env, &over_attach(port));
    let address = forward.announced(1)[0];
    assert!(address.ip().is_loopback());
    assert_ne!(address.port(), 0, "the announcement says the port chosen");

    let mut socket = client(address);
    socket.write_all(b"hello through the bridge").unwrap();
    assert_eq!(
        read_exactly(&mut socket, "hello through the bridge".len()),
        b"hello through the bridge"
    );
}

#[test]
fn connections_at_once_are_told_apart_and_each_ends_on_its_own() {
    let env = Env::new();
    let port = target(echo);
    let mut forward = start(&env, &over_attach(port));
    let address = forward.announced(1)[0];

    let mut first = client(address);
    let mut second = client(address);
    first.write_all(b"first, ").unwrap();
    second.write_all(b"second, ").unwrap();
    first.write_all(b"and more of the first").unwrap();
    // The first says it has nothing more; the second goes on.
    first.shutdown(Shutdown::Write).unwrap();
    let mut heard = Vec::new();
    first.read_to_end(&mut heard).unwrap();
    assert_eq!(heard, b"first, and more of the first");

    second.write_all(b"and still here").unwrap();
    assert_eq!(
        read_exactly(&mut second, "second, and still here".len()),
        b"second, and still here"
    );
    second.shutdown(Shutdown::Write).unwrap();
    assert!(is_over(&mut second));
}

#[test]
fn a_transfer_larger_than_the_windows_goes_both_ways_in_order() {
    let env = Env::new();
    let port = target(echo);
    let mut forward = start(&env, &over_attach(port));
    let address = forward.announced(1)[0];

    let sent = pattern(3 << 20, 9);
    let mut socket = client(address);
    let mut writer = socket.try_clone().unwrap();
    let bytes = sent.clone();
    let writing = std::thread::spawn(move || {
        writer.write_all(&bytes).unwrap();
        writer.shutdown(Shutdown::Write).unwrap();
    });
    let mut heard = Vec::new();
    socket.read_to_end(&mut heard).unwrap();
    writing.join().unwrap();
    assert_eq!(heard.len(), sent.len());
    assert!(heard == sent, "the bytes came back changed");
}

#[test]
fn a_target_that_ends_ends_the_connection_after_what_it_sent() {
    let env = Env::new();
    let port = target(|mut connection| {
        let _ = connection.write_all(b"goodbye");
    });
    let mut forward = start(&env, &over_attach(port));
    let address = forward.announced(1)[0];
    let mut socket = client(address);
    let mut heard = Vec::new();
    socket.read_to_end(&mut heard).unwrap();
    assert_eq!(heard, b"goodbye");
}

#[test]
fn a_target_that_refuses_closes_the_connection_and_is_named() {
    let env = Env::new();
    let port = closed_port();
    let mut forward = start(&env, &over_attach(port));
    let address = forward.announced(1)[0];

    let mut socket = client(address);
    assert!(is_over(&mut socket));
    let line = forward.said("connection refused");
    assert!(line.contains(&format!("127.0.0.1:{port}")), "{line}");

    // Nothing else is touched: the next connection is tried like any other.
    let mut again = client(address);
    assert!(is_over(&mut again));
}

#[test]
fn the_default_is_the_loopback_of_both_families_on_one_port() {
    let env = Env::new();
    let port = target(echo);
    let mut forward = start(
        &env,
        &[
            format!("0:127.0.0.1:{port}"),
            "--".to_owned(),
            KABLO.to_owned(),
            "attach".to_owned(),
        ],
    );
    // Machines without IPv6 announce one line, the others two; either way
    // the first is IPv4 and every address is the loopback.
    let first = forward.announced(1)[0];
    assert!(first.ip().is_ipv4() && first.ip().is_loopback());
    let mut socket = client(first);
    socket.write_all(b"via localhost").unwrap();
    assert_eq!(read_exactly(&mut socket, 13), b"via localhost");
}

#[test]
fn a_port_in_use_is_said_at_once_and_the_carrier_is_never_started() {
    let env = Env::new();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let marker = env.dir.join("carrier-ran");
    let mut forward = Forward::start(
        &env,
        &[
            "--bind",
            "127.0.0.1",
            &format!("{port}:127.0.0.1:1"),
            "--",
            "sh",
            "-c",
            &format!("touch {}", marker.display()),
        ],
    );
    let status = forward.wait_exit();
    assert_eq!(status.code(), Some(1));
    let line = forward.said("already in use");
    assert!(line.contains(&port.to_string()), "{line}");
    forward.announced_nothing();
    assert!(
        !marker.exists(),
        "the carrier started although nothing listens"
    );
}

#[test]
fn a_carrier_that_ends_before_it_answers_is_named_with_its_status() {
    let env = Env::new();
    let mut forward = Forward::start(
        &env,
        &[
            "--bind",
            "127.0.0.1",
            "0:127.0.0.1:1",
            "--",
            "sh",
            "-c",
            "exit 7",
        ],
    );
    assert_eq!(forward.wait_exit().code(), Some(1));
    forward.said("sh ended (exit status 7)");
    forward.announced_nothing();
}

#[test]
fn a_carrier_that_cannot_be_run_is_said() {
    let env = Env::new();
    let mut forward = Forward::start(
        &env,
        &[
            "--bind",
            "127.0.0.1",
            "0:127.0.0.1:1",
            "--",
            "/nonexistent/kablo-test-carrier",
        ],
    );
    assert_eq!(forward.wait_exit().code(), Some(1));
    // Named either way: a system that reports the missing program at once
    // ("cannot run"), or one under emulation, where a child that could not
    // exec is only seen to end ("ended (exit status 127)").
    let line = forward.said("kablo-test-carrier");
    assert!(
        line.contains("cannot run") || line.contains("ended (exit status 127)"),
        "{line}"
    );
}

#[test]
fn a_carrier_that_prints_something_else_and_ends_is_named_not_waited_for() {
    let env = Env::new();
    let mut forward = Forward::start(
        &env,
        &[
            "--bind",
            "127.0.0.1",
            "0:127.0.0.1:1",
            "--",
            "sh",
            "-c",
            "echo this is not a bridge; exit 0",
        ],
    );
    assert_eq!(forward.wait_exit().code(), Some(1));
    forward.said("sh ended (exit status 0)");
}

#[test]
fn the_forwarding_ends_with_its_carrier_and_the_connections_with_it() {
    let env = Env::new();
    let port = target(echo);
    let pid_file = env.dir.join("carrier.pid");
    let mut forward = Forward::start(
        &env,
        &[
            "--bind",
            "127.0.0.1",
            &format!("0:127.0.0.1:{port}"),
            "--",
            "sh",
            "-c",
            // The shell's own pid is the bridge's: `exec` keeps it.
            &format!("echo $$ > {}; exec {KABLO} attach", pid_file.display()),
        ],
    );
    let address = forward.announced(1)[0];
    let mut socket = client(address);
    socket.write_all(b"alive").unwrap();
    assert_eq!(read_exactly(&mut socket, 5), b"alive");

    let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: the pid of the bridge this test started; it is a child of the
    // `kablo forward` the test started.
    unsafe { libc::kill(pid, libc::SIGKILL) };

    assert_eq!(forward.wait_exit().code(), Some(1));
    forward.said("killed by signal 9");
    assert!(is_over(&mut socket), "the connection outlived its carrier");
}

#[test]
fn a_signal_ends_the_forwarding_and_the_carrier_with_it() {
    let env = Env::new();
    let port = target(echo);
    let pid_file = env.dir.join("carrier.pid");
    let mut forward = Forward::start(
        &env,
        &[
            "--bind",
            "127.0.0.1",
            &format!("0:127.0.0.1:{port}"),
            "--",
            "sh",
            "-c",
            &format!("echo $$ > {}; exec {KABLO} attach", pid_file.display()),
        ],
    );
    forward.announced(1);
    let pid: libc::pid_t = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: the pid of the `kablo forward` this test started.
    unsafe { libc::kill(forward.pid() as libc::pid_t, libc::SIGTERM) };
    assert_eq!(forward.wait_exit().code(), Some(128 + libc::SIGTERM));

    // The bridge is gone with it: a signal 0 finds no such process once the
    // forwarding has collected it.
    // SAFETY: signal 0 only asks whether the process exists.
    let gone = unsafe { libc::kill(pid, 0) } != 0;
    assert!(gone, "the carrier outlived the forwarding");
}

// --- bridges that are not this build's ------------------------------------------

/// How a bridge that is not this build's answers `features`.
#[derive(Clone)]
enum Kind {
    /// One from before streams: reads with the session reader alone, which
    /// skips what it does not know, and answers a `ping`.
    Old,
    /// One that has streams and whose administrator turned them off.
    Off(String),
    /// One that answers a request it does not know with an error.
    Rejects,
}

/// Plays a bridge of `kind` on `sock`: the handshake, then every `ping` is
/// answered, and `features` as the kind does. Returns when the client's end
/// closes.
fn bridge(kind: Kind, sock: UnixStream) {
    let mut reader = sock.try_clone().unwrap();
    let mut writer = sock;
    WIRE.server(&mut reader, &mut writer).unwrap();
    let mut old = PacketDecoder::new();
    let mut new = IncomingDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let mut asked = Vec::new();
        match kind {
            Kind::Old => {
                old.push(&buf[..n]);
                while let Some(packet) = old.next_packet().unwrap() {
                    asked.push(packet);
                }
            }
            _ => {
                new.push(&buf[..n]);
                while let Some(incoming) = new.next_incoming().unwrap() {
                    if let kablo_proto::v1::stream::Incoming::Session(packet) = incoming {
                        asked.push(packet);
                    }
                }
            }
        }
        for packet in asked {
            let reply = match (&kind, packet.message) {
                (_, Message::Ping(bytes)) => Some(Message::Pong(bytes)),
                (Kind::Off(reason), Message::Features) => Some(Message::FeaturesReply(
                    Features::streams_off(reason.clone()),
                )),
                (Kind::Rejects, Message::Features) => Some(Message::Error {
                    code: ErrorCode::BAD_REQUEST,
                    text: "not a request".to_owned(),
                }),
                _ => None,
            };
            if let Some(message) = reply {
                let mut bytes = Vec::new();
                Packet::connection(message).encode(&mut bytes).unwrap();
                if writer.write_all(&bytes).is_err() {
                    return;
                }
            }
        }
    }
}

/// Runs `forward::serve` against a bridge of `kind` and returns how it ended
/// and what it printed.
fn against(kind: Kind) -> (End, String, String) {
    let (ours, theirs) = UnixStream::pair().unwrap();
    let playing = std::thread::spawn(move || bridge(kind, theirs));
    let (done, ended) = mpsc::channel();
    std::thread::spawn(move || {
        let input = OwnedFd::from(ours.try_clone().unwrap());
        let output = OwnedFd::from(ours);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let end = forward::serve(Wire { input, output }, Vec::new(), None, &mut out, &mut err);
        let _ = done.send((
            end,
            String::from_utf8_lossy(&out).into_owned(),
            String::from_utf8_lossy(&err).into_owned(),
        ));
    });
    let result = ended
        .recv_timeout(PATIENCE)
        .expect("the forwarding waited for a bridge that has nothing to say");
    // The client end is gone with the end of `serve`, which is what lets the
    // bridge stop.
    playing.join().unwrap();
    result
}

#[test]
fn a_bridge_that_predates_streams_is_said_not_waited_for() {
    let (end, out, _) = against(Kind::Old);
    let End::Declined(why) = end else {
        panic!("{end:?}");
    };
    assert!(why.contains("too old"), "{why}");
    assert!(out.is_empty(), "nothing is announced: {out:?}");
}

#[test]
fn a_bridge_that_answers_features_with_an_error_is_taken_for_an_old_one() {
    let (end, out, _) = against(Kind::Rejects);
    assert!(
        matches!(&end, End::Declined(why) if why.contains("too old")),
        "{end:?}"
    );
    assert!(out.is_empty());
}

#[test]
fn an_administrator_who_turned_streams_off_is_quoted() {
    let (end, out, _) = against(Kind::Off(
        "turned off by the server's administrator".to_owned(),
    ));
    let End::Declined(why) = end else {
        panic!("{end:?}");
    };
    assert!(
        why.contains("turned off by the server's administrator"),
        "{why}"
    );
    assert!(out.is_empty());
}

#[test]
fn what_a_server_says_to_a_person_has_no_control_characters() {
    let (end, _, _) = against(Kind::Off("paused\u{1b}]0;title\u{7} for now".to_owned()));
    let End::Declined(why) = end else {
        panic!("{end:?}");
    };
    assert!(why.contains("paused]0;title for now"), "{why}");
    assert!(!why.chars().any(char::is_control), "{why:?}");
}

#[test]
fn a_carrier_that_ends_in_the_middle_of_the_handshake_ends_the_forwarding() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    drop(theirs);
    let input = OwnedFd::from(ours.try_clone().unwrap());
    let output = OwnedFd::from(ours);
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let end = forward::serve(Wire { input, output }, Vec::new(), None, &mut out, &mut err);
    assert_eq!(end, End::Carrier);
}
