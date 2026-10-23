//! The screen a relay keeps: the picture a client gets in place of the bytes
//! that are gone, and the terminal's answers while nobody is attached.
//!
//! "The same screen" is checked the way a client lives it: a terminal that has
//! the picture and then the output after it is compared with a terminal that was
//! given everything the program ever wrote. Both have no scrollback, so what is
//! compared is what is on the screen and the state around it (modes, cursor,
//! scrolling region, the other screen).

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

use common::{Conn, Env, PATIENCE, rebuilt, same, viewing, whole};
use kablo::link::Link;
use kablo_proto::v1::{ErrorCode, Message, OFFSET_END, Packet, PacketDecoder, Size, View};
use kablo_proto::{SessionId, WIRE};

const SH: [&str; 2] = ["--", "/bin/sh"];

/// A small ring, so that a few hundred lines wrap it.
const SMALL_RING: [&str; 4] = ["--journal-size", "16K", "--", "/bin/sh"];

fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: a signal to a process of this test's own session.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

/// Waits until `witness` has everything the session has written.
fn settle(control: &mut Conn, witness: &mut Conn) {
    loop {
        let status = control.status();
        witness.pump(Duration::from_millis(20));
        if status.offset == witness.next {
            return;
        }
    }
}

/// Types a loop that prints `lines` numbered lines of 200 bytes each, and waits
/// for the last.
fn print_lines(witness: &mut Conn, lines: u32) {
    let filler = "x".repeat(200);
    witness.type_in(&format!(
        "i=0; while [ $i -lt {lines} ]; do echo L$i-{filler}; i=$((i+1)); sleep 0.01; done\n"
    ));
    witness.wait_for(&format!("L{}-", lines - 1), 0);
}

#[test]
fn a_client_that_missed_more_than_the_ring_holds_gets_the_screen_and_then_the_stream() {
    let mut env = Env::new();
    let id = env.start("wrap", &SMALL_RING);
    let mut witness = env.connect(&id);
    witness.attach(0);
    print_lines(&mut witness, 300);
    let mut control = env.connect(&id);
    settle(&mut control, &mut witness);
    let status = control.status();
    assert!(status.retained_from > 0, "the ring wrapped");

    let mut late = env.connect(&id);
    let (start, truncated) = late.attach(0);
    assert!(truncated, "what it asked for is gone");
    late.until("the picture", |c| {
        c.pictures.last().is_some_and(|p| p.complete)
    });
    let picture = late.picture().clone();
    assert_eq!(picture.offset, start, "the stream goes on from the picture");
    assert!(start >= status.retained_from);
    assert_eq!((picture.cols, picture.rows), (80, 24));

    // More output after the picture, for both.
    witness.type_in("echo after-$((1+2))\n");
    witness.wait_for("after-3\r\n", 0);
    late.wait_for("after-3\r\n", 0);
    settle(&mut control, &mut witness);
    late.until("the same end", |c| c.next == witness.next);

    // Not one byte of the program's own stream was changed or asked for again.
    let at = usize::try_from(start).unwrap();
    assert_eq!(late.data, witness.data[at..]);
    same(rebuilt(&picture, &late.data), whole(&witness.data, 80, 24));
}

#[test]
fn a_picture_is_made_for_the_size_the_client_says() {
    let mut env = Env::new();
    let id = env.start("size", &SH);
    let mut witness = env.connect(&id);
    witness.attach(0);
    witness.type_in("echo hello\n");
    witness.wait_for("hello\r\n", 0);

    let mut other = env.connect(&id);
    other.attach_with_picture(OFFSET_END, 100, 30);
    assert_eq!((other.picture().cols, other.picture().rows), (100, 30));
    let size = other.status().size;
    assert_eq!((size.cols, size.rows), (100, 30), "the terminal took it");
    // A view that says no size leaves the terminal as it is.
    let mut quiet = env.connect(&id);
    quiet.send(Message::Attach {
        offset: OFFSET_END,
        view: Some(viewing(Size::default(), true, false)),
    });
    quiet.until("a picture", |c| {
        c.pictures.last().is_some_and(|p| p.complete)
    });
    assert_eq!((quiet.picture().cols, quiet.picture().rows), (100, 30));
}

#[test]
fn a_program_on_the_other_screen_comes_with_its_modes() {
    let mut env = Env::new();
    let id = env.start("alt", &SH);
    let mut witness = env.connect(&id);
    witness.attach(0);
    // What a full-screen program leaves on a terminal: the other screen, a
    // scrolling region, cursor keys, mouse and paste modes, a hidden cursor,
    // bold text, a link and a title.
    witness.type_in(concat!(
        "printf '\\033[?1049h\\033[2J\\033[5;10Hbuffer \\033[1;31mred\\033[0m\\033[3;20r",
        "\\033[?1h\\033[?1000h\\033[?1006h\\033[?2004h\\033[?25l\\033]2;vim\\007",
        "\\033]8;;http://example.com\\033\\\\a link\\033]8;;\\033\\\\\\033[12;5H'; echo done-$((6*7))\n"
    ));
    witness.wait_for("done-42\r\n", 0);
    let mut control = env.connect(&id);
    settle(&mut control, &mut witness);

    // The offset is still in the journal; the client wants the picture anyway.
    let mut fresh = env.connect(&id);
    let (start, truncated) = fresh.attach_with_picture(0, 80, 24);
    assert!(!truncated, "nothing was cut");
    let picture = fresh.picture().clone();
    assert_eq!(picture.offset, start);
    let text = String::from_utf8_lossy(&picture.bytes).into_owned();
    assert!(
        text.contains("\x1b[?1049h"),
        "the other screen is on: {text:?}"
    );
    assert!(text.contains("buffer "), "{text:?}");
    assert!(text.contains("\x1b[?2004h"), "{text:?}");

    witness.type_in("echo more-$((3*3))\n");
    witness.wait_for("more-9\r\n", 0);
    settle(&mut control, &mut witness);
    fresh.until("the same end", |c| c.next == witness.next);
    same(rebuilt(&picture, &fresh.data), whole(&witness.data, 80, 24));
}

/// A program that asks where the cursor is and says what it was told, as hex.
///
/// It waits long (ten seconds) for the answer, so that a busy machine cannot make it
/// give up first, and then a moment for a second one: `got:<first>:<second>`.
const ASK_FOR_THE_CURSOR: &str = "stty raw -echo min 0 time 100; printf '\\033[6n'; \
     got=$(dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
     stty min 0 time 5; \
     more=$(dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d ' \\n'); \
     stty sane; echo \"got:$got:$more\"";

/// Waits until the session has read `bytes` of what the program wrote: the
/// relay has seen it and answered what it asked.
fn wait_for_output(env: &Env, id: &str, bytes: u64) {
    let mut control = env.connect(id);
    let start = Instant::now();
    while control.status().offset < bytes {
        assert!(start.elapsed() < PATIENCE, "the program wrote nothing");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_program_that_asks_while_nobody_is_attached_is_answered() {
    let mut env = Env::new();
    // It asks at once, with no client in sight.
    let id = env.start("ask", &["--", "/bin/sh", "-c", ASK_FOR_THE_CURSOR]);
    wait_for_output(&env, &id, 4);
    let mut late = env.connect(&id);
    late.attach(0);
    // The cursor is at the first row and column; the answer is `ESC [ 1 ; 1 R`,
    // once.
    late.wait_for("got:1b5b313b3152:\r\n", 0);
}

#[test]
fn a_client_that_is_attached_is_the_one_that_answers() {
    let mut env = Env::new();
    // The program waits to be told to ask, so that the client is attached when
    // it does. (A shell's own terminal handling between commands is no place
    // for a test about answers: the program is a script.)
    let program = format!("read go; {ASK_FOR_THE_CURSOR}");
    let id = env.start("asked", &["--", "/bin/sh", "-c", &program]);
    let mut client = env.connect(&id);
    client.attach(0);
    client.type_in("go\n");
    // The question reaches the client; the session does not answer for it.
    // (Were it to, its answer would be in the program's input before the client
    // even sees the question, and the program would read that one first.)
    client.wait_for("\x1b[6n", 0);
    client.type_in("\x1b[9;9R");
    // Exactly the client's answer, and not the screen's before or after it.
    client.wait_for("got:1b5b393b3952:\r\n", 0);
}

#[test]
fn a_relay_that_died_is_replaced_by_one_that_has_the_screen() {
    let mut env = Env::new();
    let id = env.start("crash", &SMALL_RING);
    let mut witness = env.connect(&id);
    witness.attach(0);
    print_lines(&mut witness, 300);
    let mut control = env.connect(&id);
    settle(&mut control, &mut witness);
    assert!(control.status().retained_from > 0, "the ring wrapped");

    let relay = env.find("relay", &id).expect("a relay");
    signal(relay, libc::SIGKILL);
    witness.until_closed();

    // The warden starts another; it has the journal and nothing else, and
    // builds the screen from it.
    let mut back = env.connect(&id);
    let (_, truncated) = back.attach_with_picture(OFFSET_END, 80, 24);
    assert!(!truncated);
    assert_ne!(env.find("relay", &id), Some(relay));
    let picture = back.picture().clone();
    same(rebuilt(&picture, &[]), whole(&witness.data, 80, 24));
}

#[test]
fn a_relay_that_was_replaced_on_purpose_has_the_screen_too() {
    let mut env = Env::new();
    let id = env.start("swap", &SMALL_RING);
    let newer = env.newer_kablo("screen");
    let mut witness = env.connect(&id);
    witness.attach(0);
    print_lines(&mut witness, 300);
    let mut control = env.connect(&id);
    settle(&mut control, &mut witness);
    assert!(control.status().retained_from > 0, "the ring wrapped");

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
    witness.until_closed();

    let mut back = env.connect(&id);
    back.attach_with_picture(OFFSET_END, 80, 24);
    let picture = back.picture().clone();
    same(rebuilt(&picture, &[]), whole(&witness.data, 80, 24));

    // And it goes on keeping it.
    let mut again = env.connect(&id);
    again.attach(OFFSET_END);
    again.type_in("echo still-$((2+3))\n");
    again.wait_for("still-5\r\n", 0);
}

/// How many times the stream holds `needle`.
fn count(stream: &[u8], needle: &[u8]) -> usize {
    stream
        .windows(needle.len())
        .filter(|w| *w == needle)
        .count()
}

#[test]
fn a_relay_that_takes_over_does_not_answer_what_was_asked_before_it() {
    let mut env = Env::new();
    // The terminal is raw and `cat` writes back what the program is sent, so
    // every answer the session gives shows in the stream.
    let id = env.start(
        "echo",
        &[
            "--",
            "/bin/sh",
            "-c",
            "stty raw -echo; printf '\\033[6n'; exec cat",
        ],
    );
    let newer = env.newer_kablo("echo");
    wait_for_output(&env, &id, 4);
    let mut before = env.connect(&id);
    before.attach(0);
    before.wait_for("\x1b[1;1R", 0);
    assert_eq!(count(&before.data, b"\x1b[1;1R"), 1, "answered once");
    drop(before);

    let said = env
        .kablo()
        .arg("upgrade")
        .arg(&id)
        .arg(&newer)
        .output()
        .unwrap();
    assert!(said.status.success());
    // Connecting waits for the relay that took over.
    let mut after = env.connect(&id);
    after.attach(0);
    after.pump(Duration::from_millis(500));
    assert_eq!(
        count(&after.data, b"\x1b[1;1R"),
        1,
        "the new relay read the question again and did not answer it: {:?}",
        String::from_utf8_lossy(&after.data)
    );
}

/// Asks where the cursor is and sends back whatever it is told, so that every
/// answer the program gets shows in the stream: `ESC [ 6 n` (4 bytes) and, with
/// nobody to hold it back, the session's answer `ESC [ 1 ; 1 R` (6).
const ASK_AND_ECHO: [&str; 4] = [
    "--",
    "/bin/sh",
    "-c",
    "stty raw -echo; printf '\\033[6n'; exec cat",
];

/// What the session's stream is when `ASK_AND_ECHO` has been answered once.
const ASKED_AND_ANSWERED: u64 = 10;

/// What a terminal does: answers a cursor position question when it is shown
/// one, and reads for `wait` (a question that is not shown is not answered).
fn answer_like_a_terminal(client: &mut Conn, wait: Duration) {
    let start = Instant::now();
    let mut answered_up_to = 0;
    while start.elapsed() < wait {
        client.pump(Duration::from_millis(20));
        let asked = count(&client.data, b"\x1b[6n");
        for _ in answered_up_to..asked {
            client.type_in("\x1b[1;1R");
        }
        answered_up_to = asked;
    }
}

#[test]
fn a_client_gets_the_bytes_and_is_told_how_far_the_session_answered() {
    let mut env = Env::new();
    let id = env.start("told", &ASK_AND_ECHO);
    wait_for_output(&env, &id, ASKED_AND_ANSWERED);
    let mut client = env.connect(&id);
    // The bytes are all there, question and answer; it is up to the client not
    // to answer a second time, and it is told what the session answered.
    assert_eq!(client.attach(0), (0, false));
    assert_eq!(client.answered, ASKED_AND_ANSWERED);
    client.wait_for("\x1b[6n\x1b[1;1R", 0);
}

#[test]
fn a_terminal_that_cannot_hold_its_answers_back_is_shown_a_picture_not_the_question() {
    let mut env = Env::new();
    let id = env.start("picture", &ASK_AND_ECHO);
    wait_for_output(&env, &id, ASKED_AND_ANSWERED);
    let mut client = env.connect(&id);
    let (start, truncated) = client.attach_if_answered(0, 80, 24);
    assert!(truncated, "the picture takes the place of the bytes");
    assert_eq!(start, ASKED_AND_ANSWERED, "the stream goes on from the end");
    assert_eq!(client.picture().offset, start);
    assert_eq!(client.answered, ASKED_AND_ANSWERED);
    // Whatever it is shown it answers: it was shown no question, so the
    // program gets no second answer.
    answer_like_a_terminal(&mut client, Duration::from_millis(500));
    assert!(client.data.is_empty(), "{:?}", client.data);
    let mut witness = env.connect(&id);
    witness.attach(0);
    witness.pump(Duration::from_millis(300));
    assert_eq!(count(&witness.data, b"\x1b[1;1R"), 1, "answered once");
    // The same terminal asked for the bytes would have answered again.
    let mut before = env.connect(&id);
    before.attach(0);
    answer_like_a_terminal(&mut before, Duration::from_millis(500));
    witness.pump(Duration::from_millis(300));
    assert_eq!(count(&witness.data, b"\x1b[1;1R"), 2, "the duplicate");
}

#[test]
fn a_terminal_that_cannot_hold_its_answers_back_gets_bytes_when_nothing_was_asked() {
    let mut env = Env::new();
    let id = env.start("plain", &["--", "/bin/sh", "-c", "echo hello; exec cat"]);
    wait_for_output(&env, &id, 7);
    let mut client = env.connect(&id);
    assert_eq!(client.attach_if_answered(0, 80, 24), (0, false));
    client.wait_for("hello\r\n", 0);
    // And when what it asks for starts after the question there is no picture
    // either.
    let ask = env.start("past", &ASK_AND_ECHO);
    wait_for_output(&env, &ask, ASKED_AND_ANSWERED);
    let mut late = env.connect(&ask);
    assert_eq!(
        late.attach_if_answered(ASKED_AND_ANSWERED, 80, 24),
        (ASKED_AND_ANSWERED, false)
    );
}

#[test]
fn a_question_a_client_answered_is_not_shown_again_to_a_terminal_that_would_answer() {
    let mut env = Env::new();
    // The program asks once a client is there to answer.
    let program = "read go; stty raw -echo; printf '\\033[6n'; exec cat";
    let id = env.start("answered-by-a-client", &["--", "/bin/sh", "-c", program]);
    let mut first = env.connect(&id);
    first.attach(0);
    first.type_in("go\n");
    first.wait_for("\x1b[6n", 0);
    first.type_in("\x1b[9;9R");
    // The program has it: `cat` sends it back.
    first.wait_for("\x1b[9;9R", 0);
    first.send(Message::Detach);
    let mut control = env.connect(&id);
    let start = Instant::now();
    while control.status().clients != 0 {
        assert!(start.elapsed() < PATIENCE, "the client never left");
        std::thread::sleep(Duration::from_millis(20));
    }

    // The session never wrote an answer of its own, yet the question is
    // answered: a terminal that answers what it is shown is not shown it again.
    let mut back = env.connect(&id);
    let (_, truncated) = back.attach_if_answered(0, 80, 24);
    assert!(truncated);
    assert!(back.answered > 0);
    let mut witness = env.connect(&id);
    witness.attach(0);
    witness.pump(Duration::from_millis(300));
    assert_eq!(count(&witness.data, b"\x1b[1;1R"), 0);
    assert_eq!(count(&witness.data, b"\x1b[9;9R"), 1);
}

#[test]
fn a_relay_that_took_over_does_not_show_a_question_it_cannot_say_was_unanswered() {
    let mut env = Env::new();
    let asking = env.start("swapped-asking", &ASK_AND_ECHO);
    let quiet = env.start(
        "swapped-quiet",
        &["--", "/bin/sh", "-c", "echo hi; exec cat"],
    );
    wait_for_output(&env, &asking, ASKED_AND_ANSWERED);
    wait_for_output(&env, &quiet, 4);
    for id in [&asking, &quiet] {
        let newer = env.newer_kablo(id);
        let said = env
            .kablo()
            .arg("upgrade")
            .arg(id)
            .arg(&newer)
            .output()
            .unwrap();
        assert!(
            said.status.success(),
            "{}",
            String::from_utf8_lossy(&said.stderr)
        );
    }

    // The question was answered before the relay changed: still a picture.
    let mut after = env.connect(&asking);
    assert!(after.attach_if_answered(0, 80, 24).1);
    // Output that held none is still bytes.
    let mut plain = env.connect(&quiet);
    assert_eq!(plain.attach_if_answered(0, 80, 24), (0, false));
    plain.wait_for("hi\r\n", 0);
}

#[test]
fn the_scrollback_a_session_is_started_with_is_what_its_picture_holds() {
    let mut env = Env::new();
    let none = env.start("hist0", &["--history", "0", "--", "/bin/sh"]);
    let some = env.start("hist", &["--history", "500", "--", "/bin/sh"]);
    for id in [&none, &some] {
        let mut witness = env.connect(id);
        witness.attach(0);
        witness.type_in("i=0; while [ $i -lt 100 ]; do echo row$i; i=$((i+1)); done\n");
        witness.wait_for("row99\r\n", 0);
    }
    let mut a = env.connect(&none);
    a.attach_with_picture(OFFSET_END, 80, 24);
    let mut b = env.connect(&some);
    b.attach_with_picture(OFFSET_END, 80, 24);
    let (a, b) = (
        String::from_utf8_lossy(&a.picture().bytes).into_owned(),
        String::from_utf8_lossy(&b.picture().bytes).into_owned(),
    );
    assert!(
        !a.contains("row3\r") && !a.contains("row3\x1b"),
        "no scrollback: {a:?}"
    );
    assert!(b.contains("row3"), "500 rows of it: {b:?}");
    // A number past what a screen may keep is refused where it is typed.
    let refused = env.try_start("hist-big", &["--history", "100001", "--", "/bin/sh"]);
    assert!(refused.is_err());
}

/// A stand-in for a session's relay that sends half a picture and goes, then
/// is reached again by the link and sends the picture whole. It reports every
/// `attach` it was sent.
fn half_a_picture_then_a_whole_one(
    path: std::path::PathBuf,
    id: SessionId,
) -> mpsc::Receiver<Message> {
    let listener = UnixListener::bind(path).unwrap();
    let (heard, attaches) = mpsc::channel();
    std::thread::spawn(move || {
        let serve = |first: bool| {
            let (mut sock, _) = listener.accept().unwrap();
            WIRE.server(&mut sock.try_clone().unwrap(), &mut sock)
                .unwrap();
            let mut decoder = PacketDecoder::new();
            let mut buf = vec![0u8; 4096];
            let mut resized = false;
            let mut answered = false;
            let send = |sock: &mut std::os::unix::net::UnixStream, message: Message| {
                let mut bytes = Vec::new();
                Packet::about(id.clone(), message)
                    .encode(&mut bytes)
                    .unwrap();
                sock.write_all(&bytes).unwrap();
            };
            while !(answered && (!first || resized)) {
                let n = sock.read(&mut buf).unwrap();
                if n == 0 {
                    return;
                }
                decoder.push(&buf[..n]);
                while let Some(packet) = decoder.next_packet().unwrap() {
                    match packet.message {
                        attach @ Message::Attach { .. } if !answered => {
                            answered = true;
                            let _ = heard.send(attach);
                            let truncated = true;
                            send(
                                &mut sock,
                                Message::Attached {
                                    offset: 100,
                                    truncated,
                                    answered: 0,
                                },
                            );
                            send(
                                &mut sock,
                                Message::Picture {
                                    offset: 100,
                                    cols: 100,
                                    rows: 30,
                                    more: first,
                                    data: b"half".to_vec(),
                                },
                            );
                            if !first {
                                send(
                                    &mut sock,
                                    Message::Output {
                                        offset: 100,
                                        data: b"live".to_vec(),
                                    },
                                );
                            }
                        }
                        Message::Resize(_) => resized = true,
                        _ => {}
                    }
                }
            }
            if !first {
                // Stay until the client is done with us.
                let _ = sock.read(&mut buf);
            }
        };
        serve(true);
        serve(false);
    });
    attaches
}

#[test]
fn a_picture_that_was_cut_short_by_a_new_relay_is_sent_again_from_the_start() {
    let env = Env::new();
    let name = env.name("link");
    let id = SessionId::new(&name).unwrap();
    let attaches = half_a_picture_then_a_whole_one(env.socket(&name), id.clone());
    let now = Instant::now();
    let mut link = Link::open(&env.dir, id.clone(), now).unwrap();
    let mut got = Vec::new();
    let view = |cols, rows| {
        View::new(Size {
            cols,
            rows,
            xpixel: 0,
            ypixel: 0,
        })
    };
    link.send(
        Packet::about(
            id.clone(),
            Message::Attach {
                offset: 0,
                view: Some(view(100, 30)),
            },
        ),
        now,
        &mut got,
    );
    let mut resized = false;
    let start = Instant::now();
    while !got
        .iter()
        .any(|packet| matches!(packet.message, Message::Output { .. }))
    {
        assert!(start.elapsed() < PATIENCE, "{got:?}");
        let now = Instant::now();
        let mut set: Vec<_> = link.pollfd(true).into_iter().collect();
        let wait = Duration::from_millis(30);
        kablo::sys::poll(&mut set, Some(wait)).unwrap();
        if let Some(fd) = set.first().filter(|fd| fd.revents != 0) {
            link.ready(fd.revents, now, &mut got);
        }
        link.tick(Instant::now(), &mut got);
        // The client resizes once it has seen half the picture, and the relay
        // that sent it goes.
        let half = got
            .iter()
            .any(|packet| matches!(packet.message, Message::Picture { more: true, .. }));
        if half && !resized {
            resized = true;
            link.send(
                Packet::about(id.clone(), Message::Resize(view(120, 40).size)),
                Instant::now(),
                &mut got,
            );
        }
    }

    let kinds: Vec<&str> = got
        .iter()
        .map(|packet| match &packet.message {
            Message::Attached { .. } => "attached",
            Message::Picture { more: true, .. } => "piece",
            Message::Picture { more: false, .. } => "last piece",
            Message::Output { .. } => "output",
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        ["attached", "piece", "attached", "last piece", "output"],
        "the second attached is not swallowed: the client begins the picture again"
    );
    let first = attaches.recv_timeout(PATIENCE).unwrap();
    let second = attaches.recv_timeout(PATIENCE).unwrap();
    assert_eq!(
        first,
        Message::Attach {
            offset: 0,
            view: Some(view(100, 30))
        }
    );
    // From where the client was, at its latest size, and asking for the picture.
    assert_eq!(
        second,
        Message::Attach {
            offset: 100,
            view: Some(viewing(view(120, 40).size, true, false))
        }
    );
}

#[test]
fn a_terminal_bigger_than_a_screen_may_be_is_refused_and_changes_nothing() {
    let mut env = Env::new();
    let id = env.start("big", &SH);
    let mut client = env.connect(&id);
    client.attach(0);
    let before = client.status().size;
    let size = |cols, rows| Size {
        cols,
        rows,
        xpixel: 0,
        ypixel: 0,
    };

    client.send(Message::Resize(size(65535, 65535)));
    client.until("an error", |c| !c.errors.is_empty());
    assert_eq!(client.errors[0].0, ErrorCode::BAD_REQUEST);
    assert_eq!(client.status().size, before, "the terminal is as it was");

    // The same for a client that comes with it, which is not attached for it.
    let mut other = env.connect(&id);
    other.send(Message::Attach {
        offset: OFFSET_END,
        view: Some(viewing(size(5000, 24), true, false)),
    });
    other.until("an error", |c| !c.errors.is_empty());
    assert_eq!(other.errors[0].0, ErrorCode::BAD_REQUEST);
    let status = other.status();
    assert_eq!((status.size, status.clients), (before, 1));

    // And a session is not started with one.
    let refused = env.try_start("big2", &["--cols", "1001", "--", "/bin/sh"]);
    assert!(refused.is_err());
}

#[test]
fn a_picture_turns_on_the_pixel_mouse_and_size_report_modes_the_program_turned_on() {
    let mut env = Env::new();
    let id = env.start("kept", &SH);
    let mut witness = env.connect(&id);
    witness.attach(0);
    let has = |picture: &[u8], needle: &[u8]| picture.windows(needle.len()).any(|w| w == needle);

    // The shell writes the sequences (what is typed is the text `\033[?1016h`, not the
    // escape), and the screen follows the program's output, not its echo.
    witness.type_in("printf '\\033[?1016h\\033[?2048h'; echo done-$((40+2))\n");
    witness.wait_for("done-42\r\n", 0);
    let mut late = env.connect(&id);
    late.attach_with_picture(OFFSET_END, 80, 24);
    assert!(has(&late.picture().bytes, b"\x1b[?1016h"));
    assert!(has(&late.picture().bytes, b"\x1b[?2048h"));

    witness.type_in("printf '\\033[?1016l'; echo done-$((40+3))\n");
    witness.wait_for("done-43\r\n", 0);
    let mut later = env.connect(&id);
    later.attach_with_picture(OFFSET_END, 80, 24);
    assert!(!has(&later.picture().bytes, b"\x1b[?1016h"));
    assert!(has(&later.picture().bytes, b"\x1b[?2048h"));
}
