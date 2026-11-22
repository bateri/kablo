//! The files a profile is kept in on this machine, and the rules the records in them follow.
//!
//! A profile is a folder (`~/.local/share/bateri/browser/profiles/<app>-<name>-<sum>`, mode 0700,
//! the files in it 0600):
//!
//! - `state`: the whole profile as of its `head` (written whole, by a compaction);
//! - `log`: the changes after it, one `record` frame appended for each, under the lock;
//! - `blobs/<sha256>`: the files the records point at, each whole or not there;
//! - `lock`: the file the locks are taken on. A writer holds it exclusively, and a reader that
//!   looks at the log holds it shared, so a reader never sees a compaction half done.
//!
//! Every process that serves the profile keeps its own picture of it in memory and brings it up
//! to date from the log ([`Store::refresh`]), by number (`seq`) and not by position in the file,
//! so that the process that comes after a compaction goes on from where it was.
//!
//! A file this build does not read (written by a newer one) is never written to ([`Error::Newer`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kablo_proto::profile::{
    self, BlobRef, Body, Digest, Entry, LogHeader, MAX_BLOB, MAX_KEY, MAX_VALUE, Put, PutResult,
    Sha256, StateHeader, Stored, hex, kind,
};

use crate::sys;

const STATE: &str = "state";
const LOG: &str = "log";
const LOCK: &str = "lock";
const BLOBS: &str = "blobs";
/// The files of an upload that has not come to its end, in the blobs' folder.
const PART_PREFIX: &str = ".part-";

/// How long a deletion is remembered: a computer that has been away longer is brought up to date
/// by the whole profile.
const TOMBSTONE_MS: u64 = 30 * 24 * 3600 * 1000;
/// How long a blob no record points at is kept: it may have been uploaded for a record that is
/// still on its way.
const BLOB_GRACE: Duration = Duration::from_secs(10 * 60);
/// How long the file of an upload that never came to its end is kept.
const PART_GRACE: Duration = Duration::from_secs(3600);
/// The log is compacted when it holds this many records, or is this long.
const LOG_RECORDS: u64 = 2000;
const LOG_BYTES: u64 = 4 * 1024 * 1024;

/// What went wrong with the files.
#[derive(Debug)]
pub enum Error {
    /// The disk said no.
    Io(io::Error),
    /// A file was written by a `kablo` newer than this one; nothing has been written to it.
    Newer,
    /// A file is not what the store writes.
    Foreign(&'static str),
    /// The profile's folder was deleted while it was served (`kablo profile off`).
    Gone,
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<kablo_proto::Error> for Error {
    fn from(e: kablo_proto::Error) -> Error {
        Error::Io(io::Error::other(e))
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Newer => write!(f, "the profile was written by a newer kablo"),
            Error::Foreign(what) => write!(f, "the profile's {what} is not one this kablo wrote"),
            Error::Gone => write!(f, "the profile is not there any more"),
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

/// What tells one record from another.
type Identity = (u8, Vec<u8>);

fn identity(body: &Body) -> Identity {
    (body.kind, body.key.clone())
}

/// Milliseconds since the epoch.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Whether `app` is a name of an app: `[a-z0-9-]`, 1 to 32.
pub fn app_ok(app: &str) -> bool {
    name_ok(app)
}

/// Whether `name` is a name of a profile: `[a-z0-9-]`, 1 to 32.
pub fn name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The name of a profile's folder: the app, the name and the first sixteen hexadecimal digits of
/// the sum of both (so that `a-b` + `c` and `a` + `b-c` are two folders).
pub fn folder_name(app: &str, name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(app.as_bytes());
    hasher.update(&[0]);
    hasher.update(name.as_bytes());
    let sum = hex(&hasher.finish());
    format!("{app}-{name}-{}", sum.get(..16).unwrap_or(&sum))
}

fn make_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Writes `bytes` to `path` whole or not at all: to a file beside it, then a rename.
fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".tmp.{}", std::process::id()));
    let tmp = path.with_file_name(name);
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| fs::rename(&tmp, path));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn random_id() -> io::Result<[u8; 16]> {
    let mut id = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut id)?;
    Ok(id)
}

/// One profile's files, and the picture of it this process keeps.
pub struct Store {
    dir: PathBuf,
    lock: File,
    depot: [u8; 16],
    head: u64,
    horizon: u64,
    generation: u64,
    /// How much of the log has been read: its head and the whole records after it.
    offset: u64,
    /// How long the log file is: more than `offset` when a record was cut off at its end.
    log_len: u64,
    log_records: u64,
    entries: HashMap<Identity, Entry>,
    /// Records the client has not been told of yet, in the order they came.
    pending: VecDeque<Entry>,
}

impl Store {
    /// Opens the profile in `dir`, making it when it is not there.
    pub fn open(dir: &Path) -> Result<Store> {
        make_dir(dir)?;
        make_dir(&dir.join(BLOBS))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(dir.join(LOCK))?;
        let mut store = Store {
            dir: dir.to_path_buf(),
            lock,
            depot: [0; 16],
            head: 0,
            horizon: 0,
            generation: 0,
            offset: 0,
            log_len: 0,
            log_records: 0,
            entries: HashMap::new(),
            pending: VecDeque::new(),
        };
        store.locked(true, Store::load_or_make)?;
        Ok(store)
    }

    fn locked<T>(&mut self, exclusive: bool, f: impl FnOnce(&mut Store) -> Result<T>) -> Result<T> {
        let fd = self.lock.as_raw_fd();
        sys::flock(fd, exclusive)?;
        let out = f(self);
        let _ = sys::unlock(fd);
        out
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join(STATE)
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join(LOG)
    }

    /// Which server this is.
    pub fn depot(&self) -> [u8; 16] {
        self.depot
    }

    /// The number of the newest record.
    pub fn head(&self) -> u64 {
        self.head
    }

    /// The number of the newest deletion let go of.
    pub fn horizon(&self) -> u64 {
        self.horizon
    }

    // ---- reading the files ----

    fn load_or_make(&mut self) -> Result<()> {
        if !self.state_path().exists() {
            self.depot = random_id()?;
            self.head = 0;
            self.horizon = 0;
            self.generation = 1;
            self.entries.clear();
            self.write_state()?;
            self.write_log(1)?;
            return Ok(());
        }
        self.load()?;
        // A log that was never made (the process that made the state ended before it): made now.
        if self.generation == 0 {
            self.write_log(1)?;
        }
        if self.log_len > self.offset {
            // A record cut off at the end (a process that ended in the middle of writing it):
            // what is left of it goes, so that the next record follows the last whole one.
            let file = OpenOptions::new().write(true).open(self.log_path())?;
            file.set_len(self.offset)?;
            self.log_len = self.offset;
        }
        let now = now_ms();
        if self.needs_compaction(now) {
            self.compact_inner(now)?;
        }
        Ok(())
    }

    /// Reads `state` and `log` whole.
    fn load(&mut self) -> Result<()> {
        let mut state = io::BufReader::new(File::open(self.state_path())?);
        let header = match profile::read_state_header(&mut state) {
            Ok(Stored::Current(header)) => header,
            Ok(Stored::Newer(_)) => return Err(Error::Newer),
            Ok(Stored::Foreign) | Err(_) => return Err(Error::Foreign("state file")),
        };
        self.depot = header.depot;
        self.head = header.head;
        self.horizon = header.horizon;
        self.entries.clear();
        loop {
            match profile::read_state_entry(&mut state) {
                Ok(Some(entry)) => {
                    self.entries.insert(identity(&entry.body), entry);
                }
                Ok(None) => break,
                Err(_) => return Err(Error::Foreign("state file")),
            }
        }
        self.generation = 0;
        self.offset = 0;
        self.log_len = 0;
        self.log_records = 0;
        let bytes = match fs::read(self.log_path()) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        self.log_len = bytes.len() as u64;
        let mut cursor = io::Cursor::new(bytes.as_slice());
        match profile::read_log_header(&mut cursor) {
            Ok(Stored::Current(LogHeader { generation })) => self.generation = generation,
            Ok(Stored::Newer(_)) => return Err(Error::Newer),
            // Shorter than a header: the log was being made.
            Ok(Stored::Foreign) | Err(_) if self.log_len < log_header_len() => {
                self.log_len = 0;
                return Ok(());
            }
            Ok(Stored::Foreign) | Err(_) => return Err(Error::Foreign("log")),
        }
        self.offset = cursor.position();
        self.read_records(&mut cursor, false);
        Ok(())
    }

    /// Takes the whole records from `cursor` (a log read from `self.offset`'s place) into the
    /// picture; what is cut off at its end is left alone. `fresh` says the records are news to
    /// the client (a refresh), and not what the picture was built from.
    fn read_records(&mut self, cursor: &mut io::Cursor<&[u8]>, fresh: bool) {
        loop {
            let before = cursor.position();
            match profile::read_log_entry(cursor) {
                Ok(Some(entry)) => {
                    self.offset += cursor.position() - before;
                    self.log_records += 1;
                    // A record the state already has (a compaction that ended before it made
                    // the new log): what the state says is newer.
                    if entry.seq <= self.head {
                        continue;
                    }
                    self.head = entry.seq;
                    if fresh {
                        self.pending.push_back(entry.clone());
                    }
                    self.entries.insert(identity(&entry.body), entry);
                }
                Ok(None) | Err(_) => break,
            }
        }
    }

    /// Brings the picture up to date with what other processes wrote.
    pub fn refresh(&mut self) -> Result<()> {
        self.locked(false, Store::refresh_inner)
    }

    fn refresh_inner(&mut self) -> Result<()> {
        // A log that is not there is a folder that was deleted: the profile never has none while
        // it is served (a lock keeps the one that makes it from being seen half made).
        let mut file = match File::open(self.log_path()) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(Error::Gone),
            Err(e) => return Err(e.into()),
        };
        let mut head = vec![0u8; usize::try_from(log_header_len()).unwrap_or(0)];
        let generation = match file.read_exact(&mut head) {
            Ok(()) => match profile::read_log_header(&mut head.as_slice()) {
                Ok(Stored::Current(LogHeader { generation })) => generation,
                Ok(Stored::Newer(_)) => return Err(Error::Newer),
                _ => return Err(Error::Foreign("log")),
            },
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if generation != self.generation {
            // A compaction: the picture is taken again, and the client is told of what is newer
            // than it was.
            let was = self.head;
            self.load()?;
            let mut news: Vec<Entry> = self
                .entries
                .values()
                .filter(|e| e.seq > was)
                .cloned()
                .collect();
            news.sort_by_key(|e| e.seq);
            self.pending.extend(news);
            return Ok(());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.log_len = self.offset + bytes.len() as u64;
        let mut cursor = io::Cursor::new(bytes.as_slice());
        self.read_records(&mut cursor, true);
        Ok(())
    }

    /// The records the client is yet to be told of.
    pub fn take_pending(&mut self) -> Vec<Entry> {
        self.pending.drain(..).collect()
    }

    /// The records that bring a client up to date: with `full`, every record there is and every
    /// deletion that is remembered, and otherwise every one newer than `since`; oldest first. A
    /// client that joins whole is told of the deletions too: a record it has that the home has
    /// deleted would otherwise be put back (its `put` is a conflict with the deletion's number,
    /// and the answer to a conflict is the newer record from the stream). What was waiting to be
    /// told is dropped: it is in what this returns or after it.
    pub fn catch_up(&mut self, since: u64, full: bool) -> Vec<Entry> {
        self.pending.clear();
        let mut out: Vec<Entry> = self
            .entries
            .values()
            .filter(|e| full || e.seq > since)
            .cloned()
            .collect();
        out.sort_by_key(|e| e.seq);
        out
    }

    // ---- writing ----

    fn write_state(&self) -> io::Result<()> {
        let mut bytes = profile::encode_state_header(&StateHeader {
            depot: self.depot,
            head: self.head,
            horizon: self.horizon,
        });
        let mut all: Vec<&Entry> = self.entries.values().collect();
        all.sort_by_key(|e| e.seq);
        for entry in all {
            bytes.extend_from_slice(&profile::encode_state_entry(entry));
        }
        write_whole(&self.state_path(), &bytes)
    }

    fn write_log(&mut self, generation: u64) -> io::Result<()> {
        let bytes = profile::encode_log_header(&LogHeader { generation });
        write_whole(&self.log_path(), &bytes)?;
        self.generation = generation;
        self.offset = bytes.len() as u64;
        self.log_len = self.offset;
        self.log_records = 0;
        Ok(())
    }

    /// Takes a record from a client. `Ok` says how it went and the number the record has (the
    /// current one on a conflict).
    pub fn put(&mut self, put: &Put, writer: [u8; 16]) -> Result<(PutResult, u64)> {
        if let Some(refusal) = refusal_of(&put.body) {
            return Ok((refusal, 0));
        }
        let now = now_ms();
        let outcome = self.locked(true, |s| {
            s.refresh_inner()?;
            for blob in put.body.blobs() {
                if !s.has_blob(blob) {
                    return Ok((PutResult::MissingBlob, 0));
                }
            }
            let current = s.entries.get(&identity(&put.body)).map(|e| e.seq);
            let agrees = match current {
                Some(seq) => seq == put.base,
                None => put.base <= s.horizon,
            };
            if !agrees {
                return Ok((PutResult::Conflict, current.unwrap_or(0)));
            }
            let entry = Entry {
                seq: s.head + 1,
                body: put.body.clone(),
                writer,
                time: now,
            };
            let bytes = profile::encode_log_entry(&entry)?;
            let mut file = OpenOptions::new().write(true).open(s.log_path())?;
            file.seek(SeekFrom::Start(s.offset))?;
            file.write_all(&bytes)?;
            file.set_len(s.offset + bytes.len() as u64)?;
            file.sync_data()?;
            s.offset += bytes.len() as u64;
            s.log_len = s.offset;
            s.log_records += 1;
            s.head = entry.seq;
            s.pending.push_back(entry.clone());
            let seq = entry.seq;
            s.entries.insert(identity(&entry.body), entry);
            Ok((PutResult::Done, seq))
        })?;
        if outcome.0 == PutResult::Done && self.needs_compaction(now) {
            self.locked(true, |s| s.compact_inner(now))?;
        }
        Ok(outcome)
    }

    fn needs_compaction(&self, now: u64) -> bool {
        self.log_records >= LOG_RECORDS
            || self.offset >= LOG_BYTES
            || self.entries.values().any(|e| gone_by(e, now))
    }

    /// Writes the profile whole to `state`, starts a new log, lets go of what has run out and
    /// deletes the blobs that no record needs.
    fn compact_inner(&mut self, now: u64) -> Result<()> {
        self.refresh_inner()?;
        let dropped: Vec<Identity> = self
            .entries
            .iter()
            .filter(|(_, e)| gone_by(e, now))
            .map(|(id, _)| id.clone())
            .collect();
        for id in dropped {
            if let Some(entry) = self.entries.remove(&id)
                && entry.body.deleted
            {
                self.horizon = self.horizon.max(entry.seq);
            }
        }
        self.write_state()?;
        self.write_log(self.generation + 1)?;
        self.sweep_blobs();
        Ok(())
    }

    // ---- blobs ----

    fn blob_dir(&self) -> PathBuf {
        self.dir.join(BLOBS)
    }

    fn blob_path(&self, digest: &Digest) -> PathBuf {
        self.blob_dir().join(hex(digest))
    }

    fn has_blob(&self, blob: &BlobRef) -> bool {
        fs::metadata(self.blob_path(&blob.sha256))
            .is_ok_and(|m| m.is_file() && m.len() == blob.size)
    }

    /// Those of `digests` that are not here.
    pub fn missing(&self, digests: &[Digest]) -> Vec<Digest> {
        digests
            .iter()
            .filter(|d| !fs::metadata(self.blob_path(d)).is_ok_and(|m| m.is_file()))
            .copied()
            .collect()
    }

    /// The blob, open for reading, and how long it is.
    pub fn open_blob(&self, digest: &Digest) -> io::Result<Option<(File, u64)>> {
        match File::open(self.blob_path(digest)) {
            Ok(file) => {
                let meta = file.metadata()?;
                Ok(meta.is_file().then_some((file, meta.len())))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Starts taking a blob in parts.
    pub fn begin_upload(
        &self,
        digest: Digest,
        total: u64,
        client: &[u8; 16],
    ) -> io::Result<Upload> {
        // The name is this process's own: a connection that was let go of may not have ended when
        // the same computer's next one comes, and two writers of one file would mix its bytes.
        let path = self.blob_dir().join(format!(
            "{PART_PREFIX}{}-{}-{}",
            hex(&digest),
            hex_of(client),
            std::process::id()
        ));
        // A part that is there is an upload this connection began before: it starts again.
        let _ = fs::remove_file(&path);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)?;
        Ok(Upload {
            file,
            path,
            hasher: Sha256::new(),
            digest,
            total,
            written: 0,
        })
    }

    /// Puts a finished upload in its place: the blob is there when this returns `true`; `false`
    /// when the sum of what came is not the one it was named by.
    pub fn finish_upload(&self, upload: Upload) -> io::Result<bool> {
        let Upload {
            file,
            path,
            hasher,
            digest,
            ..
        } = upload;
        file.sync_all()?;
        drop(file);
        if hasher.finish() != digest {
            let _ = fs::remove_file(&path);
            return Ok(false);
        }
        fs::rename(&path, self.blob_path(&digest))?;
        Ok(true)
    }

    /// Deletes the blobs no record points at that are older than their grace, and the files of
    /// uploads that were never finished.
    fn sweep_blobs(&self) {
        let kept: HashSet<String> = self
            .entries
            .values()
            .flat_map(|e| e.body.blobs().map(|b| hex(&b.sha256)).collect::<Vec<_>>())
            .collect();
        let Ok(read) = fs::read_dir(self.blob_dir()) else {
            return;
        };
        for entry in read.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Ok(meta) = entry.metadata() else { continue };
            let age = meta
                .modified()
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .unwrap_or(Duration::ZERO);
            let drop_it = if name.starts_with(PART_PREFIX) {
                age > PART_GRACE
            } else {
                !kept.contains(&name) && age > BLOB_GRACE
            };
            if meta.is_file() && drop_it {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn hex_of(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// An upload on its way.
pub struct Upload {
    file: File,
    path: PathBuf,
    hasher: Sha256,
    digest: Digest,
    total: u64,
    written: u64,
}

impl Upload {
    /// Takes the next bytes: they have to begin where the last ended, and not go past the end.
    pub fn write(&mut self, start: u64, bytes: &[u8]) -> io::Result<()> {
        let end = self.written + bytes.len() as u64;
        if start != self.written || end > self.total {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a part of a blob out of its place",
            ));
        }
        self.file.write_all(bytes)?;
        self.hasher.update(bytes);
        self.written = end;
        Ok(())
    }

    /// Whether every byte has come.
    pub fn complete(&self) -> bool {
        self.written == self.total
    }

    /// Gives the upload up: the file of its parts goes.
    pub fn abandon(self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn log_header_len() -> u64 {
    profile::encode_log_header(&LogHeader { generation: 0 }).len() as u64
}

/// Why a record is turned down before anything is looked at, or `None`.
fn refusal_of(body: &Body) -> Option<PutResult> {
    let known = matches!(
        body.kind,
        kind::COOKIE | kind::LOCAL_STORAGE | kind::DETACHED | kind::INDEXED_DB
    );
    if !known || body.key.is_empty() {
        return Some(PutResult::Bad);
    }
    let too_big = body.key.len() > MAX_KEY
        || body.value.len() > MAX_VALUE
        || body.blobs().any(|b| b.size > MAX_BLOB);
    too_big.then_some(PutResult::TooLarge)
}

/// Whether a record has run its course by `now`: a deletion that is old enough, or something
/// whose time is out.
fn gone_by(entry: &Entry, now: u64) -> bool {
    if entry.body.deleted {
        entry.time.saturating_add(TOMBSTONE_MS) < now
    } else {
        entry.body.expires.is_some_and(|at| at <= now)
    }
}
