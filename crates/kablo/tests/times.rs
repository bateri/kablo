//! What a session says of when things happened: when it started, when the
//! program in front did, whether that program is the session's own, and when
//! the program last wrote. Real sessions, over the real socket.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::time::{Duration, Instant};

use common::{Env, PATIENCE};
use kablo::sys::unix_now;
use kablo_proto::v1::{Message, State, Status};

/// The process tables count whole seconds, and Linux derives them from a boot
/// time and clock ticks: a few seconds either way is the same moment.
const SLACK: u64 = 3;

fn near(at: u64, before: u64, after: u64) {
    assert!(
        at + SLACK >= before && at <= after + SLACK,
        "{at} is not between {before} and {after}"
    );
}

/// Has the shell the client is attached to print a number it was not typed as
/// (the echo of what is typed would be taken for it), and waits until the
/// client has it: output that the relay itself has read. What the program
/// wrote before the relay began to read the terminal was read by the warden,
/// and the relay does not know when.
fn answered(conn: &mut common::Conn, base: u64) {
    let from = conn.data.len();
    conn.type_in(&format!("echo $(({base}+2))\n"));
    conn.wait_for(&format!("{}", base + 2), from);
    conn.pump(Duration::from_millis(300));
}

#[test]
fn a_program_run_by_new_is_in_front_of_itself_and_started_with_the_session() {
    let mut env = Env::new();
    let before = unix_now().unwrap();
    let id = env.start("own", &["--", "/bin/sleep", "600"]);
    let after = unix_now().unwrap();
    let mut conn = env.connect(&id);
    let status = conn.status();

    let started = status
        .started
        .expect("a running session says when it started");
    near(started, before, after);
    // The program is the session, so the one in front is its own, and started with it.
    assert_eq!(status.front_is_own, Some(true));
    assert_eq!(status.front_started, Some(started));
    // It has not written a byte.
    assert_eq!(status.output_at, None);
    assert_eq!(status.program.as_deref(), Some(&b"sleep"[..]));
}

#[test]
fn a_shell_with_a_program_in_front_is_told_from_one_at_its_prompt() {
    let mut env = Env::new();
    let id = env.start("shell", &["--", "/bin/sh"]);
    let mut conn = env.connect(&id);
    conn.attach(0);
    answered(&mut conn, 40);

    let at_the_prompt = conn.status();
    let started = at_the_prompt.started.expect("the session's start");
    assert_eq!(at_the_prompt.front_is_own, Some(true));
    assert_eq!(at_the_prompt.front_started, Some(started));
    let wrote = at_the_prompt.output_at.expect("the answer was output");
    near(wrote, started, unix_now().unwrap());

    // A program started from the prompt is in front, and not the session's.
    conn.type_in("sleep 30\n");
    conn.until("sleep", |c| {
        c.states
            .last()
            .and_then(|state| state.program.as_deref())
            .is_some_and(|name| name == b"sleep")
    });
    let running = conn.status();
    assert_eq!(running.front_is_own, Some(false));
    assert_eq!(running.started, Some(started));
    // Two reads of the boot time can differ by a second on Linux.
    let front = running.front_started.expect("the program in front started");
    near(front, started, unix_now().unwrap());
    assert!(front + SLACK >= started, "{front} is before {started}");

    // Interrupted, the shell is in front again and it is the session's own.
    conn.type_in("\x03");
    conn.until("the shell again", |c| {
        c.states
            .last()
            .and_then(|state| state.program.as_deref())
            .is_some_and(|name| name != b"sleep")
    });
    let back = conn.status();
    assert_eq!(back.front_is_own, Some(true));
    assert_eq!(back.front_started, Some(started));
}

#[test]
fn the_time_of_the_last_output_moves_when_the_program_writes() {
    let mut env = Env::new();
    let id = env.start("quiet", &["--", "/bin/sh"]);
    let mut conn = env.connect(&id);
    conn.attach(0);
    answered(&mut conn, 40);
    let first = conn.status().output_at.expect("the answer was output");

    // Whole seconds: let the next write fall in a later one.
    std::thread::sleep(Duration::from_millis(1100));
    answered(&mut conn, 50);
    let later = conn.status().output_at.expect("there was more output");
    assert!(later > first, "{later} is not after {first}");
    near(later, first, unix_now().unwrap());
}

#[test]
fn a_session_that_has_ended_still_says_when_it_started_and_nothing_else() {
    let mut env = Env::new();
    let id = env.start("ended", &["--", "/bin/sleep", "600"]);
    let mut conn = env.connect(&id);
    let running = conn.status();
    let started = running
        .started
        .expect("a running session says when it started");

    // Nobody is attached, so nobody is told and the session stays to be read.
    conn.send(Message::Terminate { force: false });
    let start = Instant::now();
    let ended = loop {
        let status: Status = conn.status();
        if status.state == State::ENDED {
            break status;
        }
        assert!(start.elapsed() < PATIENCE, "the session did not end");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(ended.started, Some(started));
    assert_eq!(
        (ended.front_started, ended.front_is_own, ended.output_at),
        (None, None, None)
    );
    assert_eq!(ended.program, None);
}

#[test]
fn list_tells_the_times_in_its_table_and_in_its_json() {
    let mut env = Env::new();
    let id = env.start("listed", &["--", "/bin/sleep", "600"]);

    let table = env.kablo().arg("list").output().unwrap();
    assert!(table.status.success());
    let table = String::from_utf8_lossy(&table.stdout).into_owned();
    let mut lines = table.lines();
    let header = lines.next().expect("a header");
    assert!(header.contains("CLIENTS  AGE"), "{table}");
    assert!(header.trim_end().ends_with("QUIET  LABEL"), "{table}");
    let row = lines
        .find(|line| line.starts_with(&id))
        .expect("the session");
    // A moment old, and it has written nothing (no label: the row ends at QUIET).
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(cells.last(), Some(&"-"), "{table}");
    assert!(
        cells
            .get(cells.len().saturating_sub(2))
            .is_some_and(|age| age.ends_with('s')),
        "{table}"
    );

    let json = env.kablo().args(["list", "--json"]).output().unwrap();
    assert!(json.status.success());
    let json = String::from_utf8_lossy(&json.stdout).into_owned();
    assert!(json.contains("\"started\":1"), "{json}");
    assert!(json.contains("\"front_started\":1"), "{json}");
    assert!(
        json.contains("\"front_is_own\":true,\"output_at\":null,"),
        "{json}"
    );
}
