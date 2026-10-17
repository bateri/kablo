//! The bridge's mailbox, with real bridges and real sessions: a program on the
//! machine connects to it, asks where the app the bridge carries stands, and
//! hands it bytes. The app is a test with a connection to the bridge, as the
//! app is a program with one to `kablo attach`.
//!
//! The program is this test, or this test's binary run again inside a session
//! (`mail_sender_in_a_session`, which does nothing unless it is told where the
//! mailbox is): the terminal an answer or a ticket names is found by the
//! kernel's word on the process that connected, so a sender has to be in the
//! terminal to be found there.

// Test support: a failure here is a failed test.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{Bridge, Env, PATIENCE, signal};
use kablo::sockdir;
use kablo_proto::v1::stream::{self, FailCode, Incoming, IncomingDecoder, Side};
use kablo_proto::v1::{ErrorCode, Message, Packet, Standing};
use kablo_proto::{SessionId, WIRE};

const SH: [&str; 2] = ["--", "/bin/sh"];

/// The variable that tells `mail_sender_in_a_session` where the mailbox is.
const MAILBOX: &str = "KABLO_TEST_MAILBOX";

/// What a sender has seen of one of its streams.
#[derive(Default)]
struct Got {
    /// The window the bridge offered in `opened`: what the sender may send.
    opened: Option<u32>,
    failed: Option<(FailCode, String)>,
    data: Vec<u8>,
    eof: bool,
    closed: bool,
    /// Bytes of credit received.
    credit: u64,
    /// Bytes sent so far.
    sent: u64,
    /// Whether received bytes are held back (no credit is given for them).
    hold: bool,
}

/// A program connected to a mailbox.
struct Sender {
    sock: UnixStream,
    decoder: IncomingDecoder,
    streams: HashMap<u32, Got>,
    next_stream: u32,
    /// The connection is over.
    ended: bool,
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
            streams: HashMap::new(),
            next_stream: 0,
            ended: false,
        }
    }

    /// Opens a stream to hand bytes over, giving `window`, and returns its
    /// number.
    fn open(&mut self, window: u32) -> u32 {
        self.next_stream += 1;
        let id = self.next_stream;
        self.stream_message(id, stream::Message::open_mail(window));
        id
    }

    fn stream_message(&mut self, id: u32, message: stream::Message) {
        let mut bytes = Vec::new();
        stream::Packet::new(id, message)
            .encode(Side::Client, &mut bytes)
            .unwrap();
        self.sock.write_all(&bytes).unwrap();
    }

    /// Sends bytes on a stream, in `data` messages of at most 32 KiB. The
    /// caller keeps within the window.
    fn send_data(&mut self, id: u32, bytes: &[u8]) {
        let mut wire = Vec::new();
        for chunk in bytes.chunks(32 * 1024) {
            stream::Packet::new(id, stream::Message::Data(chunk.to_vec()))
                .encode(Side::Client, &mut wire)
                .unwrap();
        }
        self.sock.write_all(&wire).unwrap();
        self.got(id).sent += bytes.len() as u64;
    }

    fn got(&mut self, id: u32) -> &mut Got {
        self.streams.entry(id).or_default()
    }

    /// How many bytes the sender may send on `id` now.
    fn allowed(&mut self, id: u32) -> u64 {
        let got = self.got(id);
        (u64::from(got.opened.unwrap_or(0)) + got.credit).saturating_sub(got.sent)
    }

    /// Reads for up to `wait` and files what arrived.
    fn pump(&mut self, wait: Duration) {
        // A socket the other end has closed may refuse a timeout; the read
        // then says the same.
        let _ = self.sock.set_read_timeout(Some(wait));
        let mut buf = vec![0u8; 64 * 1024];
        match self.sock.read(&mut buf) {
            Ok(0) => self.ended = true,
            Ok(n) => self.decoder.push(&buf[..n]),
            Err(e) if common::nothing_yet(&e) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => self.ended = true,
            Err(e) => panic!("the mailbox does not answer: {e}"),
        }
        let mut credits = Vec::new();
        while let Some(incoming) = self.decoder.next_incoming().unwrap() {
            let Incoming::Stream {
                from: Side::Server,
                packet,
            } = incoming
            else {
                continue;
            };
            let id = packet.stream;
            let got = self.got(id);
            match packet.message {
                stream::Message::Opened { window } => got.opened = Some(window),
                stream::Message::Failed { code, text } => got.failed = Some((code, text)),
                stream::Message::Data(bytes) => {
                    got.data.extend_from_slice(&bytes);
                    if !got.hold {
                        credits.push((id, bytes.len() as u32));
                    }
                }
                stream::Message::Credit(bytes) => got.credit += u64::from(bytes),
                stream::Message::Eof => got.eof = true,
                stream::Message::Close => got.closed = true,
                other => panic!("the mailbox sent {other:?}"),
            }
        }
        for (id, bytes) in credits {
            self.stream_message(id, stream::Message::Credit(bytes));
        }
    }

    /// Pumps until `done` holds or the patience is out.
    fn until(&mut self, what: &str, done: impl Fn(&mut Sender) -> bool) {
        let start = Instant::now();
        loop {
            self.pump(Duration::from_millis(20));
            if done(self) {
                return;
            }
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {what}");
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
        self.sock.set_read_timeout(Some(PATIENCE)).unwrap();
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

    // An app that has not said it takes tickets is handed nothing: the
    // conversation is refused.
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
fn the_bridge_says_it_has_a_mailbox_and_that_it_takes_what_the_app_says_it_takes() {
    let env = Env::new();
    let mut bridge = Bridge::start(&env);
    bridge.send(None, Message::Features);
    bridge.until("the features", |b| !b.features.is_empty());
    assert!(bridge.features[0].mailbox);

    // Asking says nothing about what the app takes; saying so does.
    let mut sender = Sender::to(&mailbox_of(&env, &bridge));
    assert!(!sender.ask(None).1.tickets);
    bridge.take_tickets();
    assert!(sender.ask(None).1.tickets);
    bridge.send(None, Message::Features);
    bridge.until("the features", |b| b.features.len() == 3);
    assert!(sender.ask(None).1.tickets, "a question is not a statement");
}

// --- handing bytes over ------------------------------------------------------

/// A sender opens a stream with `to_sender` as its window and the app answers
/// the ticket with `to_app`. Returns the sender's stream, the app's and the
/// terminal the ticket named, once both ends are open.
fn hand_over(
    app: &mut Bridge,
    sender: &mut Sender,
    to_sender: u32,
    to_app: u32,
) -> (u32, u32, Option<String>) {
    let seen = app.tickets.len();
    let s = sender.open(to_sender);
    app.until("a ticket", |b| b.tickets.len() > seen);
    let (terminal, ticket) = app.tickets[seen].clone();
    let a = app.answer_ticket(ticket, to_app);
    app.until("the app's stream to open", |b| {
        b.flows
            .get(&a)
            .is_some_and(|f| f.opened.is_some() || f.failed.is_some())
    });
    sender.until("the sender's stream to open", |x| {
        x.streams
            .get(&s)
            .is_some_and(|g| g.opened.is_some() || g.failed.is_some())
    });
    (s, a, terminal)
}

/// An app that takes tickets, and a sender connected to its bridge.
fn app_and_sender(env: &Env) -> (Bridge, Sender) {
    let mut app = Bridge::start(env);
    app.take_tickets();
    let sender = Sender::to(&mailbox_of(env, &app));
    (app, sender)
}

#[test]
fn what_a_program_hands_over_reaches_the_app_as_it_is_and_what_the_app_says_goes_back() {
    let env = Env::new();
    let (mut app, mut sender) = app_and_sender(&env);

    let s = sender.open(1000);
    app.until("the ticket", |b| !b.tickets.is_empty());
    assert_eq!(
        app.tickets,
        [(None, 1)],
        "from no terminal, and the first ticket"
    );
    let a = app.answer_ticket(1, 2000);
    app.until("the app's stream to open", |b| {
        b.flows.get(&a).is_some_and(|f| f.opened.is_some())
    });
    sender.until("the sender's stream to open", |x| {
        x.streams.get(&s).is_some_and(|g| g.opened.is_some())
    });
    // Each end is told the window the other gave.
    assert_eq!(app.flows[&a].opened, Some(1000));
    assert_eq!(sender.streams[&s].opened, Some(2000));

    let up: Vec<u8> = (0..=255).chain(0..100).collect();
    sender.send_data(s, &up);
    app.until("the bytes", |b| b.flows[&a].data.len() == up.len());
    assert_eq!(app.flows[&a].data, up);
    app.stream_send(a, b"and back\r\n\x1b[0m", 5);
    sender.until("the answer", |x| x.streams[&s].data.len() == 14);
    assert_eq!(sender.streams[&s].data, b"and back\r\n\x1b[0m");

    // One end is done sending and the other goes on.
    sender.stream_message(s, stream::Message::Eof);
    app.until("the eof", |b| b.flows[&a].eof);
    app.stream_send(a, b"more", 4);
    sender.until("more", |x| x.streams[&s].data.len() == 18);
    app.stream_eof(a);
    sender.until("the eof", |x| x.streams[&s].eof);
    assert!(!sender.streams[&s].closed && !app.flows[&a].closed);
}

#[test]
fn the_app_closing_is_the_end_of_the_conversation_for_the_program_and_the_programs_for_the_app() {
    let env = Env::new();
    let (mut app, mut sender) = app_and_sender(&env);

    let (s, a, _) = hand_over(&mut app, &mut sender, 1000, 1000);
    app.stream_close(a);
    sender.until("the close", |x| x.streams[&s].closed);
    assert_eq!(sender.streams[&s].failed, None);

    // And the other way, on a stream that has seen data.
    let (s, a, _) = hand_over(&mut app, &mut sender, 1000, 1000);
    sender.send_data(s, b"x");
    app.until("the byte", |b| b.flows[&a].data.len() == 1);
    sender.stream_message(s, stream::Message::Close);
    app.until("the close", |b| b.flows[&a].closed);
}

#[test]
fn a_ticket_the_app_declines_fails_the_open_with_the_apps_words() {
    let env = Env::new();
    let (mut app, mut sender) = app_and_sender(&env);

    let s = sender.open(1000);
    app.until("the ticket", |b| !b.tickets.is_empty());
    app.send(
        None,
        Message::Decline {
            ticket: 1,
            text: "not at the moment".to_owned(),
        },
    );
    sender.until("the failure", |x| x.streams[&s].failed.is_some());
    assert_eq!(
        sender.streams[&s].failed,
        Some((FailCode::DECLINED, "not at the moment".to_owned()))
    );
    // The ticket is spent.
    let a = app.answer_ticket(1, 1000);
    app.until("the failure", |b| b.flows[&a].failed.is_some());
    assert_eq!(
        app.flows[&a].failed.as_ref().unwrap().0,
        FailCode::BAD_REQUEST
    );
    assert!(!sender.streams[&s].closed);
}

#[test]
fn a_ticket_that_is_not_outstanding_is_a_failed_open() {
    let env = Env::new();
    let (mut app, mut sender) = app_and_sender(&env);

    // One that was never given.
    let a = app.answer_ticket(99, 1000);
    app.until("the failure", |b| b.flows[&a].failed.is_some());
    assert_eq!(
        app.flows[&a].failed.as_ref().unwrap().0,
        FailCode::BAD_REQUEST
    );

    // One the program gave up before it was answered.
    let s = sender.open(1000);
    app.until("the ticket", |b| !b.tickets.is_empty());
    sender.stream_message(s, stream::Message::Close);
    sender.send(None, Message::Ping(vec![1]));
    assert!(matches!(
        sender.next(),
        Some(Incoming::Session(Packet {
            message: Message::Pong(_),
            ..
        }))
    ));
    let late = app.answer_ticket(1, 1000);
    app.until("the failure", |b| b.flows[&late].failed.is_some());
    assert_eq!(
        app.flows[&late].failed.as_ref().unwrap().0,
        FailCode::BAD_REQUEST
    );

    // And one that was answered.
    let (_, a, _) = hand_over(&mut app, &mut sender, 1000, 1000);
    let twice = app.answer_ticket(2, 1000);
    app.until("the failure", |b| b.flows[&twice].failed.is_some());
    assert!(app.flows[&a].failed.is_none() && !app.flows[&a].closed);
}

#[test]
fn what_is_not_mail_is_refused_on_a_mailbox_that_takes_mail() {
    let env = Env::new();
    let (mut app, mut sender) = app_and_sender(&env);

    sender.next_stream += 1;
    let target = sender.next_stream;
    sender.stream_message(
        target,
        stream::Message::Open {
            window: 1000,
            port: 80,
            host: "localhost".to_owned(),
        },
    );
    sender.until("the failure", |x| x.streams[&target].failed.is_some());
    assert_eq!(
        sender.streams[&target].failed.as_ref().unwrap().0,
        FailCode::BAD_REQUEST
    );
    // A window nobody takes.
    let big = sender.open(stream::MAX_WINDOW + 1);
    sender.until("the failure", |x| x.streams[&big].failed.is_some());
    assert_eq!(
        sender.streams[&big].failed.as_ref().unwrap().0,
        FailCode::LIMIT
    );
    assert!(app.tickets.is_empty(), "the app was asked nothing");
    app.pump(Duration::from_millis(50));
    assert!(app.tickets.is_empty());
}

/// The program `mail_open_in_a_session` runs in a session's terminal: opens a
/// stream on the mailbox it is told of and says what became of it. Run by
/// itself, as a test, it does nothing.
#[test]
fn mail_open_in_a_session() {
    let Some(mailbox) = std::env::var_os(MAILBOX) else {
        return;
    };
    let mut sender = Sender::to(Path::new(&mailbox));
    let s = sender.open(4096);
    sender.until("an answer", |x| {
        x.streams
            .get(&s)
            .is_some_and(|g| g.opened.is_some() || g.failed.is_some())
    });
    let got = &sender.streams[&s];
    println!(
        "mail-open opened={} failed={}",
        got.opened.is_some(),
        got.failed.is_some()
    );
}

#[test]
fn a_ticket_names_the_terminal_the_program_runs_in_and_none_for_one_that_runs_in_none() {
    let mut env = Env::new();
    let id = env.start("mail-ticket", &SH);
    let (mut app, mut outside) = app_and_sender(&env);
    let mailbox = mailbox_of(&env, &app);
    let exe = std::env::current_exe().unwrap();
    let command = format!(
        "{MAILBOX}={} {} --exact mail_open_in_a_session --nocapture\n",
        mailbox.display(),
        exe.display()
    );
    let mut shell = env.connect(&id);
    shell.attach(0);
    let before = shell.data.len();
    shell.type_in(&command);

    // The program in the terminal: its ticket names the terminal.
    app.until("the ticket", |b| !b.tickets.is_empty());
    assert_eq!(app.tickets[0].0.as_deref(), Some(id.as_str()));
    let a = app.answer_ticket(app.tickets[0].1, 1000);
    app.until("the stream", |b| b.flows[&a].opened.is_some());
    shell.until("the program to hear of it", |c| {
        String::from_utf8_lossy(c.data.get(before..).unwrap_or_default())
            .contains("mail-open opened=true")
    });

    // This test is in no terminal of the bridge's.
    let (_, _, terminal) = hand_over(&mut app, &mut outside, 1000, 1000);
    assert_eq!(terminal, None);
}

#[test]
fn when_a_program_goes_away_the_app_is_told_and_when_the_app_does_the_programs_are_not() {
    let env = Env::new();
    let (mut app, mut sender) = app_and_sender(&env);
    let mut other = Sender::to(&mailbox_of(&env, &app));
    let (_, a, _) = hand_over(&mut app, &mut sender, 1000, 1000);
    let (os, oa, _) = hand_over(&mut app, &mut other, 1000, 1000);

    // The program leaves, with a conversation open and one asked of a ticket.
    drop(sender);
    app.until("the close", |b| b.flows[&a].closed);
    assert!(!app.flows[&oa].closed, "the other conversation is its own");

    // The app leaves: the bridge ends with it, and the program sees its
    // connection end, with no `close` on the stream.
    let mailbox = mailbox_of(&env, &app);
    app.hang_up();
    assert!(app.wait_exit().success());
    other.until("the end of the connection", |x| x.ended);
    assert!(!other.streams[&os].closed, "the connection ended, no more");
    assert!(!mailbox.exists());
}

#[test]
fn mail_in_both_directions_comes_whole_when_it_is_megabytes() {
    let env = Env::new();
    let (mut app, mut sender) = app_and_sender(&env);
    // Windows far under what is sent, so that credit is what keeps it going.
    let (s, a, _) = hand_over(&mut app, &mut sender, 256 * 1024, 128 * 1024);

    let up = common::pattern(5 * 1024 * 1024, 3);
    let down = common::pattern(6 * 1024 * 1024 + 17, 9);
    let (mut up_sent, mut down_sent) = (0usize, 0usize);
    let start = Instant::now();
    while app.flows[&a].total < up.len() as u64 || sender.streams[&s].data.len() < down.len() {
        assert!(start.elapsed() < PATIENCE, "timed out");
        let room = (sender.allowed(s) as usize)
            .min(up.len() - up_sent)
            .min(96 * 1024);
        if room > 0 {
            sender.send_data(s, &up[up_sent..up_sent + room]);
            up_sent += room;
        }
        let flow = app.flow(a);
        let room = ((u64::from(flow.opened.unwrap()) + flow.credit).saturating_sub(flow.sent)
            as usize)
            .min(down.len() - down_sent)
            .min(96 * 1024);
        if room > 0 {
            app.stream_send(a, &down[down_sent..down_sent + room], 8 * 1024);
            down_sent += room;
        }
        sender.pump(Duration::from_millis(1));
        app.pump(Duration::from_millis(1));
    }
    assert!(app.flows[&a].data == up, "what the program sent");
    assert!(sender.streams[&s].data == down, "what the app sent");
}

#[test]
fn a_program_that_does_not_read_holds_up_nothing_else_and_nothing_past_its_window() {
    let mut env = Env::new();
    let id = env.start("mail-slow", &SH);
    let (mut app, mut slow) = app_and_sender(&env);
    let mut brisk = Sender::to(&mailbox_of(&env, &app));
    let window = 1024 * 1024;
    let (s, a, _) = hand_over(&mut app, &mut slow, window, 4096);
    let (bs, ba, _) = hand_over(&mut app, &mut brisk, 4096, 4096);

    // The app sends the slow program a whole window, which it does not read:
    // far more than a connection that does not read its answers is let leave.
    slow.got(s).hold = true;
    app.stream_send(a, &common::pattern(window as usize, 5), 32 * 1024);
    app.attach(&id, 0);
    app.type_in(&id, "echo reached-the-app\n");
    app.wait_for(&id, "reached-the-app\r\n");

    // The rest of the bridge goes on: the other conversation, both ways.
    brisk.send_data(bs, b"quick");
    app.until("the bytes", |b| b.flows[&ba].data == b"quick");
    app.stream_send(ba, b"quick back", 100);
    brisk.until("the answer", |x| x.streams[&bs].data == b"quick back");

    // The slow program was not closed for it, and gets all of it when it reads.
    slow.streams.get_mut(&s).unwrap().hold = false;
    slow.until("all of it", |x| x.streams[&s].data.len() == window as usize);
    assert!(slow.streams[&s].data == common::pattern(window as usize, 5));

    // The app that sends more than it was given loses the conversation.
    let (s2, a2, _) = hand_over(&mut app, &mut slow, 1000, 4096);
    app.stream_send(a2, &[0u8; 1001], 1001);
    app.until("the close", |b| b.flows[&a2].closed);
    slow.until("the close", |x| x.streams[&s2].closed);
    assert!(!slow.ended, "the connection stays");
    let _ = a;
}

#[test]
fn conversations_and_forwarded_connections_share_the_numbers_of_the_streams() {
    let env = Env::new();
    let (mut app, mut sender) = app_and_sender(&env);
    let port = common::target(common::echo);

    let (first, _) = app.connect_stream("127.0.0.1", port, 65536).unwrap();
    let (s, a, _) = hand_over(&mut app, &mut sender, 4096, 4096);
    let (second, _) = app.connect_stream("127.0.0.1", port, 65536).unwrap();
    assert!(first < a && a < second);

    app.stream_send(first, b"one", 3);
    app.stream_send(second, b"two", 3);
    sender.send_data(s, b"mail");
    app.until("everything", |b| {
        b.flows[&first].data == b"one"
            && b.flows[&second].data == b"two"
            && b.flows[&a].data == b"mail"
    });
}
