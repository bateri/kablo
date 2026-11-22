//! `kablo profile`: what a browser's profile is kept as on a server, and the wire bateri talks to
//! the process that keeps it. A second protocol beside the session one: its own signature
//! ([`SIGNATURE`]) and its own versions ([`WIRE`]), the [`hello`](crate::hello) and
//! [`frame`](crate::frame) layers under it, and [`tlv`] blocks for every payload. It runs over the
//! command stream of a session (the program's argv is `kablo profile`, its standard input and
//! output are the connection), so no byte of the session protocol is added to.
//!
//! A profile is a log of records (a cookie, an item of Local Storage, an origin's IndexedDB). The
//! server numbers them with a counter that only goes up ([`Entry::seq`]); a writer says which
//! record it saw last ([`Put::base`]) and is turned down when it is no longer the latest. What a
//! record's value is stays the writer's: the server reads the key (to tell records apart), the
//! blobs it points at and the end of its life, and no more, so that a value may be enveloped
//! (enciphered) later without a change of format ([`Body::envelope`]).
//!
//! # Connection
//!
//! The client opens ([`Client::Open`]), the server answers ([`Server::Opened`]) with the
//! records to bring the client up to date ([`Server::Record`]) and says when they are done
//! ([`Server::CaughtUp`]); what comes after is live. Writing is [`Client::Put`]. A record that
//! points at blobs is put after its blobs are ([`Client::Have`] asks which are missing,
//! [`Client::Blob`] sends them in parts).
//!
//! The fixtures (`tests/fixtures/profile-v1/`) hold the bytes of every message, of the header of
//! the file a profile is kept in ([`StateHeader`]) and of the one its changes are appended to
//! ([`LogHeader`]).

use std::io::{Read, Write};

use crate::frame::{self, RawFrame};
use crate::hello::Protocol;
use crate::tlv::{self, Writer};
use crate::{Error, Result};

pub use crate::sha256::{Digest, Sha256, hex, sum};

/// What the hello of this protocol opens with.
pub const SIGNATURE: &[u8] = b"KABLO-PROFILE\0";
/// The lowest version of this protocol this build reads.
pub const MIN_VERSION: u8 = 1;
/// The highest version of this protocol this build reads.
pub const MAX_VERSION: u8 = 1;
/// This protocol as this build speaks it.
pub const WIRE: Protocol = Protocol::new(SIGNATURE, MIN_VERSION, MAX_VERSION);
/// The channel of this protocol's frames.
pub const CHANNEL: u8 = 2;

/// What the file with the whole of a profile begins with.
pub const STATE_SIGNATURE: &[u8] = b"KABLO-PROFILE-STATE\0";
/// What the file the changes are appended to begins with.
pub const LOG_SIGNATURE: &[u8] = b"KABLO-PROFILE-LOG\0";
/// The format of both files this build writes and reads.
pub const STORE_FORMAT: u8 = 1;

/// The longest value of a record, in bytes; a longer one goes in a blob.
pub const MAX_VALUE: usize = 64 * 1024;
/// The longest key of a record, in bytes.
pub const MAX_KEY: usize = 4 * 1024;
/// The most bytes of a blob a message carries.
pub const MAX_PART: usize = 512 * 1024;
/// The biggest blob.
pub const MAX_BLOB: u64 = 256 * 1024 * 1024;

/// Kinds of record ([`Body::kind`]).
pub mod kind {
    /// A cookie.
    pub const COOKIE: u8 = 1;
    /// An item of Local Storage.
    pub const LOCAL_STORAGE: u8 = 2;
    /// An item of a storage that belongs to a tab (session storage), kept apart for it.
    pub const DETACHED: u8 = 3;
    /// An origin's IndexedDB.
    pub const INDEXED_DB: u8 = 4;
}

/// Codes of [`Server::Error`].
pub mod code {
    /// No version in common.
    pub const VERSION: u16 = 1;
    /// A profile's name that is no name.
    pub const NAME: u16 = 2;
    /// The profile is not kept on this server (the server's switch is off).
    pub const OFF: u16 = 3;
    /// The server could not do it.
    pub const INTERNAL: u16 = 4;
    /// The profile was written by a newer `kablo` that this one does not read.
    pub const NEWER: u16 = 5;
}

/// The keys of the TLV blocks. A key means one thing in the messages it is used in, and the
/// numbers of a record's fields are the same wherever a record goes.
mod key {
    // open
    pub const APP: u16 = 1;
    pub const NAME: u16 = 2;
    pub const CLIENT: u16 = 3;
    pub const SINCE: u16 = 4;
    pub const DEPOT: u16 = 5;
    // put
    pub const REQ: u16 = 1;
    pub const BASE: u16 = 12;
    // have, get_blob, blob_ok, blob_missing, have_reply
    pub const SHA: u16 = 1;
    // blob
    pub const TOTAL: u16 = 2;
    pub const START: u16 = 3;
    pub const BYTES: u16 = 4;
    // opened
    pub const HEAD: u16 = 1;
    pub const HORIZON: u16 = 2;
    pub const MODE: u16 = 3;
    pub const OPENED_DEPOT: u16 = 4;
    // put_reply
    pub const RESULT: u16 = 2;
    pub const SEQ_OF_REPLY: u16 = 3;
    // error
    pub const CODE: u16 = 1;
    pub const TEXT: u16 = 2;
    // the fields of a record
    pub const KIND: u16 = 10;
    pub const KEY: u16 = 11;
    pub const DELETED: u16 = 14;
    pub const ENVELOPE: u16 = 15;
    pub const VALUE: u16 = 16;
    pub const LINKED: u16 = 17;
    pub const EXPIRES: u16 = 20;
    pub const VALUE_BLOB: u16 = 21;
    // record
    pub const SEQ: u16 = 13;
    pub const WRITER: u16 = 18;
    pub const TIME: u16 = 19;
}

mod message {
    // From the client.
    pub const OPEN: u8 = 0x01;
    pub const PUT: u8 = 0x02;
    pub const HAVE: u8 = 0x03;
    pub const BLOB: u8 = 0x04;
    pub const GET_BLOB: u8 = 0x05;
    // From the server: the high bit.
    pub const OPENED: u8 = 0x81;
    pub const RECORD: u8 = 0x82;
    pub const CAUGHT_UP: u8 = 0x83;
    pub const PUT_REPLY: u8 = 0x84;
    pub const HAVE_REPLY: u8 = 0x85;
    pub const BLOB_FROM_SERVER: u8 = 0x86;
    pub const BLOB_OK: u8 = 0x87;
    pub const BLOB_MISSING: u8 = 0x88;
    pub const ERROR: u8 = 0x8f;
}

/// A blob a record points at: the sum of its bytes and how many there are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlobRef {
    /// The sum of the bytes.
    pub sha256: Digest,
    /// How many bytes.
    pub size: u64,
}

/// What a record is. The server tells records apart by `kind` and `key` together; what `key` and
/// `value` hold is the writer's own format, read by no one else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Body {
    /// What the record is of ([`kind`]).
    pub kind: u8,
    /// What tells the record from others of its kind: the writer's canonical TLV block.
    pub key: Vec<u8>,
    /// The record says the thing is gone; it has no value.
    pub deleted: bool,
    /// What the value is wrapped in: 0 is not wrapped. A value of another envelope is carried
    /// whole.
    pub envelope: u8,
    /// The value (at most [`MAX_VALUE`] bytes), inside the envelope.
    pub value: Vec<u8>,
    /// The blobs the value points at (the files of an IndexedDB), in the open: the server keeps
    /// them as long as a record does.
    pub linked: Vec<BlobRef>,
    /// The value, when it is over [`MAX_VALUE`]: it is in this blob instead of in `value`.
    pub value_blob: Option<BlobRef>,
    /// When the record stops being worth keeping, in milliseconds since the epoch, in the open.
    pub expires: Option<u64>,
}

impl Body {
    /// The blobs the record keeps alive.
    pub fn blobs(&self) -> impl Iterator<Item = &BlobRef> {
        self.linked.iter().chain(self.value_blob.iter())
    }
}

/// How the server brings a client up to date ([`Opened::mode`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// What came after the client's `since`.
    Diff,
    /// The whole profile: the client has never joined this one (or its server was made again).
    Full,
    /// The whole profile, and what the client has that it lacks is dropped: the client was gone
    /// longer than the server remembers deletions.
    FullReplace,
}

impl Mode {
    fn code(self) -> u8 {
        match self {
            Mode::Diff => 0,
            Mode::Full => 1,
            Mode::FullReplace => 2,
        }
    }

    fn from_code(code: u8) -> Result<Mode> {
        match code {
            0 => Ok(Mode::Diff),
            1 => Ok(Mode::Full),
            2 => Ok(Mode::FullReplace),
            _ => Err(Error::Malformed("mode")),
        }
    }
}

/// What the server answers a [`Put`] with ([`PutReply::result`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutResult {
    /// Written; `seq` is its number.
    Done,
    /// The record has changed since the writer's `base`; `seq` is its number now.
    Conflict,
    /// A blob the record points at is not on the server.
    MissingBlob,
    /// A value, a key or a blob over the limits.
    TooLarge,
    /// A record that is no record (an unknown kind, no key).
    Bad,
}

impl PutResult {
    fn code(self) -> u8 {
        match self {
            PutResult::Done => 0,
            PutResult::Conflict => 1,
            PutResult::MissingBlob => 2,
            PutResult::TooLarge => 3,
            PutResult::Bad => 4,
        }
    }

    fn from_code(code: u8) -> Result<PutResult> {
        match code {
            0 => Ok(PutResult::Done),
            1 => Ok(PutResult::Conflict),
            2 => Ok(PutResult::MissingBlob),
            3 => Ok(PutResult::TooLarge),
            4 => Ok(PutResult::Bad),
            _ => Err(Error::Malformed("put result")),
        }
    }
}

/// The client's first message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Open {
    /// The app the profile belongs to (`bateri`).
    pub app: String,
    /// The profile's name, `[a-z0-9-]` and at most 32 characters.
    pub name: String,
    /// Who this is: random, made once for a machine and a profile.
    pub client: [u8; 16],
    /// The newest record this client has seen; 0 when it never joined.
    pub since: u64,
    /// The server the client saw it on ([`Opened::depot`]); none when it never joined.
    pub depot: Option<[u8; 16]>,
}

/// A record the client writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Put {
    /// The client's number for the request, to tell the answer by.
    pub req: u32,
    /// The record.
    pub body: Body,
    /// The number the record had when the client last saw it; 0 for "there was none".
    pub base: u64,
}

/// A part of a blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    /// The sum of the whole blob.
    pub sha256: Digest,
    /// How many bytes the whole blob has.
    pub total: u64,
    /// Where in the blob these bytes begin.
    pub start: u64,
    /// The bytes (at most [`MAX_PART`]).
    pub bytes: Vec<u8>,
}

/// What the client says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Client {
    /// Opens the profile.
    Open(Open),
    /// Writes a record.
    Put(Put),
    /// Asks which of these blobs the server does not have.
    Have(Vec<Digest>),
    /// A part of a blob.
    Blob(Part),
    /// Asks for a blob.
    GetBlob(Digest),
}

/// The server's answer to the open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    /// The number of the newest record.
    pub head: u64,
    /// The number of the newest deletion the server has let go of: a client that has been away
    /// since before it cannot be told what was deleted.
    pub horizon: u64,
    /// How the client is brought up to date.
    pub mode: Mode,
    /// Which server this is: random, made with the profile.
    pub depot: [u8; 16],
}

/// A record as the server has it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its number: the server's counter, which only goes up.
    pub seq: u64,
    /// The record.
    pub body: Body,
    /// Who wrote it ([`Open::client`]).
    pub writer: [u8; 16],
    /// When the server took it, in milliseconds since the epoch.
    pub time: u64,
}

/// The server's answer to a [`Put`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutReply {
    /// The request it answers.
    pub req: u32,
    /// How it went.
    pub result: PutResult,
    /// The record's number: new when it was written, the current one on a conflict.
    pub seq: u64,
}

/// An error that ends the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// What went wrong ([`code`]).
    pub code: u16,
    /// Words for a person.
    pub text: String,
}

/// What the server says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Server {
    /// The answer to the open.
    Opened(Opened),
    /// A record: first the ones that bring the client up to date, then the ones that come live.
    Record(Entry),
    /// The records that bring the client up to date are done; `head` is the newest number.
    CaughtUp {
        /// The newest number.
        head: u64,
    },
    /// The answer to a put.
    PutReply(PutReply),
    /// The blobs asked about that the server does not have.
    HaveReply(Vec<Digest>),
    /// A part of a blob that was asked for.
    Blob(Part),
    /// Every part of a blob came and its sum held.
    BlobOk(Digest),
    /// The blob asked for is not here; the connection goes on.
    BlobMissing(Digest),
    /// The connection ends.
    Error(Failure),
}

// ---- reading and writing the parts ----

fn digest_of(value: &[u8], what: &'static str) -> Result<Digest> {
    Digest::try_from(value).map_err(|_| Error::Malformed(what))
}

fn id_of(value: &[u8], what: &'static str) -> Result<[u8; 16]> {
    <[u8; 16]>::try_from(value).map_err(|_| Error::Malformed(what))
}

fn blob_ref_of(value: &[u8]) -> Result<BlobRef> {
    let (sum, size) = value
        .split_at_checked(32)
        .ok_or(Error::Malformed("blob reference"))?;
    Ok(BlobRef {
        sha256: digest_of(sum, "blob reference")?,
        size: tlv::read_u64(size, "blob reference")?,
    })
}

fn put_blob_ref(writer: &mut Writer, key: u16, blob: &BlobRef) {
    let mut bytes = blob.sha256.to_vec();
    bytes.extend_from_slice(&blob.size.to_be_bytes());
    writer.put(key, &bytes);
}

fn put_body(writer: &mut Writer, body: &Body) {
    writer.put_u8(key::KIND, body.kind);
    writer.put(key::KEY, &body.key);
    if body.deleted {
        writer.put_u8(key::DELETED, 1);
    }
    writer.put_u8(key::ENVELOPE, body.envelope);
    if !body.deleted && body.value_blob.is_none() {
        writer.put(key::VALUE, &body.value);
    }
    for blob in &body.linked {
        put_blob_ref(writer, key::LINKED, blob);
    }
    if let Some(blob) = &body.value_blob {
        put_blob_ref(writer, key::VALUE_BLOB, blob);
    }
    if let Some(at) = body.expires {
        writer.put_u64(key::EXPIRES, at);
    }
}

/// Reads the fields of a record out of the entries of a block, one at a time.
#[derive(Default)]
struct BodyReader {
    body: Body,
    kind: bool,
    key: bool,
}

impl BodyReader {
    /// Takes the entry when it is a field of a record; `false` when it is not.
    fn take(&mut self, key: u16, value: &[u8]) -> Result<bool> {
        match key {
            key::KIND => {
                self.body.kind = tlv::read_u8(value, "record kind")?;
                self.kind = true;
            }
            key::KEY => {
                self.body.key = value.to_vec();
                self.key = true;
            }
            key::DELETED => self.body.deleted = tlv::read_u8(value, "record deleted")? != 0,
            key::ENVELOPE => self.body.envelope = tlv::read_u8(value, "record envelope")?,
            key::VALUE => self.body.value = value.to_vec(),
            key::LINKED => self.body.linked.push(blob_ref_of(value)?),
            key::VALUE_BLOB => self.body.value_blob = Some(blob_ref_of(value)?),
            key::EXPIRES => self.body.expires = Some(tlv::read_u64(value, "record expires")?),
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> Result<Body> {
        if self.kind && self.key {
            Ok(self.body)
        } else {
            Err(Error::Malformed("record without a kind or a key"))
        }
    }
}

fn frame_of(kind: u8, payload: Vec<u8>) -> RawFrame {
    RawFrame {
        kind,
        channel: CHANNEL,
        session: Vec::new(),
        payload,
    }
}

fn part_payload(part: &Part) -> Vec<u8> {
    let mut w = Writer::new();
    w.put(key::SHA, &part.sha256);
    w.put_u64(key::TOTAL, part.total);
    w.put_u64(key::START, part.start);
    w.put(key::BYTES, &part.bytes);
    w.finish()
}

fn part_of(payload: &[u8]) -> Result<Part> {
    let (mut sha256, mut total, mut start, mut bytes) = (None, None, None, Vec::new());
    for entry in tlv::entries(payload) {
        let (k, v) = entry?;
        match k {
            key::SHA => sha256 = Some(digest_of(v, "blob sum")?),
            key::TOTAL => total = Some(tlv::read_u64(v, "blob total")?),
            key::START => start = Some(tlv::read_u64(v, "blob start")?),
            key::BYTES => bytes = v.to_vec(),
            _ => {}
        }
    }
    match (sha256, total, start) {
        (Some(sha256), Some(total), Some(start)) => Ok(Part {
            sha256,
            total,
            start,
            bytes,
        }),
        _ => Err(Error::Malformed("blob part")),
    }
}

fn digests_payload(digests: &[Digest]) -> Vec<u8> {
    let mut w = Writer::new();
    for digest in digests {
        w.put(key::SHA, digest);
    }
    w.finish()
}

fn digests_of(payload: &[u8]) -> Result<Vec<Digest>> {
    let mut out = Vec::new();
    for entry in tlv::entries(payload) {
        let (k, v) = entry?;
        if k == key::SHA {
            out.push(digest_of(v, "sum")?);
        }
    }
    Ok(out)
}

fn one_digest_of(payload: &[u8]) -> Result<Digest> {
    digests_of(payload)?
        .pop()
        .ok_or(Error::Malformed("message without a sum"))
}

impl Client {
    /// The frame this message is.
    pub fn to_frame(&self) -> RawFrame {
        match self {
            Client::Open(open) => {
                let mut w = Writer::new();
                w.put(key::APP, open.app.as_bytes());
                w.put(key::NAME, open.name.as_bytes());
                w.put(key::CLIENT, &open.client);
                w.put_u64(key::SINCE, open.since);
                if let Some(depot) = &open.depot {
                    w.put(key::DEPOT, depot);
                }
                frame_of(message::OPEN, w.finish())
            }
            Client::Put(put) => {
                let mut w = Writer::new();
                w.put_u32(key::REQ, put.req);
                put_body(&mut w, &put.body);
                w.put_u64(key::BASE, put.base);
                frame_of(message::PUT, w.finish())
            }
            Client::Have(digests) => frame_of(message::HAVE, digests_payload(digests)),
            Client::Blob(part) => frame_of(message::BLOB, part_payload(part)),
            Client::GetBlob(digest) => frame_of(
                message::GET_BLOB,
                digests_payload(std::slice::from_ref(digest)),
            ),
        }
    }

    /// The message a frame is. `None` for a frame this protocol does not know (another channel,
    /// or a kind that came after this version): it is skipped.
    pub fn from_frame(frame: &RawFrame) -> Result<Option<Client>> {
        if frame.channel != CHANNEL {
            return Ok(None);
        }
        let payload = frame.payload.as_slice();
        Ok(Some(match frame.kind {
            message::OPEN => {
                let (mut app, mut name, mut client, mut since, mut depot) =
                    (None, None, None, 0, None);
                for entry in tlv::entries(payload) {
                    let (k, v) = entry?;
                    match k {
                        key::APP => app = Some(tlv::read_text(v, "open app")?),
                        key::NAME => name = Some(tlv::read_text(v, "open name")?),
                        key::CLIENT => client = Some(id_of(v, "open client")?),
                        key::SINCE => since = tlv::read_u64(v, "open since")?,
                        key::DEPOT => depot = Some(id_of(v, "open depot")?),
                        _ => {}
                    }
                }
                match (app, name, client) {
                    (Some(app), Some(name), Some(client)) => Client::Open(Open {
                        app,
                        name,
                        client,
                        since,
                        depot,
                    }),
                    _ => return Err(Error::Malformed("open")),
                }
            }
            message::PUT => {
                let (mut req, mut base, mut reader) = (None, None, BodyReader::default());
                for entry in tlv::entries(payload) {
                    let (k, v) = entry?;
                    if reader.take(k, v)? {
                        continue;
                    }
                    match k {
                        key::REQ => req = Some(tlv::read_u32(v, "put request")?),
                        key::BASE => base = Some(tlv::read_u64(v, "put base")?),
                        _ => {}
                    }
                }
                match (req, base) {
                    (Some(req), Some(base)) => Client::Put(Put {
                        req,
                        body: reader.finish()?,
                        base,
                    }),
                    _ => return Err(Error::Malformed("put")),
                }
            }
            message::HAVE => Client::Have(digests_of(payload)?),
            message::BLOB => Client::Blob(part_of(payload)?),
            message::GET_BLOB => Client::GetBlob(one_digest_of(payload)?),
            _ => return Ok(None),
        }))
    }
}

/// A record as the payload of a `record` frame and of an entry of the state file.
pub fn entry_payload(entry: &Entry) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_u64(key::SEQ, entry.seq);
    put_body(&mut w, &entry.body);
    w.put(key::WRITER, &entry.writer);
    w.put_u64(key::TIME, entry.time);
    w.finish()
}

/// The record a payload holds.
pub fn entry_of(payload: &[u8]) -> Result<Entry> {
    let (mut seq, mut writer, mut time, mut reader) = (None, None, 0, BodyReader::default());
    for entry in tlv::entries(payload) {
        let (k, v) = entry?;
        if reader.take(k, v)? {
            continue;
        }
        match k {
            key::SEQ => seq = Some(tlv::read_u64(v, "record seq")?),
            key::WRITER => writer = Some(id_of(v, "record writer")?),
            key::TIME => time = tlv::read_u64(v, "record time")?,
            _ => {}
        }
    }
    match (seq, writer) {
        (Some(seq), Some(writer)) => Ok(Entry {
            seq,
            body: reader.finish()?,
            writer,
            time,
        }),
        _ => Err(Error::Malformed("record")),
    }
}

impl Server {
    /// The frame this message is.
    pub fn to_frame(&self) -> RawFrame {
        match self {
            Server::Opened(opened) => {
                let mut w = Writer::new();
                w.put_u64(key::HEAD, opened.head);
                w.put_u64(key::HORIZON, opened.horizon);
                w.put_u8(key::MODE, opened.mode.code());
                w.put(key::OPENED_DEPOT, &opened.depot);
                frame_of(message::OPENED, w.finish())
            }
            Server::Record(entry) => frame_of(message::RECORD, entry_payload(entry)),
            Server::CaughtUp { head } => {
                let mut w = Writer::new();
                w.put_u64(key::HEAD, *head);
                frame_of(message::CAUGHT_UP, w.finish())
            }
            Server::PutReply(reply) => {
                let mut w = Writer::new();
                w.put_u32(key::REQ, reply.req);
                w.put_u8(key::RESULT, reply.result.code());
                w.put_u64(key::SEQ_OF_REPLY, reply.seq);
                frame_of(message::PUT_REPLY, w.finish())
            }
            Server::HaveReply(digests) => frame_of(message::HAVE_REPLY, digests_payload(digests)),
            Server::Blob(part) => frame_of(message::BLOB_FROM_SERVER, part_payload(part)),
            Server::BlobOk(digest) => frame_of(
                message::BLOB_OK,
                digests_payload(std::slice::from_ref(digest)),
            ),
            Server::BlobMissing(digest) => frame_of(
                message::BLOB_MISSING,
                digests_payload(std::slice::from_ref(digest)),
            ),
            Server::Error(failure) => {
                let mut w = Writer::new();
                w.put_u16(key::CODE, failure.code);
                w.put(key::TEXT, failure.text.as_bytes());
                frame_of(message::ERROR, w.finish())
            }
        }
    }

    /// The message a frame is; `None` for one this protocol does not know.
    pub fn from_frame(frame: &RawFrame) -> Result<Option<Server>> {
        if frame.channel != CHANNEL {
            return Ok(None);
        }
        let payload = frame.payload.as_slice();
        Ok(Some(match frame.kind {
            message::OPENED => {
                let (mut head, mut horizon, mut mode, mut depot) = (None, None, None, None);
                for entry in tlv::entries(payload) {
                    let (k, v) = entry?;
                    match k {
                        key::HEAD => head = Some(tlv::read_u64(v, "opened head")?),
                        key::HORIZON => horizon = Some(tlv::read_u64(v, "opened horizon")?),
                        key::MODE => mode = Some(Mode::from_code(tlv::read_u8(v, "opened mode")?)?),
                        key::OPENED_DEPOT => depot = Some(id_of(v, "opened depot")?),
                        _ => {}
                    }
                }
                match (head, horizon, mode, depot) {
                    (Some(head), Some(horizon), Some(mode), Some(depot)) => {
                        Server::Opened(Opened {
                            head,
                            horizon,
                            mode,
                            depot,
                        })
                    }
                    _ => return Err(Error::Malformed("opened")),
                }
            }
            message::RECORD => Server::Record(entry_of(payload)?),
            message::CAUGHT_UP => {
                let mut head = None;
                for entry in tlv::entries(payload) {
                    let (k, v) = entry?;
                    if k == key::HEAD {
                        head = Some(tlv::read_u64(v, "caught_up head")?);
                    }
                }
                Server::CaughtUp {
                    head: head.ok_or(Error::Malformed("caught_up"))?,
                }
            }
            message::PUT_REPLY => {
                let (mut req, mut result, mut seq) = (None, None, 0);
                for entry in tlv::entries(payload) {
                    let (k, v) = entry?;
                    match k {
                        key::REQ => req = Some(tlv::read_u32(v, "put_reply request")?),
                        key::RESULT => {
                            result =
                                Some(PutResult::from_code(tlv::read_u8(v, "put_reply result")?)?)
                        }
                        key::SEQ_OF_REPLY => seq = tlv::read_u64(v, "put_reply seq")?,
                        _ => {}
                    }
                }
                match (req, result) {
                    (Some(req), Some(result)) => Server::PutReply(PutReply { req, result, seq }),
                    _ => return Err(Error::Malformed("put_reply")),
                }
            }
            message::HAVE_REPLY => Server::HaveReply(digests_of(payload)?),
            message::BLOB_FROM_SERVER => Server::Blob(part_of(payload)?),
            message::BLOB_OK => Server::BlobOk(one_digest_of(payload)?),
            message::BLOB_MISSING => Server::BlobMissing(one_digest_of(payload)?),
            message::ERROR => {
                let (mut code, mut text) = (None, String::new());
                for entry in tlv::entries(payload) {
                    let (k, v) = entry?;
                    match k {
                        key::CODE => code = Some(tlv::read_u16(v, "error code")?),
                        key::TEXT => text = String::from_utf8_lossy(v).into_owned(),
                        _ => {}
                    }
                }
                Server::Error(Failure {
                    code: code.ok_or(Error::Malformed("error"))?,
                    text,
                })
            }
            _ => return Ok(None),
        }))
    }
}

/// Writes a message of the client.
pub fn write_client<W: Write>(writer: &mut W, message: &Client) -> Result<()> {
    frame::write_frame(writer, &message.to_frame())
}

/// Reads the next message of the client; frames of other channels and kinds are skipped.
/// `Ok(None)` is the stream ending between frames.
pub fn read_client<R: Read>(reader: &mut R) -> Result<Option<Client>> {
    frame::read_with(reader, |frame| Client::from_frame(&frame))
}

/// Writes a message of the server.
pub fn write_server<W: Write>(writer: &mut W, message: &Server) -> Result<()> {
    frame::write_frame(writer, &message.to_frame())
}

/// Reads the next message of the server; frames of other channels and kinds are skipped.
pub fn read_server<R: Read>(reader: &mut R) -> Result<Option<Server>> {
    frame::read_with(reader, |frame| Server::from_frame(&frame))
}

// ---- the files a profile is kept in ----

/// What the first bytes of a file of the store say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stored<T> {
    /// A header this build reads.
    Current(T),
    /// A file of a format newer than this build's: nothing is written to it.
    Newer(u8),
    /// Not a file of the store.
    Foreign,
}

/// The head of the state file: the whole profile as of `head`, after which the entries follow
/// (each a length, `u32`, and the payload of a `record`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateHeader {
    /// Which server this is ([`Opened::depot`]).
    pub depot: [u8; 16],
    /// The number of the newest record.
    pub head: u64,
    /// The number of the newest deletion let go of ([`Opened::horizon`]).
    pub horizon: u64,
}

/// The bytes a state file begins with.
pub fn encode_state_header(header: &StateHeader) -> Vec<u8> {
    let mut out = STATE_SIGNATURE.to_vec();
    out.push(STORE_FORMAT);
    out.extend_from_slice(&header.depot);
    out.extend_from_slice(&header.head.to_be_bytes());
    out.extend_from_slice(&header.horizon.to_be_bytes());
    out
}

/// Reads the head of a state file.
pub fn read_state_header<R: Read>(reader: &mut R) -> Result<Stored<StateHeader>> {
    match read_signature(reader, STATE_SIGNATURE)? {
        Stored::Current(()) => {}
        Stored::Newer(format) => return Ok(Stored::Newer(format)),
        Stored::Foreign => return Ok(Stored::Foreign),
    }
    let depot = read_array::<_, 16>(reader)?;
    let head = u64::from_be_bytes(read_array(reader)?);
    let horizon = u64::from_be_bytes(read_array(reader)?);
    Ok(Stored::Current(StateHeader {
        depot,
        head,
        horizon,
    }))
}

/// The bytes of an entry of a state file: its length and its payload.
pub fn encode_state_entry(entry: &Entry) -> Vec<u8> {
    let payload = entry_payload(entry);
    let mut out = u32::try_from(payload.len())
        .unwrap_or(u32::MAX)
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(&payload);
    out
}

/// Reads the next entry of a state file; `Ok(None)` at its end.
pub fn read_state_entry<R: Read>(reader: &mut R) -> Result<Option<Entry>> {
    let mut length = [0u8; 4];
    let Some((first, rest)) = length.split_first_mut() else {
        return Ok(None);
    };
    match reader.read(std::slice::from_mut(first))? {
        0 => return Ok(None),
        _ => reader.read_exact(rest)?,
    }
    let length = usize::try_from(u32::from_be_bytes(length)).unwrap_or(usize::MAX);
    if length > frame::MAX_PAYLOAD {
        return Err(Error::PayloadTooLarge { len: length });
    }
    let mut payload = Vec::new();
    reader
        .by_ref()
        .take(u64::try_from(length).unwrap_or(u64::MAX))
        .read_to_end(&mut payload)?;
    if payload.len() != length {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof).into());
    }
    entry_of(&payload).map(Some)
}

/// The head of the log: the changes since the state file, as `record` frames, after it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogHeader {
    /// Which log this is: a compaction starts a new one with the next number.
    pub generation: u64,
}

/// The bytes a log begins with.
pub fn encode_log_header(header: &LogHeader) -> Vec<u8> {
    let mut out = LOG_SIGNATURE.to_vec();
    out.push(STORE_FORMAT);
    out.extend_from_slice(&header.generation.to_be_bytes());
    out
}

/// Reads the head of a log.
pub fn read_log_header<R: Read>(reader: &mut R) -> Result<Stored<LogHeader>> {
    match read_signature(reader, LOG_SIGNATURE)? {
        Stored::Current(()) => {}
        Stored::Newer(format) => return Ok(Stored::Newer(format)),
        Stored::Foreign => return Ok(Stored::Foreign),
    }
    Ok(Stored::Current(LogHeader {
        generation: u64::from_be_bytes(read_array(reader)?),
    }))
}

/// The bytes a log gains for a record.
pub fn encode_log_entry(entry: &Entry) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    frame::encode_frame(&mut out, &Server::Record(entry.clone()).to_frame())?;
    Ok(out)
}

/// Reads the next record of a log; `Ok(None)` at its end.
pub fn read_log_entry<R: Read>(reader: &mut R) -> Result<Option<Entry>> {
    match frame::read_with(reader, |frame| Server::from_frame(&frame))? {
        Some(Server::Record(entry)) => Ok(Some(entry)),
        Some(_) => Err(Error::Malformed("a log holds records only")),
        None => Ok(None),
    }
}

fn read_array<R: Read, const N: usize>(reader: &mut R) -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    reader.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_signature<R: Read>(reader: &mut R, signature: &[u8]) -> Result<Stored<()>> {
    let mut have = vec![0u8; signature.len()];
    match reader.read_exact(&mut have) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Stored::Foreign),
        Err(e) => return Err(e.into()),
    }
    if have != signature {
        return Ok(Stored::Foreign);
    }
    let [format] = read_array(reader)?;
    if format > STORE_FORMAT {
        Ok(Stored::Newer(format))
    } else {
        Ok(Stored::Current(()))
    }
}
