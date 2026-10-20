//! The bytes of version 1, message by message.
//!
//! The files in `fixtures/v1` are the format: annotated hex, written by hand
//! from the description in `docs/PROTOCOL.md`, not produced by the codec. Each
//! test here reads them and compares in **both** directions (the codec's
//! output against the file, and the file's meaning against the expected
//! message), because writing then reading back would pass on a codec that is
//! consistently wrong. A fixture is never edited once a version has shipped; a
//! change of format is `fixtures/v2` and a second module.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use kablo_proto::frame::{self, RawFrame};
use kablo_proto::hello::Reply;
use kablo_proto::v1::stream::{
    self, Command, FailCode, Incoming, IncomingDecoder, Side, read_incoming,
};
use kablo_proto::v1::{
    self, Capture, ClientName, Exit, Features, Message, NewSession, Offers, Packet, PacketDecoder,
    Size, Standing, State, Status, Stroke, TerminalState, View, key, kind, read_packet,
    write_packet,
};
use kablo_proto::{CHANNEL_SESSION, CHANNEL_STREAM, Error, SessionId, WIRE, tlv};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/v1")
}

/// Hex bytes, whitespace-separated, with `#` starting a comment.
fn parse_hex(text: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for line in text.lines() {
        for token in line.split('#').next().unwrap().split_whitespace() {
            assert_eq!(token.len(), 2, "{token:?}");
            out.push(u8::from_str_radix(token, 16).unwrap());
        }
    }
    out
}

fn fixture(name: &str) -> Vec<u8> {
    let path = dir().join(format!("{name}.hex"));
    parse_hex(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display())))
}

fn id(s: &str) -> SessionId {
    SessionId::new(s).unwrap()
}

fn work(message: Message) -> Packet {
    Packet::about(id("work"), message)
}

/// `value` after `edit` has set what it should. `Features`, `Status` and
/// `NewSession` are `#[non_exhaustive]`: a crate other than `kablo-proto` makes
/// one with its constructor and sets the fields, never with a struct expression.
fn with<T>(mut value: T, edit: impl FnOnce(&mut T)) -> T {
    edit(&mut value);
    value
}

fn running_work() -> Status {
    with(Status::new(id("work")), |s| {
        s.pid = 4242;
        s.program = Some(b"vim".to_vec());
        s.cwd = Some(b"/home/u/src".to_vec());
        s.size = Size {
            cols: 120,
            rows: 40,
            xpixel: 960,
            ypixel: 800,
        };
        s.clients = 2;
        s.state = State::RUNNING;
        s.offset = 123_456;
        s.retained_from = 100;
        s.exit = None;
        s.version = "0.1.0".into();
        s.warden_version = String::new();
        s.secret = false;
        s.remote = None;
        s.remote_port = None;
        s.label = None;
    })
}

/// An ssh in the foreground, asking for a password: the state the `state`
/// and `status_reply_terminal` fixtures hold.
fn asking_for_a_password() -> TerminalState {
    terminal(|state| {
        state.program = Some(b"ssh".to_vec());
        state.cwd = Some(b"/home/u/src".to_vec());
        state.secret = true;
        state.remote = Some(b"user@host".to_vec());
        state.remote_port = Some(2222);
    })
}

/// A terminal state with nothing said, then what `set` says (the type cannot
/// be written as a struct expression outside its crate).
fn terminal(set: impl FnOnce(&mut TerminalState)) -> TerminalState {
    let mut state = TerminalState::default();
    set(&mut state);
    state
}

/// A view of a terminal of `size`, with the two pictures' flags as given.
fn viewing(size: Size, picture: bool, picture_if_answered: bool) -> View {
    let mut view = View::new(size);
    view.picture = picture;
    view.picture_if_answered = picture_if_answered;
    view
}

/// A view of a terminal of `size` that asks for the session to be kept after
/// its end is told, and for nothing else.
fn keeping(size: Size) -> View {
    let mut view = View::new(size);
    view.keep_after_end = true;
    view
}

/// A view of a terminal of `size` whose client calls itself `name`.
fn named(size: Size, name: &str) -> View {
    let mut view = View::new(size);
    view.name = ClientName::new(name);
    view
}

/// A command with all three parts said.
fn whole_command(argv: Vec<Vec<u8>>, env: Vec<Vec<u8>>, cwd: Option<Vec<u8>>) -> Command {
    let mut command = Command::new(argv);
    command.env = env;
    command.cwd = cwd;
    command
}

fn broken_build() -> Status {
    with(Status::new(id("build-1")), |s| {
        s.pid = 4300;
        s.size = Size {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        };
        s.state = State::BROKEN;
        s.offset = 77;
        s.version = "0.1.0".into();
    })
}

/// Every message fixture and what it says.
fn messages() -> Vec<(&'static str, Packet)> {
    let full_new = with(NewSession::default(), |n| {
        n.argv = vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"echo hi".to_vec()];
        n.env = vec![
            (b"TERM".to_vec(), b"xterm-256color".to_vec()),
            (b"FOO".to_vec(), b"a=b".to_vec()),
        ];
        n.cwd = Some(b"/tmp".to_vec());
        n.size = Some(Size {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        });
        n.journal_size = Some(1 << 20);
        n.history = None;
    });
    vec![
        ("attach", work(Message::attach(0x1000))),
        ("attach_end", work(Message::attach(v1::OFFSET_END))),
        (
            "attach_view",
            work(Message::Attach {
                offset: 0x1000,
                view: Some(viewing(
                    Size {
                        cols: 120,
                        rows: 40,
                        xpixel: 960,
                        ypixel: 800,
                    },
                    true,
                    false,
                )),
            }),
        ),
        (
            "attach_view_answered",
            work(Message::Attach {
                offset: 0x1000,
                view: Some(viewing(
                    Size {
                        cols: 120,
                        rows: 40,
                        xpixel: 960,
                        ypixel: 800,
                    },
                    false,
                    true,
                )),
            }),
        ),
        (
            "attach_view_keep",
            work(Message::Attach {
                offset: 0x1000,
                view: Some(keeping(Size {
                    cols: 120,
                    rows: 40,
                    xpixel: 960,
                    ypixel: 800,
                })),
            }),
        ),
        (
            "attach_view_named",
            work(Message::Attach {
                offset: 0x1000,
                view: Some(named(
                    Size {
                        cols: 120,
                        rows: 40,
                        xpixel: 960,
                        ypixel: 800,
                    },
                    "MacBook",
                )),
            }),
        ),
        ("detach", work(Message::Detach)),
        (
            "resize",
            work(Message::Resize(Size {
                cols: 120,
                rows: 40,
                xpixel: 960,
                ypixel: 800,
            })),
        ),
        ("terminate", work(Message::Terminate { force: false })),
        ("terminate_force", work(Message::Terminate { force: true })),
        (
            "upgrade",
            work(Message::Upgrade {
                path: b"/usr/local/bin/kablo".to_vec(),
            }),
        ),
        ("status", work(Message::Status)),
        ("input", work(Message::Input(b"ls\r".to_vec()))),
        ("list", Packet::connection(Message::List)),
        (
            "new_empty",
            Packet::connection(Message::New(NewSession::default())),
        ),
        (
            "new_full",
            Packet::about(id("build-1"), Message::New(full_new)),
        ),
        (
            "new_history",
            Packet::about(
                id("build-1"),
                Message::New(with(NewSession::default(), |n| {
                    n.argv = vec![b"/bin/sh".to_vec()];
                    n.history = Some(5000);
                })),
            ),
        ),
        (
            "ping",
            Packet::connection(Message::Ping(b"pingdata".to_vec())),
        ),
        (
            "attached",
            work(Message::Attached {
                offset: 0x1000,
                truncated: false,
                answered: 0,
            }),
        ),
        (
            "attached_truncated",
            work(Message::Attached {
                offset: 0x2000,
                truncated: true,
                answered: 0,
            }),
        ),
        (
            "attached_answered",
            work(Message::Attached {
                offset: 0x3000,
                truncated: false,
                answered: 0x2000,
            }),
        ),
        (
            "output",
            work(Message::Output {
                offset: 0x1000,
                data: b"hello\r\n".to_vec(),
            }),
        ),
        (
            "picture",
            work(Message::Picture {
                offset: 0x1000,
                cols: 80,
                rows: 24,
                more: false,
                data: b"\x1b[Hhi".to_vec(),
            }),
        ),
        (
            "picture_more",
            work(Message::Picture {
                offset: 0x1000,
                cols: 80,
                rows: 24,
                more: true,
                data: b"hi".to_vec(),
            }),
        ),
        ("ended_code", work(Message::Ended(Exit::Code(3)))),
        ("ended_signal", work(Message::Ended(Exit::Signal(9)))),
        (
            "error",
            Packet::about(
                id("nope"),
                Message::Error {
                    code: v1::ErrorCode::UNKNOWN_SESSION,
                    text: "no such session".into(),
                },
            ),
        ),
        (
            "error_input_full",
            work(Message::Error {
                code: v1::ErrorCode::INPUT_FULL,
                text: "the program is not taking input".into(),
            }),
        ),
        (
            "error_ended",
            work(Message::Error {
                code: v1::ErrorCode::ENDED,
                text: "the program has ended".into(),
            }),
        ),
        (
            "pong",
            Packet::connection(Message::Pong(b"pingdata".to_vec())),
        ),
        ("features", Packet::connection(Message::Features)),
        (
            "features_reply",
            Packet::connection(Message::FeaturesReply(Features::with_streams())),
        ),
        (
            "features_reply_off",
            Packet::connection(Message::FeaturesReply(Features::streams_off(
                "turned off by the server's administrator",
            ))),
        ),
        (
            "features_reply_commands",
            Packet::connection(Message::FeaturesReply(
                Features::with_streams().with_commands(),
            )),
        ),
        (
            "features_reply_off_commands",
            Packet::connection(Message::FeaturesReply(
                Features::streams_off("turned off by the server's administrator").with_commands(),
            )),
        ),
        (
            "features_reply_mailbox",
            Packet::connection(Message::FeaturesReply(
                Features::with_streams().with_commands().with_mailbox(),
            )),
        ),
        (
            "features_tickets",
            Packet::connection(Message::FeaturesWith(Offers::with_tickets())),
        ),
        ("ask", Packet::connection(Message::Ask)),
        ("ask_terminal", work(Message::Ask)),
        (
            "ask_reply",
            work(Message::AskReply(
                Standing::idle(Duration::from_millis(1500))
                    .with_attached()
                    .with_tickets(),
            )),
        ),
        (
            "ask_reply_none",
            Packet::connection(Message::AskReply(Standing::idle(Duration::from_millis(
                3_600_000,
            )))),
        ),
        ("ticket", work(Message::Ticket(7))),
        ("ticket_none", Packet::connection(Message::Ticket(8))),
        (
            "decline",
            Packet::connection(Message::Decline {
                ticket: 7,
                text: "no room".into(),
            }),
        ),
        (
            "write",
            work(Message::Write(vec![
                Stroke::text("echo hi"),
                Stroke::key(key::ENTER, 0),
            ])),
        ),
        (
            "write_keys",
            work(Message::Write(vec![
                Stroke::key(u32::from('c'), key::mods::CTRL),
                Stroke::key(key::UP, 0),
                Stroke::key(key::function(5).unwrap(), key::mods::SHIFT | key::mods::ALT),
            ])),
        ),
        ("read", work(Message::Read(Capture::default()))),
        (
            "read_history",
            work(Message::Read(with(Capture::default(), |c| {
                c.history = 500;
                c.unwrap = true;
            }))),
        ),
        (
            "read_picture",
            work(Message::Read(with(Capture::default(), |c| {
                c.picture = true;
            }))),
        ),
        ("wait", work(Message::Wait)),
        ("label", work(Message::Label("deploy".into()))),
        ("label_clear", work(Message::Label(String::new()))),
        (
            "screen_text",
            work(Message::ScreenText {
                offset: 0x1000,
                cols: 80,
                rows: 24,
                more: false,
                data: b"$ echo hi\nhi\n".to_vec(),
            }),
        ),
        (
            "screen_text_more",
            work(Message::ScreenText {
                offset: 0x1000,
                cols: 80,
                rows: 24,
                more: true,
                data: b"$ echo h".to_vec(),
            }),
        ),
        (
            "new_label",
            Packet::about(
                id("build-1"),
                Message::New(with(NewSession::default(), |n| {
                    n.label = Some("tests".into());
                })),
            ),
        ),
        (
            "status_reply_label",
            work(Message::StatusReply(with(Status::new(id("work")), |s| {
                s.pid = 4242;
                s.size = Size {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                };
                s.offset = 5000;
                s.label = Some("tests".into());
            }))),
        ),
        ("status_reply", work(Message::StatusReply(running_work()))),
        (
            "status_reply_clients",
            work(Message::StatusReply(with(Status::new(id("work")), |s| {
                s.pid = 4242;
                s.size = Size {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                };
                s.clients = 2;
                s.offset = 5000;
                s.clients_named = vec![ClientName::new("MacBook"), None];
            }))),
        ),
        (
            "status_reply_terminal",
            work(Message::StatusReply(with(Status::new(id("work")), |s| {
                s.pid = 4242;
                s.program = Some(b"ssh".to_vec());
                s.cwd = Some(b"/home/u".to_vec());
                s.size = Size {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                };
                s.clients = 1;
                s.offset = 5000;
                s.version = "0.2.0".into();
                s.secret = true;
                s.remote = Some(b"user@host".to_vec());
                s.remote_port = Some(2222);
            }))),
        ),
        (
            "status_reply_warden",
            work(Message::StatusReply(with(Status::new(id("work")), |s| {
                s.pid = 4242;
                s.size = Size {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                };
                s.offset = 5000;
                s.version = "0.2.0".into();
                s.warden_version = "0.1.0".into();
            }))),
        ),
        (
            "status_reply_times",
            work(Message::StatusReply(with(Status::new(id("work")), |s| {
                s.pid = 4242;
                s.size = Size {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                };
                s.offset = 5000;
                s.started = Some(1_760_000_000);
                s.front_started = Some(1_760_007_260);
                s.front_is_own = Some(false);
                s.output_at = Some(1_760_009_000);
            }))),
        ),
        (
            "status_reply_ended",
            work(Message::StatusReply(with(Status::new(id("work")), |s| {
                s.pid = 4242;
                s.size = Size {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                };
                s.state = State::ENDED;
                s.offset = 5000;
                s.exit = Some(Exit::Code(0));
            }))),
        ),
        (
            "state",
            work(Message::TerminalState(asking_for_a_password())),
        ),
        (
            "state_empty",
            work(Message::TerminalState(TerminalState::default())),
        ),
        (
            "list_reply_empty",
            Packet::connection(Message::ListReply(Vec::new())),
        ),
        (
            "list_reply",
            Packet::connection(Message::ListReply(vec![running_work(), broken_build()])),
        ),
        (
            "created",
            Packet::about(
                id("build-1"),
                Message::Created(with(Status::new(id("build-1")), |s| {
                    s.pid = 4300;
                    s.size = Size {
                        cols: 80,
                        rows: 24,
                        xpixel: 0,
                        ypixel: 0,
                    };
                    s.version = "0.1.0".into();
                })),
            ),
        ),
    ]
}

/// Every fixture of channel 1: the end that sends it, and what it says.
fn streams() -> Vec<(&'static str, Side, stream::Packet)> {
    use stream::Message as S;
    let on = stream::Packet::new;
    vec![
        (
            "stream_open",
            Side::Client,
            on(
                1,
                S::Open {
                    window: 262_144,
                    port: 3000,
                    host: "localhost".into(),
                },
            ),
        ),
        (
            "stream_opened",
            Side::Server,
            on(1, S::Opened { window: 65_536 }),
        ),
        (
            "stream_failed",
            Side::Server,
            on(
                1,
                S::Failed {
                    code: FailCode::REFUSED,
                    text: "connection refused".into(),
                },
            ),
        ),
        (
            "stream_failed_forbidden",
            Side::Server,
            on(
                2,
                S::Failed {
                    code: FailCode::FORBIDDEN,
                    text: "turned off by the server's administrator".into(),
                },
            ),
        ),
        (
            "stream_client_data",
            Side::Client,
            on(1, S::Data(b"GET / HTTP/1.0\r\n\r\n".to_vec())),
        ),
        (
            "stream_server_data",
            Side::Server,
            on(1, S::Data(b"HTTP/1.0 200 OK\r\n".to_vec())),
        ),
        (
            "stream_client_credit",
            Side::Client,
            on(1, S::Credit(16_384)),
        ),
        (
            "stream_server_credit",
            Side::Server,
            on(1, S::Credit(32_768)),
        ),
        ("stream_client_eof", Side::Client, on(1, S::Eof)),
        ("stream_server_eof", Side::Server, on(1, S::Eof)),
        ("stream_client_close", Side::Client, on(1, S::Close)),
        ("stream_server_close", Side::Server, on(1, S::Close)),
        (
            "stream_open_command",
            Side::Client,
            on(
                3,
                S::OpenCommand {
                    window: 262_144,
                    command: tar_command(),
                },
            ),
        ),
        (
            "stream_open_command_argv",
            Side::Client,
            on(
                4,
                S::OpenCommand {
                    window: 65_536,
                    command: Command::new(vec![b"true".to_vec()]),
                },
            ),
        ),
        (
            "stream_server_stderr",
            Side::Server,
            on(
                3,
                S::Stderr(b"tar: src: No such file or directory\n".to_vec()),
            ),
        ),
        (
            "stream_exited_code",
            Side::Server,
            on(3, S::Exited(Exit::Code(2))),
        ),
        (
            "stream_exited_signal",
            Side::Server,
            on(3, S::Exited(Exit::Signal(9))),
        ),
        (
            "stream_failed_not_started",
            Side::Server,
            on(
                4,
                S::Failed {
                    code: FailCode::START_FAILED,
                    text: "No such file or directory".into(),
                },
            ),
        ),
        (
            "stream_open_ticket",
            Side::Client,
            on(
                2,
                S::OpenTicket {
                    window: 65_536,
                    ticket: 7,
                },
            ),
        ),
        (
            "stream_open_mail",
            Side::Client,
            on(1, S::open_mail(262_144)),
        ),
        (
            "stream_failed_declined",
            Side::Server,
            on(
                1,
                S::Failed {
                    code: FailCode::DECLINED,
                    text: "no room".into(),
                },
            ),
        ),
    ]
}

/// The command of the `stream_open_command` fixture.
fn tar_command() -> Command {
    whole_command(
        ["tar", "-cf", "-", "src"]
            .map(|a| a.as_bytes().to_vec())
            .to_vec(),
        vec![b"LC_ALL=C".to_vec(), b"FOO=a=b".to_vec()],
        Some(b"/home/u".to_vec()),
    )
}

/// The fixtures that are not messages.
const HANDSHAKE: [&str; 3] = ["hello", "reply", "reply_none"];

#[test]
fn every_message_is_its_fixture_byte_for_byte() {
    for (name, packet) in messages() {
        let bytes = fixture(name);
        let mut written = Vec::new();
        write_packet(&mut written, &packet).unwrap();
        assert_eq!(written, bytes, "{name}: what is written");
        let mut appended = vec![9];
        packet.encode(&mut appended).unwrap();
        assert_eq!(&appended[1..], bytes.as_slice(), "{name}: what is appended");

        let mut reader = bytes.as_slice();
        assert_eq!(
            read_packet(&mut reader).unwrap().as_ref(),
            Some(&packet),
            "{name}: what is read"
        );
        assert_eq!(
            read_packet(&mut reader).unwrap(),
            None,
            "{name}: nothing is left over"
        );

        let mut decoder = PacketDecoder::new();
        for byte in &bytes {
            decoder.push(std::slice::from_ref(byte));
        }
        assert_eq!(
            decoder.next_packet().unwrap().as_ref(),
            Some(&packet),
            "{name}: what is decoded"
        );
        assert_eq!(decoder.pending(), 0, "{name}");
    }
}

#[test]
fn the_directory_holds_exactly_the_fixtures_the_tests_name() {
    let named: BTreeSet<String> = messages()
        .iter()
        .map(|(n, _)| (*n).to_owned())
        .chain(streams().iter().map(|(n, ..)| (*n).to_owned()))
        .chain(HANDSHAKE.map(str::to_owned))
        .collect();
    assert_eq!(
        named.len(),
        messages().len() + streams().len() + HANDSHAKE.len(),
        "a name is listed twice"
    );
    let on_disk: BTreeSet<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .map(|f| {
            f.strip_suffix(".hex")
                .unwrap_or_else(|| panic!("{f} is not a fixture"))
                .to_owned()
        })
        .collect();
    assert_eq!(on_disk, named);
}

#[test]
fn every_message_kind_has_a_fixture() {
    let kinds: BTreeSet<u8> = messages().iter().map(|(_, p)| p.message.kind()).collect();
    use kind::*;
    for k in [
        ATTACH,
        DETACH,
        RESIZE,
        TERMINATE,
        UPGRADE,
        STATUS,
        INPUT,
        LIST,
        NEW,
        PING,
        FEATURES,
        ASK,
        DECLINE,
        WRITE,
        READ,
        WAIT,
        LABEL,
        ATTACHED,
        OUTPUT,
        ENDED,
        ERROR,
        PONG,
        STATUS_REPLY,
        LIST_REPLY,
        CREATED,
        PICTURE,
        FEATURES_REPLY,
        STATE,
        ASK_REPLY,
        TICKET,
        SCREEN_TEXT,
    ] {
        assert!(kinds.contains(&k), "kind {k:#04x} has no fixture");
    }
}

#[test]
fn the_handshake_fixtures() {
    let mut hello = Vec::new();
    WIRE.write_hello(&mut hello).unwrap();
    assert_eq!(hello, fixture("hello"));
    assert_eq!(
        WIRE.read_hello(&mut fixture("hello").as_slice()).unwrap(),
        (1, 1)
    );

    let mut reply = Vec::new();
    WIRE.write_reply(&mut reply, 1).unwrap();
    assert_eq!(reply, fixture("reply"));
    assert_eq!(
        WIRE.read_reply(&mut fixture("reply").as_slice()).unwrap(),
        Reply {
            selected: 1,
            min: 1,
            max: 1
        }
    );

    let mut none = Vec::new();
    WIRE.write_reply(&mut none, 0).unwrap();
    assert_eq!(none, fixture("reply_none"));
    assert_eq!(
        WIRE.read_reply(&mut fixture("reply_none").as_slice())
            .unwrap(),
        Reply {
            selected: 0,
            min: 1,
            max: 1
        }
    );
}

#[test]
fn a_whole_conversation_is_the_fixtures_one_after_another() {
    // Handshake, then every message, in one stream, with noise in front of the
    // reply as an ssh banner would put there.
    let mut stream = b"Last login: yesterday\r\n".to_vec();
    stream.extend(fixture("reply"));
    let all = messages();
    for (name, _) in &all {
        stream.extend(fixture(name));
    }

    let mut reader = stream.as_slice();
    assert_eq!(WIRE.client(&mut reader, &mut Vec::new()).unwrap(), 1);
    for (name, packet) in &all {
        assert_eq!(
            read_packet(&mut reader).unwrap().as_ref(),
            Some(packet),
            "{name}"
        );
    }
    assert_eq!(read_packet(&mut reader).unwrap(), None);

    // The same bytes from a reader that gets them in pieces.
    let framed = &stream[stream.len() - all.iter().map(|(n, _)| fixture(n).len()).sum::<usize>()..];
    for piece in [1, 5, 13, 4096] {
        let mut decoder = PacketDecoder::new();
        let mut got = Vec::new();
        for chunk in framed.chunks(piece) {
            decoder.push(chunk);
            while let Some(p) = decoder.next_packet().unwrap() {
                got.push(p);
            }
        }
        assert_eq!(
            got,
            all.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(),
            "pieces of {piece}"
        );
    }
}

fn raw(kind: u8, channel: u8, session: &str, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    frame::encode_frame(
        &mut out,
        &RawFrame {
            kind,
            channel,
            session: session.into(),
            payload: payload.to_vec(),
        },
    )
    .unwrap();
    out
}

#[test]
fn frames_of_an_unknown_kind_or_channel_are_skipped_by_their_length() {
    let mut stream = Vec::new();
    stream.extend(raw(0x7e, CHANNEL_SESSION, "work", &[1; 5000])); // a kind from the future
    stream.extend(raw(0xfe, CHANNEL_SESSION, "", &[])); // another, from the server's half
    stream.extend(raw(kind::DETACH, 1, "work", &[1, 2, 3])); // channel 1 is not this reader's: it reads sessions
    stream.extend(fixture("ping"));
    stream.extend(raw(0x7e, CHANNEL_SESSION, "", &[2; 10])); // and one trailing, to the end
    let ping = Packet::connection(Message::Ping(b"pingdata".to_vec()));
    let mut reader = stream.as_slice();
    assert_eq!(read_packet(&mut reader).unwrap(), Some(ping.clone()));
    assert_eq!(read_packet(&mut reader).unwrap(), None);
    let mut decoder = PacketDecoder::new();
    decoder.push(&stream);
    assert_eq!(decoder.next_packet().unwrap(), Some(ping));
    assert_eq!(decoder.next_packet().unwrap(), None);
    assert_eq!(decoder.pending(), 0);
}

#[test]
fn a_malformed_message_costs_that_message_and_nothing_after_it() {
    let mut stream = Vec::new();
    stream.extend(raw(kind::RESIZE, 0, "work", &[0, 1, 2])); // too short for a size
    stream.extend(raw(kind::ATTACH, 0, "bad id", &[0; 8])); // a space is not in the alphabet
    stream.extend(raw(kind::NEW, 0, "", &[0, 1, 0, 0, 0, 3, b'a', 0, b'b'])); // a NUL in an argument
    stream.extend(raw(kind::NEW, 0, "", &[0, 2, 0, 0, 0, 3, b'=', b'a', b'b'])); // a variable with no name
    stream.extend(raw(kind::NEW, 0, "", &[0, 1, 0, 0, 0, 9])); // an entry cut short
    stream.extend(raw(kind::UPGRADE, 0, "work", b"")); // no path
    stream.extend(raw(kind::ENDED, 0, "work", &[7, 0, 0, 0, 1])); // an exit kind that does not exist
    stream.extend(raw(kind::STATUS_REPLY, 0, "work", &[])); // no id inside
    stream.extend(fixture("detach"));
    let mut reader = stream.as_slice();
    for n in 0..8 {
        match read_packet(&mut reader) {
            Err(e) => assert!(e.is_recoverable(), "message {n}: {e}"),
            Ok(p) => panic!("message {n} read as {p:?}"),
        }
    }
    assert_eq!(
        read_packet(&mut reader).unwrap(),
        Some(work(Message::Detach))
    );
}

#[test]
fn a_sender_cannot_produce_what_a_receiver_refuses() {
    let nul =
        |new: NewSession| write_packet(&mut Vec::new(), &Packet::connection(Message::New(new)));
    assert!(
        nul(with(NewSession::default(), |n| {
            n.argv = vec![b"a\0b".to_vec()];
        }))
        .is_err()
    );
    assert!(
        nul(with(NewSession::default(), |n| {
            n.env = vec![(b"A=B".to_vec(), b"c".to_vec())];
        }))
        .is_err()
    );
    assert!(
        nul(with(NewSession::default(), |n| {
            n.env = vec![(b"".to_vec(), b"c".to_vec())];
        }))
        .is_err()
    );
    assert!(
        nul(with(NewSession::default(), |n| {
            n.cwd = Some(Vec::new());
        }))
        .is_err()
    );
    assert!(
        write_packet(
            &mut Vec::new(),
            &work(Message::Upgrade {
                path: vec![b'/'; 5000]
            })
        )
        .is_err()
    );
    let too_big = work(Message::Output {
        offset: 0,
        data: vec![0; v1::MAX_OUTPUT + 1],
    });
    assert!(matches!(
        write_packet(&mut Vec::new(), &too_big),
        Err(Error::PayloadTooLarge { .. })
    ));
    let biggest = work(Message::Output {
        offset: 0,
        data: vec![7; v1::MAX_OUTPUT],
    });
    let mut wire = Vec::new();
    write_packet(&mut wire, &biggest).unwrap();
    assert_eq!(read_packet(&mut wire.as_slice()).unwrap(), Some(biggest));
}

#[test]
fn a_reader_ignores_what_a_later_version_adds() {
    // A key it does not know in a status, between the known ones.
    let mut block = running_work().to_block();
    let after_id = 6 + "work".len();
    block.splice(after_id..after_id, [0x0f, 0xff, 0, 0, 0, 3, 9, 9, 9]);
    assert_eq!(Status::from_block(&block).unwrap(), running_work());

    // Fields appended to a fixed layout, and unknown flag bits.
    let mut stream = raw(
        kind::ATTACH,
        0,
        "work",
        &[0, 0, 0, 0, 0, 0, 0x10, 0, 0xde, 0xad],
    );
    stream.extend(raw(kind::TERMINATE, 0, "work", &[0b1111_1111, 0xaa]));
    stream.extend(raw(
        kind::ATTACHED,
        0,
        "work",
        &[0, 0, 0, 0, 0, 0, 0x20, 0, 0b1111_1110, 7],
    ));
    let mut reader = stream.as_slice();
    assert_eq!(
        read_packet(&mut reader).unwrap(),
        Some(work(Message::attach(0x1000)))
    );
    assert_eq!(
        read_packet(&mut reader).unwrap(),
        Some(work(Message::Terminate { force: true }))
    );
    assert_eq!(
        read_packet(&mut reader).unwrap(),
        Some(work(Message::Attached {
            offset: 0x2000,
            truncated: false,
            answered: 0
        }))
    );

    // A code and a state this version has no name for are kept as they came.
    let error = raw(kind::ERROR, 0, "work", &[0x12, 0x34]);
    let Some(Packet {
        message: Message::Error { code, text },
        ..
    }) = read_packet(&mut error.as_slice()).unwrap()
    else {
        panic!("not an error")
    };
    assert_eq!((code.0, text.as_str()), (0x1234, ""));
    let mut block = running_work().to_block();
    block.extend_from_slice(&[0, 7, 0, 0, 0, 1, 9]); // state 9, the last entry wins
    assert_eq!(Status::from_block(&block).unwrap().state, State(9));
}

#[test]
fn an_attach_is_the_one_it_always_was_unless_it_carries_a_whole_view() {
    let view = View::new(Size {
        cols: 80,
        rows: 24,
        xpixel: 0,
        ypixel: 0,
    });
    let offset = [0, 0, 0, 0, 0, 0, 0x10, 0];
    // The offset and nothing else, as the first clients sent it.
    let plain = raw(kind::ATTACH, 0, "work", &offset);
    assert_eq!(
        read_packet(&mut plain.as_slice()).unwrap(),
        Some(work(Message::attach(0x1000)))
    );
    // Less than a view after it is what a fixed layout's tail always was: ignored.
    for cut in 1..9 {
        let mut payload = offset.to_vec();
        payload.extend(std::iter::repeat_n(7, cut));
        let bytes = raw(kind::ATTACH, 0, "work", &payload);
        assert_eq!(
            read_packet(&mut bytes.as_slice()).unwrap(),
            Some(work(Message::attach(0x1000))),
            "{cut} bytes after the offset"
        );
    }
    // A whole view with nothing after it is the view of a client from before
    // the name.
    let mut payload = offset.to_vec();
    payload.extend([0, 80, 0, 24, 0, 0, 0, 0, 0]);
    let bytes = raw(kind::ATTACH, 0, "work", &payload);
    assert_eq!(
        read_packet(&mut bytes.as_slice()).unwrap(),
        Some(work(Message::Attach {
            offset: 0x1000,
            view: Some(view)
        }))
    );
    // After the view comes the name: a length of nothing says there is none,
    // and whatever a later version appends after that is ignored.
    payload.extend([0, 0xde, 0xad]);
    let bytes = raw(kind::ATTACH, 0, "work", &payload);
    assert_eq!(
        read_packet(&mut bytes.as_slice()).unwrap(),
        Some(work(Message::Attach {
            offset: 0x1000,
            view: Some(view)
        }))
    );
}

/// An `attach` of "work" from 0x1000 with a whole view of 80 x 24 and no flags,
/// then `tail`.
fn attach_with_tail(tail: &[u8]) -> Vec<u8> {
    let mut payload = vec![0, 0, 0, 0, 0, 0, 0x10, 0];
    payload.extend([0, 80, 0, 24, 0, 0, 0, 0, 0]);
    payload.extend(tail);
    raw(kind::ATTACH, 0, "work", &payload)
}

/// The name in the view of the `attach` in `bytes`.
fn attach_name(bytes: &[u8]) -> Result<Option<ClientName>, Error> {
    match read_packet(&mut &*bytes)? {
        Some(Packet {
            message: Message::Attach { view, .. },
            ..
        }) => Ok(view.unwrap().name),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_client_names_itself_after_the_flags_of_its_view_in_at_most_63_bytes() {
    let name = |text: &str| Some(ClientName::new(text).unwrap());
    // A name, and whatever follows it, which is ignored.
    let mut tail = vec![7];
    tail.extend(b"MacBook");
    assert_eq!(
        attach_name(&attach_with_tail(&tail)).unwrap(),
        name("MacBook")
    );
    tail.extend([0xde, 0xad]);
    assert_eq!(
        attach_name(&attach_with_tail(&tail)).unwrap(),
        name("MacBook")
    );
    // The longest name, and a name with a character of more than one byte.
    let longest = "m".repeat(63);
    let mut tail = vec![63];
    tail.extend(longest.as_bytes());
    assert_eq!(
        attach_name(&attach_with_tail(&tail)).unwrap(),
        name(&longest)
    );
    let mut tail = vec![8];
    tail.extend("Ömer'in".as_bytes());
    assert_eq!(
        attach_name(&attach_with_tail(&tail))
            .unwrap()
            .unwrap()
            .as_str(),
        "Ömer'in"
    );
    // A name of no bytes is none.
    assert_eq!(attach_name(&attach_with_tail(&[0])).unwrap(), None);
    // Longer than 63 bytes is not read as a name that is cut, or as none: the
    // attach is malformed, and the connection can say so and go on.
    for len in [64u8, 200] {
        let mut tail = vec![len];
        tail.extend(std::iter::repeat_n(b'm', usize::from(len)));
        let error = attach_name(&attach_with_tail(&tail)).unwrap_err();
        assert!(error.is_recoverable(), "{len}: {error}");
    }
    // So is a name that is not all there, and one that is not UTF-8.
    for tail in [&[7, b'M', b'a'][..], &[2, 0xff, 0xfe][..]] {
        let error = attach_name(&attach_with_tail(tail)).unwrap_err();
        assert!(error.is_recoverable(), "{tail:?}: {error}");
    }
    // Writing: a name is written once, after the flags; none is not written at
    // all, so the view is the one a client from before the name sent.
    let view = named(
        Size {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        },
        "iMac",
    );
    let packet = work(Message::Attach {
        offset: 0x1000,
        view: Some(view),
    });
    let frame = packet.to_frame().unwrap();
    assert_eq!(&frame.payload[17..], &[4, b'i', b'M', b'a', b'c']);
    assert_eq!(Packet::from_frame(frame).unwrap(), Some(packet));
    let mut view = view;
    view.name = None;
    let frame = work(Message::Attach {
        offset: 0x1000,
        view: Some(view),
    })
    .to_frame()
    .unwrap();
    assert_eq!(frame.payload.len(), 17);
}

#[test]
fn a_name_is_at_most_63_bytes_of_text_and_cut_where_a_character_ends() {
    assert!(ClientName::new("").is_none());
    assert!(ClientName::new(&"m".repeat(63)).is_some());
    assert!(ClientName::new(&"m".repeat(64)).is_none());
    assert_eq!(ClientName::clipped(""), None);
    assert_eq!(
        ClientName::clipped(&"m".repeat(100)).unwrap().as_str(),
        "m".repeat(63)
    );
    // 62 bytes, then a two-byte character that would end at 64.
    let text = format!("{}ö", "m".repeat(62));
    assert_eq!(ClientName::clipped(&text).unwrap().as_str(), "m".repeat(62));
    // Equal names are equal in every byte they hold.
    assert_eq!(ClientName::new("a"), ClientName::clipped("a"));
    assert_ne!(ClientName::new("a"), ClientName::new("b"));
    assert_eq!(
        format!("{:?}", ClientName::new("a").unwrap()),
        "ClientName(\"a\")"
    );
}

#[test]
fn a_status_names_the_clients_one_entry_each_in_at_most_63_bytes() {
    let plain = running_work().to_block();
    assert_eq!(Status::from_block(&plain).unwrap().clients_named, vec![]);
    // Two entries, the second of no bytes: a list, in order.
    let mut block = plain.clone();
    block.extend([0, 21, 0, 0, 0, 3, b'i', b'M', b'c']);
    block.extend([0, 21, 0, 0, 0, 0]);
    let read = Status::from_block(&block).unwrap();
    assert_eq!(read.clients, 2);
    assert_eq!(read.clients_named, vec![ClientName::new("iMc"), None]);
    assert_eq!(read.to_block(), block);
    // The most a session serves; one more is not a status.
    let many = |n: usize| {
        let mut block = plain.clone();
        for _ in 0..n {
            block.extend([0, 21, 0, 0, 0, 0]);
        }
        block
    };
    assert_eq!(
        Status::from_block(&many(v1::MAX_CLIENTS))
            .unwrap()
            .clients_named
            .len(),
        v1::MAX_CLIENTS
    );
    assert!(Status::from_block(&many(v1::MAX_CLIENTS + 1)).is_err());
    // A name of 64 bytes, and one that is not UTF-8, are not a status either.
    let mut long = plain.clone();
    long.extend([0, 21, 0, 0, 0, 64]);
    long.extend([b'm'; 64]);
    assert!(Status::from_block(&long).is_err());
    let mut bad = plain.clone();
    bad.extend([0, 21, 0, 0, 0, 1, 0xff]);
    assert!(Status::from_block(&bad).is_err());
}

#[test]
fn a_size_fits_up_to_the_most_a_screen_may_hold() {
    let size = |cols, rows| Size {
        cols,
        rows,
        xpixel: 0,
        ypixel: 0,
    };
    assert!(size(0, 0).fits(), "the unsaid");
    assert!(size(v1::MAX_COLS, v1::MAX_ROWS).fits());
    assert!(!size(v1::MAX_COLS + 1, 24).fits());
    assert!(!size(80, v1::MAX_ROWS + 1).fits());
    assert!(!size(u16::MAX, u16::MAX).fits());
}

#[test]
fn a_picture_piece_cannot_outgrow_a_frame() {
    let biggest = work(Message::Picture {
        offset: 7,
        cols: 80,
        rows: 24,
        more: true,
        data: vec![7; v1::MAX_PICTURE],
    });
    let mut wire = Vec::new();
    write_packet(&mut wire, &biggest).unwrap();
    assert_eq!(read_packet(&mut wire.as_slice()).unwrap(), Some(biggest));
    let too_big = work(Message::Picture {
        offset: 7,
        cols: 80,
        rows: 24,
        more: false,
        data: vec![7; v1::MAX_PICTURE + 1],
    });
    assert!(write_packet(&mut Vec::new(), &too_big).is_err());
}

#[test]
fn a_status_with_only_an_id_reads_as_zeros() {
    let mut block = Vec::new();
    block.extend_from_slice(&[0, 1, 0, 0, 0, 1, b'a']);
    let status = Status::from_block(&block).unwrap();
    assert_eq!(status, Status::new(id("a")));
    assert!(Status::from_block(&[]).is_err());
}

#[test]
fn a_status_from_before_the_warden_said_its_version_reads_it_as_unknown() {
    // The recorded `status_reply` has the relay's version (key 11) and nothing of the
    // warden's (key 12); a status with no warden version writes none.
    let Some(Message::StatusReply(old)) = read_packet(&mut fixture("status_reply").as_slice())
        .unwrap()
        .map(|packet| packet.message)
    else {
        panic!("not a status reply");
    };
    assert_eq!(old.version, "0.1.0");
    assert_eq!(old.warden_version, "");
    let mut block = old.to_block();
    assert!(!block.windows(2).any(|w| w == [0, 12]));
    // The same block with the key appended reads with the warden's version.
    let mut newer = with(old.clone(), |s| s.warden_version = "0.3.0".into());
    block.extend_from_slice(&[0, 12, 0, 0, 0, 5]);
    block.extend_from_slice(b"0.3.0");
    assert_eq!(Status::from_block(&block).unwrap(), newer);
    newer.warden_version.clear();
    assert_eq!(Status::from_block(&newer.to_block()).unwrap(), old);
}

#[test]
fn a_status_from_before_the_terminal_keys_reads_them_as_nothing() {
    let Some(Message::StatusReply(old)) = read_packet(&mut fixture("status_reply").as_slice())
        .unwrap()
        .map(|packet| packet.message)
    else {
        panic!("not a status reply");
    };
    assert!(!old.secret);
    assert_eq!((old.remote.as_ref(), old.remote_port), (None, None));
    // A status with nothing of the sort writes none of the keys.
    let mut block = old.to_block();
    assert!(
        !block
            .windows(2)
            .any(|w| matches!(w, [0, 13] | [0, 14] | [0, 15]))
    );
    // The same block with the keys appended reads with them.
    block.extend_from_slice(&[0, 13, 0, 0, 0, 1, 1]);
    block.extend_from_slice(&[0, 14, 0, 0, 0, 4]);
    block.extend_from_slice(b"host");
    block.extend_from_slice(&[0, 15, 0, 0, 0, 2, 0, 22]);
    let newer = with(old.clone(), |s| {
        s.secret = true;
        s.remote = Some(b"host".to_vec());
        s.remote_port = Some(22);
    });
    assert_eq!(Status::from_block(&block).unwrap(), newer);
    assert_eq!(Status::from_block(&newer.to_block()).unwrap(), newer);
    // A value of the wrong length is not read as another.
    for entry in [
        [0, 13, 0, 0, 0, 2, 1, 1].as_slice(),
        [0, 15, 0, 0, 0, 1, 22].as_slice(),
    ] {
        let mut bad = old.to_block();
        bad.extend_from_slice(entry);
        assert!(Status::from_block(&bad).is_err(), "{entry:?}");
    }
}

#[test]
fn a_status_from_before_the_times_reads_them_as_unknown() {
    // The recorded `status_reply` has none of keys 17 to 20, and a status that
    // does not know a time writes none of them: it is the same bytes.
    let Some(Message::StatusReply(old)) = read_packet(&mut fixture("status_reply").as_slice())
        .unwrap()
        .map(|packet| packet.message)
    else {
        panic!("not a status reply");
    };
    assert_eq!(
        (
            old.started,
            old.front_started,
            old.front_is_own,
            old.output_at
        ),
        (None, None, None, None)
    );
    let mut block = old.to_block();
    assert!(!block.windows(2).any(|w| matches!(w, [0, 17..=20])));
    // The same block with the keys appended reads with them. A flag that says
    // no is not a flag that is missing.
    block.extend_from_slice(&[0, 17, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 5]);
    block.extend_from_slice(&[0, 18, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 7]);
    block.extend_from_slice(&[0, 19, 0, 0, 0, 1, 0]);
    block.extend_from_slice(&[0, 20, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 9]);
    let newer = with(old.clone(), |s| {
        s.started = Some(5);
        s.front_started = Some(7);
        s.front_is_own = Some(false);
        s.output_at = Some(9);
    });
    assert_eq!(Status::from_block(&block).unwrap(), newer);
    assert_eq!(Status::from_block(&newer.to_block()).unwrap(), newer);
    let own = with(old.clone(), |s| s.front_is_own = Some(true));
    assert_eq!(Status::from_block(&own.to_block()).unwrap(), own);
    // A reader that knows none of the keys skips them (this one does not know
    // key 22 either) and reads what it does know.
    let mut future = newer.to_block();
    future.extend_from_slice(&[0, 22, 0, 0, 0, 2, 1, 2]);
    assert_eq!(Status::from_block(&future).unwrap(), newer);
    // A value of the wrong length is not read as another.
    for entry in [
        [0, 17, 0, 0, 0, 4, 0, 0, 0, 5].as_slice(),
        [0, 19, 0, 0, 0, 2, 0, 1].as_slice(),
    ] {
        let mut bad = old.to_block();
        bad.extend_from_slice(entry);
        assert!(Status::from_block(&bad).is_err(), "{entry:?}");
    }
}

#[test]
fn a_state_is_the_whole_of_it_and_a_field_with_nothing_to_say_is_not_written() {
    assert_eq!(TerminalState::default().to_block(), Vec::<u8>::new());
    assert_eq!(
        TerminalState::from_block(&[]).unwrap(),
        TerminalState::default()
    );
    let shapes = [
        terminal(|state| state.program = Some(b"sleep".to_vec())),
        terminal(|state| state.secret = true),
        terminal(|state| {
            state.program = Some(b"ssh".to_vec());
            state.remote = Some(b"prod".to_vec());
        }),
        terminal(|state| {
            state.cwd = Some(b"/".to_vec());
            state.remote_port = Some(22);
        }),
        asking_for_a_password(),
    ];
    for state in shapes {
        let block = state.to_block();
        assert_eq!(TerminalState::from_block(&block).unwrap(), state);
        // No secret on the wire means echo is on, not "as it was".
        let has_secret = tlv::entries(&block).any(|entry| entry.unwrap().0 == 3);
        assert_eq!(has_secret, state.secret);
    }
    let plain = terminal(|state| state.program = Some(b"vim".to_vec()));
    assert_eq!(plain.to_block(), [0, 1, 0, 0, 0, 3, b'v', b'i', b'm']);
}

#[test]
fn a_reader_of_a_state_skips_what_it_does_not_know_and_refuses_a_value_of_the_wrong_size() {
    let mut block = asking_for_a_password().to_block();
    // A key from a later version, first and last; a repeated key, the last wins.
    block.splice(0..0, [0x0f, 0xff, 0, 0, 0, 2, 9, 9]);
    block.extend_from_slice(&[0x0f, 0xfe, 0, 0, 0, 0]);
    block.extend_from_slice(&[0, 4, 0, 0, 0, 5]);
    block.extend_from_slice(b"other");
    let state = TerminalState::from_block(&block).unwrap();
    assert_eq!(state.remote.as_deref(), Some(b"other".as_slice()));
    assert_eq!(state.remote_port, Some(2222));
    // The bits of the secret's byte it does not define are not its business.
    assert!(
        !TerminalState::from_block(&[0, 3, 0, 0, 0, 1, 0b1111_1110])
            .unwrap()
            .secret
    );
    for bad in [
        [0, 3, 0, 0, 0, 0].as_slice(),
        [0, 3, 0, 0, 0, 2, 1, 1].as_slice(),
        [0, 5, 0, 0, 0, 1, 22].as_slice(),
        [0, 5, 0, 0, 0, 4, 0, 0, 0, 22].as_slice(),
        [0, 1, 0, 0, 0, 9].as_slice(), // an entry cut short
    ] {
        assert!(TerminalState::from_block(bad).is_err(), "{bad:?}");
    }
    // As a message: one that is malformed costs that message and nothing after it.
    let mut stream = raw(kind::STATE, 0, "work", &[0, 5, 0, 0, 0, 1, 22]);
    stream.extend(fixture("state_empty"));
    let mut reader = stream.as_slice();
    assert!(read_packet(&mut reader).unwrap_err().is_recoverable());
    assert_eq!(
        read_packet(&mut reader).unwrap(),
        Some(work(Message::TerminalState(TerminalState::default())))
    );
}

#[test]
fn an_attached_is_the_one_it_always_was_unless_it_carries_a_whole_answered_offset() {
    let head = [0, 0, 0, 0, 0, 0, 0x10, 0, 1];
    let attached = |answered| {
        work(Message::Attached {
            offset: 0x1000,
            truncated: true,
            answered,
        })
    };
    // The offset and the flags, as the first servers sent them.
    let plain = raw(kind::ATTACHED, 0, "work", &head);
    assert_eq!(
        read_packet(&mut plain.as_slice()).unwrap(),
        Some(attached(0))
    );
    // Nothing answered is written as those nine bytes, so a server that has not
    // answered anything and one from before the field are the same on the wire.
    let mut written = Vec::new();
    attached(0).encode(&mut written).unwrap();
    assert_eq!(written, plain);
    // Less than a whole offset after the flags is what a fixed layout's tail
    // always was: ignored.
    for cut in 1..8 {
        let mut payload = head.to_vec();
        payload.extend(std::iter::repeat_n(7, cut));
        let bytes = raw(kind::ATTACHED, 0, "work", &payload);
        assert_eq!(
            read_packet(&mut bytes.as_slice()).unwrap(),
            Some(attached(0)),
            "{cut} bytes after the flags"
        );
    }
    // A whole one, and whatever a later version appends after it.
    let mut payload = head.to_vec();
    payload.extend([0, 0, 0, 0, 0, 0, 0x08, 0, 0xde, 0xad]);
    let bytes = raw(kind::ATTACHED, 0, "work", &payload);
    assert_eq!(
        read_packet(&mut bytes.as_slice()).unwrap(),
        Some(attached(0x800))
    );
    // And it is written whole.
    let mut written = Vec::new();
    attached(0x800).encode(&mut written).unwrap();
    assert_eq!(written, raw(kind::ATTACHED, 0, "work", &payload[..17]));
}

#[test]
fn the_three_bits_of_a_views_flags_are_each_their_own() {
    let size = Size {
        cols: 80,
        rows: 24,
        xpixel: 0,
        ypixel: 0,
    };
    let view = |picture, picture_if_answered, keep| {
        let mut view = viewing(size, picture, picture_if_answered);
        view.keep_after_end = keep;
        view
    };
    for flags in 0u8..8 {
        let (picture, if_answered, keep) = (flags & 1 != 0, flags & 2 != 0, flags & 4 != 0);
        let packet = work(Message::Attach {
            offset: 0,
            view: Some(view(picture, if_answered, keep)),
        });
        let frame = packet.to_frame().unwrap();
        assert_eq!(frame.payload.last(), Some(&flags));
        assert_eq!(Packet::from_frame(frame).unwrap(), Some(packet));
    }
    // The bits it does not define are not its business.
    let mut payload = vec![0; 8];
    payload.extend([0, 80, 0, 24, 0, 0, 0, 0, 0b1111_1000]);
    let bytes = raw(kind::ATTACH, 0, "work", &payload);
    assert_eq!(
        read_packet(&mut bytes.as_slice()).unwrap(),
        Some(work(Message::Attach {
            offset: 0,
            view: Some(view(false, false, false))
        }))
    );
}

#[test]
fn bytes_that_are_not_a_protocol_never_panic() {
    // A fixed-seed xorshift: the same garbage on every run, no dependency.
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let seeds: Vec<Vec<u8>> = messages()
        .iter()
        .map(|(n, _)| fixture(n))
        .chain(streams().iter().map(|(n, ..)| fixture(n)))
        .collect();
    for round in 0..3000 {
        let mut bytes = if round % 2 == 0 {
            // A real frame with a few bytes flipped.
            let mut b = seeds[usize::try_from(next()).unwrap() % seeds.len()].clone();
            for _ in 0..1 + next() % 4 {
                let at = usize::try_from(next()).unwrap() % b.len();
                b[at] = u8::try_from(next() & 0xff).unwrap();
            }
            b
        } else {
            (0..next() % 64)
                .map(|_| u8::try_from(next() & 0xff).unwrap())
                .collect()
        };
        // Keep a payload from asking for megabytes we would then wait for.
        if let Some(len) = bytes.get_mut(3..7) {
            len[0] = 0;
            len[1] = 0;
        }
        let mut reader = bytes.as_slice();
        while let Ok(Some(_)) = read_packet(&mut reader) {}
        let mut decoder = PacketDecoder::new();
        decoder.push(&bytes);
        while let Ok(Some(_)) = decoder.next_packet() {}
        let mut reader = bytes.as_slice();
        while let Ok(Some(_)) = read_incoming(&mut reader) {}
        let mut decoder = IncomingDecoder::new();
        decoder.push(&bytes);
        while let Ok(Some(_)) = decoder.next_incoming() {}
        let _ = Status::from_block(&bytes);
        let _ = TerminalState::from_block(&bytes);
        let _ = Features::from_block(&bytes);
        let _ = Offers::from_block(&bytes);
        let _ = Standing::from_block(&bytes);
        let _ = Capture::from_block(&bytes);
        let _ = WIRE.read_hello(&mut bytes.as_slice());
    }
}

/// A channel 0 fixture by name.
fn session_message(name: &str) -> Packet {
    messages()
        .into_iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no session fixture {name}"))
        .1
}

/// A channel 1 fixture by name.
fn stream_message(name: &str) -> (Side, stream::Packet) {
    streams()
        .into_iter()
        .find(|(n, ..)| *n == name)
        .map(|(_, from, packet)| (from, packet))
        .unwrap_or_else(|| panic!("no stream fixture {name}"))
}

#[test]
fn every_stream_message_is_its_fixture_byte_for_byte() {
    for (name, from, packet) in streams() {
        let bytes = fixture(name);
        let mut written = Vec::new();
        stream::write_packet(&mut written, &packet, from).unwrap();
        assert_eq!(written, bytes, "{name}: what is written");
        let mut appended = vec![9];
        packet.encode(from, &mut appended).unwrap();
        assert_eq!(&appended[1..], bytes.as_slice(), "{name}: what is appended");

        let expected = Incoming::Stream {
            from,
            packet: packet.clone(),
        };
        let mut reader = bytes.as_slice();
        assert_eq!(
            read_incoming(&mut reader).unwrap().as_ref(),
            Some(&expected),
            "{name}: what is read"
        );
        assert_eq!(
            read_incoming(&mut reader).unwrap(),
            None,
            "{name}: left over"
        );

        let mut decoder = IncomingDecoder::new();
        for byte in &bytes {
            decoder.push(std::slice::from_ref(byte));
        }
        assert_eq!(
            decoder.next_incoming().unwrap().as_ref(),
            Some(&expected),
            "{name}: what is decoded"
        );
        assert_eq!(decoder.pending(), 0, "{name}");

        // The reader that reads sessions only (what a build without streams
        // is) goes over the frame by its length and finds nothing.
        assert_eq!(read_packet(&mut bytes.as_slice()).unwrap(), None, "{name}");
        let mut old = PacketDecoder::new();
        old.push(&bytes);
        assert_eq!(old.next_packet().unwrap(), None, "{name}");
        assert_eq!(old.pending(), 0, "{name}");
        // Nor does the frame mean anything on the session channel.
        let frame = frame::read_frame(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!(frame.channel, CHANNEL_STREAM, "{name}");
        assert!(frame.session.is_empty(), "{name}");
        assert_eq!(Packet::from_frame(frame).unwrap(), None, "{name}");
    }
}

#[test]
fn every_stream_kind_has_a_fixture() {
    let kinds: BTreeSet<u8> = streams()
        .iter()
        .map(|(_, from, p)| p.message.kind(*from).unwrap())
        .collect();
    use stream::kind::*;
    for k in [
        OPEN,
        CLIENT_DATA,
        CLIENT_CREDIT,
        CLIENT_EOF,
        CLIENT_CLOSE,
        OPENED,
        SERVER_DATA,
        SERVER_CREDIT,
        SERVER_EOF,
        SERVER_CLOSE,
        FAILED,
        STDERR,
        EXITED,
    ] {
        assert!(kinds.contains(&k), "kind {k:#04x} has no fixture");
    }
    // The kinds are the ones section 4 lists: the high bit marks the server.
    assert!(
        [OPEN, CLIENT_DATA, CLIENT_CREDIT, CLIENT_EOF, CLIENT_CLOSE]
            .iter()
            .all(|k| k & 0x80 == 0)
    );
    assert!(
        [
            OPENED,
            SERVER_DATA,
            SERVER_CREDIT,
            SERVER_EOF,
            SERVER_CLOSE,
            FAILED,
            STDERR,
            EXITED
        ]
        .iter()
        .all(|k| k & 0x80 != 0)
    );
    assert_eq!(kinds.len(), 13);
}

#[test]
fn the_two_channels_interleave_and_a_reader_without_streams_sees_only_sessions() {
    // (fixture, on channel 1)
    let order = [
        ("features", false),
        ("ping", false),
        ("features_reply", false),
        ("stream_open", true),
        ("pong", false),
        ("stream_opened", true),
        ("attach", false),
        ("stream_client_data", true),
        ("output", false),
        ("stream_server_credit", true),
        ("stream_server_data", true),
        ("stream_client_eof", true),
        ("stream_server_close", true),
        ("detach", false),
    ];
    let mut wire = Vec::new();
    let mut all = Vec::new();
    let mut sessions = Vec::new();
    for (name, on_stream) in order {
        wire.extend(fixture(name));
        if on_stream {
            let (from, packet) = stream_message(name);
            all.push(Incoming::Stream { from, packet });
        } else {
            let packet = session_message(name);
            sessions.push(packet.clone());
            all.push(Incoming::Session(packet));
        }
    }

    let mut reader = wire.as_slice();
    for expected in &all {
        assert_eq!(read_incoming(&mut reader).unwrap().as_ref(), Some(expected));
    }
    assert_eq!(read_incoming(&mut reader).unwrap(), None);

    let mut reader = wire.as_slice();
    for expected in &sessions {
        assert_eq!(read_packet(&mut reader).unwrap().as_ref(), Some(expected));
    }
    assert_eq!(read_packet(&mut reader).unwrap(), None);

    for piece in [1, 3, 13, 4096] {
        let mut incoming = IncomingDecoder::new();
        let mut old = PacketDecoder::new();
        let (mut got_all, mut got_sessions) = (Vec::new(), Vec::new());
        for chunk in wire.chunks(piece) {
            incoming.push(chunk);
            while let Some(i) = incoming.next_incoming().unwrap() {
                got_all.push(i);
            }
            old.push(chunk);
            while let Some(p) = old.next_packet().unwrap() {
                got_sessions.push(p);
            }
        }
        assert_eq!(got_all, all, "pieces of {piece}");
        assert_eq!(got_sessions, sessions, "pieces of {piece}");
        assert_eq!(incoming.pending() + old.pending(), 0, "pieces of {piece}");
    }
}

#[test]
fn a_stream_message_that_does_not_parse_costs_that_message_and_nothing_after_it() {
    use stream::kind as k;
    let mut wire = Vec::new();
    wire.extend(raw(k::CLIENT_DATA, CHANNEL_STREAM, "", &[0, 0, 1])); // not a whole stream number
    wire.extend(raw(k::CLIENT_CLOSE, CHANNEL_STREAM, "", &[])); // no stream number at all
    wire.extend(raw(k::OPEN, CHANNEL_STREAM, "", &[0, 0, 0, 1, 0, 0, 0])); // a window cut short
    wire.extend(raw(
        k::OPEN,
        CHANNEL_STREAM,
        "",
        &[0, 0, 0, 1, 0, 0, 0, 9, 0, 1, 0, 0, 0, 9],
    )); // an entry cut short
    wire.extend(raw(
        k::OPEN,
        CHANNEL_STREAM,
        "",
        &[0, 0, 0, 1, 0, 0, 0, 9, 0, 1, 0, 0, 0, 3, 0, 0, 0],
    )); // a port of three bytes
    wire.extend(raw(k::OPENED, CHANNEL_STREAM, "", &[0, 0, 0, 1, 0, 0])); // a window cut short
    wire.extend(raw(k::FAILED, CHANNEL_STREAM, "", &[0, 0, 0, 1, 0])); // a code cut short
    wire.extend(raw(
        k::CLIENT_CREDIT,
        CHANNEL_STREAM,
        "",
        &[0, 0, 0, 1, 0, 0],
    )); // a count cut short
    wire.extend(raw(k::SERVER_CREDIT, CHANNEL_STREAM, "", &[0, 0, 0, 1])); // no count
    wire.extend(fixture("stream_client_eof"));
    let mut reader = wire.as_slice();
    for n in 0..9 {
        match read_incoming(&mut reader) {
            Err(e) => assert!(e.is_recoverable(), "message {n}: {e}"),
            Ok(i) => panic!("message {n} read as {i:?}"),
        }
    }
    let (from, packet) = stream_message("stream_client_eof");
    assert_eq!(
        read_incoming(&mut reader).unwrap(),
        Some(Incoming::Stream { from, packet })
    );
    assert_eq!(read_incoming(&mut reader).unwrap(), None);
}

#[test]
fn frames_of_a_kind_or_channel_streams_do_not_have_are_skipped_by_their_length() {
    use stream::kind as k;
    let mut wire = Vec::new();
    wire.extend(raw(0x06, CHANNEL_STREAM, "", &[0, 0, 0, 1, 7, 7])); // no client message has this kind
    wire.extend(raw(0x89, CHANNEL_STREAM, "", &[0; 100])); // nor server message
    wire.extend(raw(0x7e, CHANNEL_STREAM, "", &[1; 5000])); // a kind from the future
    wire.extend(raw(0xfe, CHANNEL_STREAM, "", &[])); // another, from the server's half
    wire.extend(raw(k::CLIENT_DATA, 2, "", &[0, 0, 0, 1, 7])); // a channel from the future
    wire.extend(fixture("stream_server_eof"));
    wire.extend(raw(0x06, CHANNEL_STREAM, "", &[2; 10])); // and one trailing, to the end
    let (from, packet) = stream_message("stream_server_eof");
    let eof = Incoming::Stream { from, packet };
    let mut reader = wire.as_slice();
    assert_eq!(read_incoming(&mut reader).unwrap(), Some(eof.clone()));
    assert_eq!(read_incoming(&mut reader).unwrap(), None);
    let mut decoder = IncomingDecoder::new();
    decoder.push(&wire);
    assert_eq!(decoder.next_incoming().unwrap(), Some(eof));
    assert_eq!(decoder.next_incoming().unwrap(), None);
    assert_eq!(decoder.pending(), 0);
}

#[test]
fn a_reader_ignores_what_a_later_version_adds_to_a_stream_message() {
    use stream::Message as S;
    use stream::kind as k;
    let read = |kind: u8, session: &str, payload: &[u8]| match read_incoming(
        &mut raw(kind, CHANNEL_STREAM, session, payload).as_slice(),
    ) {
        Ok(Some(Incoming::Stream { packet, .. })) => packet,
        other => panic!("read as {other:?}"),
    };
    // Fields appended to a fixed layout.
    assert_eq!(
        read(k::CLIENT_CREDIT, "", &[0, 0, 0, 5, 0, 0, 1, 0, 0xde, 0xad]),
        stream::Packet::new(5, S::Credit(256))
    );
    assert_eq!(
        read(k::OPENED, "", &[0, 0, 0, 5, 0, 0, 1, 0, 0xde]),
        stream::Packet::new(5, S::Opened { window: 256 })
    );
    assert_eq!(
        read(k::SERVER_EOF, "", &[0, 0, 0, 5, 1, 2, 3]),
        stream::Packet::new(5, S::Eof)
    );
    assert_eq!(
        read(k::CLIENT_CLOSE, "", &[0, 0, 0, 5, 1]),
        stream::Packet::new(5, S::Close)
    );
    // A session id nobody asked for is not read.
    assert_eq!(
        read(k::SERVER_CLOSE, "work", &[0, 0, 0, 5]),
        stream::Packet::new(5, S::Close)
    );
    // A key it does not know in an open, between the known ones; the last of a
    // repeated key wins.
    let mut payload = vec![0, 0, 0, 6, 0, 0, 0, 1];
    payload.extend([0, 1, 0, 0, 0, 2, 0, 80]); // port 80
    payload.extend([0x0f, 0xff, 0, 0, 0, 3, 9, 9, 9]); // a key from the future
    payload.extend([0, 2, 0, 0, 0, 3, b'o', b'l', b'd']); // host "old"
    payload.extend([0, 2, 0, 0, 0, 2, b'm', b'e']); // host "me"
    assert_eq!(
        read(k::OPEN, "", &payload),
        stream::Packet::new(
            6,
            S::Open {
                window: 1,
                port: 80,
                host: "me".into()
            }
        )
    );
    // A command's open: the keys it has, in any order, with one it does not
    // know between them; the last directory wins, the port and host are not
    // read, and the arguments and variables come in the order they were sent.
    let mut payload = vec![0, 0, 0, 7, 0, 0, 0, 2];
    payload.extend([0, 5, 0, 0, 0, 2, b'/', b'a']); // directory "/a"
    payload.extend([0, 3, 0, 0, 0, 2, b's', b'h']); // program "sh"
    payload.extend([0, 1, 0, 0, 0, 2, 0, 80]); // a port, which a command has no use for
    payload.extend([0, 2, 0, 0, 0, 1, b'h']); // nor a host
    payload.extend([0x0f, 0xff, 0, 0, 0, 1, 9]); // a key from the future
    payload.extend([0, 3, 0, 0, 0, 0]); // an empty argument
    payload.extend([0, 4, 0, 0, 0, 3, b'A', b'=', b'1']);
    payload.extend([0, 5, 0, 0, 0, 2, b'/', b'b']); // directory "/b"
    payload.extend([0, 4, 0, 0, 0, 3, b'B', b'=', b'2']);
    assert_eq!(
        read(k::OPEN, "", &payload),
        stream::Packet::new(
            7,
            S::OpenCommand {
                window: 2,
                command: whole_command(
                    vec![b"sh".to_vec(), Vec::new()],
                    vec![b"A=1".to_vec(), b"B=2".to_vec()],
                    Some(b"/b".to_vec()),
                )
            }
        )
    );
    // Variables and a directory without an argument are not a command: the
    // server reads a target, and has no port to connect to.
    let mut payload = vec![0, 0, 0, 7, 0, 0, 0, 2];
    payload.extend([0, 4, 0, 0, 0, 3, b'A', b'=', b'1']);
    payload.extend([0, 5, 0, 0, 0, 2, b'/', b'a']);
    assert_eq!(
        read(k::OPEN, "", &payload),
        stream::Packet::new(
            7,
            S::Open {
                window: 2,
                port: 0,
                host: String::new()
            }
        )
    );
    // The exit status of an `exited` is read as a prefix, as in `ended`.
    assert_eq!(
        read(0x88, "", &[0, 0, 0, 7, 1, 0, 0, 0, 15, 0xde, 0xad]),
        stream::Packet::new(7, S::Exited(Exit::Signal(15)))
    );
    // A failure code this version has no name for is kept as it came.
    assert_eq!(
        read(k::FAILED, "", &[0, 0, 0, 6, 0x12, 0x34]),
        stream::Packet::new(
            6,
            S::Failed {
                code: FailCode(0x1234),
                text: String::new()
            }
        )
    );
}

#[test]
fn what_an_open_asks_for_is_answered_by_the_server_not_refused_by_the_reader() {
    use stream::Message as S;
    use stream::kind as k;
    // No keys at all: port 0 and no host, which a server answers `failed` with
    // BAD_REQUEST on that stream.
    let frame = raw(k::OPEN, CHANNEL_STREAM, "", &[0, 0, 0, 3, 0, 0, 0, 0]);
    assert_eq!(
        read_incoming(&mut frame.as_slice()).unwrap(),
        Some(Incoming::Stream {
            from: Side::Client,
            packet: stream::Packet::new(
                3,
                S::Open {
                    window: 0,
                    port: 0,
                    host: String::new()
                }
            )
        })
    );
    // A command the server will not run is read all the same, so that it can
    // answer `failed` with BAD_REQUEST on that stream: a NUL in an argument, a
    // variable with no name or no `=`, more arguments than the limit.
    let mut payload = vec![0, 0, 0, 3, 0, 0, 0, 0];
    payload.extend([0, 3, 0, 0, 0, 3, b'a', 0, b'b']);
    payload.extend([0, 4, 0, 0, 0, 2, b'=', b'x']);
    payload.extend([0, 4, 0, 0, 0, 1, b'Z']);
    for _ in 0..=stream::MAX_ARGV {
        payload.extend([0, 3, 0, 0, 0, 1, b'x']);
    }
    let frame = raw(k::OPEN, CHANNEL_STREAM, "", &payload);
    let Some(Incoming::Stream {
        packet:
            stream::Packet {
                message: S::OpenCommand { command, .. },
                ..
            },
        ..
    }) = read_incoming(&mut frame.as_slice()).unwrap()
    else {
        panic!("not a command")
    };
    assert_eq!(command.argv.len(), stream::MAX_ARGV + 2);
    assert_eq!(command.argv.first().unwrap(), b"a\0b");
    assert_eq!(command.env, [b"=x".to_vec(), b"Z".to_vec()]);
    assert!(command.check().is_err());
    // A data message over the sender's limit, and a window over the largest:
    // a reader takes both so that the receiver can close that one stream.
    let mut payload = vec![0, 0, 0, 3];
    payload.extend(vec![7; stream::MAX_DATA + 1]);
    let frame = raw(k::CLIENT_DATA, CHANNEL_STREAM, "", &payload);
    let Some(Incoming::Stream { packet, .. }) = read_incoming(&mut frame.as_slice()).unwrap()
    else {
        panic!("not a stream message")
    };
    assert_eq!(
        packet,
        stream::Packet::new(3, S::Data(vec![7; stream::MAX_DATA + 1]))
    );
    let frame = raw(
        k::OPENED,
        CHANNEL_STREAM,
        "",
        &[0, 0, 0, 3, 0xff, 0xff, 0xff, 0xff],
    );
    assert_eq!(
        read_incoming(&mut frame.as_slice()).unwrap(),
        Some(Incoming::Stream {
            from: Side::Server,
            packet: stream::Packet::new(3, S::Opened { window: u32::MAX })
        })
    );
}

#[test]
fn a_sender_cannot_produce_a_stream_message_no_conforming_end_sends() {
    use stream::Message as S;
    let write = |from: Side, message: S| {
        stream::write_packet(&mut Vec::new(), &stream::Packet::new(1, message), from)
    };
    let open = |port: u16, host: &str| S::Open {
        window: 1,
        port,
        host: host.into(),
    };
    assert!(write(Side::Client, open(80, "example.org")).is_ok());
    assert!(write(Side::Client, open(80, "::1")).is_ok());
    assert!(write(Side::Client, open(80, &"a".repeat(stream::MAX_HOST))).is_ok());
    assert!(
        write(Side::Client, open(0, "example.org")).is_err(),
        "port 0"
    );
    assert!(write(Side::Client, open(80, "")).is_err(), "no host");
    assert!(write(Side::Client, open(80, "a\0b")).is_err(), "a NUL");
    assert!(
        write(Side::Client, open(80, &"a".repeat(stream::MAX_HOST + 1))).is_err(),
        "a host over the longest name"
    );

    let run = |command: Command| S::OpenCommand { window: 1, command };
    let args = |argv: &[&str]| Command::new(argv.iter().map(|a| a.as_bytes().to_vec()).collect());
    assert!(write(Side::Client, run(tar_command())).is_ok());
    assert!(write(Side::Client, run(args(&["sh", "-c", ""]))).is_ok());
    assert!(write(Side::Client, run(args(&[]))).is_err(), "no program");
    assert!(
        write(Side::Client, run(args(&[""]))).is_err(),
        "an empty program"
    );
    assert!(write(Side::Client, run(args(&["a\0b"]))).is_err(), "a NUL");
    assert!(write(Side::Client, run(args(&["sh", "a\0"]))).is_err());
    assert!(
        write(Side::Server, run(tar_command())).is_err(),
        "from the server"
    );
    let with_env = |entries: &[&str]| {
        let mut command = args(&["env"]);
        command.env = entries.iter().map(|e| e.as_bytes().to_vec()).collect();
        command
    };
    assert!(write(Side::Client, run(with_env(&["A=", "B=c=d"]))).is_ok());
    assert!(write(Side::Client, run(with_env(&["A"]))).is_err(), "no =");
    assert!(
        write(Side::Client, run(with_env(&["=a"]))).is_err(),
        "no name"
    );
    assert!(
        write(Side::Client, run(with_env(&["A=\0"]))).is_err(),
        "a NUL"
    );
    let with_cwd = |cwd: &str| {
        let mut command = args(&["pwd"]);
        command.cwd = Some(cwd.as_bytes().to_vec());
        command
    };
    assert!(write(Side::Client, run(with_cwd("/tmp"))).is_ok());
    assert!(
        write(Side::Client, run(with_cwd(""))).is_err(),
        "an empty directory"
    );
    assert!(write(Side::Client, run(with_cwd("/a\0"))).is_err(), "a NUL");
    let many = |n: usize| args(&vec!["x"; n]);
    assert!(write(Side::Client, run(many(stream::MAX_ARGV))).is_ok());
    assert!(write(Side::Client, run(many(stream::MAX_ARGV + 1))).is_err());
    let vars = |n: usize| {
        let mut command = args(&["env"]);
        command.env = vec![b"A=1".to_vec(); n];
        command
    };
    assert!(write(Side::Client, run(vars(stream::MAX_ENV))).is_ok());
    assert!(write(Side::Client, run(vars(stream::MAX_ENV + 1))).is_err());
    let big = |len: usize| Command::new(vec![b"cat".to_vec(), vec![b'x'; len]]);
    assert!(write(Side::Client, run(big(stream::MAX_COMMAND_BYTES - 3))).is_ok());
    assert!(write(Side::Client, run(big(stream::MAX_COMMAND_BYTES - 2))).is_err());
    // Counted over everything a command carries, not each part alone: the
    // limit is over by two bytes here, though no part is near it.
    let half = stream::MAX_COMMAND_BYTES / 2;
    let spread = whole_command(
        vec![b"cat".to_vec(), vec![b'x'; half]],
        vec![[b"A=".to_vec(), vec![b'y'; half - 4]].concat()],
        Some(b"/".to_vec()),
    );
    assert!(write(Side::Client, run(spread)).is_err());

    // The two kinds only the server sends.
    assert!(write(Side::Server, S::Stderr(vec![1; stream::MAX_DATA])).is_ok());
    assert!(write(Side::Server, S::Stderr(vec![1; stream::MAX_DATA + 1])).is_err());
    assert!(write(Side::Client, S::Stderr(Vec::new())).is_err());
    assert!(write(Side::Server, S::Exited(Exit::Code(0))).is_ok());
    assert!(write(Side::Client, S::Exited(Exit::Code(0))).is_err());

    let data = |len: usize| S::Data(vec![1; len]);
    assert!(write(Side::Client, data(stream::MAX_DATA)).is_ok());
    assert!(write(Side::Server, data(stream::MAX_DATA)).is_ok());
    assert!(write(Side::Client, data(stream::MAX_DATA + 1)).is_err());
    assert!(write(Side::Server, data(stream::MAX_DATA + 1)).is_err());

    // A message goes the way its name says.
    assert!(write(Side::Server, open(80, "example.org")).is_err());
    assert!(write(Side::Client, S::Opened { window: 1 }).is_err());
    assert!(
        write(
            Side::Client,
            S::Failed {
                code: FailCode::REFUSED,
                text: String::new()
            }
        )
        .is_err()
    );
    for side in [Side::Client, Side::Server] {
        for message in [S::Data(Vec::new()), S::Credit(1), S::Eof, S::Close] {
            assert!(write(side, message).is_ok());
        }
    }
}

#[test]
fn the_limits_of_streams_are_the_ones_the_features_say() {
    let on = Features::with_streams();
    assert!(on.streams);
    assert_eq!(on.reason, None);
    assert_eq!(on.max_streams, 128);
    assert_eq!(on.max_window, 16 * 1024 * 1024);
    assert_eq!(on.max_window, stream::MAX_WINDOW);
    assert_eq!(on.max_data, 32 * 1024);
    assert_eq!(usize::try_from(on.max_data).unwrap(), stream::MAX_DATA);
    assert_eq!(stream::MAX_STREAMS, 128);
    assert_eq!(stream::MAX_CONNECTING, 16);
    assert_eq!(stream::CONNECT_TIMEOUT.as_secs(), 10);
    assert_eq!(stream::MAX_HOST, 253);
    assert_eq!(stream::MAX_ARGV, 1024);
    assert_eq!(stream::MAX_ENV, 256);
    assert_eq!(stream::MAX_COMMAND_BYTES, 256 * 1024);
    // Commands are a feature of their own, off until a server says it has them,
    // and the limits of streams do not change with them.
    assert!(!on.commands);
    let both = on.clone().with_commands();
    assert!(both.commands);
    assert_eq!(with(both, |f| f.commands = false), on);
}

#[test]
fn features_read_as_zeros_unless_the_server_says_more() {
    // Nothing said: no streams. A server that knows no streams says this or
    // nothing at all.
    assert_eq!(Features::from_block(&[]).unwrap(), Features::default());
    assert!(!Features::default().streams);
    let reply = raw(kind::FEATURES_REPLY, CHANNEL_SESSION, "", &[]);
    assert_eq!(
        read_packet(&mut reply.as_slice()).unwrap(),
        Some(Packet::connection(Message::FeaturesReply(
            Features::default()
        )))
    );

    // Only the first bit of the flag byte is read; a key from the future is
    // skipped, between the known ones; the last of a repeated key wins.
    let mut block = Vec::new();
    block.extend([0, 1, 0, 0, 0, 1, 0b1111_1110]); // streams: bit 0 clear
    block.extend([0x0f, 0xff, 0, 0, 0, 2, 9, 9]);
    block.extend([0, 3, 0, 0, 0, 4, 0, 0, 0, 5]);
    assert_eq!(
        Features::from_block(&block).unwrap(),
        with(Features::default(), |f| f.max_streams = 5)
    );
    block.extend([0, 1, 0, 0, 0, 1, 0b0000_0011]);
    block.extend([0, 3, 0, 0, 0, 4, 0, 0, 0, 6]);
    block.extend([0, 5, 0, 0, 0, 4, 0, 0, 1, 0]);
    assert_eq!(
        Features::from_block(&block).unwrap(),
        with(Features::default(), |f| {
            f.streams = true;
            f.max_streams = 6;
            f.max_data = 256;
        })
    );

    // Commands: bit 0 of its byte, absent when clear, last wins.
    assert!(!Features::default().commands);
    block.extend([0, 6, 0, 0, 0, 1, 0b1111_1111]);
    assert!(Features::from_block(&block).unwrap().commands);
    block.extend([0, 6, 0, 0, 0, 1, 0b0000_0010]);
    assert!(!Features::from_block(&block).unwrap().commands);
    // Written only when set, so that a server without commands is the bytes it
    // was before there were any.
    assert_eq!(
        Features::with_streams().to_block(),
        fixture("features_reply")[7..]
    );
    assert_eq!(Features::with_streams().with_commands().to_block().len(), {
        Features::with_streams().to_block().len() + 7
    });
    assert!(!Features::streams_off("x").commands);

    // A value of the wrong length, text that is not text, and an entry cut
    // short make the message malformed, which costs that message alone.
    for bad in [
        &[0, 6, 0, 0, 0, 2, 1, 1][..],
        &[0, 1, 0, 0, 0, 2, 1, 1][..],
        &[0, 3, 0, 0, 0, 2, 0, 1],
        &[0, 2, 0, 0, 0, 1, 0xff],
        &[0, 4, 0, 0, 0, 9, 0],
    ] {
        let mut wire = raw(kind::FEATURES_REPLY, CHANNEL_SESSION, "", bad);
        wire.extend(fixture("features"));
        let err = read_packet(&mut wire.as_slice()).unwrap_err();
        assert!(err.is_recoverable(), "{bad:?}: {err}");
    }
    // A request has no payload to be wrong about.
    let request = raw(kind::FEATURES, CHANNEL_SESSION, "", &[1, 2, 3]);
    assert_eq!(
        read_packet(&mut request.as_slice()).unwrap(),
        Some(Packet::connection(Message::Features))
    );
}

#[test]
fn the_new_kinds_have_names_and_the_kinds_after_them_do_not() {
    assert_eq!(v1::kind_name(kind::FEATURES), "features");
    assert_eq!(v1::kind_name(kind::FEATURES_REPLY), "features_reply");
    assert_eq!(v1::kind_name(kind::ASK), "ask");
    assert_eq!(v1::kind_name(kind::DECLINE), "decline");
    assert_eq!(v1::kind_name(kind::WRITE), "write");
    assert_eq!(v1::kind_name(kind::READ), "read");
    assert_eq!(v1::kind_name(kind::WAIT), "wait");
    assert_eq!(v1::kind_name(kind::LABEL), "label");
    assert_eq!(v1::kind_name(0x12), "unknown");
    assert_eq!(v1::kind_name(kind::STATE), "state");
    assert_eq!(v1::kind_name(kind::ASK_REPLY), "ask_reply");
    assert_eq!(v1::kind_name(kind::TICKET), "ticket");
    assert_eq!(v1::kind_name(kind::SCREEN_TEXT), "screen_text");
    assert_eq!(v1::kind_name(0x8f), "unknown");
    assert_eq!(stream::kind_name(stream::kind::OPEN), "stream open");
    assert_eq!(stream::kind_name(stream::kind::FAILED), "stream failed");
    assert_eq!(stream::kind_name(0x06), "unknown");
    assert_eq!(stream::kind_name(stream::kind::STDERR), "stream stderr");
    assert_eq!(stream::kind_name(stream::kind::EXITED), "stream exited");
    assert_eq!(stream::kind_name(0x89), "unknown");
    assert_eq!(FailCode::START_FAILED, FailCode(8));
    assert_eq!(FailCode::DECLINED, FailCode(9));
}

/// What a reader from before commands made of a frame of channel 1: it had the
/// client kinds 0x01-0x05 and the server kinds 0x81-0x86, and the keys 1 and 2
/// of an `open`, and skipped the rest. `None` for a frame it skipped.
fn read_before_commands(bytes: &[u8]) -> Option<(&'static str, u16, String)> {
    let frame = frame::read_frame(&mut &bytes[..]).unwrap().unwrap();
    assert_eq!(frame.channel, CHANNEL_STREAM);
    let name = match frame.kind {
        0x01 => "open",
        0x02 | 0x82 => "data",
        0x03 | 0x83 => "credit",
        0x04 | 0x84 => "eof",
        0x05 | 0x85 => "close",
        0x81 => "opened",
        0x86 => "failed",
        _ => return None,
    };
    let (mut port, mut host) = (0, String::new());
    if name == "open" {
        for entry in kablo_proto::tlv::entries(&frame.payload[8..]) {
            let (key, value) = entry.unwrap();
            match key {
                1 => port = u16::from_be_bytes([value[0], value[1]]),
                2 => host = String::from_utf8_lossy(value).into_owned(),
                _ => {}
            }
        }
    }
    Some((name, port, host))
}

#[test]
fn a_reader_from_before_commands_skips_the_new_kinds_and_reads_a_command_open_as_a_target_with_none()
 {
    use stream::Message as S;
    for (name, _, packet) in streams() {
        let old = read_before_commands(&fixture(name));
        match &packet.message {
            // The server's answer to a stream it never heard of is nothing to
            // act on: the kind was skipped.
            S::Stderr(_) | S::Exited(_) => assert_eq!(old, None, "{name}"),
            // Port 0 and no host: what a server from before commands answers
            // with `failed` (7), and a client sees that and nothing worse.
            S::OpenCommand { .. } => {
                assert_eq!(old, Some(("open", 0, String::new())), "{name}")
            }
            // An open that answers a ticket has no port and no host for it.
            S::OpenTicket { .. } => assert_eq!(old, Some(("open", 0, String::new())), "{name}"),
            S::Open { port, host, .. } => {
                assert_eq!(old, Some(("open", *port, host.clone())), "{name}")
            }
            // A code it has no name for is a failure all the same.
            S::Failed { .. } => assert_eq!(old.map(|o| o.0), Some("failed"), "{name}"),
            _ => assert!(old.is_some(), "{name}"),
        }
    }
}

#[test]
fn a_command_conversation_reads_in_order_however_it_arrives() {
    let order = [
        "stream_open_command",
        "stream_client_data",
        "stream_server_stderr",
        "stream_server_data",
        "stream_server_credit",
        "stream_client_eof",
        "stream_server_eof",
        "stream_exited_code",
        "stream_server_close",
        "stream_failed_not_started",
    ];
    let mut wire = Vec::new();
    let mut expected = Vec::new();
    for name in order {
        wire.extend(fixture(name));
        let (from, packet) = stream_message(name);
        expected.push(Incoming::Stream { from, packet });
    }
    for piece in [1, 5, 4096] {
        let mut decoder = IncomingDecoder::new();
        let mut got = Vec::new();
        for chunk in wire.chunks(piece) {
            decoder.push(chunk);
            while let Some(i) = decoder.next_incoming().unwrap() {
                got.push(i);
            }
        }
        assert_eq!(got, expected, "pieces of {piece}");
        assert_eq!(decoder.pending(), 0);
    }
    // And the command is the one the fixture says: its variables are read as
    // names and values.
    let command = tar_command();
    command.check().unwrap();
    let vars: Vec<_> = command.vars().collect();
    assert_eq!(
        vars,
        [(&b"LC_ALL"[..], &b"C"[..]), (&b"FOO"[..], &b"a=b"[..])]
    );
}

#[test]
fn a_features_with_a_body_is_the_features_it_always_was_to_a_reader_that_does_not_read_it() {
    // The kind is what a reader from before the body goes by: it took any
    // payload for the question.
    let with = session_message("features_tickets");
    assert_eq!(with.message.kind(), kind::FEATURES);
    assert_eq!(fixture("features_tickets")[0], kind::FEATURES);
    assert_eq!(
        Message::Features.kind(),
        Message::FeaturesWith(Offers::default()).kind()
    );

    // The body is always something, so it is told from none, and a client
    // that takes no tickets still says it.
    for offers in [Offers::default(), Offers::with_tickets()] {
        let packet = Packet::connection(Message::FeaturesWith(offers));
        let mut wire = Vec::new();
        write_packet(&mut wire, &packet).unwrap();
        assert_ne!(&wire[3..7], &[0, 0, 0, 0], "{offers:?}: a body");
        assert_eq!(read_packet(&mut wire.as_slice()).unwrap(), Some(packet));
    }

    // Only the first bit of the byte is read, a key from the future is
    // skipped, and the last of a repeated key wins; a body of nothing but
    // keys from the future is a body that says nothing.
    let body = |block: &[u8]| {
        let wire = raw(kind::FEATURES, CHANNEL_SESSION, "", block);
        read_packet(&mut wire.as_slice()).unwrap().unwrap().message
    };
    let mut block = vec![0x0f, 0xff, 0, 0, 0, 1, 9];
    assert_eq!(body(&block), Message::FeaturesWith(Offers::default()));
    block.extend([0, 1, 0, 0, 0, 1, 0b1111_1111]);
    assert_eq!(body(&block), Message::FeaturesWith(Offers::with_tickets()));
    block.extend([0, 1, 0, 0, 0, 1, 0b0000_0010]);
    assert_eq!(body(&block), Message::FeaturesWith(Offers::default()));

    // A body that cannot be read leaves the question as it was: the value of
    // the wrong length, and an entry cut short.
    for bad in [
        &[0, 1, 0, 0, 0, 2, 1, 1][..],
        &[0, 1, 0, 0, 0, 9, 1],
        &[1, 2, 3],
    ] {
        assert_eq!(body(bad), Message::Features, "{bad:?}");
    }
    assert_eq!(body(&[]), Message::Features);
}

#[test]
fn the_mailbox_is_written_only_when_set_and_a_reader_from_before_it_reads_the_rest() {
    assert!(!Features::default().mailbox);
    assert!(!Features::with_streams().with_commands().mailbox);
    // Written only when set: a bridge without a mailbox is the bytes it was.
    assert_eq!(
        Features::with_streams().with_commands().to_block(),
        fixture("features_reply_commands")[7..]
    );
    let with = Features::with_streams()
        .with_commands()
        .with_mailbox()
        .to_block();
    assert_eq!(with, fixture("features_reply_mailbox")[7..]);
    assert_eq!(
        with.len(),
        Features::with_streams().with_commands().to_block().len() + 7
    );
    assert!(Features::from_block(&with).unwrap().mailbox);

    // Bit 0 of its byte, absent when clear, the last wins.
    let mut block = with.clone();
    block.extend([0, 7, 0, 0, 0, 1, 0b1111_1110]);
    assert!(!Features::from_block(&block).unwrap().mailbox);
    assert!(
        !Features::from_block(&[0, 1, 0, 0, 0, 1, 1])
            .unwrap()
            .mailbox
    );
    assert!(Features::from_block(&[0, 7, 0, 0, 0, 2, 1, 1]).is_err());

    // What a reader that has the keys 1 to 6 makes of the reply: everything but
    // the mailbox, whatever follows the keys it knows.
    let before_mail = |block: &[u8]| {
        let mut features = Features::default();
        for entry in tlv::entries(block) {
            let (key, value) = entry.unwrap();
            match key {
                1 => features.streams = value[0] & 1 != 0,
                2 => features.reason = Some(String::from_utf8(value.to_vec()).unwrap()),
                3 => features.max_streams = u32::from_be_bytes(value.try_into().unwrap()),
                4 => features.max_window = u32::from_be_bytes(value.try_into().unwrap()),
                5 => features.max_data = u32::from_be_bytes(value.try_into().unwrap()),
                6 => features.commands = value[0] & 1 != 0,
                _ => {}
            }
        }
        features
    };
    assert_eq!(
        before_mail(&with),
        Features::with_streams().with_commands(),
        "the keys it knows"
    );
}

#[test]
fn an_ask_reply_reads_as_zeros_unless_the_bridge_says_more() {
    let reply = |block: &[u8]| {
        let wire = raw(kind::ASK_REPLY, CHANNEL_SESSION, "work", block);
        read_packet(&mut wire.as_slice()).unwrap().unwrap()
    };
    // Nothing said: not attached, no idle time, no tickets.
    assert_eq!(reply(&[]), work(Message::AskReply(Standing::default())));
    assert!(!Standing::default().attached && !Standing::default().tickets);
    assert_eq!(Standing::default().idle, Duration::ZERO);

    // Bit 0 of the flag bytes, a key from the future skipped between the known
    // ones, the last of a repeated key wins.
    let mut block = Vec::new();
    block.extend([0, 1, 0, 0, 0, 1, 0b1111_1110]); // attached: bit 0 clear
    block.extend([0x0f, 0xff, 0, 0, 0, 2, 9, 9]);
    block.extend([0, 2, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 5]);
    block.extend([0, 3, 0, 0, 0, 1, 0b0000_0011]);
    assert_eq!(
        reply(&block),
        work(Message::AskReply(
            Standing::idle(Duration::from_millis(5)).with_tickets()
        ))
    );
    block.extend([0, 1, 0, 0, 0, 1, 1]);
    block.extend([0, 2, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 1, 0]);
    assert_eq!(
        reply(&block),
        work(Message::AskReply(
            Standing::idle(Duration::from_millis(256))
                .with_attached()
                .with_tickets()
        ))
    );

    // Whole milliseconds, and a duration too long for the field is the longest.
    let long = Standing::idle(Duration::from_secs(u64::MAX));
    assert_eq!(
        Standing::from_block(&long.to_block()).unwrap().idle,
        Duration::from_millis(u64::MAX)
    );
    let sub = Standing::idle(Duration::from_micros(1999));
    assert_eq!(
        Standing::from_block(&sub.to_block()).unwrap().idle,
        Duration::from_millis(1)
    );

    // A value of the wrong length and an entry cut short make the message
    // malformed, which costs that message alone.
    for bad in [
        &[0, 1, 0, 0, 0, 2, 1, 1][..],
        &[0, 2, 0, 0, 0, 4, 0, 0, 0, 1],
        &[0, 3, 0, 0, 0, 9, 0],
    ] {
        let mut wire = raw(kind::ASK_REPLY, CHANNEL_SESSION, "work", bad);
        wire.extend(fixture("ask"));
        let err = read_packet(&mut wire.as_slice()).unwrap_err();
        assert!(err.is_recoverable(), "{bad:?}: {err}");
    }
}

#[test]
fn an_ask_has_nothing_to_be_wrong_about_and_a_ticket_and_a_decline_are_read_as_prefixes() {
    let ask = raw(kind::ASK, CHANNEL_SESSION, "", &[1, 2, 3]);
    assert_eq!(
        read_packet(&mut ask.as_slice()).unwrap(),
        Some(Packet::connection(Message::Ask))
    );
    // Bytes after the number of a ticket are for a later version.
    let wire = raw(kind::TICKET, CHANNEL_SESSION, "work", &[0, 0, 1, 2, 9, 9]);
    assert_eq!(
        read_packet(&mut wire.as_slice()).unwrap(),
        Some(work(Message::Ticket(258)))
    );
    // A number cut short and a decline without one are malformed, which costs
    // that message alone.
    for (k, short) in [
        (kind::TICKET, &[0, 0, 1][..]),
        (kind::DECLINE, &[]),
        (kind::DECLINE, &[0, 0]),
    ] {
        let mut wire = raw(k, CHANNEL_SESSION, "", short);
        wire.extend(fixture("ping"));
        let mut reader = wire.as_slice();
        assert!(read_packet(&mut reader).unwrap_err().is_recoverable());
        assert_eq!(
            read_packet(&mut reader).unwrap(),
            Some(session_message("ping")),
            "the frame after it is read"
        );
    }
    // A decline without text is one, and text that is not UTF-8 is read as the
    // replacement characters, as the text of an `error` is.
    let wire = raw(kind::DECLINE, CHANNEL_SESSION, "", &[0, 0, 0, 3, 0xff]);
    assert_eq!(
        read_packet(&mut wire.as_slice()).unwrap(),
        Some(Packet::connection(Message::Decline {
            ticket: 3,
            text: "\u{fffd}".into()
        }))
    );
    let wire = raw(kind::DECLINE, CHANNEL_SESSION, "", &[0, 0, 0, 3]);
    assert_eq!(
        read_packet(&mut wire.as_slice()).unwrap(),
        Some(Packet::connection(Message::Decline {
            ticket: 3,
            text: String::new()
        }))
    );
}

#[test]
fn an_open_that_answers_a_ticket_is_told_by_the_ticket_and_the_sender_of_mail_by_having_nothing() {
    use stream::Message as S;
    use stream::kind as k;
    let read = |payload: &[u8]| {
        let wire = raw(k::OPEN, CHANNEL_STREAM, "", payload);
        match read_incoming(&mut wire.as_slice()).unwrap().unwrap() {
            Incoming::Stream { packet, .. } => packet.message,
            other => panic!("not a stream message: {other:?}"),
        }
    };
    // The ticket wins over everything else in the block, and a key from the
    // future is skipped: nothing else is read.
    let mut payload = vec![0, 0, 0, 5, 0, 0, 0, 9];
    payload.extend([0, 1, 0, 0, 0, 2, 0x0b, 0xb8]); // a port
    payload.extend([0, 2, 0, 0, 0, 1, b'h']); // a host
    payload.extend([0, 3, 0, 0, 0, 2, b's', b'h']); // an argument
    payload.extend([0x0f, 0xff, 0, 0, 0, 1, 9]);
    payload.extend([0, 6, 0, 0, 0, 4, 0, 0, 0x01, 0x02]);
    assert_eq!(
        read(&payload),
        S::OpenTicket {
            window: 9,
            ticket: 258
        }
    );
    // The last of a repeated ticket wins.
    payload.extend([0, 6, 0, 0, 0, 4, 0, 0, 0, 3]);
    assert_eq!(
        read(&payload),
        S::OpenTicket {
            window: 9,
            ticket: 3
        }
    );
    // A ticket of the wrong length is malformed, which costs that message alone.
    let mut wire = raw(
        k::OPEN,
        CHANNEL_STREAM,
        "",
        &[0, 0, 0, 5, 0, 0, 0, 9, 0, 6, 0, 0, 0, 2, 0, 1],
    );
    wire.extend(fixture("stream_open"));
    assert!(
        read_incoming(&mut wire.as_slice())
            .unwrap_err()
            .is_recoverable()
    );

    // The sender of mail opens with nothing in the block, which is read as an
    // `open` with no port and no host, as it always was.
    assert_eq!(
        read(&fixture("stream_open_mail")[7..]),
        S::open_mail(262_144)
    );
    assert_eq!(
        S::open_mail(262_144),
        S::Open {
            window: 262_144,
            port: 0,
            host: String::new()
        }
    );
    // Nothing else with no port or no host can be written.
    let write = |message: S| {
        stream::write_packet(
            &mut Vec::new(),
            &stream::Packet::new(1, message),
            Side::Client,
        )
    };
    let open = |port: u16, host: &str| S::Open {
        window: 1,
        port,
        host: host.into(),
    };
    assert!(write(open(0, "")).is_ok());
    assert!(write(open(0, "example.org")).is_err(), "port 0");
    assert!(write(open(80, "")).is_err(), "no host");
    // And only the client opens, with a ticket as with a target.
    assert!(
        stream::write_packet(
            &mut Vec::new(),
            &stream::Packet::new(
                1,
                S::OpenTicket {
                    window: 1,
                    ticket: 1
                }
            ),
            Side::Server
        )
        .is_err()
    );
}

#[test]
fn a_conversation_of_mail_reads_in_order_however_it_arrives() {
    // (fixture, on channel 1)
    let order = [
        ("features_tickets", false),
        ("ping", false),
        ("features_reply_mailbox", false),
        ("pong", false),
        ("ask", false),
        ("ask_reply", false),
        ("stream_open_mail", true),
        ("ticket", false),
        ("stream_open_ticket", true),
        ("stream_opened", true),
        ("stream_client_data", true),
        ("ticket_none", false),
        ("decline", false),
        ("stream_failed_declined", true),
        ("ask_terminal", false),
        ("ask_reply_none", false),
        ("stream_server_close", true),
    ];
    let mut wire = Vec::new();
    let mut all = Vec::new();
    let mut sessions = Vec::new();
    for (name, on_stream) in order {
        wire.extend(fixture(name));
        if on_stream {
            let (from, packet) = stream_message(name);
            all.push(Incoming::Stream { from, packet });
        } else {
            let packet = session_message(name);
            sessions.push(packet.clone());
            all.push(Incoming::Session(packet));
        }
    }
    for piece in [1, 3, 13, 4096] {
        let mut incoming = IncomingDecoder::new();
        let mut old = PacketDecoder::new();
        let (mut got_all, mut got_sessions) = (Vec::new(), Vec::new());
        for chunk in wire.chunks(piece) {
            incoming.push(chunk);
            while let Some(i) = incoming.next_incoming().unwrap() {
                got_all.push(i);
            }
            old.push(chunk);
            while let Some(p) = old.next_packet().unwrap() {
                got_sessions.push(p);
            }
        }
        assert_eq!(got_all, all, "pieces of {piece}");
        assert_eq!(got_sessions, sessions, "pieces of {piece}");
        assert_eq!(incoming.pending() + old.pending(), 0, "pieces of {piece}");
    }
}

#[test]
fn the_keys_have_the_numbers_kitty_gives_them() {
    // The four kitty's protocol writes in its sequences.
    assert_eq!(
        [key::TAB, key::ENTER, key::ESCAPE, key::BACKSPACE],
        [9, 13, 27, 127]
    );
    // The others, in the private use area, as kitty's own key table numbers them
    // (0xe000 is escape; enter, tab and backspace take the next three, and these
    // follow).
    assert_eq!(
        [
            key::INSERT,
            key::DELETE,
            key::LEFT,
            key::RIGHT,
            key::UP,
            key::DOWN,
            key::PAGE_UP,
            key::PAGE_DOWN,
            key::HOME,
            key::END,
        ],
        [
            57348, 57349, 57350, 57351, 57352, 57353, 57354, 57355, 57356, 57357
        ]
    );
    // F13 and F35 are the numbers kitty's protocol publishes; F1 to F12 come before
    // F13 in the table and are one after the other.
    assert_eq!(key::function(1), Some(57364));
    assert_eq!(key::function(12), Some(57375));
    assert_eq!(key::function(13), Some(57376));
    assert_eq!(key::function(35), Some(57398));
    assert_eq!(key::function(0), None);
    assert_eq!(key::function(36), None);
    // The modifiers are kitty's bits.
    assert_eq!(
        [
            key::mods::SHIFT,
            key::mods::ALT,
            key::mods::CTRL,
            key::mods::SUPER,
            key::mods::HYPER,
            key::mods::META
        ],
        [1, 2, 4, 8, 16, 32]
    );
}

#[test]
fn the_kinds_of_driving_go_the_way_their_halves_say() {
    for (k, from_server) in [
        (kind::WRITE, false),
        (kind::READ, false),
        (kind::WAIT, false),
        (kind::LABEL, false),
        (kind::SCREEN_TEXT, true),
    ] {
        assert_eq!(k >= 0x80, from_server, "{k:#04x}");
    }
}

#[test]
fn a_write_is_one_list_of_text_and_keys_in_the_order_they_were_made() {
    // Two keys of the block, each many times and mixed: the order is the order
    // of the strokes, which the rule that the last entry of a key wins must not
    // touch.
    let strokes = vec![
        Stroke::text("a"),
        Stroke::key(key::TAB, 0),
        Stroke::text(vec![0xff, 0x00, 0xfe]), // bytes, not text a reader may refuse
        Stroke::text(""),
        Stroke::key(u32::from('x'), key::mods::CTRL | key::mods::ALT),
        Stroke::text("b"),
        Stroke::key(key::ENTER, 0),
    ];
    let wire = |message: Message| {
        let mut out = Vec::new();
        write_packet(&mut out, &work(message)).unwrap();
        out
    };
    let bytes = wire(Message::Write(strokes.clone()));
    assert_eq!(
        read_packet(&mut bytes.as_slice()).unwrap(),
        Some(work(Message::Write(strokes)))
    );

    // Nothing to do is a block with nothing in it, and reads back as that.
    let nothing = wire(Message::Write(Vec::new()));
    assert_eq!(&nothing[3..7], [0, 0, 0, 0]);
    assert_eq!(
        read_packet(&mut nothing.as_slice()).unwrap(),
        Some(work(Message::Write(Vec::new())))
    );

    // A key the reader does not know is skipped where it stands, and bytes after the
    // five of a key press are for a later version.
    let mut block = vec![0, 1, 0, 0, 0, 1, b'a'];
    block.extend([0x0f, 0xff, 0, 0, 0, 2, 9, 9]); // from the future
    block.extend([0, 2, 0, 0, 0, 7, 0, 0, 0, 13, 0, 0xde, 0xad]); // Enter, and more
    block.extend([0, 1, 0, 0, 0, 1, b'b']);
    let wire = raw(kind::WRITE, 0, "work", &block);
    assert_eq!(
        read_packet(&mut wire.as_slice()).unwrap(),
        Some(work(Message::Write(vec![
            Stroke::text("a"),
            Stroke::key(key::ENTER, 0),
            Stroke::text("b"),
        ])))
    );

    // A key press cut short, and an entry cut short, are malformed and cost that
    // message and nothing after it.
    let mut stream = raw(kind::WRITE, 0, "work", &[0, 2, 0, 0, 0, 4, 0, 0, 0, 13]);
    stream.extend(raw(kind::WRITE, 0, "work", &[0, 1, 0, 0, 0, 9, b'a']));
    stream.extend(fixture("wait"));
    let mut reader = stream.as_slice();
    for n in 0..2 {
        assert!(
            read_packet(&mut reader).unwrap_err().is_recoverable(),
            "message {n}"
        );
    }
    assert_eq!(read_packet(&mut reader).unwrap(), Some(work(Message::Wait)));
}

#[test]
fn a_read_is_the_rows_on_screen_unless_it_asks_for_more() {
    // What is not asked is not written: the first request is the empty one.
    let plain = Capture::default();
    assert!(!plain.picture && plain.history == 0 && !plain.unwrap);
    assert_eq!(plain.to_block(), Vec::<u8>::new());
    assert_eq!(Capture::from_block(&[]).unwrap(), plain);

    // Each key on its own, a key from the future skipped, and a flag byte read by
    // its first bit alone.
    let history = Capture::from_block(&[0, 2, 0, 0, 0, 4, 0, 0, 0, 7]).unwrap();
    assert_eq!(
        history,
        with(Capture::default(), |c| {
            c.history = 7;
        })
    );
    let block = [
        0x0f,
        0xff,
        0,
        0,
        0,
        1,
        9, // from the future
        0,
        1,
        0,
        0,
        0,
        1,
        0b1111_1111, // picture, and bits nobody defined
        0,
        3,
        0,
        0,
        0,
        1,
        0b1111_1110, // no unwrap: bit 0 is clear
    ];
    assert_eq!(
        Capture::from_block(&block).unwrap(),
        with(Capture::default(), |c| {
            c.picture = true;
        })
    );
    // A value of the wrong size is malformed.
    for block in [
        &[0, 1, 0, 0, 0, 2, 1, 1][..],
        &[0, 2, 0, 0, 0, 2, 0, 1],
        &[0, 3, 0, 0, 0, 0],
    ] {
        assert!(Capture::from_block(block).is_err(), "{block:?}");
    }
    // The request round-trips whole.
    let all = with(Capture::default(), |c| {
        c.picture = true;
        c.history = u32::MAX;
        c.unwrap = true;
    });
    assert_eq!(Capture::from_block(&all.to_block()).unwrap(), all);
}

#[test]
fn a_label_is_sixty_four_bytes_at_most_and_an_empty_one_is_none() {
    let label = |text: &str| work(Message::Label(text.into()));
    let new = |text: &str| {
        Packet::connection(Message::New(with(NewSession::default(), |n| {
            n.label = Some(text.into());
        })))
    };
    let too_long = "x".repeat(v1::MAX_LABEL + 1);
    let longest = "x".repeat(v1::MAX_LABEL);
    // Bytes, not characters: 32 two-byte characters fit, 33 do not.
    let wide = "é".repeat(32);
    for packet in [label(&longest), label(&wide), new(&longest), new(&wide)] {
        let mut wire = Vec::new();
        write_packet(&mut wire, &packet).unwrap();
        assert_eq!(read_packet(&mut wire.as_slice()).unwrap(), Some(packet));
    }
    for packet in [label(&too_long), label(&"é".repeat(33)), new(&too_long)] {
        assert!(
            write_packet(&mut Vec::new(), &packet).is_err(),
            "a sender cannot write {packet:?}"
        );
    }
    // A receiver refuses what it is sent, and the frame after it is read.
    let mut stream = raw(kind::LABEL, 0, "work", too_long.as_bytes());
    stream.extend(raw(kind::LABEL, 0, "work", &[0xff, 0xfe])); // not UTF-8
    let mut entry = vec![0, 7];
    entry.extend((too_long.len() as u32).to_be_bytes());
    entry.extend(too_long.as_bytes());
    stream.extend(raw(kind::NEW, 0, "", &entry));
    stream.extend(fixture("wait"));
    let mut reader = stream.as_slice();
    for n in 0..3 {
        assert!(
            read_packet(&mut reader).unwrap_err().is_recoverable(),
            "message {n}"
        );
    }
    assert_eq!(read_packet(&mut reader).unwrap(), Some(work(Message::Wait)));

    // Empty clears: it is no bytes at all, and a new with an empty label is the
    // new with none.
    assert_eq!(&fixture("label_clear")[3..7], [0, 0, 0, 0]);
    let mut wire = Vec::new();
    write_packet(&mut wire, &new("")).unwrap();
    assert_eq!(
        read_packet(&mut wire.as_slice()).unwrap(),
        Some(Packet::connection(Message::New(NewSession::default())))
    );
    assert_eq!(wire, fixture("new_empty"));
}

#[test]
fn a_status_and_a_new_are_read_with_a_label_or_without_one() {
    // The statuses recorded before labels read as having none, and write the
    // same bytes: the label is not in them.
    let Some(Message::StatusReply(old)) = read_packet(&mut fixture("status_reply").as_slice())
        .unwrap()
        .map(|packet| packet.message)
    else {
        panic!("not a status reply");
    };
    assert_eq!(old.label, None);
    let keys_of =
        |block: &[u8]| -> Vec<u16> { tlv::entries(block).map(|entry| entry.unwrap().0).collect() };
    assert!(!keys_of(&old.to_block()).contains(&16));

    // The block with the key appended reads with the label, and one from before
    // the key is the same as one with no label said.
    let mut block = old.to_block();
    block.extend_from_slice(&[0, 16, 0, 0, 0, 5]);
    block.extend_from_slice(b"tests");
    let labeled = with(old.clone(), |s| s.label = Some("tests".into()));
    assert_eq!(Status::from_block(&block).unwrap(), labeled);
    assert_eq!(Status::from_block(&labeled.to_block()).unwrap(), labeled);
    // An empty label is none, written as nothing, and the last entry wins.
    let empty = with(old.clone(), |s| s.label = Some(String::new()));
    assert_eq!(empty.to_block(), old.to_block());
    block.extend_from_slice(&[0, 16, 0, 0, 0, 0]);
    assert_eq!(Status::from_block(&block).unwrap(), old);
    // A status carries what the server says: a label longer than a client may set
    // is read, not refused, so one file cannot make a `list` fail.
    let long = with(old.clone(), |s| s.label = Some("y".repeat(200)));
    assert_eq!(Status::from_block(&long.to_block()).unwrap(), long);
    // A label that is not text is malformed.
    let mut bad = old.to_block();
    bad.extend_from_slice(&[0, 16, 0, 0, 0, 1, 0xff]);
    assert!(Status::from_block(&bad).is_err());

    // The same of a `new`: the recorded ones have no label.
    for name in ["new_empty", "new_full", "new_history"] {
        let Some(Message::New(new)) = read_packet(&mut fixture(name).as_slice())
            .unwrap()
            .map(|packet| packet.message)
        else {
            panic!("{name} is not a new");
        };
        assert_eq!(new.label, None, "{name}");
    }
    // And a list of statuses carries each one's label.
    let list = Packet::connection(Message::ListReply(vec![
        running_work(),
        with(broken_build(), |s| s.label = Some("build".into())),
    ]));
    let mut wire = Vec::new();
    write_packet(&mut wire, &list).unwrap();
    assert_eq!(read_packet(&mut wire.as_slice()).unwrap(), Some(list));
}

#[test]
fn a_screen_text_piece_cannot_outgrow_a_frame_and_may_end_anywhere() {
    let piece = |data: Vec<u8>| {
        work(Message::ScreenText {
            offset: 7,
            cols: 80,
            rows: 24,
            more: true,
            data,
        })
    };
    let biggest = piece(vec![b'x'; v1::MAX_SCREEN_TEXT]);
    let mut wire = Vec::new();
    write_packet(&mut wire, &biggest).unwrap();
    assert_eq!(read_packet(&mut wire.as_slice()).unwrap(), Some(biggest));
    assert!(write_packet(&mut Vec::new(), &piece(vec![b'x'; v1::MAX_SCREEN_TEXT + 1])).is_err());

    // The text is the pieces put together; a piece may end in the middle of a
    // character, so a piece is bytes and not text.
    let whole = "héllo\nwörld\n".as_bytes();
    let (first, second) = whole.split_at(2); // between the two bytes of é
    assert!(std::str::from_utf8(first).is_err());
    let mut wire = Vec::new();
    for data in [first.to_vec(), second.to_vec()] {
        write_packet(&mut wire, &piece(data)).unwrap();
    }
    let mut reader = wire.as_slice();
    let mut text = Vec::new();
    while let Some(Packet {
        message: Message::ScreenText { data, .. },
        ..
    }) = read_packet(&mut reader).unwrap()
    {
        text.extend(data);
    }
    assert_eq!(text, whole);

    // A piece with less than its header is malformed.
    let wire = raw(kind::SCREEN_TEXT, 0, "work", &[0; 12]);
    assert!(
        read_packet(&mut wire.as_slice())
            .unwrap_err()
            .is_recoverable()
    );
}
