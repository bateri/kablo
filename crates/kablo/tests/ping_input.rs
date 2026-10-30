//! A `ping` that names a session is answered after the program's terminal has taken the input
//! queued before it: it is how a client that types more than a terminal holds learns how much
//! of it arrived, and so how much more it may send.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::path::PathBuf;
use std::time::Duration;

use common::{Bridge, Env};
use kablo_proto::v1::Message;

/// Several times what a terminal holds before its program reads (a kilobyte on macOS, some
/// kilobytes on Linux): the rest waits in the session.
const TYPED: usize = 256 * 1024;

/// A program that reads nothing until a file appears next to it, then reads all of what was
/// typed. The terminal is raw and does not echo.
fn reader(env: &Env) -> (PathBuf, PathBuf) {
    let script = env.script(
        "reader",
        &format!(
            "#!/bin/sh\n\
             stty raw -echo\n\
             go=\"$(dirname \"$0\")/go\"\n\
             echo READY\n\
             while [ ! -e \"$go\" ]; do sleep 0.05; done\n\
             head -c {TYPED} > /dev/null\n\
             echo END\n"
        ),
    );
    let go = script.parent().unwrap().join("go");
    (script, go)
}

fn pongs(conn: &common::Conn) -> usize {
    conn.replies
        .iter()
        .filter(|p| matches!(p.message, Message::Pong(_)))
        .count()
}

#[test]
fn a_ping_waits_for_the_terminal_to_take_the_input_before_it() {
    let mut env = Env::new();
    let (script, go) = reader(&env);
    let id = env.start("barrier", &["--", script.to_str().unwrap()]);
    let mut conn = env.connect(&id);
    conn.attach(0);
    conn.wait_for("READY", 0);

    // The first ping has nothing before it: answered at once.
    conn.send(Message::Ping(b"empty".to_vec()));
    conn.until("a pong with nothing typed", |c| pongs(c) == 1);

    for _ in 0..TYPED / 1024 {
        conn.type_in(&"x".repeat(1024));
    }
    conn.send(Message::Ping(b"typed".to_vec()));
    conn.pump(Duration::from_millis(600));
    assert_eq!(pongs(&conn), 1, "the program has read nothing yet");
    assert!(conn.errors.is_empty(), "{:?}", conn.errors);

    // The program reads: the terminal takes the rest and the ping is answered.
    std::fs::write(&go, "").unwrap();
    conn.until("the pong after the input", |c| pongs(c) == 2);
    let last = conn.replies.iter().rev().find_map(|p| match &p.message {
        Message::Pong(bytes) => Some(bytes.clone()),
        _ => None,
    });
    assert_eq!(last.as_deref(), Some(&b"typed"[..]));
    conn.wait_for("END", 0);
}

#[test]
fn pings_are_answered_in_order_as_the_terminal_takes_what_is_before_each() {
    let mut env = Env::new();
    let (script, go) = reader(&env);
    let id = env.start("ordered", &["--", script.to_str().unwrap()]);
    let mut conn = env.connect(&id);
    conn.attach(0);
    conn.wait_for("READY", 0);

    for round in 0..4u8 {
        for _ in 0..TYPED / 4 / 1024 {
            conn.type_in(&"y".repeat(1024));
        }
        conn.send(Message::Ping(vec![round]));
    }
    std::fs::write(&go, "").unwrap();
    conn.until("four pongs", |c| pongs(c) == 4);
    let order: Vec<Vec<u8>> = conn
        .replies
        .iter()
        .filter_map(|p| match &p.message {
            Message::Pong(bytes) => Some(bytes.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(order, vec![vec![0], vec![1], vec![2], vec![3]]);
}

#[test]
fn a_ping_after_input_reaches_the_session_through_the_bridge() {
    let mut env = Env::new();
    let (script, go) = reader(&env);
    let id = env.start("bridged", &["--", script.to_str().unwrap()]);
    let mut bridge = Bridge::start(&env);
    bridge.attach(&id, 0);
    bridge.wait_for(&id, "READY");

    // A ping with nothing typed is the bridge's own pulse.
    bridge.send(Some(&id), Message::Ping(b"pulse".to_vec()));
    bridge.until("the pulse", |b| b.pongs.len() == 1);

    let chunk = "z".repeat(TYPED / 4);
    for _ in 0..4 {
        bridge.type_in(&id, &chunk);
    }
    bridge.send(Some(&id), Message::Ping(b"typed".to_vec()));
    bridge.pump(Duration::from_millis(600));
    assert_eq!(bridge.pongs.len(), 1, "the program has read nothing yet");
    assert!(bridge.errors.is_empty(), "{:?}", bridge.errors);

    std::fs::write(&go, "").unwrap();
    bridge.until("the pong after the input", |b| b.pongs.len() == 2);
    assert_eq!(bridge.pongs[1], b"typed");
    bridge.wait_for(&id, "END");
}

#[test]
fn a_ping_with_no_session_is_a_pulse_even_with_input_waiting() {
    let mut env = Env::new();
    let (script, _go) = reader(&env);
    let id = env.start("pulse", &["--", script.to_str().unwrap()]);
    let mut bridge = Bridge::start(&env);
    bridge.attach(&id, 0);
    bridge.wait_for(&id, "READY");
    let chunk = "z".repeat(TYPED / 4);
    for _ in 0..4 {
        bridge.type_in(&id, &chunk);
    }
    bridge.send(None, Message::Ping(b"pulse".to_vec()));
    bridge.until("the pulse", |b| !b.pongs.is_empty());
    assert_eq!(bridge.pongs[0], b"pulse");
}
