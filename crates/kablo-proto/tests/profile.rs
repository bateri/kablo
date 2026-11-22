//! The bytes of the profile protocol, version 1, message by message, and of the files a profile
//! is kept in.
//!
//! The files in `fixtures/profile-v1` are the format: annotated hex. Each case here compares in
//! **both** directions (the codec's output against the file, and the file's meaning against the
//! expected message). `BLESS=1 cargo test -p kablo-proto --test profile` writes the files from the
//! codec, with comments that say what each byte is; the result is read against
//! `docs/PROTOCOL.md` and committed, and from then on the file is the format. The test at the end
//! checks a few messages against bytes written out by hand from the description, so that the
//! codec is not the only witness. A fixture is never edited once a version has shipped; a change
//! of format is `fixtures/profile-v2`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use kablo_proto::frame::{self, RawFrame};
use kablo_proto::profile::{
    self as wire, BlobRef, Body, Client, Entry, Failure, LogHeader, Mode, Open, Opened, Part, Put,
    PutReply, PutResult, Server, StateHeader, Stored, WIRE, kind,
};
use kablo_proto::{Error, tlv};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/profile-v1")
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

fn bytes(of: &[u8]) -> String {
    of.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A frame's bytes, said line by line: its header, then each entry of its block.
fn annotate_frame(name: &str, what: &str, raw: &[u8]) -> String {
    let mut out = format!("# {name}: {what}\n");
    let (head, rest) = raw.split_at(frame::HEADER_LEN);
    let session = usize::from(head[2]);
    let (_, payload) = rest.split_at(session);
    out += &format!("{:<36} # kind 0x{:02x}\n", bytes(&head[..1]), head[0]);
    out += &format!("{:<36} # channel {}\n", bytes(&head[1..2]), head[1]);
    out += &format!("{:<36} # session id length {session}\n", bytes(&head[2..3]));
    out += &format!(
        "{:<36} # payload length {}\n",
        bytes(&head[3..7]),
        payload.len()
    );
    for entry in tlv::entries(payload) {
        let (key, value) = entry.unwrap();
        let mut line = key.to_be_bytes().to_vec();
        line.extend_from_slice(&(value.len() as u32).to_be_bytes());
        line.extend_from_slice(value);
        out += &format!("{:<36} # key {key}, length {}\n", bytes(&line), value.len());
    }
    out
}

/// A file of the format that is not a frame: said part by part (a length, then what it is; the
/// last part is the rest).
fn annotate_plain(name: &str, what: &str, raw: &[u8], parts: &[(usize, &str)]) -> String {
    let mut out = format!("# {name}: {what}\n");
    let mut rest = raw;
    for (len, label) in parts {
        let (now, later) = rest.split_at((*len).min(rest.len()));
        out += &format!("{:<36} # {label}\n", bytes(now));
        rest = later;
    }
    assert!(rest.is_empty(), "{name}: bytes the parts do not name");
    out
}

/// What reading a file's bytes is to say.
type Check = Box<dyn Fn(&[u8])>;

/// What a fixture holds, once its bytes are known.
struct Case {
    name: &'static str,
    what: &'static str,
    /// The bytes the codec writes.
    bytes: Vec<u8>,
    /// Whether the bytes are a frame (said by its entries) or not.
    framed: bool,
    /// For what is not a frame: its parts, by length.
    parts: &'static [(usize, &'static str)],
    /// What reading the file's bytes has to say.
    check: Check,
}

fn client(name: &'static str, what: &'static str, message: Client) -> Case {
    let mut out = Vec::new();
    wire::write_client(&mut out, &message).unwrap();
    Case {
        name,
        what,
        bytes: out,
        framed: true,
        parts: &[],
        check: Box::new(move |file| {
            assert_eq!(
                wire::read_client(&mut &file[..]).unwrap().as_ref(),
                Some(&message),
                "{name}"
            );
        }),
    }
}

fn server(name: &'static str, what: &'static str, message: Server) -> Case {
    let mut out = Vec::new();
    wire::write_server(&mut out, &message).unwrap();
    Case {
        name,
        what,
        bytes: out,
        framed: true,
        parts: &[],
        check: Box::new(move |file| {
            assert_eq!(
                wire::read_server(&mut &file[..]).unwrap().as_ref(),
                Some(&message),
                "{name}"
            );
        }),
    }
}

const SUM_A: [u8; 32] = [0xaa; 32];
const SUM_B: [u8; 32] = [0xbb; 32];
const CLIENT: [u8; 16] = [0xc1; 16];
const DEPOT: [u8; 16] = [0xd0; 16];

fn cookie_body() -> Body {
    Body {
        kind: kind::COOKIE,
        key: b"\x00\x01\x00\x00\x00\x0c.example.com".to_vec(),
        value: b"session=abc".to_vec(),
        expires: Some(1_800_000_000_000),
        ..Body::default()
    }
}

fn indexed_db_body() -> Body {
    Body {
        kind: kind::INDEXED_DB,
        key: b"http://example.com/".to_vec(),
        value: b"{\"files\":1}".to_vec(),
        linked: vec![
            BlobRef {
                sha256: SUM_A,
                size: 70_000,
            },
            BlobRef {
                sha256: SUM_B,
                size: 3,
            },
        ],
        ..Body::default()
    }
}

fn big_value_body() -> Body {
    Body {
        kind: kind::LOCAL_STORAGE,
        key: b"big".to_vec(),
        value_blob: Some(BlobRef {
            sha256: SUM_A,
            size: 100_000,
        }),
        ..Body::default()
    }
}

fn deleted_body() -> Body {
    Body {
        kind: kind::COOKIE,
        key: b"gone".to_vec(),
        deleted: true,
        ..Body::default()
    }
}

fn entry(seq: u64, body: Body) -> Entry {
    Entry {
        seq,
        body,
        writer: CLIENT,
        time: 1_700_000_000_000,
    }
}

fn cases() -> Vec<Case> {
    let open = Open {
        app: "bateri".into(),
        name: "main".into(),
        client: CLIENT,
        since: 41,
        depot: Some(DEPOT),
    };
    let mut cases = vec![
        client(
            "open",
            "the client opens a profile it has seen before",
            Client::Open(open.clone()),
        ),
        client(
            "open_first",
            "the client opens a profile it never joined: no `since`, no depot",
            Client::Open(Open {
                since: 0,
                depot: None,
                ..open
            }),
        ),
        client(
            "put",
            "a cookie record written over the one it saw (`base` 7)",
            Client::Put(Put {
                req: 9,
                body: cookie_body(),
                base: 7,
            }),
        ),
        client(
            "put_blobs",
            "a record that points at two blobs, written where there was none (`base` 0)",
            Client::Put(Put {
                req: 10,
                body: indexed_db_body(),
                base: 0,
            }),
        ),
        client(
            "put_value_blob",
            "a record whose value is in a blob",
            Client::Put(Put {
                req: 11,
                body: big_value_body(),
                base: 3,
            }),
        ),
        client(
            "put_deleted",
            "a deletion: no value",
            Client::Put(Put {
                req: 12,
                body: deleted_body(),
                base: 5,
            }),
        ),
        client(
            "have",
            "which of these blobs does the server lack",
            Client::Have(vec![SUM_A, SUM_B]),
        ),
        client(
            "blob",
            "a part of a blob of 5 bytes: bytes 2 to 4",
            Client::Blob(Part {
                sha256: SUM_A,
                total: 5,
                start: 2,
                bytes: b"cde".to_vec(),
            }),
        ),
        client(
            "get_blob",
            "the client asks for a blob",
            Client::GetBlob(SUM_B),
        ),
        server(
            "opened",
            "the server answers an open: a difference follows",
            Server::Opened(Opened {
                head: 50,
                horizon: 12,
                mode: Mode::Diff,
                depot: DEPOT,
            }),
        ),
        server(
            "opened_full",
            "the server answers with the whole profile (mode 1)",
            Server::Opened(Opened {
                head: 50,
                horizon: 12,
                mode: Mode::Full,
                depot: DEPOT,
            }),
        ),
        server(
            "opened_full_replace",
            "the whole profile, and what the client has that it lacks goes (mode 2)",
            Server::Opened(Opened {
                head: 50,
                horizon: 12,
                mode: Mode::FullReplace,
                depot: DEPOT,
            }),
        ),
        server(
            "record",
            "a record as the server has it",
            Server::Record(entry(42, cookie_body())),
        ),
        server(
            "record_blobs",
            "a record with blobs",
            Server::Record(entry(43, indexed_db_body())),
        ),
        server(
            "record_deleted",
            "a deletion as the server has it",
            Server::Record(entry(44, deleted_body())),
        ),
        server(
            "caught_up",
            "the records that bring the client up to date are done",
            Server::CaughtUp { head: 50 },
        ),
        server(
            "put_reply",
            "the put is written: its number",
            Server::PutReply(PutReply {
                req: 9,
                result: PutResult::Done,
                seq: 51,
            }),
        ),
        server(
            "put_reply_conflict",
            "the record changed since the base: its number now",
            Server::PutReply(PutReply {
                req: 9,
                result: PutResult::Conflict,
                seq: 49,
            }),
        ),
        server(
            "put_reply_missing_blob",
            "a blob it points at is not here",
            Server::PutReply(PutReply {
                req: 10,
                result: PutResult::MissingBlob,
                seq: 0,
            }),
        ),
        server(
            "have_reply",
            "the blobs asked about that are not here",
            Server::HaveReply(vec![SUM_B]),
        ),
        server(
            "blob_from_server",
            "a part of a blob that was asked for",
            Server::Blob(Part {
                sha256: SUM_B,
                total: 3,
                start: 0,
                bytes: b"abc".to_vec(),
            }),
        ),
        server(
            "blob_ok",
            "every part of a blob came and its sum held",
            Server::BlobOk(SUM_A),
        ),
        server(
            "blob_missing",
            "the blob asked for is not here",
            Server::BlobMissing(SUM_B),
        ),
        server(
            "error",
            "the server turns the open down (this server keeps no profiles) and ends",
            Server::Error(Failure {
                code: wire::code::OFF,
                text: "this server does not keep profiles".into(),
            }),
        ),
    ];

    let state_header = StateHeader {
        depot: DEPOT,
        head: 50,
        horizon: 12,
    };
    cases.push(Case {
        name: "state_header",
        what: "the head of the state file: signature, format 1, depot, head, horizon",
        bytes: wire::encode_state_header(&state_header),
        framed: false,
        parts: &[
            (20, "signature KABLO-PROFILE-STATE, then a zero"),
            (1, "format 1"),
            (16, "depot"),
            (8, "head 50"),
            (8, "horizon 12"),
        ],
        check: Box::new(move |file| {
            assert_eq!(
                wire::read_state_header(&mut &file[..]).unwrap(),
                Stored::Current(state_header.clone())
            );
        }),
    });
    let state_entry = entry(42, cookie_body());
    cases.push(Case {
        name: "state_entry",
        what: "an entry of the state file: the length (u32) and the payload of a record",
        bytes: wire::encode_state_entry(&state_entry),
        framed: false,
        parts: &[
            (4, "length of the payload"),
            (usize::MAX, "the payload of a record: a block of entries"),
        ],
        check: Box::new(move |file| {
            assert_eq!(
                wire::read_state_entry(&mut &file[..]).unwrap(),
                Some(state_entry.clone())
            );
        }),
    });
    cases.push(Case {
        name: "log_header",
        what: "the head of the log: signature, format 1, generation 7",
        bytes: wire::encode_log_header(&LogHeader { generation: 7 }),
        framed: false,
        parts: &[
            (18, "signature KABLO-PROFILE-LOG, then a zero"),
            (1, "format 1"),
            (8, "generation 7"),
        ],
        check: Box::new(|file| {
            assert_eq!(
                wire::read_log_header(&mut &file[..]).unwrap(),
                Stored::Current(LogHeader { generation: 7 })
            );
        }),
    });
    let log_entry = entry(45, indexed_db_body());
    cases.push(Case {
        name: "log_entry",
        what: "an entry of the log: a `record` frame",
        bytes: wire::encode_log_entry(&log_entry).unwrap(),
        framed: true,
        parts: &[],
        check: Box::new(move |file| {
            assert_eq!(
                wire::read_log_entry(&mut &file[..]).unwrap(),
                Some(log_entry.clone())
            );
        }),
    });
    let mut hello = Vec::new();
    WIRE.write_hello(&mut hello).unwrap();
    cases.push(Case {
        name: "hello",
        what: "the client's hello: signature, lowest version, highest version",
        bytes: hello,
        framed: false,
        parts: &[
            (14, "signature KABLO-PROFILE, then a zero"),
            (1, "lowest version"),
            (1, "highest version"),
        ],
        check: Box::new(|file| {
            assert_eq!(WIRE.read_hello(&mut &file[..]).unwrap(), (1, 1));
        }),
    });
    let mut reply = Vec::new();
    WIRE.write_reply(&mut reply, 1).unwrap();
    cases.push(Case {
        name: "hello_reply",
        what: "the server's reply: signature, lowest, highest, selected version",
        bytes: reply,
        framed: false,
        parts: &[
            (14, "signature KABLO-PROFILE, then a zero"),
            (1, "lowest version"),
            (1, "highest version"),
            (1, "selected version"),
        ],
        check: Box::new(|file| {
            let reply = WIRE.read_reply(&mut &file[..]).unwrap();
            assert_eq!((reply.min, reply.max, reply.selected), (1, 1, 1));
        }),
    });
    cases
}

fn annotated(case: &Case) -> String {
    if case.framed {
        annotate_frame(case.name, case.what, &case.bytes)
    } else {
        annotate_plain(case.name, case.what, &case.bytes, case.parts)
    }
}

#[test]
fn every_message_and_file_head_is_its_fixture_in_both_directions() {
    let bless = std::env::var_os("BLESS").is_some();
    if bless {
        std::fs::create_dir_all(dir()).unwrap();
    }
    for case in cases() {
        let path = dir().join(format!("{}.hex", case.name));
        if bless {
            std::fs::write(&path, annotated(&case)).unwrap();
        }
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e} (BLESS=1 writes it)", path.display()));
        let file = parse_hex(&text);
        assert_eq!(file, case.bytes, "{}: what the codec writes", case.name);
        (case.check)(&file);
    }
}

#[test]
fn the_fixtures_are_the_cases_and_nothing_else() {
    let expected: BTreeSet<String> = cases().iter().map(|c| format!("{}.hex", c.name)).collect();
    let found: BTreeSet<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(found, expected);
}

// ---- bytes written out by hand, from the description ----

/// A frame of this protocol, by hand: kind, channel 2, no session id, the payload's length.
fn by_hand(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![kind, 2, 0];
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[test]
fn some_messages_are_what_the_description_says_byte_by_byte() {
    // get_blob: 0x05, one entry: key 1, the sum.
    let mut payload = vec![0, 1, 0, 0, 0, 32];
    payload.extend_from_slice(&[0xbb; 32]);
    let mut got = Vec::new();
    wire::write_client(&mut got, &Client::GetBlob(SUM_B)).unwrap();
    assert_eq!(got, by_hand(0x05, &payload));

    // put_reply: 0x84, key 1 request (u32), key 2 result (u8), key 3 number (u64).
    let payload = [
        0, 1, 0, 0, 0, 4, 0, 0, 0, 9, //
        0, 2, 0, 0, 0, 1, 1, //
        0, 3, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 49,
    ];
    let mut got = Vec::new();
    wire::write_server(
        &mut got,
        &Server::PutReply(PutReply {
            req: 9,
            result: PutResult::Conflict,
            seq: 49,
        }),
    )
    .unwrap();
    assert_eq!(got, by_hand(0x84, &payload));

    // open (first): 0x01, app, name, client, since 0; no depot.
    let mut payload = vec![0, 1, 0, 0, 0, 6];
    payload.extend_from_slice(b"bateri");
    payload.extend_from_slice(&[0, 2, 0, 0, 0, 4]);
    payload.extend_from_slice(b"main");
    payload.extend_from_slice(&[0, 3, 0, 0, 0, 16]);
    payload.extend_from_slice(&[0xc1; 16]);
    payload.extend_from_slice(&[0, 4, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 0]);
    let mut got = Vec::new();
    wire::write_client(
        &mut got,
        &Client::Open(Open {
            app: "bateri".into(),
            name: "main".into(),
            client: CLIENT,
            since: 0,
            depot: None,
        }),
    )
    .unwrap();
    assert_eq!(got, by_hand(0x01, &payload));

    // caught_up: 0x83, key 1 the head.
    let payload = [0, 1, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0, 0, 50];
    let mut got = Vec::new();
    wire::write_server(&mut got, &Server::CaughtUp { head: 50 }).unwrap();
    assert_eq!(got, by_hand(0x83, &payload));

    // The signatures and heads of the files.
    assert_eq!(wire::SIGNATURE, b"KABLO-PROFILE\0");
    assert_eq!(wire::STATE_SIGNATURE, b"KABLO-PROFILE-STATE\0");
    assert_eq!(wire::LOG_SIGNATURE, b"KABLO-PROFILE-LOG\0");
    let head = wire::encode_log_header(&LogHeader { generation: 7 });
    let mut by_hand = b"KABLO-PROFILE-LOG\0".to_vec();
    by_hand.push(1);
    by_hand.extend_from_slice(&7u64.to_be_bytes());
    assert_eq!(head, by_hand);
}

// ---- what a reader does with what it does not know ----

#[test]
fn a_reader_skips_what_it_does_not_know_and_refuses_what_is_cut() {
    let message = Client::GetBlob(SUM_A);
    let mut payload = message.to_frame().payload;
    // An entry of a key that came after this version.
    payload.extend_from_slice(&[0x7f, 0x00, 0, 0, 0, 2, 9, 9]);
    let grown = by_hand(0x05, &payload);
    assert_eq!(
        wire::read_client(&mut &grown[..]).unwrap(),
        Some(message.clone())
    );

    // A kind that came after it, a frame of another channel, then the message.
    let mut stream = by_hand(0x7e, b"later");
    let mut other_channel = by_hand(0x05, b"x");
    other_channel[1] = 9;
    stream.extend_from_slice(&other_channel);
    wire::write_client(&mut stream, &message).unwrap();
    assert_eq!(
        wire::read_client(&mut &stream[..]).unwrap(),
        Some(message.clone())
    );

    // The end of the stream between frames is not an error; the end inside one is.
    assert_eq!(wire::read_client(&mut &[][..]).unwrap(), None);
    let mut cut = Vec::new();
    wire::write_client(&mut cut, &message).unwrap();
    cut.pop();
    assert!(wire::read_client(&mut &cut[..]).is_err());

    // A message that lacks what it needs, and one with a sum of the wrong length.
    let lacking = by_hand(0x01, &[0, 1, 0, 0, 0, 1, b'a']);
    assert!(matches!(
        wire::read_client(&mut &lacking[..]),
        Err(Error::Malformed(_))
    ));
    let mut short = vec![0, 1, 0, 0, 0, 31];
    short.extend_from_slice(&[1; 31]);
    let short = by_hand(0x05, &short);
    assert!(wire::read_client(&mut &short[..]).is_err());

    // Nothing is read past the limit of a frame.
    let mut too_long = vec![0x05, 2, 0];
    too_long.extend_from_slice(&(frame::MAX_PAYLOAD as u32 + 1).to_be_bytes());
    assert!(wire::read_client(&mut &too_long[..]).is_err());
}

#[test]
fn a_file_of_a_newer_format_or_of_another_kind_is_told_apart() {
    let mut newer = wire::STATE_SIGNATURE.to_vec();
    newer.push(wire::STORE_FORMAT + 1);
    newer.extend_from_slice(&[0; 40]);
    assert_eq!(
        wire::read_state_header(&mut &newer[..]).unwrap(),
        Stored::Newer(wire::STORE_FORMAT + 1)
    );
    let mut newer = wire::LOG_SIGNATURE.to_vec();
    newer.push(wire::STORE_FORMAT + 1);
    assert_eq!(
        wire::read_log_header(&mut &newer[..]).unwrap(),
        Stored::Newer(wire::STORE_FORMAT + 1)
    );
    // The log's signature is not the state's, nor the other way.
    let log = wire::encode_log_header(&LogHeader { generation: 1 });
    let state = wire::encode_state_header(&StateHeader {
        depot: DEPOT,
        head: 0,
        horizon: 0,
    });
    assert_eq!(
        wire::read_state_header(&mut &log[..]).unwrap(),
        Stored::Foreign
    );
    assert_eq!(
        wire::read_log_header(&mut &state[..]).unwrap(),
        Stored::Foreign
    );
    assert_eq!(
        wire::read_state_header(&mut &b"not a file of the store, at all"[..]).unwrap(),
        Stored::Foreign
    );
}

#[test]
fn a_frame_of_the_log_is_a_frame_of_the_wire() {
    let one = entry(1, cookie_body());
    let mut file = wire::encode_log_header(&LogHeader { generation: 1 });
    file.extend_from_slice(&wire::encode_log_entry(&one).unwrap());
    let mut reader = &file[..];
    assert_eq!(
        wire::read_log_header(&mut reader).unwrap(),
        Stored::Current(LogHeader { generation: 1 })
    );
    // Read from the stream as the wire's own message.
    let raw: RawFrame = frame::read_frame(&mut reader).unwrap().unwrap();
    assert_eq!(Server::from_frame(&raw).unwrap(), Some(Server::Record(one)));
}
