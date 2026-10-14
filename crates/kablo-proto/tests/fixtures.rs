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

use kablo_proto::frame::{self, RawFrame};
use kablo_proto::hello::Reply;
use kablo_proto::v1::{
    self, Exit, Message, NewSession, Packet, PacketDecoder, Size, State, Status, kind, read_packet,
    write_packet,
};
use kablo_proto::{CHANNEL_SESSION, Error, SessionId, WIRE};

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

fn running_work() -> Status {
    Status {
        id: id("work"),
        pid: 4242,
        program: Some(b"vim".to_vec()),
        cwd: Some(b"/home/u/src".to_vec()),
        size: Size {
            cols: 120,
            rows: 40,
            xpixel: 960,
            ypixel: 800,
        },
        clients: 2,
        state: State::RUNNING,
        offset: 123_456,
        retained_from: 100,
        exit: None,
        version: "0.1.0".into(),
    }
}

fn broken_build() -> Status {
    Status {
        pid: 4300,
        size: Size {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        },
        state: State::BROKEN,
        offset: 77,
        version: "0.1.0".into(),
        ..Status::new(id("build-1"))
    }
}

/// Every message fixture and what it says.
fn messages() -> Vec<(&'static str, Packet)> {
    let full_new = NewSession {
        argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"echo hi".to_vec()],
        env: vec![
            (b"TERM".to_vec(), b"xterm-256color".to_vec()),
            (b"FOO".to_vec(), b"a=b".to_vec()),
        ],
        cwd: Some(b"/tmp".to_vec()),
        size: Some(Size {
            cols: 80,
            rows: 24,
            xpixel: 0,
            ypixel: 0,
        }),
        journal_size: Some(1 << 20),
    };
    vec![
        ("attach", work(Message::Attach { offset: 0x1000 })),
        (
            "attach_end",
            work(Message::Attach {
                offset: v1::OFFSET_END,
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
            "ping",
            Packet::connection(Message::Ping(b"pingdata".to_vec())),
        ),
        (
            "attached",
            work(Message::Attached {
                offset: 0x1000,
                truncated: false,
            }),
        ),
        (
            "attached_truncated",
            work(Message::Attached {
                offset: 0x2000,
                truncated: true,
            }),
        ),
        (
            "output",
            work(Message::Output {
                offset: 0x1000,
                data: b"hello\r\n".to_vec(),
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
            "pong",
            Packet::connection(Message::Pong(b"pingdata".to_vec())),
        ),
        ("status_reply", work(Message::StatusReply(running_work()))),
        (
            "status_reply_ended",
            work(Message::StatusReply(Status {
                pid: 4242,
                size: Size {
                    cols: 80,
                    rows: 24,
                    xpixel: 0,
                    ypixel: 0,
                },
                state: State::ENDED,
                offset: 5000,
                exit: Some(Exit::Code(0)),
                ..Status::new(id("work"))
            })),
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
                Message::Created(Status {
                    pid: 4300,
                    size: Size {
                        cols: 80,
                        rows: 24,
                        xpixel: 0,
                        ypixel: 0,
                    },
                    version: "0.1.0".into(),
                    ..Status::new(id("build-1"))
                }),
            ),
        ),
    ]
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
        .chain(HANDSHAKE.map(str::to_owned))
        .collect();
    assert_eq!(
        named.len(),
        messages().len() + HANDSHAKE.len(),
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
        ATTACHED,
        OUTPUT,
        ENDED,
        ERROR,
        PONG,
        STATUS_REPLY,
        LIST_REPLY,
        CREATED,
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
    stream.extend(raw(kind::DETACH, 1, "work", &[1, 2, 3])); // a known kind on a channel we do not have
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
        nul(NewSession {
            argv: vec![b"a\0b".to_vec()],
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        nul(NewSession {
            env: vec![(b"A=B".to_vec(), b"c".to_vec())],
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        nul(NewSession {
            env: vec![(b"".to_vec(), b"c".to_vec())],
            ..Default::default()
        })
        .is_err()
    );
    assert!(
        nul(NewSession {
            cwd: Some(Vec::new()),
            ..Default::default()
        })
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
        Some(work(Message::Attach { offset: 0x1000 }))
    );
    assert_eq!(
        read_packet(&mut reader).unwrap(),
        Some(work(Message::Terminate { force: true }))
    );
    assert_eq!(
        read_packet(&mut reader).unwrap(),
        Some(work(Message::Attached {
            offset: 0x2000,
            truncated: false
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
fn a_status_with_only_an_id_reads_as_zeros() {
    let mut block = Vec::new();
    block.extend_from_slice(&[0, 1, 0, 0, 0, 1, b'a']);
    let status = Status::from_block(&block).unwrap();
    assert_eq!(status, Status::new(id("a")));
    assert!(Status::from_block(&[]).is_err());
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
    let seeds: Vec<Vec<u8>> = messages().iter().map(|(n, _)| fixture(n)).collect();
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
        let _ = Status::from_block(&bytes);
        let _ = WIRE.read_hello(&mut bytes.as_slice());
    }
}
