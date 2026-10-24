//! A client asks a session to drop the input its program has not read: what was typed to
//! the terminal and waits there, and what the session still holds back for it. What is
//! typed after that arrives; nothing else of the session is touched.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::path::PathBuf;

use common::{Bridge, Env};
use kablo_proto::v1::{ErrorCode, Message};

/// More than a terminal takes of what is typed to it before its program reads (a kilobyte on
/// macOS, some kilobytes on Linux): the rest waits in the session, so a drop that reached
/// only the terminal would leave the session writing it afterwards.
const STALE: usize = 64 * 1024;

/// A program that reads nothing until a file appears next to it, and then takes five bytes
/// of what is waiting and says them. The terminal is raw and does not echo, so that what the
/// stream holds is the program's own words and not what was typed.
fn reader(env: &Env) -> (PathBuf, PathBuf) {
    let script = env.script(
        "reader",
        "#!/bin/sh\n\
         stty raw -echo\n\
         go=\"$(dirname \"$0\")/go\"\n\
         echo READY\n\
         while [ ! -e \"$go\" ]; do sleep 0.05; done\n\
         printf 'GOT:'\n\
         head -c 5\n\
         echo\n\
         echo END\n",
    );
    let go = script.parent().unwrap().join("go");
    (script, go)
}

fn stale() -> Vec<String> {
    (0..STALE / 1024)
        .map(|chunk| format!("{chunk:04x}").repeat(256))
        .collect()
}

#[test]
fn the_input_a_program_did_not_read_is_dropped_and_what_is_typed_after_arrives() {
    let mut env = Env::new();
    let (script, go) = reader(&env);
    let id = env.start("dropped", &["--", script.to_str().unwrap()]);
    let mut conn = env.connect(&id);
    conn.attach(0);
    conn.wait_for("READY", 0);

    for chunk in stale() {
        conn.type_in(&chunk);
    }
    conn.send(Message::DropInput);
    // The status is answered after the drop was dealt with: the connection is one line.
    conn.status();
    assert!(conn.errors.is_empty(), "{:?}", conn.errors);
    assert!(!conn.closed, "and the connection goes on");

    // Now the program reads: what is there is what was typed after the drop.
    std::fs::write(&go, "").unwrap();
    conn.type_in("fresh");
    conn.wait_for("GOT:fresh\n", 0);
    conn.wait_for("END", 0);
    assert!(
        !String::from_utf8_lossy(&conn.data).contains("GOT:0000"),
        "the program read what was dropped: {:?}",
        String::from_utf8_lossy(&conn.data)
    );
}

#[test]
fn without_the_drop_the_program_reads_what_was_typed_first() {
    // The witness of the test above: the same program, the same typing, no drop.
    let mut env = Env::new();
    let (script, go) = reader(&env);
    let id = env.start("kept", &["--", script.to_str().unwrap()]);
    let mut conn = env.connect(&id);
    conn.attach(0);
    conn.wait_for("READY", 0);

    for chunk in stale() {
        conn.type_in(&chunk);
    }
    conn.status();
    std::fs::write(&go, "").unwrap();
    conn.wait_for("GOT:00000\n", 0);
}

#[test]
fn a_drop_with_nothing_waiting_changes_nothing() {
    let mut env = Env::new();
    let id = env.start("empty", &["--", "/bin/sh"]);
    let mut conn = env.connect(&id);
    conn.attach(0);
    conn.type_in("echo before-$((6*7))\n");
    conn.wait_for("before-42\r\n", 0);

    conn.send(Message::DropInput);
    conn.send(Message::DropInput);
    // Nothing of the output, the record or the connection is touched.
    assert_eq!(conn.status().retained_from, 0);
    assert!(conn.errors.is_empty(), "{:?}", conn.errors);

    conn.type_in("echo after-$((6*8))\n");
    conn.wait_for("after-48\r\n", 0);
    assert!(!conn.closed);
}

#[test]
fn a_drop_from_a_connection_that_is_not_attached_is_refused_and_drops_nothing() {
    let mut env = Env::new();
    let (script, go) = reader(&env);
    let id = env.start("refused", &["--", script.to_str().unwrap()]);
    let mut witness = env.connect(&id);
    witness.attach(0);
    witness.wait_for("READY", 0);
    witness.type_in("kept!");
    witness.status();

    let mut idle = env.connect(&id);
    idle.send(Message::DropInput);
    idle.until("an error", |c| !c.errors.is_empty());
    assert_eq!(idle.errors[0].0, ErrorCode::NOT_ATTACHED);
    assert!(!idle.closed, "and the connection goes on");

    std::fs::write(&go, "").unwrap();
    witness.wait_for("GOT:kept!\n", 0);
}

#[test]
fn a_drop_goes_through_the_bridge_to_the_session_it_names() {
    let mut env = Env::new();
    let (script, go) = reader(&env);
    let id = env.start("bridged", &["--", script.to_str().unwrap()]);
    let mut bridge = Bridge::start(&env);

    // Nothing is attached through this connection: said at once.
    bridge.send(Some(&id), Message::DropInput);
    let (session, code, _) = bridge.next_error();
    assert_eq!(
        (session.as_deref(), code),
        (Some(id.as_str()), ErrorCode::NOT_ATTACHED)
    );

    bridge.attach(&id, 0);
    bridge.wait_for(&id, "READY");
    for chunk in stale() {
        bridge.type_in(&id, &chunk);
    }
    bridge.send(Some(&id), Message::DropInput);
    bridge.status(&id);

    std::fs::write(&go, "").unwrap();
    bridge.type_in(&id, "fresh");
    bridge.wait_for(&id, "GOT:fresh\n");
}
