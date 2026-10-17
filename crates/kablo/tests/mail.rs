//! The bridge's mailbox, with real bridges and real sessions: a program on the
//! machine connects to it and asks where the app the bridge carries stands.
//!
//! The program is this test, or this test's binary run again inside a session
//! (`mail_sender_in_a_session`, which does nothing unless it is told where the
//! mailbox is): the terminal an answer names is found by the kernel's word on
//! the process that connected, so a sender has to be in the terminal to be
//! found there.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{Bridge, Env, PATIENCE};
use kablo::sockdir;
use kablo_proto::v1::stream::{self, FailCode, Incoming, IncomingDecoder, Side};
use kablo_proto::v1::{ErrorCode, Message, Packet, Standing};
use kablo_proto::{SessionId, WIRE};

const SH: [&str; 2] = ["--", "/bin/sh"];

/// The variable that tells `mail_sender_in_a_session` where the mailbox is.
const MAILBOX: &str = "KABLO_TEST_MAILBOX";

/// A program connected to a mailbox.
struct Sender {
    sock: UnixStream,
    decoder: IncomingDecoder,
}

impl Sender {
    fn to(mailbox: &Path) -> Sender {
        let mut sock = UnixStream::connect(mailbox).unwrap();
        sock.set_read_timeout(Some(PATIENCE)).unwrap();
        WIRE.client(&mut sock.try_clone().unwrap(), &mut sock)
            .unwrap();
        Sender {
            sock,
            decoder: IncomingDecoder::new(),
        }
    }

    fn send(&mut self, session: Option<&str>, message: Message) {
        let session = session.map(|id| SessionId::new(id).unwrap());
        let mut bytes = Vec::new();
        Packet { session, message }.encode(&mut bytes).unwrap();
        self.sock.write_all(&bytes).unwrap();
    }

    /// The next thing the mailbox says; `None` when it has closed the
    /// connection.
    fn next(&mut self) -> Option<Incoming> {
        let mut buf = [0u8; 4096];
        loop {
            if let Some(incoming) = self.decoder.next_incoming().unwrap() {
                return Some(incoming);
            }
            match self.sock.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return None,
                Err(e) => panic!("the mailbox does not answer: {e}"),
            }
        }
    }

    /// Asks, naming the terminal or not, and returns the terminal the answer
    /// is about and how the app stands there.
    fn ask(&mut self, terminal: Option<&str>) -> (Option<String>, Standing) {
        self.send(terminal, Message::Ask);
        match self.next() {
            Some(Incoming::Session(Packet {
                session,
                message: Message::AskReply(standing),
            })) => (session.map(|id| id.as_str().to_owned()), standing),
            other => panic!("not an answer: {other:?}"),
        }
    }
}

/// The mailbox of `bridge`.
fn mailbox_of(env: &Env, bridge: &Bridge) -> PathBuf {
    let path = sockdir::mailbox_path(&env.dir, bridge.pid());
    assert!(path.exists(), "{} is not there", path.display());
    path
}

fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: a signal to a bridge this test started.
    unsafe { libc::kill(pid as libc::pid_t, signal) };
}

/// The program `the_sender_in_a_session` runs in a session's terminal: asks
/// the mailbox it is told of, without naming a terminal, and prints what it
/// is told. Run by itself, as a test, it does nothing.
#[test]
fn mail_sender_in_a_session() {
    let Some(mailbox) = std::env::var_os(MAILBOX) else {
        return;
    };
    let (terminal, standing) = Sender::to(Path::new(&mailbox)).ask(None);
    println!(
        "mail-reply session={} attached={}",
        terminal.as_deref().unwrap_or("-"),
        standing.attached
    );
}

/// What the sender run in a session said, once it has said it all.
fn said_in(shell: &mut common::Conn, command: &str) -> String {
    let before = shell.data.len();
    shell.type_in(command);
    shell.until("the sender to answer", |c| {
        reply_line(c.data.get(before..).unwrap_or_default()).is_some()
    });
    reply_line(shell.data.get(before..).unwrap()).unwrap()
}

fn reply_line(data: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    let line = text.get(text.find("mail-reply ")?..)?;
    Some(line.get(..line.find("\r\n")?)?.to_owned())
}

#[test]
fn a_bridge_keeps_a_mailbox_that_is_no_session_and_takes_it_away_when_it_goes() {
    let mut env = Env::new();
    let id = env.start("listed", &SH);
    let mut bridge = Bridge::start(&env);
    let path = mailbox_of(&env, &bridge);

    let boxes = sockdir::mailboxes(&env.dir).unwrap();
    assert_eq!(boxes.len(), 1);
    assert_eq!((boxes[0].pid, &boxes[0].path), (bridge.pid(), &path));
    let listed: Vec<String> = bridge
        .list()
        .iter()
        .map(|status| status.id.as_str().to_owned())
        .collect();
    assert_eq!(
        listed,
        [id.clone()],
        "the mailbox is not listed as a session"
    );
    let out = env.kablo().arg("list").output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&id) && !text.contains("mail-"), "{text}");
    assert_eq!(sockdir::session_ids(&env.dir).unwrap().len(), 1);

    // The client going away ends the bridge, and the name goes with it.
    bridge.hang_up();
    assert!(bridge.wait_exit().success());
    assert!(!path.exists());
}

#[test]
fn the_name_of_a_bridge_that_was_killed_is_cleared_and_that_of_one_that_is_only_silent_is_not() {
    let env = Env::new();
    let mut killed = Bridge::start(&env);
    let silent = Bridge::start(&env);
    let (dead_path, silent_path) = (mailbox_of(&env, &killed), mailbox_of(&env, &silent));

    // A bridge that cannot accept (it is stopped, as a busy one is slow)
    // keeps its name.
    signal(silent.pid(), libc::SIGSTOP);
    signal(killed.pid(), libc::SIGKILL);
    killed.wait_exit();
    let boxes = sockdir::mailboxes(&env.dir).unwrap();
    signal(silent.pid(), libc::SIGCONT);

    assert_eq!(boxes.len(), 1, "{boxes:?}");
    assert_eq!(boxes[0].pid, silent.pid());
    assert!(!dead_path.exists());
    assert!(silent_path.exists());
}

#[test]
fn a_program_outside_every_session_is_from_no_terminal_and_the_app_is_where_it_attached() {
    let mut env = Env::new();
    let id = env.start("mail-out", &SH);
    let mut bridge = Bridge::start(&env);
    let mut sender = Sender::to(&mailbox_of(&env, &bridge));

    // Not attached to anything yet.
    let (terminal, standing) = sender.ask(None);
    assert_eq!(terminal, None);
    assert!(!standing.attached && !standing.tickets);

    bridge.attach(&id, 0);
    // Asked about the terminal it names, whoever asks.
    let (terminal, standing) = sender.ask(Some(&id));
    assert_eq!(terminal.as_deref(), Some(id.as_str()));
    assert!(standing.attached);
    let (_, standing) = sender.ask(Some("another"));
    assert!(
        !standing.attached,
        "not to a terminal it is not attached to"
    );
    // And the asker is still from none: it is told by the kernel, not by
    // what it asks.
    assert_eq!(sender.ask(None).0, None);
}

#[test]
fn a_program_in_a_session_is_from_that_session() {
    let mut env = Env::new();
    let first = env.start("mail-in-a", &SH);
    let second = env.start("mail-in-b", &SH);
    let mut bridge = Bridge::start(&env);
    let mailbox = mailbox_of(&env, &bridge);
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{MAILBOX}={} {} --exact mail_sender_in_a_session --nocapture\n",
        mailbox.display(),
        exe.display()
    );

    // The app is at the second session only.
    bridge.attach(&second, 0);
    let mut shell = env.connect(&first);
    shell.attach(0);
    let said = said_in(&mut shell, &command);
    assert_eq!(said, format!("mail-reply session={first} attached=false"));

    // And once it is at the first as well.
    bridge.attach(&first, 0);
    let said = said_in(&mut shell, &command);
    assert_eq!(said, format!("mail-reply session={first} attached=true"));

    let mut other = env.connect(&second);
    other.attach(0);
    let said = said_in(&mut other, &command);
    assert_eq!(said, format!("mail-reply session={second} attached=true"));
}

#[test]
fn the_app_is_idle_since_it_attached_or_typed_and_a_bridge_just_started_has_just_been_used() {
    let mut env = Env::new();
    let id = env.start("mail-idle", &SH);
    let mut bridge = Bridge::start(&env);
    let mut sender = Sender::to(&mailbox_of(&env, &bridge));
    let wait = Duration::from_millis(600);

    // Not attached: the time since it used any session, which a bridge that
    // has just connected has done by connecting.
    std::thread::sleep(wait);
    let (_, standing) = sender.ask(Some(&id));
    assert!(!standing.attached);
    let idle = standing.idle;
    assert!(idle >= wait, "{idle:?}");

    // Attaching is a use.
    bridge.attach(&id, 0);
    let (_, standing) = sender.ask(Some(&id));
    assert!(standing.attached && standing.idle < idle, "{standing:?}");

    // Typing is a use.
    std::thread::sleep(wait);
    let (_, standing) = sender.ask(Some(&id));
    let idle = standing.idle;
    assert!(standing.attached && idle >= wait, "{idle:?}");
    bridge.type_in(&id, "echo typed-for-idle\n");
    bridge.wait_for(&id, "typed-for-idle\r\n");
    let (_, standing) = sender.ask(Some(&id));
    assert!(standing.idle < idle, "{standing:?} after {idle:?}");

    // Detached, the app is no longer at the terminal, and its last use is
    // the same.
    bridge.send(Some(&id), Message::Detach);
    bridge.status(&id);
    let (_, standing) = sender.ask(Some(&id));
    assert!(!standing.attached && standing.idle < idle, "{standing:?}");
}

#[test]
fn an_app_that_comes_back_through_a_new_bridge_has_been_used_more_lately_than_the_one_it_left() {
    let mut env = Env::new();
    let id = env.start("mail-back", &SH);
    let mut old = Bridge::start(&env);
    old.attach(&id, 0);
    std::thread::sleep(Duration::from_millis(700));

    // The connection of the old bridge is half dead: it is still there and
    // says it is attached. The app comes back through a new one.
    let mut new = Bridge::start(&env);
    new.attach(&id, 0);
    let (_, of_old) = Sender::to(&mailbox_of(&env, &old)).ask(Some(&id));
    let (_, of_new) = Sender::to(&mailbox_of(&env, &new)).ask(Some(&id));
    assert!(of_old.attached && of_new.attached);
    assert!(
        of_new.idle < of_old.idle,
        "new {:?}, old {:?}",
        of_new.idle,
        of_old.idle
    );
    assert_eq!(sockdir::mailboxes(&env.dir).unwrap().len(), 2);
}

#[test]
fn a_program_that_leaves_before_it_is_answered_does_no_harm() {
    let env = Env::new();
    let bridge = Bridge::start(&env);
    let mailbox = mailbox_of(&env, &bridge);
    for _ in 0..3 {
        let mut gone = Sender::to(&mailbox);
        gone.send(None, Message::Ask);
        drop(gone);
    }
    let mut sender = Sender::to(&mailbox);
    assert_eq!(sender.ask(None).0, None);
}

#[test]
fn what_a_mailbox_does_not_take_is_refused_and_the_connection_goes_on() {
    let env = Env::new();
    let bridge = Bridge::start(&env);
    let mut sender = Sender::to(&mailbox_of(&env, &bridge));

    sender.send(None, Message::Features);
    assert!(matches!(
        sender.next(),
        Some(Incoming::Session(Packet {
            message: Message::Error {
                code: ErrorCode::BAD_REQUEST,
                ..
            },
            ..
        }))
    ));

    // No mail is handed over yet: a conversation is refused, whoever opens it.
    let mut bytes = Vec::new();
    stream::Packet::new(1, stream::Message::open_mail(65536))
        .encode(Side::Client, &mut bytes)
        .unwrap();
    sender.sock.write_all(&bytes).unwrap();
    match sender.next() {
        Some(Incoming::Stream {
            from: Side::Server,
            packet,
        }) => {
            assert_eq!(packet.stream, 1);
            assert!(
                matches!(
                    packet.message,
                    stream::Message::Failed {
                        code: FailCode::UNREACHABLE,
                        ..
                    }
                ),
                "{packet:?}"
            );
        }
        other => panic!("not a refusal: {other:?}"),
    }

    sender.send(None, Message::Ping(vec![4, 2]));
    assert!(matches!(
        sender.next(),
        Some(Incoming::Session(Packet {
            message: Message::Pong(bytes),
            ..
        })) if bytes == [4, 2]
    ));
}

#[test]
fn the_bridge_does_not_say_it_has_a_mailbox_yet() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    bridge.send(None, Message::Features);
    bridge.until("the features", |b| !b.features.is_empty());
    assert!(!bridge.features[0].mailbox);
}
