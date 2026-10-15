//! The bytes of the wire between a warden and its relay, version 1.
//!
//! Same rules as the public wire's fixtures (see `kablo-proto/tests/fixtures.rs`):
//! the files are annotated hex written by hand from the description, compared in
//! both directions, and never edited once a version has shipped. The warden
//! outlives every binary that starts it, so a relay built years later has to
//! understand these bytes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::path::PathBuf;

use kablo::inner::{self, Adopt, Decoder, Exit, INNER, Renewal, ToRelay, ToWarden, Wire};
use kablo_proto::SessionId;
use kablo_proto::hello::Reply;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/inner-v1")
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

fn work() -> SessionId {
    SessionId::new("work").unwrap()
}

fn to_relay() -> Vec<(&'static str, ToRelay)> {
    vec![
        (
            "adopt",
            ToRelay::Adopt(Adopt {
                id: work(),
                child_pid: 4242,
                exit: None,
                warden_version: None,
            }),
        ),
        (
            "adopt_ended",
            ToRelay::Adopt(Adopt {
                id: work(),
                child_pid: 4242,
                exit: Some(Exit::Signal(15)),
                warden_version: None,
            }),
        ),
        (
            "adopt_versioned",
            ToRelay::Adopt(Adopt {
                id: work(),
                child_pid: 4242,
                exit: None,
                warden_version: Some("0.1.0".into()),
            }),
        ),
        ("go", ToRelay::Go),
        ("quit", ToRelay::Quit),
        ("child_ended_code", ToRelay::ChildEnded(Exit::Code(7))),
        ("child_ended_signal", ToRelay::ChildEnded(Exit::Signal(9))),
        (
            "upgrade_failed",
            ToRelay::UpgradeFailed("new relay exited".into()),
        ),
        ("renewed", ToRelay::Renewed("0.2.0".into())),
    ]
}

fn to_warden() -> Vec<(&'static str, ToWarden)> {
    vec![
        ("ready", ToWarden::Ready),
        ("released", ToWarden::Released { offset: 0x2a000 }),
        (
            "upgrade",
            ToWarden::Upgrade {
                path: b"/opt/kablo/bin/kablo".to_vec(),
            },
        ),
        ("terminate", ToWarden::Terminate { force: false }),
        ("terminate_force", ToWarden::Terminate { force: true }),
        ("delivered", ToWarden::Delivered),
        ("can_renew", ToWarden::CanRenew { min: 1, max: 2 }),
    ]
}

fn renewal() -> Renewal {
    Renewal {
        id: work(),
        dir: b"/tmp/kablo-1000".to_vec(),
        shell_pid: 4242,
        master: 3,
        journal: 4,
        listener: 5,
        relay_chan: 6,
        relay_pid: 4300,
        relay_serial: 2,
        relay_binary: b"/opt/kablo/0.2.0/kablo".to_vec(),
        relay_exe: b"/opt/kablo/0.2.0/kablo".to_vec(),
        serials: 2,
        strikes: 1,
        ever_ready: true,
        chunk: 65536,
        delivered: false,
    }
}

fn check<M: Wire + PartialEq + std::fmt::Debug>(name: &str, message: &M) {
    let bytes = fixture(name);
    let mut written = Vec::new();
    inner::write(&mut written, message).unwrap();
    assert_eq!(written, bytes, "{name}: what is written");
    let mut reader = bytes.as_slice();
    assert_eq!(
        inner::read::<M, _>(&mut reader).unwrap().as_ref(),
        Some(message),
        "{name}: what is read"
    );
    assert_eq!(
        inner::read::<M, _>(&mut reader).unwrap(),
        None,
        "{name}: nothing is left over"
    );
    let mut decoder = Decoder::<M>::new();
    for byte in &bytes {
        decoder.push(std::slice::from_ref(byte));
    }
    assert_eq!(
        decoder.next_message().unwrap().as_ref(),
        Some(message),
        "{name}: what is decoded"
    );
}

#[test]
fn every_message_is_its_fixture_byte_for_byte() {
    for (name, message) in to_relay() {
        check(name, &message);
    }
    for (name, message) in to_warden() {
        check(name, &message);
    }
    check("renewal", &renewal());
}

#[test]
fn the_directory_holds_exactly_the_fixtures_the_tests_name() {
    let named: BTreeSet<String> = to_relay()
        .iter()
        .map(|(n, _)| *n)
        .chain(to_warden().iter().map(|(n, _)| *n))
        .chain(["hello", "reply", "renewal"])
        .map(str::to_owned)
        .collect();
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
fn the_handshake_fixtures() {
    let mut hello = Vec::new();
    INNER.write_hello(&mut hello).unwrap();
    assert_eq!(hello, fixture("hello"));
    assert_eq!(
        INNER.read_hello(&mut fixture("hello").as_slice()).unwrap(),
        (1, 1)
    );
    let mut reply = Vec::new();
    INNER.write_reply(&mut reply, 1).unwrap();
    assert_eq!(reply, fixture("reply"));
    assert_eq!(
        INNER.read_reply(&mut fixture("reply").as_slice()).unwrap(),
        Reply {
            selected: 1,
            min: 1,
            max: 1
        }
    );
}

#[test]
fn the_two_wires_do_not_mistake_each_other() {
    // The public signature is not this wire's, and the reverse.
    let public = kablo_proto::WIRE;
    let mut public_hello = Vec::new();
    public.write_hello(&mut public_hello).unwrap();
    assert!(INNER.read_hello(&mut public_hello.as_slice()).is_err());
    assert!(public.read_hello(&mut fixture("hello").as_slice()).is_err());
}

#[test]
fn a_direction_skips_what_the_other_says_and_what_it_does_not_know() {
    // The warden's frames are not the relay's: a reader of the other direction
    // finds nothing it knows, and reads on to the end without an error.
    let mut stream = fixture("go");
    stream.extend(fixture("ready"));
    stream.extend(fixture("quit"));
    let mut reader = stream.as_slice();
    assert_eq!(
        inner::read::<ToWarden, _>(&mut reader).unwrap(),
        Some(ToWarden::Ready)
    );
    let mut reader = stream.as_slice();
    assert_eq!(
        inner::read::<ToRelay, _>(&mut reader).unwrap(),
        Some(ToRelay::Go)
    );
    assert_eq!(
        inner::read::<ToRelay, _>(&mut reader).unwrap(),
        Some(ToRelay::Quit)
    );
    assert_eq!(inner::read::<ToRelay, _>(&mut reader).unwrap(), None);
}

#[test]
fn what_a_later_version_adds_is_ignored() {
    // A key we do not know in an adopt, and a frame of a kind we do not know.
    let mut adopt = fixture("adopt");
    // payload length lives in bytes 3..7; append an unknown entry and fix it.
    let extra = [0x0f, 0xff, 0, 0, 0, 2, 1, 2];
    let len =
        u32::from_be_bytes(adopt[3..7].try_into().unwrap()) + u32::try_from(extra.len()).unwrap();
    adopt[3..7].copy_from_slice(&len.to_be_bytes());
    adopt.extend_from_slice(&extra);
    let mut stream = vec![0x7e, 0, 0, 0, 0, 0, 3, 9, 9, 9];
    stream.extend(adopt);
    let got = inner::read::<ToRelay, _>(&mut stream.as_slice()).unwrap();
    assert_eq!(
        got,
        Some(ToRelay::Adopt(Adopt {
            id: work(),
            child_pid: 4242,
            exit: None,
            warden_version: None,
        }))
    );
}

#[test]
fn an_adopt_from_before_the_warden_said_its_version_reads_it_as_unknown() {
    // The recorded adopts are the bytes a warden of the first build wrote: no
    // version, and they still read.
    for name in ["adopt", "adopt_ended"] {
        let Some(ToRelay::Adopt(adopt)) =
            inner::read::<ToRelay, _>(&mut fixture(name).as_slice()).unwrap()
        else {
            panic!("{name} is not an adopt");
        };
        assert_eq!(adopt.warden_version, None, "{name}");
        assert_eq!(adopt.id, work(), "{name}");
    }
}

#[test]
fn the_new_messages_are_skipped_by_the_direction_they_are_not_for() {
    // Each sits in the other direction's stream as a kind that direction does
    // not know, which is how a build from before them meets them: skipped,
    // and what follows still reads.
    let mut stream = fixture("renewed");
    stream.extend(fixture("can_renew"));
    stream.extend(fixture("go"));
    let mut reader = stream.as_slice();
    assert_eq!(
        inner::read::<ToWarden, _>(&mut reader).unwrap(),
        Some(ToWarden::CanRenew { min: 1, max: 2 })
    );
    assert_eq!(inner::read::<ToWarden, _>(&mut reader).unwrap(), None);
    let mut reader = stream.as_slice();
    assert_eq!(
        inner::read::<ToRelay, _>(&mut reader).unwrap(),
        Some(ToRelay::Renewed("0.2.0".into()))
    );
    assert_eq!(
        inner::read::<ToRelay, _>(&mut reader).unwrap(),
        Some(ToRelay::Go)
    );
}

#[test]
fn a_renewal_carries_every_flag_and_only_a_version_that_is_read() {
    let flipped = Renewal {
        ever_ready: false,
        delivered: true,
        strikes: 0,
        ..renewal()
    };
    let mut bytes = Vec::new();
    inner::write(&mut bytes, &flipped).unwrap();
    assert_eq!(
        inner::read::<Renewal, _>(&mut bytes.as_slice()).unwrap(),
        Some(flipped)
    );

    // The version is the first entry's value: after the frame's 7 bytes and the
    // entry's 6.
    let mut newer = fixture("renewal");
    newer[13] = inner::RENEWAL_VERSION + 1;
    assert!(
        inner::read::<Renewal, _>(&mut newer.as_slice())
            .unwrap_err()
            .is_recoverable()
    );
    let mut older = fixture("renewal");
    older[13] = inner::RENEWAL_MIN_VERSION - 1;
    assert!(inner::read::<Renewal, _>(&mut older.as_slice()).is_err());

    // A renewal is its own kind: neither direction of the wire reads it, and a
    // reader of it finds nothing in the messages.
    assert_eq!(
        inner::read::<ToRelay, _>(&mut fixture("renewal").as_slice()).unwrap(),
        None
    );
    assert_eq!(
        inner::read::<ToWarden, _>(&mut fixture("renewal").as_slice()).unwrap(),
        None
    );
    assert_eq!(
        inner::read::<Renewal, _>(&mut fixture("go").as_slice()).unwrap(),
        None
    );
}

#[test]
fn a_renewal_without_what_the_new_image_needs_is_refused_and_extra_is_skipped() {
    // Cut each entry in turn out of the recorded block: the entries whose
    // absence reads as a zero still read, the others are refused.
    let whole = fixture("renewal");
    let payload = &whole[7..];
    let mut at = 0;
    let mut refused = 0;
    while at < payload.len() {
        let len = u32::from_be_bytes(payload[at + 2..at + 6].try_into().unwrap()) as usize;
        let key = u16::from_be_bytes(payload[at..at + 2].try_into().unwrap());
        let mut cut = payload[..at].to_vec();
        cut.extend_from_slice(&payload[at + 6 + len..]);
        let mut frame = whole[..3].to_vec();
        frame.extend_from_slice(&u32::try_from(cut.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&cut);
        let read = inner::read::<Renewal, _>(&mut frame.as_slice());
        // Counters and flags (keys 13 to 15 and 17) read as zero when absent.
        if matches!(key, 13..=15 | 17) {
            assert!(read.unwrap().is_some(), "key {key}");
        } else {
            assert!(read.is_err(), "key {key}");
            refused += 1;
        }
        at += 6 + len;
    }
    assert_eq!(refused, 13);

    let extra = [0x0f, 0xff, 0, 0, 0, 2, 1, 2];
    let mut bigger = whole.clone();
    let len = u32::from_be_bytes(bigger[3..7].try_into().unwrap()) + 8;
    bigger[3..7].copy_from_slice(&len.to_be_bytes());
    bigger.extend_from_slice(&extra);
    assert_eq!(
        inner::read::<Renewal, _>(&mut bigger.as_slice()).unwrap(),
        Some(renewal())
    );
}

#[test]
fn bad_payloads_are_recoverable_errors() {
    // An adopt without an id, a released with a short offset, a child-ended with an unknown exit kind.
    let bad = |kind: u8, payload: &[u8]| {
        let mut f = vec![kind, 0, 0];
        f.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
        f.extend_from_slice(payload);
        f
    };
    let mut stream = bad(0x01, &[0, 2, 0, 0, 0, 4, 0, 0, 0, 1]);
    stream.extend(bad(0x04, &[0, 1, 0, 0, 0, 5, 9, 0, 0, 0, 1]));
    stream.extend(fixture("go"));
    let mut reader = stream.as_slice();
    for _ in 0..2 {
        assert!(
            inner::read::<ToRelay, _>(&mut reader)
                .unwrap_err()
                .is_recoverable()
        );
    }
    assert_eq!(
        inner::read::<ToRelay, _>(&mut reader).unwrap(),
        Some(ToRelay::Go)
    );
    let released = bad(0x82, &[0, 1, 0, 0, 0, 3, 0, 0, 1]);
    assert!(
        inner::read::<ToWarden, _>(&mut released.as_slice())
            .unwrap_err()
            .is_recoverable()
    );
}
