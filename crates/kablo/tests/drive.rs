//! Driving a session without attaching to it: typing, reading the screen,
//! waiting for the program's end and the label, over the relay's own socket and
//! over the bridge. Real sessions.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::{Bridge, Conn, Env, PATIENCE, rebuilt, same, whole};
use kablo::link::{self, Link};
use kablo_proto::v1::key::{self, mods};
use kablo_proto::v1::{
    Capture, ErrorCode, Exit, Message, NewSession, Packet, PacketDecoder, State, Stroke,
};
use kablo_proto::{SessionId, WIRE};

/// A program that asks where the cursor is and says what it was told, as hex:
/// `got:<first>:<second>`. (The one in `screen.rs`.)
const ASK_FOR_THE_CURSOR: &str = "stty raw -echo min 0 time 100; printf '\\033[6n'; \
     got=$(dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
     stty min 0 time 5; \
     more=$(dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
     stty sane; echo \"got:$got:$more\"";

/// A program that gives back, byte for byte, what is typed to it, once it says
/// `ready`: a terminal with nothing between the typing and the output.
const ECHO: [&str; 4] = [
    "--",
    "/bin/sh",
    "-c",
    "stty raw -echo; echo ready; exec cat",
];

/// A program that gives back what is typed to it a line at a time.
const LINES: [&str; 4] = ["--", "/bin/sh", "-c", "stty -echo; echo ready; exec cat"];

fn text(s: &str) -> Stroke {
    Stroke::text(s)
}

fn enter() -> Stroke {
    Stroke::key(key::ENTER, 0)
}

/// A connection attached from the start of `id`, which has said `ready`.
fn witness(env: &Env, id: &str) -> Conn {
    let mut witness = env.connect(id);
    witness.attach(0);
    witness.wait_for("ready", 0);
    witness
}

fn assert_err(got: Result<(), (ErrorCode, String)>, code: ErrorCode) {
    match got {
        Err((got, _)) => assert_eq!(got, code),
        Ok(()) => panic!("expected an error, {code:?}"),
    }
}

// --- the terminal's own answers ---------------------------------------------

#[test]
fn a_connection_that_waits_does_not_stand_in_for_a_terminal() {
    let mut env = Env::new();
    // It asks a moment after the waiter is there, with nobody attached.
    let program = format!("sleep 0.4; {ASK_FOR_THE_CURSOR}");
    let id = env.start("wait-ask", &["--", "/bin/sh", "-c", &program]);
    let mut waiter = env.connect(&id);
    waiter.send(Message::Wait);
    waiter.until("the end", |c| c.ended.is_some());
    assert_eq!(waiter.ended, Some(Exit::Code(0)));

    // The relay answered the question all the same, once; a client that comes
    // after sees what the program said it was told.
    let mut late = env.connect(&id);
    late.attach(0);
    late.wait_for("got:1b5b313b3152:\r\n", 0);
}

#[test]
fn a_connection_that_types_leaves_the_answer_to_the_client_that_is_attached() {
    let mut env = Env::new();
    let program = format!("read go; {ASK_FOR_THE_CURSOR}");
    let id = env.start("write-ask", &["--", "/bin/sh", "-c", &program]);
    let mut client = env.connect(&id);
    client.attach(0);
    let mut writer = env.connect(&id);
    writer.write(vec![text("go"), enter()]).unwrap();
    // The question reaches the client and the session does not answer for it:
    // the program is told exactly what the client's terminal says.
    client.wait_for("\x1b[6n", 0);
    // The writer was never a client: the one there is, is the one.
    assert_eq!(writer.status().clients, 1);
    client.type_in("\x1b[9;9R");
    client.wait_for("got:1b5b393b3952:\r\n", 0);
}

// --- typing -----------------------------------------------------------------

#[test]
fn text_and_keys_are_typed_in_the_modes_the_program_has_set() {
    let mut env = Env::new();
    let id = env.start("keys", &ECHO);
    let mut witness = witness(&env, &id);
    let mut writer = env.connect(&id);

    // Plain: the arrow is `CSI A`, control c is the control character, and the
    // text and the key go in one piece in their order.
    let from = witness.data.len();
    writer
        .write(vec![
            text("hi"),
            Stroke::key(key::UP, 0),
            Stroke::key(u32::from('c'), mods::CTRL),
            enter(),
        ])
        .unwrap();
    witness.wait_for("hi\x1b[A\x03\r", from);

    // The program turns on the application cursor keys (here, by what it
    // writes: `cat` gives back the sequence that was typed), and the arrow is
    // `SS3 A` from then on.
    writer.write(vec![text("\x1b[?1h")]).unwrap();
    witness.wait_for("\x1b[?1h", from);
    let from = witness.data.len();
    writer.write(vec![Stroke::key(key::UP, 0)]).unwrap();
    witness.wait_for("\x1bOA", from);

    // And it asks for the kitty keyboard protocol: control c is `CSI u`.
    writer.write(vec![text("\x1b[>1u")]).unwrap();
    witness.wait_for("\x1b[>1u", from);
    let from = witness.data.len();
    writer
        .write(vec![Stroke::key(u32::from('c'), mods::CTRL)])
        .unwrap();
    witness.wait_for("\x1b[99;5u", from);
}

#[test]
fn a_write_with_a_key_that_cannot_be_pressed_types_none_of_it() {
    let mut env = Env::new();
    let id = env.start("badkey", &ECHO);
    let mut witness = witness(&env, &id);
    let mut writer = env.connect(&id);
    assert_err(
        writer.write(vec![text("zzz"), Stroke::key(0xe000, 0)]),
        ErrorCode::BAD_REQUEST,
    );
    assert_err(
        writer.write(vec![Stroke::key(key::function(20).unwrap(), 0)]),
        ErrorCode::BAD_REQUEST,
    );
    writer.write(vec![text("after")]).unwrap();
    witness.wait_for("after", 0);
    assert!(
        !String::from_utf8_lossy(&witness.data).contains("zzz"),
        "nothing of the refused write was typed"
    );
}

#[test]
fn a_write_to_a_program_that_has_ended_is_an_error() {
    let mut env = Env::new();
    let id = env.start("ended-write", &["--", "/bin/sh", "-c", "exit 0"]);
    let mut waiter = env.connect(&id);
    waiter.send(Message::Wait);
    waiter.until("the end", |c| c.ended.is_some());
    let mut writer = env.connect(&id);
    assert_err(writer.write(vec![text("late")]), ErrorCode::ENDED);
    // The pong came after the error and nothing else did.
    assert_eq!(writer.errors.len(), 1);
}

#[test]
fn a_write_is_queued_whole_or_refused_whole() {
    let mut env = Env::new();
    // Nothing reads what is typed (a terminal in raw mode keeps what it is given
    // up to its own small limit, where a line that is never ended is dropped):
    // the relay holds up to a megabyte of it.
    let id = env.start(
        "full",
        &[
            "--",
            "/bin/sh",
            "-c",
            "stty raw -echo; echo ready; exec sleep 600",
        ],
    );
    let _witness = witness(&env, &id);
    let mut writer = env.connect(&id);
    let piece = vec![Stroke::text(vec![b'a'; 900 * 1024])];
    writer.write(piece.clone()).unwrap();
    assert_err(writer.write(piece), ErrorCode::INPUT_FULL);
    // Refused whole, so what is left of the room is what was there.
    writer.write(vec![text("small")]).unwrap();
}

// --- reading the screen -----------------------------------------------------

#[test]
fn the_screen_is_read_as_text_with_scrollback_and_wrapped_rows_joined() {
    let mut env = Env::new();
    let long = "y".repeat(100);
    let program = format!(
        "i=0; while [ $i -lt 40 ]; do echo L$i; i=$((i+1)); done; echo {long}; exec sleep 600"
    );
    let id = env.start("read", &["--", "/bin/sh", "-c", &program]);
    let mut witness = env.connect(&id);
    witness.attach(0);
    witness.wait_for(&format!("{long}\r\n"), 0);
    let mut reader = env.connect(&id);
    let status = reader.status();

    // The rows on screen, and the offset of what they show. 40 lines and the long
    // one, which wraps to two rows, and the row the cursor is on: 43 rows, 19 of
    // them above the screen.
    let (offset, size, rows) = reader.read_text(Capture::default());
    assert_eq!((offset, size), (status.offset, (80, 24)));
    let lines: Vec<&str> = rows.lines().collect();
    assert_eq!(lines.len(), 24);
    assert_eq!(lines[0], "L19");
    assert_eq!(lines[21], "y".repeat(80), "the long line wrapped");
    assert_eq!(lines[22], "y".repeat(20));
    assert_eq!(lines[23], "");

    let mut capture = Capture::default();
    capture.history = 5;
    let (_, _, with_history) = reader.read_text(capture);
    let lines: Vec<&str> = with_history.lines().collect();
    assert_eq!(lines.len(), 29);
    assert_eq!(lines[0], "L14");
    assert_eq!(lines[5], "L19");

    capture.history = 100_000;
    capture.unwrap = true;
    let (_, _, all) = reader.read_text(capture);
    let lines: Vec<&str> = all.lines().collect();
    assert_eq!(lines[0], "L0");
    assert_eq!(lines[40], long, "the wrapped rows are one line again");
    assert_eq!(
        lines.len(),
        42,
        "scrollback, the screen and the cursor's row"
    );

    // Reading changes nothing a client is told or sees: only the witness is
    // attached, and the offset is where it was.
    assert_eq!(reader.status().clients, 1);
    assert_eq!(reader.status().offset, status.offset);
}

#[test]
fn a_long_screen_comes_in_pieces_that_are_one_text() {
    let mut env = Env::new();
    let program = "i=0; while [ $i -lt 1500 ]; do \
        echo L$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; \
        i=$((i+1)); done; echo end-of-it; exec sleep 600";
    let id = env.start("pieces", &["--", "/bin/sh", "-c", program]);
    let mut reader = env.connect(&id);
    let mut witness = env.connect(&id);
    witness.attach(0);
    witness.wait_for("end-of-it", 0);
    reader.replies.clear();
    let mut capture = Capture::default();
    capture.history = 100_000;
    reader.send(Message::Read(capture));
    reader.until("the last piece", |c| {
        c.replies
            .iter()
            .any(|p| matches!(p.message, Message::ScreenText { more: false, .. }))
    });
    let pieces: Vec<bool> = reader
        .replies
        .iter()
        .filter_map(|p| match p.message {
            Message::ScreenText { more, .. } => Some(more),
            _ => None,
        })
        .collect();
    assert!(pieces.len() > 1, "{} piece", pieces.len());
    assert!(pieces[..pieces.len() - 1].iter().all(|more| *more));
    let (_, _, again) = reader.read_text(capture);
    assert!(again.contains("L0-xxx"), "the scrollback is there");
    assert!(again.contains("end-of-it"));
}

#[test]
fn the_screen_is_read_as_a_picture_without_attaching() {
    let mut env = Env::new();
    let id = env.start("picture", &LINES);
    let mut witness = witness(&env, &id);
    witness.type_in("a line of text\n");
    witness.wait_for("a line of text\r\n", 0);
    let mut reader = env.connect(&id);
    let mut capture = Capture::default();
    capture.picture = true;
    reader.send(Message::Read(capture));
    reader.until("a picture", |c| {
        c.pictures.last().is_some_and(|p| p.complete)
    });
    assert!(reader.attached.is_none(), "no attached came before it");
    let picture = reader.picture().clone();
    assert_eq!((picture.cols, picture.rows), (80, 24));
    assert_eq!(picture.offset, witness.next, "the picture is of the end");
    same(rebuilt(&picture, &[]), whole(&witness.data, 80, 24));
    // And nothing is streamed to a connection that has not attached.
    reader.pump(Duration::from_millis(300));
    assert!(reader.data.is_empty());
}

// --- waiting ----------------------------------------------------------------

#[test]
fn a_wait_is_answered_after_the_last_of_the_output_is_on_the_screen() {
    let mut env = Env::new();
    for n in 0..4 {
        let program = format!("sleep 0.3; echo last-line-{n}; exit 3");
        let id = env.start(&format!("wait{n}"), &["--", "/bin/sh", "-c", &program]);
        let mut waiter = env.connect(&id);
        waiter.send(Message::Wait);
        waiter.until("the end", |c| c.ended.is_some());
        assert_eq!(waiter.ended, Some(Exit::Code(3)));
        // What a read finds right after is all of it.
        let (_, _, rows) = waiter.read_text(Capture::default());
        assert!(rows.starts_with(&format!("last-line-{n}\n")), "{rows:?}");
    }
}

#[test]
fn a_wait_does_not_end_the_session_and_one_after_the_end_is_answered_at_once() {
    let mut env = Env::new();
    let id = env.start("kept", &["--", "/bin/sh", "-c", "echo bye; exit 2"]);
    let mut first = env.connect(&id);
    first.send(Message::Wait);
    first.until("the end", |c| c.ended.is_some());
    // The end was heard and the session is still there for whoever comes: not
    // delivered.
    std::thread::sleep(Duration::from_millis(400));
    assert!(env.find("warden", &id).is_some());
    let mut after = env.connect(&id);
    assert_eq!(after.status().state, State::ENDED);
    after.send(Message::Wait);
    after.until("the end, at once", |c| c.ended.is_some());
    assert_eq!(after.ended, Some(Exit::Code(2)));
    first.pump(Duration::from_millis(100));
    assert!(!first.closed, "the waiter's connection is left alone");

    // A client that attaches is what delivers it, and then it goes.
    let mut client = env.connect(&id);
    client.attach(0);
    client.until("the end", |c| c.ended.is_some());
    env.wait_gone(&id);
}

#[cfg(target_os = "linux")]
fn waits(pid: u32) -> u64 {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
        .map(|count| count.trim().parse().unwrap())
        .expect("a count of context switches")
}

#[cfg(target_os = "linux")]
#[test]
fn a_relay_that_is_typed_to_and_waited_on_with_nobody_attached_does_not_wake() {
    let mut env = Env::new();
    let id = env.start("idle", &["--", "/bin/sleep", "600"]);
    let mut waiter = env.connect(&id);
    waiter.send(Message::Wait);
    let mut writer = env.connect(&id);
    writer.write(vec![text("x")]).unwrap();
    // No look at the terminal follows typing nobody is shown.
    std::thread::sleep(Duration::from_millis(700));
    let relay = env.find("relay", &id).expect("a relay");
    let before = waits(relay);
    std::thread::sleep(Duration::from_millis(2000));
    let woken = waits(relay) - before;
    assert!(
        woken <= 2,
        "the relay woke {woken} times in two seconds of nothing"
    );
}

// --- the label --------------------------------------------------------------

fn new_with_label(label: Option<&str>, argv: &[&str]) -> NewSession {
    let mut new = NewSession::default();
    new.argv = argv.iter().map(|a| a.as_bytes().to_vec()).collect();
    new.label = label.map(str::to_owned);
    new
}

#[test]
fn a_label_is_made_with_the_session_seen_in_its_status_changed_and_cleared() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let id = env.name("label");
    env.track(&id);
    let made = bridge
        .try_new_with(
            Some(&id),
            new_with_label(Some("the build"), &["/bin/sleep", "600"]),
        )
        .unwrap();
    assert_eq!(made.label.as_deref(), Some("the build"));
    let mut conn = env.connect(&id);
    assert_eq!(conn.status().label.as_deref(), Some("the build"));
    assert_eq!(bridge.status(&id).label.as_deref(), Some("the build"));
    assert_eq!(
        bridge
            .list()
            .iter()
            .find(|s| s.id.as_str() == id)
            .unwrap()
            .label
            .as_deref(),
        Some("the build")
    );

    // Changed through the bridge: it says nothing, and the next status says it.
    bridge.send(Some(&id), Message::Label("the tests".to_owned()));
    bridge.send(Some(&id), Message::Ping(b"done".to_vec()));
    bridge.until("the pong", |b| !b.pongs.is_empty());
    assert!(bridge.errors.is_empty());
    assert_eq!(conn.status().label.as_deref(), Some("the tests"));
    bridge.send(Some(&id), Message::Label(String::new()));
    bridge.send(Some(&id), Message::Ping(b"done".to_vec()));
    bridge.until("the second pong", |b| b.pongs.len() > 1);
    assert_eq!(conn.status().label, None);

    // A session that is not there has no label to give, and a label with no
    // session is a malformed request; the relay's own socket takes none.
    bridge.send(Some("no-such"), Message::Label("x".to_owned()));
    let (session, code, _) = bridge.next_error();
    assert_eq!(
        (session.as_deref(), code),
        (Some("no-such"), ErrorCode::UNKNOWN_SESSION)
    );
    bridge.errors.clear();
    bridge.send(None, Message::Label("x".to_owned()));
    let (_, code, _) = bridge.next_error();
    assert_eq!(code, ErrorCode::BAD_REQUEST);
    conn.errors.clear();
    conn.send(Message::Label("x".to_owned()));
    conn.until("an error", |c| !c.errors.is_empty());
    assert_eq!(conn.errors[0].0, ErrorCode::BAD_REQUEST);
}

#[test]
fn a_label_stays_when_the_relay_is_replaced_and_while_the_session_waits_to_be_heard() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let id = env.name("keep");
    env.track(&id);
    bridge
        .try_new_with(
            Some(&id),
            new_with_label(Some("kept"), &["/bin/sh", "-c", "read go"]),
        )
        .unwrap();
    let newer = env.newer_kablo("label");
    let said = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&newer)
        .output()
        .unwrap();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );
    let mut conn = env.connect(&id);
    assert_eq!(conn.status().label.as_deref(), Some("kept"));

    // The program ends and nobody has been told: the session is listed for a
    // day, with its label, so that `wait build; capture build` works.
    conn.write(vec![text("go"), enter()]).unwrap();
    let start = Instant::now();
    while conn.status().state != State::ENDED {
        assert!(start.elapsed() < PATIENCE, "the program did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(conn.status().label.as_deref(), Some("kept"));

    // Heard, delivered: the session goes, and its label with it.
    let mut client = env.connect(&id);
    client.attach(0);
    client.until("the end", |c| c.ended.is_some());
    env.wait_gone(&id);
    assert!(!kablo::sockdir::label_path(&env.dir, &SessionId::new(&id).unwrap()).exists());
}

#[test]
fn a_session_that_is_terminated_and_heard_takes_its_label_with_it() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let id = env.name("terminated");
    env.track(&id);
    bridge
        .try_new_with(
            Some(&id),
            new_with_label(Some("going"), &["/bin/sleep", "600"]),
        )
        .unwrap();
    let mut client = env.connect(&id);
    client.attach(0);
    client.send(Message::Terminate { force: false });
    client.until("the end", |c| c.ended.is_some());
    env.wait_gone(&id);
    assert!(!kablo::sockdir::label_path(&env.dir, &SessionId::new(&id).unwrap()).exists());
}

#[test]
fn a_session_made_under_the_name_of_one_that_is_gone_does_not_have_its_label() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let id = env.name("stale");
    env.track(&id);
    // What a relay that was killed leaves behind.
    let session = SessionId::new(&id).unwrap();
    kablo::sockdir::write_label(&env.dir, &session, "of the old one").unwrap();
    let made = bridge
        .try_new_with(Some(&id), new_with_label(None, &["/bin/sleep", "600"]))
        .unwrap();
    assert_eq!(made.label, None);
    assert!(!kablo::sockdir::label_path(&env.dir, &session).exists());
    // And a session that refuses to start (the name is taken) leaves the label
    // of the one that has it alone.
    kablo::sockdir::write_label(&env.dir, &session, "of this one").unwrap();
    let again = bridge.try_new_with(
        Some(&id),
        new_with_label(Some("not this"), &["/bin/sleep", "600"]),
    );
    assert_eq!(again.unwrap_err().0, ErrorCode::SESSION_EXISTS);
    assert_eq!(
        kablo::sockdir::read_label(&env.dir, &session).as_deref(),
        Some("of this one")
    );
}

// --- through the bridge -----------------------------------------------------

#[test]
fn the_bridge_types_reads_and_confirms() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let id = env.name("bridge");
    env.track(&id);
    bridge
        .try_new_with(
            Some(&id),
            new_with_label(None, &["/bin/sh", "-c", "stty -echo; echo ready; exec cat"]),
        )
        .unwrap();
    let mut witness = witness(&env, &id);

    bridge.write(&id, vec![text("hi"), enter()]).unwrap();
    witness.wait_for("hi\r\n", 0);
    let (offset, size, rows) = bridge.read_text(&id, Capture::default());
    assert!(rows.starts_with("ready\nhi\n"), "{rows:?}");
    assert_eq!(size, (80, 24));
    assert_eq!(offset, witness.next);

    // The pong that follows says what the write had: its error.
    let got = bridge.write(&id, vec![Stroke::key(0xe000, 0)]);
    assert_err(got, ErrorCode::BAD_REQUEST);
    assert_eq!(bridge.errors.len(), 1);
    // A ping with no session is the bridge's own, and so is one for a session
    // there is no link to, or none a write waits to be confirmed at.
    let pongs = bridge.pongs.len();
    bridge.send(None, Message::Ping(b"a".to_vec()));
    bridge.send(Some("nobody"), Message::Ping(b"b".to_vec()));
    bridge.until("both pongs", |b| b.pongs.len() >= pongs + 2);
    // The bridge is not a client of the session: nothing was attached.
    assert_eq!(env.connect(&id).status().clients, 1, "only the witness");
}

#[test]
fn a_wait_through_the_bridge_goes_on_over_an_upgrade_and_gets_the_end() {
    let mut env = Env::new();
    let mut bridge = Bridge::start(&env);
    let id = env.name("bridge-wait");
    env.track(&id);
    bridge
        .try_new_with(
            Some(&id),
            new_with_label(None, &["/bin/sh", "-c", "read go; echo gone; exit 5"]),
        )
        .unwrap();
    bridge.send(Some(&id), Message::Wait);
    // Make sure the relay has it before it is replaced: its answer to a status
    // comes after what was asked on the same connection.
    bridge.status(&id);
    let newer = env.newer_kablo("wait");
    let said = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&newer)
        .output()
        .unwrap();
    assert!(
        said.status.success(),
        "{}",
        String::from_utf8_lossy(&said.stderr)
    );

    let mut writer = env.connect(&id);
    writer.write(vec![text("go"), enter()]).unwrap();
    bridge.until("the end", |b| {
        b.sessions.get(&id).is_some_and(|s| s.ended.is_some())
    });
    assert_eq!(bridge.seen(&id).ended, Some(Exit::Code(5)));
    // A read after the end finds all of the output, through the bridge too.
    let (_, _, rows) = bridge.read_text(&id, Capture::default());
    assert!(rows.contains("gone\n"), "{rows:?}");
}

// --- the link, with a relay that is made to behave --------------------------

/// A relay that takes `connections` connections in turn. `serve` is given what
/// connection `n` has heard so far and says when it is done with it: `Some` is what
/// to send, and a connection that is sent nothing is closed at once, as a relay that
/// is replaced closes its own; one that is sent something stays until the link
/// closes it.
fn fake_relay(
    path: std::path::PathBuf,
    id: SessionId,
    serve: impl Fn(usize, &[Message]) -> Option<Vec<Message>> + Send + 'static,
    connections: usize,
) -> mpsc::Receiver<Vec<Message>> {
    let listener = UnixListener::bind(path).unwrap();
    let (heard, heard_rx) = mpsc::channel();
    std::thread::spawn(move || {
        for n in 0..connections {
            let (mut sock, _) = listener.accept().unwrap();
            WIRE.server(&mut sock.try_clone().unwrap(), &mut sock)
                .unwrap();
            let mut decoder = PacketDecoder::new();
            let mut buf = vec![0u8; 4096];
            let mut messages = Vec::new();
            let mut stay = false;
            loop {
                let read = sock.read(&mut buf).unwrap();
                if read == 0 {
                    break;
                }
                decoder.push(&buf[..read]);
                while let Some(packet) = decoder.next_packet().unwrap() {
                    messages.push(packet.message);
                }
                if let Some(replies) = serve(n, &messages) {
                    stay = !replies.is_empty();
                    for reply in replies {
                        let mut bytes = Vec::new();
                        Packet::about(id.clone(), reply).encode(&mut bytes).unwrap();
                        sock.write_all(&bytes).unwrap();
                    }
                    break;
                }
            }
            let _ = heard.send(messages);
            if stay {
                let _ = sock.read(&mut buf);
            }
        }
    });
    heard_rx
}

/// Runs `link` until `done` holds of what it has said, or the patience is out.
fn run(link: &mut Link, got: &mut Vec<Packet>, done: impl Fn(&Link, &[Packet]) -> bool) {
    let start = Instant::now();
    while !done(link, got) {
        assert!(start.elapsed() < PATIENCE, "{got:?}");
        let mut set: Vec<_> = link.pollfd(true).into_iter().collect();
        kablo::sys::poll(&mut set, Some(Duration::from_millis(30))).unwrap();
        if let Some(fd) = set.first().filter(|fd| fd.revents != 0) {
            link.ready(fd.revents, Instant::now(), got);
        }
        link.tick(Instant::now(), got);
    }
}

#[test]
fn a_write_whose_confirmation_was_lost_with_the_connection_is_said_to_be_unknown() {
    let env = Env::new();
    let name = env.name("unsure");
    let id = SessionId::new(&name).unwrap();
    // The relay reads the write and the ping and goes: no pong.
    let heard = fake_relay(
        env.socket(&name),
        id.clone(),
        |_, messages| {
            messages
                .iter()
                .any(|m| matches!(m, Message::Ping(_)))
                .then(Vec::new)
        },
        1,
    );
    let now = Instant::now();
    let mut link = Link::open(&env.dir, id.clone(), now).unwrap();
    let mut got = Vec::new();
    let strokes = vec![Stroke::text("rm -rf build"), enter()];
    link.send(
        Packet::about(id.clone(), Message::Write(strokes.clone())),
        now,
        &mut got,
    );
    link.send(
        Packet::about(id.clone(), Message::Ping(b"w".to_vec())),
        now,
        &mut got,
    );
    run(&mut link, &mut got, |link, got| {
        link.is_gone() && !got.is_empty()
    });
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(
        got[0].message,
        Message::Error {
            code: ErrorCode::UNAVAILABLE,
            text: link::UNSURE.to_owned()
        }
    );
    // The relay got the write and the ping, once each, and nobody sent them again.
    let heard = heard.recv_timeout(PATIENCE).unwrap();
    assert_eq!(
        heard,
        [Message::Write(strokes), Message::Ping(b"w".to_vec())]
    );
}

#[test]
fn a_read_and_a_wait_are_asked_again_of_the_relay_that_comes_and_a_write_is_not() {
    let env = Env::new();
    let name = env.name("again");
    let id = SessionId::new(&name).unwrap();
    let heard = fake_relay(
        env.socket(&name),
        id.clone(),
        |n, messages| match (n, messages.len()) {
            // The first goes when it has what was asked, answering nothing.
            (0, 3) => Some(Vec::new()),
            // The second answers: the text, and the end.
            (1, 2) => Some(vec![
                Message::ScreenText {
                    offset: 7,
                    cols: 80,
                    rows: 24,
                    more: false,
                    data: b"text\n".to_vec(),
                },
                Message::Ended(Exit::Code(4)),
            ]),
            _ => None,
        },
        2,
    );
    let now = Instant::now();
    let mut link = Link::open(&env.dir, id.clone(), now).unwrap();
    let mut got = Vec::new();
    let strokes = vec![Stroke::text("typed once")];
    link.send(
        Packet::about(id.clone(), Message::Write(strokes.clone())),
        now,
        &mut got,
    );
    link.send(
        Packet::about(id.clone(), Message::Read(Capture::default())),
        now,
        &mut got,
    );
    link.send(Packet::about(id.clone(), Message::Wait), now, &mut got);
    run(&mut link, &mut got, |link, _| link.is_gone());
    let kinds: Vec<&str> = got
        .iter()
        .map(|p| match p.message {
            Message::ScreenText { .. } => "text",
            Message::Ended(_) => "ended",
            _ => "other",
        })
        .collect();
    assert_eq!(kinds, ["text", "ended"], "{got:?}");
    let first = heard.recv_timeout(PATIENCE).unwrap();
    let second = heard.recv_timeout(PATIENCE).unwrap();
    assert_eq!(
        first,
        [
            Message::Write(strokes),
            Message::Read(Capture::default()),
            Message::Wait
        ]
    );
    assert_eq!(second, [Message::Read(Capture::default()), Message::Wait]);
}
