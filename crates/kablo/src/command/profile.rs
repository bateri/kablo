//! `kablo profile`: keeps a browser profile on this machine for the bateri apps that connect to it
//! (see [`kablo_proto::profile`]). bateri opens it over the command stream of a session, so the
//! program's standard input and output are the connection: the hello of the protocol, then its
//! frames. One process serves one connection of one profile; any number of them serve the same
//! profile at once and see each other's records through the files ([`store`]).
//!
//! `kablo profile off` deletes every profile on this machine and leaves a mark that none is
//! kept (the server is not one the person trusts with their sign-ins, or is shared);
//! `kablo profile on` takes the mark away. With the mark there, an open is answered with
//! [`code::OFF`] and nothing is made.

mod store;

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use kablo_bateri::paths;
use kablo_proto::profile::{
    self as wire, Client, Failure, MAX_BLOB, MAX_PART, Mode, Open, Opened, Part, PutReply, Server,
    code,
};

use crate::cli::usage_error;
use store::{Store, Upload};

/// How often the files are looked at for what other processes wrote.
const LOOK: Duration = Duration::from_millis(200);

/// Runs `kablo profile` (without the words before it).
pub fn run(args: &[OsString], err: &mut dyn Write) -> u8 {
    let words: Vec<&str> = args.iter().filter_map(|a| a.to_str()).collect();
    if words.len() != args.len() {
        return usage_error(err, "profile takes no such words");
    }
    let Some(home) = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
    else {
        let _ = writeln!(err, "kablo: HOME is not set");
        return 1;
    };
    let browser = home.join(paths::DIR).join(paths::BROWSER);
    match words.as_slice() {
        [] => serve(&browser, err),
        ["off"] => report(switch_off(&browser), err),
        ["on"] => report(switch_on(&browser), err),
        _ => usage_error(err, "usage: kablo profile [off | on]"),
    }
}

fn report(done: io::Result<()>, err: &mut dyn Write) -> u8 {
    match done {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "kablo: {e}");
            1
        }
    }
}

fn make_dir(path: &std::path::Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Marks the server as one that keeps no profiles, and deletes the ones it has. The mark comes
/// first, so that nothing is made while the old ones are deleted.
fn switch_off(browser: &std::path::Path) -> io::Result<()> {
    make_dir(browser)?;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(browser.join(paths::PROFILES_OFF))?;
    match fs::remove_dir_all(browser.join(paths::PROFILES)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn switch_on(browser: &std::path::Path) -> io::Result<()> {
    match fs::remove_file(browser.join(paths::PROFILES_OFF)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// What the reader of the connection tells the loop.
enum Incoming {
    Message(Client),
    Closed,
    Broken(String),
}

/// The server's end of the connection.
struct Out<W: Write> {
    inner: W,
}

impl<W: Write> Out<W> {
    /// Says a message; a connection that cannot be written to is one that is gone.
    fn send(&mut self, message: &Server) -> Result<(), Ended> {
        wire::write_server(&mut self.inner, message)
            .map_err(io::Error::other)
            .and_then(|()| self.inner.flush())
            .map_err(|_| Ended::Gone)
    }

    fn fail(&mut self, code: u16, text: &str) {
        let _ = self.send(&Server::Error(Failure {
            code,
            text: text.into(),
        }));
    }
}

fn serve(browser: &std::path::Path, err: &mut dyn Write) -> u8 {
    let mut input = io::stdin();
    let mut out = Out {
        inner: io::stdout().lock(),
    };
    if let Err(e) = wire::WIRE.server(&mut input, &mut out.inner) {
        let _ = writeln!(err, "kablo: {e}");
        return 1;
    }
    let open = match wire::read_client(&mut input) {
        Ok(Some(Client::Open(open))) => open,
        Ok(None) => return 0,
        Ok(Some(_)) | Err(_) => {
            out.fail(code::INTERNAL, "the first message is the open");
            return 1;
        }
    };
    // Before a file or a folder is touched: the names are the only thing the folder's name is made of.
    if !store::app_ok(&open.app) || !store::name_ok(&open.name) {
        out.fail(code::NAME, "not the name of a profile");
        return 1;
    }
    if browser.join(paths::PROFILES_OFF).exists() {
        out.fail(code::OFF, "this server does not keep profiles");
        return 0;
    }
    let dir = browser
        .join(paths::PROFILES)
        .join(store::folder_name(&open.app, &open.name));
    let mut store = match Store::open(&dir) {
        Ok(store) => store,
        Err(store::Error::Newer) => {
            out.fail(code::NEWER, "the profile was written by a newer kablo");
            return 0;
        }
        Err(e) => {
            out.fail(code::INTERNAL, &e.to_string());
            return 1;
        }
    };
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        loop {
            let incoming = match wire::read_client(&mut input) {
                Ok(Some(message)) => Incoming::Message(message),
                Ok(None) => Incoming::Closed,
                Err(e) => Incoming::Broken(e.to_string()),
            };
            let last = !matches!(incoming, Incoming::Message(_));
            if tx.send(incoming).is_err() || last {
                break;
            }
        }
    });
    match conversation(&mut store, &open, &mut out, &rx) {
        Ok(status) => status,
        // The connection went (a write failed): nothing to say to nobody.
        Err(Ended::Gone) => 0,
        // `kablo profile off` deleted the profile under it: the same answer as an open now.
        Err(Ended::Store(store::Error::Gone)) if browser.join(paths::PROFILES_OFF).exists() => {
            out.fail(code::OFF, "this server does not keep profiles");
            0
        }
        Err(Ended::Store(e)) => {
            out.fail(code::INTERNAL, &e.to_string());
            1
        }
    }
}

/// What ends a conversation that did not end by itself.
enum Ended {
    /// The client is not there.
    Gone,
    /// The files said no.
    Store(store::Error),
}

impl From<io::Error> for Ended {
    fn from(e: io::Error) -> Ended {
        Ended::Store(store::Error::Io(e))
    }
}

impl From<store::Error> for Ended {
    fn from(e: store::Error) -> Ended {
        Ended::Store(e)
    }
}

/// How a client that says what it has is brought up to date.
fn mode_of(open: &Open, store: &Store) -> Mode {
    let same_server = open.depot == Some(store.depot());
    if !same_server || open.since == 0 || open.since > store.head() {
        Mode::Full
    } else if open.since < store.horizon() {
        Mode::FullReplace
    } else {
        Mode::Diff
    }
}

fn conversation<W: Write>(
    store: &mut Store,
    open: &Open,
    out: &mut Out<W>,
    rx: &mpsc::Receiver<Incoming>,
) -> Result<u8, Ended> {
    store.refresh()?;
    let mode = mode_of(open, store);
    out.send(&Server::Opened(Opened {
        head: store.head(),
        horizon: store.horizon(),
        mode,
        depot: store.depot(),
    }))?;
    let records = store.catch_up(open.since, mode != Mode::Diff);
    let head = store.head();
    for entry in records {
        out.send(&Server::Record(entry))?;
    }
    out.send(&Server::CaughtUp { head })?;

    let mut uploads: HashMap<wire::Digest, Upload> = HashMap::new();
    loop {
        match rx.recv_timeout(LOOK) {
            Ok(Incoming::Message(message)) => {
                let flow = answer(store, open, out, message, &mut uploads)?;
                if let Some(status) = flow {
                    return Ok(status);
                }
            }
            Ok(Incoming::Closed) | Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(0),
            Ok(Incoming::Broken(why)) => {
                out.fail(code::INTERNAL, &why);
                return Ok(1);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        store.refresh()?;
        for entry in store.take_pending() {
            out.send(&Server::Record(entry))?;
        }
    }
}

/// Answers one message. `Some(status)` ends the connection.
fn answer<W: Write>(
    store: &mut Store,
    open: &Open,
    out: &mut Out<W>,
    message: Client,
    uploads: &mut HashMap<wire::Digest, Upload>,
) -> Result<Option<u8>, Ended> {
    match message {
        Client::Open(_) => {
            out.fail(code::INTERNAL, "the profile is open already");
            return Ok(Some(1));
        }
        Client::Put(put) => {
            let (result, seq) = store.put(&put, open.client)?;
            out.send(&Server::PutReply(PutReply {
                req: put.req,
                result,
                seq,
            }))?;
        }
        Client::Have(digests) => {
            out.send(&Server::HaveReply(store.missing(&digests)))?;
        }
        Client::GetBlob(digest) => match store.open_blob(&digest)? {
            None => out.send(&Server::BlobMissing(digest))?,
            Some((file, total)) => send_blob(out, digest, file, total)?,
        },
        Client::Blob(part) => return upload(store, open, out, part, uploads),
    }
    Ok(None)
}

/// Sends a blob in parts.
fn send_blob<W: Write>(
    out: &mut Out<W>,
    digest: wire::Digest,
    mut file: fs::File,
    total: u64,
) -> Result<(), Ended> {
    let mut start = 0u64;
    let mut chunk = vec![0u8; MAX_PART];
    loop {
        let read = file.read(&mut chunk)?;
        let bytes = chunk.get(..read).unwrap_or_default().to_vec();
        out.send(&Server::Blob(Part {
            sha256: digest,
            total,
            start,
            bytes,
        }))?;
        start += read as u64;
        if read == 0 || start >= total {
            return Ok(());
        }
    }
}

/// Takes a part of a blob; the blob is put in its place when the last part came.
fn upload<W: Write>(
    store: &mut Store,
    open: &Open,
    out: &mut Out<W>,
    part: Part,
    uploads: &mut HashMap<wire::Digest, Upload>,
) -> Result<Option<u8>, Ended> {
    let digest = part.sha256;
    if part.bytes.len() > MAX_PART || part.total > MAX_BLOB {
        out.fail(code::INTERNAL, "a blob or a part of it is over the limit");
        return Ok(Some(1));
    }
    let current = match uploads.entry(digest) {
        Entry::Occupied(taken) => taken.into_mut(),
        Entry::Vacant(free) => {
            if store.missing(&[digest]).is_empty() {
                // Already here: a blob sent whole again is answered at its first part, and the
                // parts after it are of no use.
                if part.start == 0 {
                    out.send(&Server::BlobOk(digest))?;
                }
                return Ok(None);
            }
            if part.start != 0 {
                out.fail(code::INTERNAL, "a blob must begin at its start");
                return Ok(Some(1));
            }
            free.insert(store.begin_upload(digest, part.total, &open.client)?)
        }
    };
    if current.write(part.start, &part.bytes).is_err() {
        if let Some(upload) = uploads.remove(&digest) {
            upload.abandon();
        }
        out.fail(code::INTERNAL, "a part of a blob is out of its place");
        return Ok(Some(1));
    }
    if current.complete()
        && let Some(done) = uploads.remove(&digest)
    {
        if store.finish_upload(done)? {
            out.send(&Server::BlobOk(digest))?;
        } else {
            out.fail(
                code::INTERNAL,
                "the sum of the blob is not the one it was named by",
            );
            return Ok(Some(1));
        }
    }
    Ok(None)
}
