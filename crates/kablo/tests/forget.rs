//! A client asks a session to forget its past: the scrollback its screen keeps and the record
//! of the output so far. A client that comes for the past is told it is gone and is given the
//! screen; the ones that are attached are not touched.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{Bridge, Env};
use kablo_proto::frame::{RawFrame, encode_frame};
use kablo_proto::v1::{ErrorCode, Message, OFFSET_END};

const SH: [&str; 2] = ["--", "/bin/sh"];

/// The kind after the last this version has: to it what `forget` is to a relay from before
/// `forget` existed. (A server that does not know a kind skips the frame and reads on.)
const UNKNOWN_KIND: u8 = 0x13;

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Types a loop that prints a hundred numbered rows, and waits for the line after them.
const ROWS: &str = "i=0; while [ $i -lt 100 ]; do printf 'ROW-%03d\\n' $i; i=$((i+1)); done; echo done-$((40+2))\n";

#[test]
fn a_client_that_comes_after_a_forget_is_given_the_screen_and_no_scrollback() {
    let mut env = Env::new();
    let id = env.start("forget", &SH);
    let mut witness = env.connect(&id);
    witness.attach(0);
    witness.type_in(ROWS);
    witness.wait_for("done-42\r\n", 0);

    // Before: the picture of the screen has the rows that scrolled off it.
    let mut before = env.connect(&id);
    before.attach_with_picture(OFFSET_END, 80, 24);
    let picture = lossy(&before.picture().bytes);
    assert!(
        picture.contains("ROW-002"),
        "the scrollback is in it: {picture:?}"
    );
    assert_eq!(before.status().retained_from, 0);

    // The client that is attached asks. The status after it is the relay's turn after the
    // forget: the past ends where it was asked.
    witness.send(Message::Forget);
    let forgotten = witness.status();
    assert!(forgotten.retained_from > 0);
    assert!(forgotten.retained_from <= forgotten.offset);
    let at = forgotten.retained_from;

    // The ones that are attached are not touched: they are given what they are owed.
    witness.type_in("echo still-$((3+4))\n");
    witness.wait_for("still-7\r\n", 0);
    before.type_in("echo also-$((3+5))\n");
    before.wait_for("also-8\r\n", 0);
    assert!(!before.closed && !witness.closed);

    // One that comes for what was forgotten is told so and given the screen, which has the
    // rows that were on it and none of the rows that had scrolled off.
    let mut late = env.connect(&id);
    let (start, truncated) = late.attach_with_picture(0, 80, 24);
    assert!(truncated, "what it asked for is gone");
    assert!(start >= at);
    let picture = lossy(&late.picture().bytes);
    assert!(
        picture.contains("ROW-099"),
        "the screen is there: {picture:?}"
    );
    assert!(
        !picture.contains("ROW-002") && !picture.contains("ROW-050"),
        "the scrollback is not: {picture:?}"
    );

    // One that has everything from the moment of the forget on is not cut short.
    let mut kept = env.connect(&id);
    assert_eq!(kept.attach(at), (at, false));
    kept.wait_for("still-7\r\n", 0);
}

#[test]
fn a_forget_from_a_connection_that_is_not_attached_is_refused_and_forgets_nothing() {
    let mut env = Env::new();
    let id = env.start("refused", &SH);
    let mut witness = env.connect(&id);
    witness.attach(0);
    witness.type_in("echo marker-$((6*7))\n");
    witness.wait_for("marker-42\r\n", 0);

    let mut idle = env.connect(&id);
    idle.send(Message::Forget);
    idle.until("an error", |c| !c.errors.is_empty());
    assert_eq!(idle.errors[0].0, ErrorCode::NOT_ATTACHED);
    assert!(!idle.closed, "and the connection goes on");
    assert_eq!(idle.status().retained_from, 0);
}

#[test]
fn a_forget_goes_through_the_bridge_to_the_session_it_names() {
    let mut env = Env::new();
    let id = env.start("bridged", &SH);
    let mut bridge = Bridge::start(&env);

    // Nothing is attached through this connection: said at once.
    bridge.send(Some(&id), Message::Forget);
    let (session, code, _) = bridge.next_error();
    assert_eq!(
        (session.as_deref(), code),
        (Some(id.as_str()), ErrorCode::NOT_ATTACHED)
    );

    bridge.attach(&id, 0);
    bridge.type_in(&id, "echo marker-$((6*7))\n");
    bridge.wait_for(&id, "marker-42\r\n");
    assert_eq!(bridge.status(&id).retained_from, 0);
    bridge.send(Some(&id), Message::Forget);
    let status = bridge.status(&id);
    assert!(status.retained_from > 0 && status.retained_from <= status.offset);
}

#[test]
fn a_kind_a_server_does_not_know_is_skipped_and_the_connection_goes_on() {
    let mut env = Env::new();
    let id = env.start("skipped", &SH);

    let mut conn = env.connect(&id);
    conn.send_raw(UNKNOWN_KIND, &[1, 2, 3]);
    conn.send_raw(UNKNOWN_KIND, &[]);
    assert_eq!(conn.status().retained_from, 0);
    assert!(conn.errors.is_empty(), "{:?}", conn.errors);
    assert_eq!(conn.attach(0), (0, false));

    let mut bridge = Bridge::start(&env);
    let mut bytes = Vec::new();
    encode_frame(
        &mut bytes,
        &RawFrame {
            kind: UNKNOWN_KIND,
            channel: kablo_proto::CHANNEL_SESSION,
            session: id.as_bytes().to_vec(),
            payload: vec![1, 2, 3],
        },
    )
    .unwrap();
    bridge.send_bytes(&bytes);
    assert_eq!(bridge.attach(&id, 0), (0, false));
    assert_eq!(bridge.status(&id).retained_from, 0);
}
