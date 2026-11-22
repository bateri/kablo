//! `kablo profile`, the real command over its standard input and output.
//!
//! Every test has a home of its own (`HOME` is the only place the command keeps anything), and
//! the only processes a test starts are the `kablo profile` commands it speaks to: it ends each
//! of them itself.

// Test support: a failure here is a failed test.
#![allow(
    dead_code,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kablo_proto::profile::{
    self as wire, BlobRef, Body, Client, Digest, Entry, MAX_BLOB, MAX_KEY, MAX_PART, MAX_VALUE,
    Mode, Open, Opened, Part, Put, PutReply, PutResult, Server, StateHeader, code, hex, kind, sum,
};

/// How long a test waits for the command to say something before it calls it failed.
const PATIENCE: Duration = Duration::from_secs(20);

static SERIAL: AtomicUsize = AtomicUsize::new(0);

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// A home directory that is the test's own.
struct Home {
    dir: PathBuf,
}

impl Home {
    fn new() -> Home {
        let n = SERIAL.fetch_add(1, Ordering::Relaxed);
        let dir = PathBuf::from(format!("/tmp/kp{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        Home { dir }
    }

    fn browser(&self) -> PathBuf {
        self.dir.join(".local/share/bateri/browser")
    }

    /// The folder of the profile: the name the server gives it is a contract (a profile is found
    /// again by it), so it is made here from its description and not asked of the command.
    fn folder(&self, app: &str, name: &str) -> PathBuf {
        let mut named = app.as_bytes().to_vec();
        named.push(0);
        named.extend_from_slice(name.as_bytes());
        self.browser()
            .join("profiles")
            .join(format!("{app}-{name}-{}", &hex(&sum(&named))[..16]))
    }

    fn main(&self) -> PathBuf {
        self.folder("bateri", "main")
    }

    fn kablo(&self, words: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_kablo"));
        command.arg("profile").args(words).env("HOME", &self.dir);
        command.env_remove("KABLO_DIR");
        command
    }

    /// Runs `kablo profile off` or `on`.
    fn switch(&self, word: &str) {
        let status = self.kablo(&[word]).status().unwrap();
        assert!(status.success(), "kablo profile {word}: {status}");
    }

    /// Makes the profile's folder as the command would have left it, with these entries in the
    /// state, these in the log and the blobs' folder.
    fn seed(&self, app: &str, name: &str, state: &Seed) {
        let dir = self.folder(app, name);
        fs::create_dir_all(dir.join("blobs")).unwrap();
        let mut bytes = wire::encode_state_header(&StateHeader {
            depot: state.depot,
            head: state.head,
            horizon: state.horizon,
        });
        for entry in &state.entries {
            bytes.extend_from_slice(&wire::encode_state_entry(entry));
        }
        fs::write(dir.join("state"), bytes).unwrap();
        let mut log = wire::encode_log_header(&wire::LogHeader { generation: 3 });
        for entry in &state.log {
            log.extend_from_slice(&wire::encode_log_entry(entry).unwrap());
        }
        log.extend_from_slice(&state.torn);
        fs::write(dir.join("log"), log).unwrap();
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// What `Home::seed` writes.
struct Seed {
    depot: [u8; 16],
    head: u64,
    horizon: u64,
    entries: Vec<Entry>,
    log: Vec<Entry>,
    /// Bytes after the log's last whole record: a record that was being written.
    torn: Vec<u8>,
}

impl Seed {
    fn new(head: u64, horizon: u64) -> Seed {
        Seed {
            depot: DEPOT,
            head,
            horizon,
            entries: Vec::new(),
            log: Vec::new(),
            torn: Vec::new(),
        }
    }
}

const DEPOT: [u8; 16] = [0xd0; 16];

fn entry(seq: u64, kind: u8, key: &str, value: &str) -> Entry {
    Entry {
        seq,
        body: body(kind, key, value),
        writer: [7; 16],
        time: now_ms(),
    }
}

fn body(kind: u8, key: &str, value: &str) -> Body {
    Body {
        kind,
        key: key.as_bytes().to_vec(),
        value: value.as_bytes().to_vec(),
        ..Body::default()
    }
}

fn gone(kind: u8, key: &str) -> Body {
    Body {
        kind,
        key: key.as_bytes().to_vec(),
        deleted: true,
        ..Body::default()
    }
}

fn open(client: u8, since: u64, depot: Option<[u8; 16]>) -> Open {
    named("bateri", "main", client, since, depot)
}

fn named(app: &str, name: &str, client: u8, since: u64, depot: Option<[u8; 16]>) -> Open {
    Open {
        app: app.into(),
        name: name.into(),
        client: [client; 16],
        since,
        depot,
    }
}

type Heard = Result<Option<Server>, String>;

/// A running `kablo profile` and the words it has said.
struct Conn {
    child: Child,
    stdin: Option<ChildStdin>,
    heard: Receiver<Heard>,
    /// Records that came while the test waited for something else, in the order they came.
    stash: Vec<Entry>,
}

impl Conn {
    fn start(home: &Home, first: &Client) -> Conn {
        let mut child = home
            .kablo(&[])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        wire::WIRE.client(&mut stdout, &mut stdin).unwrap();
        let (tx, heard) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let message = wire::read_server(&mut stdout).map_err(|e| e.to_string());
                let last = !matches!(message, Ok(Some(_)));
                if tx.send(message).is_err() || last {
                    break;
                }
            }
        });
        let mut conn = Conn {
            child,
            stdin: Some(stdin),
            heard,
            stash: Vec::new(),
        };
        conn.send(first);
        conn
    }

    fn send(&mut self, message: &Client) {
        let stdin = self.stdin.as_mut().unwrap();
        wire::write_client(stdin, message).unwrap();
        stdin.flush().unwrap();
    }

    /// The next thing said; `None` when the command has said its last.
    fn next(&mut self) -> Option<Server> {
        self.heard
            .recv_timeout(PATIENCE)
            .expect("the command said nothing")
            .expect("what the command said is not a message")
    }

    /// The next thing said that is not a record: those that come meanwhile are put by.
    fn reply(&mut self) -> Option<Server> {
        loop {
            match self.next() {
                Some(Server::Record(entry)) => self.stash.push(entry),
                other => return other,
            }
        }
    }

    fn next_within(&mut self, wait: Duration) -> Option<Server> {
        match self.heard.recv_timeout(wait) {
            Ok(message) => message.unwrap(),
            Err(_) => None,
        }
    }

    /// The next record, from those that came before it was asked for or from the stream.
    fn record(&mut self) -> Entry {
        if !self.stash.is_empty() {
            return self.stash.remove(0);
        }
        match self.next() {
            Some(Server::Record(entry)) => entry,
            other => panic!("a record was expected, not {other:?}"),
        }
    }

    fn records(&mut self, count: usize) -> Vec<Entry> {
        (0..count).map(|_| self.record()).collect()
    }

    /// Nothing is said for a while.
    fn quiet(&mut self) {
        assert!(self.stash.is_empty(), "{:?}", self.stash);
        if let Some(message) = self.next_within(Duration::from_millis(700)) {
            panic!("nothing was to be said, but: {message:?}");
        }
    }

    /// Writes a record and waits for the answer.
    fn put(&mut self, body: &Body, base: u64) -> PutReply {
        self.send(&Client::Put(Put {
            req: 1,
            body: body.clone(),
            base,
        }));
        loop {
            match self.next() {
                Some(Server::PutReply(reply)) => return reply,
                Some(Server::Record(entry)) => self.stash.push(entry),
                other => panic!("a put reply was expected, not {other:?}"),
            }
        }
    }

    fn done(&mut self, body: &Body, base: u64) -> u64 {
        let reply = self.put(body, base);
        assert_eq!(reply.result, PutResult::Done, "{body:?} on {base}");
        reply.seq
    }

    /// The command ends, and with what status.
    fn finish(mut self) -> ExitStatus {
        self.stdin = None;
        let until = Instant::now() + PATIENCE;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < until, "the command did not end");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Hangs up.
    fn hang_up(self) {
        let status = self.finish();
        assert!(status.success(), "{status}");
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Only the process this started.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An open that went through.
struct Joined {
    conn: Conn,
    opened: Opened,
    /// The records the client is brought up to date with.
    records: Vec<Entry>,
}

fn join(home: &Home, first: Open) -> Joined {
    let mut conn = Conn::start(home, &Client::Open(first));
    let opened = match conn.next() {
        Some(Server::Opened(opened)) => opened,
        other => panic!("the open was expected to be answered, not with {other:?}"),
    };
    let mut records = Vec::new();
    loop {
        match conn.next() {
            Some(Server::Record(entry)) => records.push(entry),
            Some(Server::CaughtUp { head }) => {
                assert_eq!(head, opened.head);
                return Joined {
                    conn,
                    opened,
                    records,
                };
            }
            other => panic!("records or the end of them were expected, not {other:?}"),
        }
    }
}

/// An open that is turned down: the code, and how the command ended.
fn refused(home: &Home, first: Open) -> (u16, ExitStatus) {
    let mut conn = Conn::start(home, &Client::Open(first));
    let code = match conn.next() {
        Some(Server::Error(failure)) => failure.code,
        other => panic!("an error was expected, not {other:?}"),
    };
    assert!(conn.next().is_none(), "nothing is said after an error");
    (code, conn.finish())
}

fn seqs(records: &[Entry]) -> Vec<u64> {
    records.iter().map(|e| e.seq).collect()
}

fn keys(records: &[Entry]) -> Vec<String> {
    records
        .iter()
        .map(|e| String::from_utf8(e.body.key.clone()).unwrap())
        .collect()
}

fn mode(joined: &Joined) -> Mode {
    joined.opened.mode
}

// ---- numbers and the rule of writing ----

#[test]
fn records_are_numbered_one_by_one_and_a_kind_and_a_key_together_are_one_record() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    assert_eq!(
        (mode(&a), a.opened.head, a.opened.horizon),
        (Mode::Full, 0, 0)
    );
    assert_ne!(a.opened.depot, [0; 16]);
    assert!(a.records.is_empty());
    assert_eq!(a.conn.done(&body(kind::COOKIE, "a", "1"), 0), 1);
    assert_eq!(a.conn.done(&body(kind::LOCAL_STORAGE, "b", "2"), 0), 2);
    // The same key of another kind is another record.
    assert_eq!(a.conn.done(&body(kind::LOCAL_STORAGE, "a", "3"), 0), 3);
    // The same record again, over what was seen: the next number.
    assert_eq!(a.conn.done(&body(kind::COOKIE, "a", "4"), 1), 4);
    let heard = a.conn.records(4);
    assert_eq!(seqs(&heard), [1, 2, 3, 4]);
    assert!(heard.iter().all(|e| e.writer == [1; 16] && e.time > 0));
    assert_eq!(heard[3].body.value, b"4");
    a.conn.hang_up();
}

#[test]
fn a_write_over_something_newer_than_it_saw_is_turned_down_with_the_number_it_is_now() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let k = body(kind::COOKIE, "k", "v1");
    assert_eq!(a.conn.done(&k, 0), 1);
    // Another that did not see it.
    let late = a.conn.put(&body(kind::COOKIE, "k", "late"), 0);
    assert_eq!((late.result, late.seq), (PutResult::Conflict, 1));
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k", "v2"), 1), 2);
    let stale = a.conn.put(&body(kind::COOKIE, "k", "stale"), 1);
    assert_eq!((stale.result, stale.seq), (PutResult::Conflict, 2));
    // What was turned down is not there.
    let b = join(&home, open(2, 0, None));
    assert_eq!(b.records.len(), 1);
    assert_eq!(b.records[0].body.value, b"v2");
}

#[test]
fn a_record_that_is_gone_is_written_over_what_a_deletion_that_was_let_go_of_covers() {
    let home = Home::new();
    let mut seed = Seed::new(12, 10);
    seed.entries.push(entry(11, kind::COOKIE, "kept", "x"));
    home.seed("bateri", "main", &seed);
    let mut a = join(&home, open(1, 12, Some(DEPOT)));
    assert_eq!(mode(&a), Mode::Diff);
    // There is no such record and the base is no newer than what was let go of: the same as
    // having seen it not there.
    assert_eq!(a.conn.done(&body(kind::COOKIE, "a", "1"), 0), 13);
    assert_eq!(a.conn.done(&body(kind::COOKIE, "b", "1"), 10), 14);
    // A base newer than that, for a record that is not there, is one that has been written and
    // removed since: not the same.
    let c = a.conn.put(&body(kind::COOKIE, "c", "1"), 11);
    assert_eq!((c.result, c.seq), (PutResult::Conflict, 0));
}

#[test]
fn a_deletion_is_a_record_and_comes_to_those_who_ask_for_what_is_newer() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let depot = a.opened.depot;
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k", "v"), 0), 1);
    assert_eq!(a.conn.done(&gone(kind::COOKIE, "k"), 1), 2);
    a.conn.hang_up();
    let b = join(&home, open(2, 1, Some(depot)));
    assert_eq!(mode(&b), Mode::Diff);
    assert_eq!(seqs(&b.records), [2]);
    assert!(b.records[0].body.deleted);
    // Whoever has seen nothing is told of the deletion too: a record of theirs that was deleted
    // here would otherwise be put back (a put over a deletion is a conflict, and the answer to a
    // conflict is the newer record from the stream).
    let c = join(&home, open(3, 0, Some(depot)));
    assert_eq!(mode(&c), Mode::Full);
    assert_eq!(seqs(&c.records), [2]);
    assert!(c.records[0].body.deleted);
    assert_eq!(c.opened.head, 2);
}

#[test]
fn a_put_whose_answer_was_lost_comes_back_as_the_clients_own_record_and_a_conflict() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let depot = a.opened.depot;
    assert_eq!(a.conn.done(&body(kind::COOKIE, "seen", "v"), 0), 1);
    let seen = a.conn.record();
    assert_eq!(seen.seq, 1);
    // The put goes, the answer does not come back.
    let lost = body(kind::LOCAL_STORAGE, "k", "sent");
    a.conn.send(&Client::Put(Put {
        req: 2,
        body: lost.clone(),
        base: 0,
    }));
    assert!(matches!(a.conn.next(), Some(Server::PutReply(_))));
    drop(a);
    let mut again = join(&home, open(1, 1, Some(depot)));
    assert_eq!(mode(&again), Mode::Diff);
    assert_eq!(seqs(&again.records), [2]);
    assert_eq!(again.records[0].writer, [1; 16]);
    assert_eq!(again.records[0].body, lost);
    // Sent again over what it saw, it is what is there: a conflict that names the number.
    let retry = again.conn.put(&lost, 0);
    assert_eq!((retry.result, retry.seq), (PutResult::Conflict, 2));
}

// ---- more than one process ----

#[test]
fn two_connections_to_one_profile_each_see_what_the_other_writes() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let mut b = join(&home, open(2, 0, Some(a.opened.depot)));
    assert_eq!(a.opened.depot, b.opened.depot);
    assert_eq!(a.conn.done(&body(kind::COOKIE, "from-a", "1"), 0), 1);
    let heard = b.conn.record();
    assert_eq!(
        (heard.seq, heard.writer, heard.body.key),
        (1, [1; 16], b"from-a".to_vec())
    );
    assert_eq!(b.conn.done(&body(kind::COOKIE, "from-b", "1"), 0), 2);
    let heard = a.conn.record();
    // Its own record comes back too, first the one before it.
    let own = a.conn.record();
    let mut got = [heard, own];
    got.sort_by_key(|e| e.seq);
    assert_eq!(seqs(&got), [1, 2]);
    assert_eq!(got[1].writer, [2; 16]);
    // A write that does not know of the other's is a conflict, though the other is another
    // process: the files decide.
    let c = a.conn.put(&body(kind::COOKIE, "from-b", "mine"), 0);
    assert_eq!((c.result, c.seq), (PutResult::Conflict, 2));
}

#[test]
fn a_connection_goes_on_from_where_it_was_when_another_compacts_the_profile() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let mut b = join(&home, open(2, 0, Some(a.opened.depot)));
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k1", "1"), 0), 1);
    // B has heard the first before the profile is compacted.
    assert_eq!(seqs(&[b.conn.record()]), [1]);
    // A record that has run out of time is written and, as soon as it is, the profile is
    // written whole to a new state and the log begins again.
    let mut old = body(kind::COOKIE, "old", "x");
    old.expires = Some(1);
    assert_eq!(a.conn.done(&old, 0), 2);
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k2", "1"), 0), 3);
    let mut heard = Vec::new();
    while !heard.iter().any(|e: &Entry| e.seq == 3) {
        heard.push(b.conn.record());
    }
    // What B heard before is not told again, and what it has not heard comes once and in order
    // (the record that ran out may or may not be among them, as B looked before or after).
    let got = seqs(&heard);
    assert!(got.ends_with(&[3]), "{got:?}");
    assert!(got.windows(2).all(|w| w[0] < w[1]), "{got:?}");
    assert!(got.iter().all(|&seq| seq > 1), "{got:?}");
    // And it is B that goes on being heard from afterwards, and being written to.
    assert_eq!(b.conn.done(&body(kind::COOKIE, "k3", "1"), 0), 4);
    let late = a.conn.records(4);
    assert!(late.iter().any(|e| e.seq == 4), "{late:?}");
    // The record that ran out is not there for who comes now.
    let c = join(&home, open(3, 0, None));
    let mut names = keys(&c.records);
    names.sort();
    assert_eq!(names, ["k1", "k2", "k3"]);
}

#[test]
fn a_connection_that_went_comes_back_and_is_told_what_is_newer_than_it_has_in_order() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let b = join(&home, open(2, 0, Some(a.opened.depot)));
    let depot = b.opened.depot;
    assert_eq!(a.conn.done(&body(kind::COOKIE, "one", "1"), 0), 1);
    drop(b);
    for (i, key) in ["two", "three", "four"].iter().enumerate() {
        assert_eq!(a.conn.done(&body(kind::COOKIE, key, "1"), 0), 2 + i as u64);
    }
    let mut again = join(&home, open(2, 1, Some(depot)));
    assert_eq!(mode(&again), Mode::Diff);
    assert_eq!(keys(&again.records), ["two", "three", "four"]);
    assert_eq!(seqs(&again.records), [2, 3, 4]);
    again.conn.quiet();
}

// ---- which way a client is brought up to date ----

#[test]
fn the_server_chooses_how_a_client_is_brought_up_to_date() {
    let home = Home::new();
    let mut seed = Seed::new(10, 5);
    seed.entries.push(entry(4, kind::COOKIE, "old", "x"));
    seed.entries.push(entry(8, kind::COOKIE, "mid", "x"));
    seed.entries.push(entry(10, kind::COOKIE, "new", "x"));
    home.seed("bateri", "main", &seed);
    let case = |since: u64, depot: Option<[u8; 16]>| {
        let joined = join(&home, open(1, since, depot));
        let out = (mode(&joined), keys(&joined.records));
        assert_eq!(joined.opened.depot, DEPOT);
        assert_eq!((joined.opened.head, joined.opened.horizon), (10, 5));
        out
    };
    let all = ["old", "mid", "new"].map(String::from).to_vec();
    // A client that does not know this server, or that never joined, or that is ahead of it.
    assert_eq!(case(7, None), (Mode::Full, all.clone()));
    assert_eq!(case(7, Some([9; 16])), (Mode::Full, all.clone()));
    assert_eq!(case(0, Some(DEPOT)), (Mode::Full, all.clone()));
    assert_eq!(case(11, Some(DEPOT)), (Mode::Full, all.clone()));
    // Away longer than deletions are remembered.
    assert_eq!(case(3, Some(DEPOT)), (Mode::FullReplace, all));
    // Otherwise what is newer than it has.
    assert_eq!(
        case(5, Some(DEPOT)),
        (Mode::Diff, vec!["mid".into(), "new".into()])
    );
    assert_eq!(case(8, Some(DEPOT)), (Mode::Diff, vec!["new".to_string()]));
    assert_eq!(case(10, Some(DEPOT)), (Mode::Diff, vec![]));
}

#[test]
fn a_server_made_again_is_a_first_join_for_a_client_that_had_the_old_one() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let old = a.opened.depot;
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k", "v"), 0), 1);
    a.conn.hang_up();
    home.switch("off");
    home.switch("on");
    let again = join(&home, open(1, 1, Some(old)));
    assert_ne!(again.opened.depot, old);
    assert_eq!(mode(&again), Mode::Full);
    assert_eq!(again.opened.head, 0);
    assert!(again.records.is_empty());
}

#[test]
fn deletions_that_are_old_are_let_go_of_by_the_compaction_and_the_horizon_says_so() {
    let home = Home::new();
    let mut seed = Seed::new(6, 0);
    let mut stone = entry(4, kind::COOKIE, "gone", "");
    stone.body.deleted = true;
    stone.time = now_ms() - 31 * 24 * 3600 * 1000;
    let mut fresh = entry(5, kind::COOKIE, "fresh-stone", "");
    fresh.body.deleted = true;
    seed.entries = vec![stone, fresh, entry(6, kind::COOKIE, "kept", "v")];
    home.seed("bateri", "main", &seed);
    let a = join(&home, open(1, 3, Some(DEPOT)));
    // The stone of thirty days ago is let go of as the profile is opened; a client from before
    // it cannot be told what was deleted.
    assert_eq!((a.opened.head, a.opened.horizon), (6, 4));
    assert_eq!(mode(&a), Mode::FullReplace);
    assert_eq!(keys(&a.records), ["fresh-stone", "kept"]);
    // From after it, what is newer: the stone that is fresh is still told of.
    let b = join(&home, open(2, 4, Some(DEPOT)));
    assert_eq!(mode(&b), Mode::Diff);
    assert_eq!(keys(&b.records), ["fresh-stone", "kept"]);
}

// ---- blobs ----

fn blob_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 31 % 251) as u8).collect()
}

fn upload(conn: &mut Conn, bytes: &[u8]) -> Digest {
    let digest = sum(bytes);
    let mut start = 0;
    loop {
        let end = (start + MAX_PART / 2).min(bytes.len());
        conn.send(&Client::Blob(Part {
            sha256: digest,
            total: bytes.len() as u64,
            start: start as u64,
            bytes: bytes[start..end].to_vec(),
        }));
        start = end;
        if start >= bytes.len() {
            break;
        }
    }
    assert_eq!(conn.reply(), Some(Server::BlobOk(digest)));
    digest
}

fn fetch(conn: &mut Conn, digest: Digest) -> Option<Vec<u8>> {
    conn.send(&Client::GetBlob(digest));
    let mut got = Vec::new();
    loop {
        match conn.reply() {
            Some(Server::BlobMissing(d)) => {
                assert_eq!(d, digest);
                return None;
            }
            Some(Server::Blob(part)) => {
                assert_eq!(part.start, got.len() as u64);
                assert!(part.bytes.len() <= MAX_PART);
                got.extend_from_slice(&part.bytes);
                if got.len() as u64 >= part.total {
                    return Some(got);
                }
            }
            other => panic!("a blob was expected, not {other:?}"),
        }
    }
}

#[test]
fn a_record_that_points_at_a_blob_the_server_lacks_waits_for_it() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let bytes = blob_bytes(MAX_PART + 1000);
    let digest = sum(&bytes);
    let mut with = body(kind::INDEXED_DB, "db", "{}");
    with.linked = vec![BlobRef {
        sha256: digest,
        size: bytes.len() as u64,
    }];
    assert_eq!(a.conn.put(&with, 0).result, PutResult::MissingBlob);
    let other = sum(b"other");
    a.conn.send(&Client::Have(vec![digest, other]));
    assert_eq!(a.conn.reply(), Some(Server::HaveReply(vec![digest, other])));
    assert_eq!(upload(&mut a.conn, &bytes), digest);
    a.conn.send(&Client::Have(vec![digest, other]));
    assert_eq!(a.conn.reply(), Some(Server::HaveReply(vec![other])));
    // A blob sent whole again is there, and is said to be.
    assert_eq!(upload(&mut a.conn, &bytes), digest);
    assert_eq!(a.conn.done(&with, 0), 1);
    // A size that is not the one of the file that is there is not that blob.
    let mut wrong = with.clone();
    wrong.key = b"db2".to_vec();
    wrong.linked[0].size += 1;
    assert_eq!(a.conn.put(&wrong, 0).result, PutResult::MissingBlob);
    // The same through the other door: a value too large for a record is a blob.
    let mut big = body(kind::LOCAL_STORAGE, "big", "");
    big.value_blob = Some(BlobRef {
        sha256: digest,
        size: bytes.len() as u64,
    });
    assert_eq!(a.conn.done(&big, 0), 2);
    assert_eq!(fetch(&mut a.conn, digest).as_deref(), Some(&bytes[..]));
    assert_eq!(fetch(&mut a.conn, other), None);
    // An empty file is a blob like any.
    assert_eq!(upload(&mut a.conn, b""), sum(b""));
    assert_eq!(fetch(&mut a.conn, sum(b"")).as_deref(), Some(&b""[..]));
}

#[test]
fn a_blob_whose_bytes_are_not_the_ones_it_is_named_by_is_not_kept() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let named_by = sum(b"what it was to be");
    a.conn.send(&Client::Blob(Part {
        sha256: named_by,
        total: 5,
        start: 0,
        bytes: b"other".to_vec(),
    }));
    match a.conn.next() {
        Some(Server::Error(failure)) => assert_eq!(failure.code, code::INTERNAL),
        other => panic!("an error was expected, not {other:?}"),
    }
    assert!(!a.conn.finish().success());
    assert!(!home.main().join("blobs").join(hex(&named_by)).exists());
}

fn age(path: &std::path::Path, by: Duration) {
    OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - by)
        .unwrap();
}

#[test]
fn the_compaction_deletes_old_blobs_that_nothing_points_at_and_nothing_else() {
    let home = Home::new();
    let kept_digest = sum(b"kept");
    let mut seed = Seed::new(1, 0);
    let mut pointing = entry(1, kind::INDEXED_DB, "db", "{}");
    pointing.body.linked = vec![BlobRef {
        sha256: kept_digest,
        size: 4,
    }];
    seed.entries.push(pointing);
    home.seed("bateri", "main", &seed);
    let blobs = home.main().join("blobs");
    let write = |name: String, old: Option<Duration>| {
        let path = blobs.join(name);
        File::create(&path).unwrap().write_all(b"x").unwrap();
        if let Some(by) = old {
            age(&path, by);
        }
        path
    };
    let hour = Duration::from_secs(3600);
    let pointed_at = write(hex(&kept_digest), Some(hour));
    let fresh = write(hex(&sum(b"fresh")), None);
    let a_few_minutes = write(hex(&sum(b"few")), Some(Duration::from_secs(120)));
    let stale = write(hex(&sum(b"stale")), Some(hour));
    let part_of_today = write(".part-today".into(), Some(Duration::from_secs(600)));
    let part_of_long_ago = write(".part-long-ago".into(), Some(3 * hour));
    let mut a = join(&home, open(1, 1, Some(DEPOT)));
    // A record that ran out is what starts a compaction here.
    let mut old = body(kind::COOKIE, "old", "x");
    old.expires = Some(1);
    a.conn.done(&old, 0);
    assert!(pointed_at.exists(), "a blob a record points at");
    assert!(fresh.exists(), "a blob that may be waiting for its record");
    assert!(a_few_minutes.exists());
    assert!(part_of_today.exists());
    assert!(!stale.exists(), "an old blob nothing points at");
    assert!(!part_of_long_ago.exists(), "an upload never finished");
}

// ---- the switch ----

#[test]
fn off_deletes_every_profile_and_turns_down_the_open_and_on_lets_it_through_again() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k", "v"), 0), 1);
    a.conn.hang_up();
    assert!(home.main().exists());
    home.switch("off");
    assert!(home.browser().join("profiles-off").exists());
    assert!(!home.browser().join("profiles").exists());
    let (turned_down, status) = refused(&home, open(1, 0, None));
    assert_eq!(turned_down, code::OFF);
    assert!(status.success(), "{status}");
    assert!(!home.browser().join("profiles").exists(), "none is made");
    // Off again and on again are the same as once.
    home.switch("off");
    home.switch("on");
    home.switch("on");
    assert!(!home.browser().join("profiles-off").exists());
    let b = join(&home, open(1, 1, None));
    assert_eq!((b.opened.head, mode(&b)), (0, Mode::Full));
    assert!(b.records.is_empty());
}

// ---- files that are not what this build writes ----

fn files_of(home: &Home) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for name in ["state", "log"] {
        if let Ok(bytes) = fs::read(home.main().join(name)) {
            out.push((name.to_string(), bytes));
        }
    }
    out
}

#[test]
fn a_profile_a_newer_kablo_wrote_is_turned_down_and_not_written_to() {
    // The state is of a newer format.
    let home = Home::new();
    fs::create_dir_all(home.main()).unwrap();
    let mut state = wire::STATE_SIGNATURE.to_vec();
    state.push(wire::STORE_FORMAT + 1);
    state.extend_from_slice(b"whatever a newer kablo keeps here");
    fs::write(home.main().join("state"), &state).unwrap();
    let before = files_of(&home);
    let (turned_down, status) = refused(&home, open(1, 0, None));
    assert_eq!(turned_down, code::NEWER);
    assert!(status.success());
    assert_eq!(files_of(&home), before);
    assert!(!home.main().join("log").exists());

    // The log is.
    let home = Home::new();
    home.seed("bateri", "main", &Seed::new(0, 0));
    let mut log = wire::LOG_SIGNATURE.to_vec();
    log.push(wire::STORE_FORMAT + 1);
    log.extend_from_slice(b"and the records of a newer format");
    fs::write(home.main().join("log"), &log).unwrap();
    let before = files_of(&home);
    assert_eq!(refused(&home, open(1, 0, None)).0, code::NEWER);
    assert_eq!(files_of(&home), before);
}

#[test]
fn a_log_that_ends_in_a_record_cut_short_is_opened_and_written_after_its_last_whole_one() {
    let home = Home::new();
    let mut seed = Seed::new(1, 0);
    seed.entries.push(entry(1, kind::COOKIE, "state", "v"));
    seed.log.push(entry(2, kind::COOKIE, "log", "v"));
    let third = wire::encode_log_entry(&entry(3, kind::COOKIE, "cut", "v")).unwrap();
    seed.torn = third[..third.len() - 3].to_vec();
    home.seed("bateri", "main", &seed);
    let mut a = join(&home, open(1, 0, None));
    assert_eq!(keys(&a.records), ["state", "log"]);
    assert_eq!(a.opened.head, 2);
    assert_eq!(a.conn.done(&body(kind::COOKIE, "after", "v"), 0), 3);
    a.conn.hang_up();
    let b = join(&home, open(2, 0, None));
    assert_eq!(keys(&b.records), ["state", "log", "after"]);
}

#[test]
fn a_log_that_was_never_finished_is_made_again_and_one_that_is_not_a_log_is_not_touched() {
    let home = Home::new();
    let mut seed = Seed::new(1, 0);
    seed.entries.push(entry(1, kind::COOKIE, "state", "v"));
    home.seed("bateri", "main", &seed);
    fs::write(home.main().join("log"), &wire::LOG_SIGNATURE[..4]).unwrap();
    let mut a = join(&home, open(1, 0, None));
    assert_eq!(keys(&a.records), ["state"]);
    assert_eq!(a.conn.done(&body(kind::COOKIE, "after", "v"), 0), 2);
    a.conn.hang_up();

    let home = Home::new();
    home.seed("bateri", "main", &seed);
    let garbage = vec![0x5a; 200];
    fs::write(home.main().join("log"), &garbage).unwrap();
    let before = files_of(&home);
    let (turned_down, status) = refused(&home, open(1, 0, None));
    assert_eq!(turned_down, code::INTERNAL);
    assert!(!status.success());
    assert_eq!(files_of(&home), before);
}

// ---- what is turned down ----

#[test]
fn names_that_are_not_names_make_no_folder() {
    let home = Home::new();
    let long = "a".repeat(33);
    for (app, name) in [
        ("bateri", "Main"),
        ("bateri", "a/b"),
        ("bateri", ".."),
        ("bateri", ""),
        ("bateri", long.as_str()),
        ("", "main"),
        ("Bateri", "main"),
        ("a b", "main"),
    ] {
        let (turned_down, status) = refused(&home, named(app, name, 1, 0, None));
        assert_eq!(turned_down, code::NAME, "{app:?} {name:?}");
        assert!(!status.success());
    }
    assert!(!home.browser().exists());
    // The longest that is allowed, and the characters that are.
    let ok = "a".repeat(32);
    let joined = join(&home, named("bateri", &ok, 1, 0, None));
    assert_eq!(joined.opened.head, 0);
    let dash = join(&home, named("my-app", "0-9", 1, 0, None));
    assert_eq!(dash.opened.head, 0);
    assert!(home.folder("my-app", "0-9").exists());
}

#[test]
fn two_names_that_run_together_are_two_profiles() {
    let home = Home::new();
    let mut a = join(&home, named("a-b", "c", 1, 0, None));
    let mut b = join(&home, named("a", "b-c", 1, 0, None));
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k", "v"), 0), 1);
    assert_eq!(b.opened.head, 0);
    assert_ne!(home.folder("a-b", "c"), home.folder("a", "b-c"));
    assert!(home.folder("a-b", "c").exists() && home.folder("a", "b-c").exists());
    b.conn.quiet();
}

#[test]
fn a_record_that_is_no_record_or_is_too_large_is_turned_down_and_the_connection_goes_on() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let result = |a: &mut Joined, body: &Body| a.conn.put(body, 0).result;
    assert_eq!(result(&mut a, &body(9, "k", "v")), PutResult::Bad);
    assert_eq!(result(&mut a, &body(0, "k", "v")), PutResult::Bad);
    assert_eq!(result(&mut a, &body(kind::COOKIE, "", "v")), PutResult::Bad);
    let value = "v".repeat(MAX_VALUE + 1);
    assert_eq!(
        result(&mut a, &body(kind::COOKIE, "k", &value)),
        PutResult::TooLarge
    );
    let key = "k".repeat(MAX_KEY + 1);
    assert_eq!(
        result(&mut a, &body(kind::COOKIE, &key, "v")),
        PutResult::TooLarge
    );
    let mut huge = body(kind::INDEXED_DB, "db", "{}");
    huge.linked = vec![BlobRef {
        sha256: sum(b"huge"),
        size: MAX_BLOB + 1,
    }];
    assert_eq!(result(&mut a, &huge), PutResult::TooLarge);
    // The limits themselves are allowed, and the other kind that is.
    let at = "v".repeat(MAX_VALUE);
    assert_eq!(
        result(&mut a, &body(kind::DETACHED, "k", &at)),
        PutResult::Done
    );
    // Nothing of what was turned down took a number.
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k", "v"), 0), 2);
}

#[test]
fn the_first_message_is_the_open() {
    let home = Home::new();
    let mut conn = Conn::start(&home, &Client::Have(vec![]));
    match conn.next() {
        Some(Server::Error(failure)) => assert_eq!(failure.code, code::INTERNAL),
        other => panic!("an error was expected, not {other:?}"),
    }
    assert!(!conn.finish().success());
    assert!(!home.browser().exists());
}

#[test]
fn what_is_kept_is_for_its_owner_alone() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let bytes = blob_bytes(10);
    upload(&mut a.conn, &bytes);
    a.conn.done(&body(kind::COOKIE, "k", "v"), 0);
    let mode_of = |path: PathBuf| fs::metadata(path).unwrap().permissions().mode() & 0o777;
    let dir = home.main();
    assert_eq!(mode_of(home.browser().join("profiles")), 0o700);
    assert_eq!(mode_of(dir.clone()), 0o700);
    assert_eq!(mode_of(dir.join("blobs")), 0o700);
    for file in ["state", "log", "lock"] {
        assert_eq!(mode_of(dir.join(file)), 0o600, "{file}");
    }
    assert_eq!(mode_of(dir.join("blobs").join(hex(&sum(&bytes)))), 0o600);
    // `off` leaves a mark that is the owner's too.
    drop(a);
    home.switch("off");
    assert_eq!(mode_of(home.browser().join("profiles-off")), 0o600);
}

// ---- more of the same, found later ----

#[test]
fn what_the_state_already_has_is_not_read_again_from_a_log_that_was_not_replaced() {
    // A compaction that ended after it wrote the state and before it made the new log leaves the
    // old log, whose records are all in the state, or have been let go of from it.
    let home = Home::new();
    let mut seed = Seed::new(3, 0);
    seed.entries.push(entry(3, kind::COOKIE, "b", "state"));
    seed.log.push(entry(2, kind::COOKIE, "a", "old"));
    seed.log.push(entry(3, kind::COOKIE, "b", "log"));
    home.seed("bateri", "main", &seed);
    let mut a = join(&home, open(1, 0, None));
    assert_eq!(keys(&a.records), ["b"]);
    assert_eq!(a.records[0].body.value, b"state");
    assert_eq!(a.opened.head, 3);
    assert_eq!(a.conn.done(&body(kind::COOKIE, "c", "v"), 0), 4);
}

#[test]
fn a_record_that_ran_out_is_not_there_and_is_written_again_over_base_zero() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    let mut old = body(kind::COOKIE, "old", "x");
    old.expires = Some(1);
    assert_eq!(a.conn.done(&old, 0), 1);
    assert_eq!(a.conn.done(&body(kind::COOKIE, "other", "x"), 0), 2);
    // Let go of by the compaction as it was written, with no deletion to say so: a client that
    // saw it (number 1) and writes over it is told the number is 0, which is "not there", and
    // writes it again as new.
    let again = a.conn.put(&old, 1);
    assert_eq!((again.result, again.seq), (PutResult::Conflict, 0));
    old.expires = Some(now_ms() + 3_600_000);
    assert_eq!(a.conn.done(&old, 0), 3);
}

#[test]
fn a_connection_to_a_profile_that_is_switched_off_is_told_so() {
    let home = Home::new();
    let mut a = join(&home, open(1, 0, None));
    assert_eq!(a.conn.done(&body(kind::COOKIE, "k", "v"), 0), 1);
    assert_eq!(seqs(&[a.conn.record()]), [1]);
    home.switch("off");
    match a.conn.next() {
        Some(Server::Error(failure)) => assert_eq!(failure.code, code::OFF),
        other => panic!("an error was expected, not {other:?}"),
    }
    assert!(a.conn.finish().success());
}
