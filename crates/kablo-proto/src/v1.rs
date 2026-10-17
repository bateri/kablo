//! Version 1 of the messages.
//!
//! A message is a frame of a known kind on the session channel, plus the
//! session it concerns (empty for the connection-level ones). The kinds with
//! the high bit clear go client to server, the others server to client. The
//! frames of the other channel in use, the one that carries streams, are the
//! messages of [`stream`].
//! Payloads that are one fixed layout are read as a prefix (a later version
//! may append fields); payloads that may grow are [`tlv`] blocks.
//!
//! Version 1 grows only by addition: a new kind (a reader that does not know it
//! skips the frame by its length), a field appended to a fixed layout, a new entry
//! in a block. A message that exists is never changed, and its fixture never
//! with it; a change that cannot be made that way is a new version with its own
//! module and fixtures, and v1 stays readable.

use std::io::{Read, Write};
use std::time::Duration;

use crate::frame::{self, CHANNEL_SESSION, FrameDecoder, MAX_PAYLOAD, RawFrame};
use crate::tlv;
use crate::{Error, Result, SessionId};

pub mod stream;

/// The kind byte of each message.
pub mod kind {
    /// `attach`: stream a session from an offset.
    pub const ATTACH: u8 = 0x01;
    /// `detach`: stop streaming a session.
    pub const DETACH: u8 = 0x02;
    /// `resize`: the terminal's size changed.
    pub const RESIZE: u8 = 0x03;
    /// `terminate`: end the session.
    pub const TERMINATE: u8 = 0x04;
    /// `upgrade`: replace the relay with another binary.
    pub const UPGRADE: u8 = 0x05;
    /// `status`: ask for a session's state.
    pub const STATUS: u8 = 0x06;
    /// `input`: bytes for the program.
    pub const INPUT: u8 = 0x07;
    /// `list`: ask for every session.
    pub const LIST: u8 = 0x08;
    /// `new`: start a session.
    pub const NEW: u8 = 0x09;
    /// `ping`: ask the server to answer.
    pub const PING: u8 = 0x0a;
    /// `features`: ask what the server can do beyond sessions.
    pub const FEATURES: u8 = 0x0b;
    /// `ask`: ask the bridge where the asker stands for mail.
    pub const ASK: u8 = 0x0c;
    /// `decline`: refuse a ticket.
    pub const DECLINE: u8 = 0x0d;
    /// `attached`: the answer to `attach`.
    pub const ATTACHED: u8 = 0x81;
    /// `output`: bytes from the program.
    pub const OUTPUT: u8 = 0x82;
    /// `ended`: the program has exited.
    pub const ENDED: u8 = 0x83;
    /// `error`: a request failed.
    pub const ERROR: u8 = 0x84;
    /// `pong`: the answer to `ping`.
    pub const PONG: u8 = 0x85;
    /// `status_reply`: the answer to `status`.
    pub const STATUS_REPLY: u8 = 0x86;
    /// `list_reply`: the answer to `list`.
    pub const LIST_REPLY: u8 = 0x87;
    /// `created`: the answer to `new`.
    pub const CREATED: u8 = 0x88;
    /// `picture`: a piece of the screen as VT bytes.
    pub const PICTURE: u8 = 0x89;
    /// `features_reply`: the answer to `features`.
    pub const FEATURES_REPLY: u8 = 0x8a;
    /// `state`: what the session's terminal is doing, said unprompted.
    pub const STATE: u8 = 0x8b;
    /// `ask_reply`: the answer to `ask`.
    pub const ASK_REPLY: u8 = 0x8c;
    /// `ticket`: bytes are waiting for the client.
    pub const TICKET: u8 = 0x8d;
}

/// The name of a message kind, for diagnostics (`"unknown"` for a kind this
/// version does not have).
pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        kind::ATTACH => "attach",
        kind::DETACH => "detach",
        kind::RESIZE => "resize",
        kind::TERMINATE => "terminate",
        kind::UPGRADE => "upgrade",
        kind::STATUS => "status",
        kind::INPUT => "input",
        kind::LIST => "list",
        kind::NEW => "new",
        kind::PING => "ping",
        kind::FEATURES => "features",
        kind::ASK => "ask",
        kind::DECLINE => "decline",
        kind::ATTACHED => "attached",
        kind::OUTPUT => "output",
        kind::ENDED => "ended",
        kind::ERROR => "error",
        kind::PONG => "pong",
        kind::STATUS_REPLY => "status_reply",
        kind::LIST_REPLY => "list_reply",
        kind::CREATED => "created",
        kind::PICTURE => "picture",
        kind::FEATURES_REPLY => "features_reply",
        kind::STATE => "state",
        kind::ASK_REPLY => "ask_reply",
        kind::TICKET => "ticket",
        _ => "unknown",
    }
}

/// The attach offset that means "the current end; replay nothing".
pub const OFFSET_END: u64 = u64::MAX;

/// The most program bytes one `output` message can carry.
pub const MAX_OUTPUT: usize = MAX_PAYLOAD - 8;

/// The most bytes of a screen's picture one `picture` message can carry.
pub const MAX_PICTURE: usize = MAX_PAYLOAD - 13;

/// The most columns and rows a session's terminal can be given. The relay's
/// screen holds every cell of it twice (two screens), so a size is memory, and
/// no terminal a person has comes near these.
pub const MAX_COLS: u16 = 1000;

/// See [`MAX_COLS`].
pub const MAX_ROWS: u16 = 1000;

/// The most rows of scrollback a session's screen can be asked to keep.
pub const MAX_HISTORY: u32 = 100_000;

/// The scrollback rows a session keeps when it is not asked for any number.
pub const DEFAULT_HISTORY: u32 = 2_000;

/// The longest binary path an `upgrade` can name.
pub const MAX_PATH: usize = 4096;

/// A terminal's size: cells and, for programs that ask, pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Size {
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
    /// Width in pixels (0 when unknown).
    pub xpixel: u16,
    /// Height in pixels (0 when unknown).
    pub ypixel: u16,
}

impl Size {
    /// Bytes on the wire.
    pub const LEN: usize = 8;

    /// Whether the size is one a session's terminal can have, the unsaid
    /// (zero) included: no more than [`MAX_COLS`] by [`MAX_ROWS`].
    pub fn fits(&self) -> bool {
        self.cols <= MAX_COLS && self.rows <= MAX_ROWS
    }

    fn write(&self, out: &mut Vec<u8>) {
        for v in [self.cols, self.rows, self.xpixel, self.ypixel] {
            out.extend_from_slice(&v.to_be_bytes());
        }
    }

    fn read(fields: &mut Fields<'_>) -> Result<Size> {
        Ok(Size {
            cols: fields.u16()?,
            rows: fields.u16()?,
            xpixel: fields.u16()?,
            ypixel: fields.u16()?,
        })
    }
}

/// How a client looks at the session it attaches to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct View {
    /// The client's terminal. The session's terminal takes this size before
    /// anything is sent, so a picture of the screen is made for it. A size
    /// with no columns or no rows says nothing.
    pub size: Size,
    /// Send a picture of the screen even if the offset asked for is still in
    /// the journal.
    pub picture: bool,
    /// The client's terminal answers the questions in the bytes it is given and
    /// cannot be told to keep quiet about some of them (it writes the bytes to
    /// a terminal that is not the client's own to hold back). When the bytes
    /// from the offset asked for hold a question the server has already
    /// answered, send a picture instead of them: the program must not be
    /// answered twice.
    pub picture_if_answered: bool,
}

impl View {
    /// Bytes on the wire.
    const LEN: usize = Size::LEN + 1;

    /// The wire form: a size and a flags byte (bit 0: picture, bit 1:
    /// picture if answered).
    fn write(&self, out: &mut Vec<u8>) {
        self.size.write(out);
        out.push(u8::from(self.picture) | (u8::from(self.picture_if_answered) << 1));
    }

    fn read(fields: &mut Fields<'_>) -> Result<View> {
        let size = Size::read(fields)?;
        let flags = fields.u8()?;
        Ok(View {
            size,
            picture: flags & 1 != 0,
            picture_if_answered: flags & 2 != 0,
        })
    }
}

/// How a program ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// It called `exit` with this status.
    Code(i32),
    /// A signal killed it.
    Signal(i32),
}

impl Exit {
    /// Bytes on the wire: a kind (0 code, 1 signal) and an `i32`.
    pub const LEN: usize = 5;

    /// The wire form.
    pub fn to_bytes(self) -> [u8; Exit::LEN] {
        let (kind, value) = match self {
            Exit::Code(v) => (0, v),
            Exit::Signal(v) => (1, v),
        };
        let [a, b, c, d] = value.to_be_bytes();
        [kind, a, b, c, d]
    }

    /// Reads the wire form; bytes past it are ignored.
    pub fn from_bytes(bytes: &[u8]) -> Result<Exit> {
        Exit::read(&mut Fields::new(bytes, "exit status"))
    }

    fn read(fields: &mut Fields<'_>) -> Result<Exit> {
        let kind = fields.u8()?;
        let value = i32::from_be_bytes(fields.array()?);
        match kind {
            0 => Ok(Exit::Code(value)),
            1 => Ok(Exit::Signal(value)),
            _ => Err(Error::Malformed("exit status")),
        }
    }
}

/// Where a session is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct State(pub u8);

impl State {
    /// The program is running.
    pub const RUNNING: State = State(0);
    /// The program has ended; the session waits for someone to hear of it.
    pub const ENDED: State = State(1);
    /// The relay keeps failing; the program is alive but nobody can attach.
    pub const BROKEN: State = State(2);
}

/// Why a request failed. A code this version does not name still arrives
/// as itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorCode(pub u16);

impl ErrorCode {
    /// No session has that id.
    pub const UNKNOWN_SESSION: ErrorCode = ErrorCode(1);
    /// A session with that id exists already.
    pub const SESSION_EXISTS: ErrorCode = ErrorCode(2);
    /// The request does not make sense (a missing session id, a bad value).
    pub const BAD_REQUEST: ErrorCode = ErrorCode(3);
    /// The connection is not attached to the session.
    pub const NOT_ATTACHED: ErrorCode = ErrorCode(4);
    /// The relay could not be replaced; the old one keeps serving.
    pub const UPGRADE_FAILED: ErrorCode = ErrorCode(5);
    /// The session cannot be served right now.
    pub const UNAVAILABLE: ErrorCode = ErrorCode(6);
    /// An `input` was not written: the program is not reading what was typed
    /// before it, and these bytes would go past what the session holds for it.
    /// None of the message was taken; the connection and every other request
    /// are unaffected.
    pub const INPUT_FULL: ErrorCode = ErrorCode(7);
}

/// What a `new` asks for. Every field is optional: an empty request starts
/// the user's login shell at the server's default size.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NewSession {
    /// The command as an argument vector (never a shell string); empty means
    /// the user's login shell.
    pub argv: Vec<Vec<u8>>,
    /// Extra environment, `(name, value)`, over the defaults.
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    /// The starting directory.
    pub cwd: Option<Vec<u8>>,
    /// The starting size.
    pub size: Option<Size>,
    /// The journal's capacity in bytes.
    pub journal_size: Option<u64>,
    /// How many rows of scrollback the session's screen keeps, up to
    /// [`MAX_HISTORY`] ([`DEFAULT_HISTORY`] when not said).
    pub history: Option<u32>,
}

mod new_key {
    pub const ARG: u16 = 1;
    pub const ENV: u16 = 2;
    pub const CWD: u16 = 3;
    pub const SIZE: u16 = 4;
    pub const JOURNAL_SIZE: u16 = 5;
    pub const HISTORY: u16 = 6;
}

impl NewSession {
    /// Nothing that ends up in a `CString` may hold a NUL, a variable's name
    /// is non-empty and has no `=`, and a directory is not empty. Checked on
    /// both ends, so a sender cannot even produce what a receiver refuses.
    fn check(&self) -> Result<()> {
        let clean = |b: &[u8]| !b.contains(&0);
        if !self.argv.iter().all(|a| clean(a)) {
            return Err(Error::Malformed("argv"));
        }
        if !self
            .env
            .iter()
            .all(|(k, v)| !k.is_empty() && !k.contains(&b'=') && clean(k) && clean(v))
        {
            return Err(Error::Malformed("env"));
        }
        if self.cwd.as_ref().is_some_and(|c| c.is_empty() || !clean(c)) {
            return Err(Error::Malformed("cwd"));
        }
        Ok(())
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.check()?;
        let mut w = tlv::Writer::new();
        for arg in &self.argv {
            w.put(new_key::ARG, arg);
        }
        for (name, value) in &self.env {
            let mut entry = name.clone();
            entry.push(b'=');
            entry.extend_from_slice(value);
            w.put(new_key::ENV, &entry);
        }
        if let Some(cwd) = &self.cwd {
            w.put(new_key::CWD, cwd);
        }
        if let Some(size) = &self.size {
            let mut bytes = Vec::with_capacity(Size::LEN);
            size.write(&mut bytes);
            w.put(new_key::SIZE, &bytes);
        }
        if let Some(bytes) = self.journal_size {
            w.put_u64(new_key::JOURNAL_SIZE, bytes);
        }
        if let Some(rows) = self.history {
            w.put_u32(new_key::HISTORY, rows);
        }
        Ok(w.finish())
    }

    fn decode(block: &[u8]) -> Result<NewSession> {
        let mut new = NewSession::default();
        for entry in tlv::entries(block) {
            let (key, value) = entry?;
            match key {
                new_key::ARG => new.argv.push(value.to_vec()),
                new_key::ENV => {
                    let (name, value) = split_env(value).ok_or(Error::Malformed("env"))?;
                    new.env.push((name.to_vec(), value.to_vec()));
                }
                new_key::CWD => new.cwd = Some(value.to_vec()),
                new_key::SIZE => new.size = Some(Size::read(&mut Fields::new(value, "size"))?),
                new_key::JOURNAL_SIZE => {
                    new.journal_size = Some(tlv::read_u64(value, "journal size")?)
                }
                new_key::HISTORY => new.history = Some(tlv::read_u32(value, "history")?),
                _ => {}
            }
        }
        new.check()?;
        Ok(new)
    }
}

fn split_env(entry: &[u8]) -> Option<(&[u8], &[u8])> {
    let at = entry.iter().position(|&b| b == b'=')?;
    let (name, rest) = entry.split_at_checked(at)?;
    Some((name, rest.get(1..)?))
}

/// What the server knows of one session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// The session.
    pub id: SessionId,
    /// The shell's process id.
    pub pid: u32,
    /// The name of the program in the foreground of the terminal.
    pub program: Option<Vec<u8>>,
    /// The foreground program's working directory.
    pub cwd: Option<Vec<u8>>,
    /// The terminal's current size.
    pub size: Size,
    /// How many clients are attached.
    pub clients: u32,
    /// Where the session is in its life.
    pub state: State,
    /// Total bytes the program has written so far.
    pub offset: u64,
    /// The oldest offset the journal still holds.
    pub retained_from: u64,
    /// How the program ended, once it has.
    pub exit: Option<Exit>,
    /// The version of the relay serving the session (empty if unknown).
    pub version: String,
    /// The version of the warden that keeps the session (empty if it did not
    /// say: a warden from before it did).
    pub warden_version: String,
    /// What [`TerminalState::secret`] says.
    pub secret: bool,
    /// What [`TerminalState::remote`] says.
    pub remote: Option<Vec<u8>>,
    /// What [`TerminalState::remote_port`] says.
    pub remote_port: Option<u16>,
}

mod status_key {
    pub const ID: u16 = 1;
    pub const PID: u16 = 2;
    pub const PROGRAM: u16 = 3;
    pub const CWD: u16 = 4;
    pub const SIZE: u16 = 5;
    pub const CLIENTS: u16 = 6;
    pub const STATE: u16 = 7;
    pub const OFFSET: u16 = 8;
    pub const RETAINED_FROM: u16 = 9;
    pub const EXIT: u16 = 10;
    pub const VERSION: u16 = 11;
    pub const WARDEN_VERSION: u16 = 12;
    pub const SECRET: u16 = 13;
    pub const REMOTE: u16 = 14;
    pub const REMOTE_PORT: u16 = 15;
}

impl Status {
    /// A status with everything but the id at its zero.
    pub fn new(id: SessionId) -> Status {
        Status {
            id,
            pid: 0,
            program: None,
            cwd: None,
            size: Size::default(),
            clients: 0,
            state: State::RUNNING,
            offset: 0,
            retained_from: 0,
            exit: None,
            version: String::new(),
            warden_version: String::new(),
            secret: false,
            remote: None,
            remote_port: None,
        }
    }

    /// The wire form, a [`tlv`] block with the keys in ascending order. What
    /// the terminal's state adds (`secret`, `remote`, `remote_port`) is written
    /// only when there is something to say, so a session with none of it and
    /// a status from before the keys existed are the same bytes.
    pub fn to_block(&self) -> Vec<u8> {
        let mut w = tlv::Writer::new();
        w.put(status_key::ID, self.id.as_str().as_bytes());
        w.put_u32(status_key::PID, self.pid);
        if let Some(program) = &self.program {
            w.put(status_key::PROGRAM, program);
        }
        if let Some(cwd) = &self.cwd {
            w.put(status_key::CWD, cwd);
        }
        let mut size = Vec::with_capacity(Size::LEN);
        self.size.write(&mut size);
        w.put(status_key::SIZE, &size);
        w.put_u32(status_key::CLIENTS, self.clients);
        w.put_u8(status_key::STATE, self.state.0);
        w.put_u64(status_key::OFFSET, self.offset);
        w.put_u64(status_key::RETAINED_FROM, self.retained_from);
        if let Some(exit) = self.exit {
            w.put(status_key::EXIT, &exit.to_bytes());
        }
        if !self.version.is_empty() {
            w.put(status_key::VERSION, self.version.as_bytes());
        }
        if !self.warden_version.is_empty() {
            w.put(status_key::WARDEN_VERSION, self.warden_version.as_bytes());
        }
        if self.secret {
            w.put_u8(status_key::SECRET, 1);
        }
        if let Some(remote) = &self.remote {
            w.put(status_key::REMOTE, remote);
        }
        if let Some(port) = self.remote_port {
            w.put_u16(status_key::REMOTE_PORT, port);
        }
        w.finish()
    }

    /// Reads the wire form. Only the id is required; a key that is absent
    /// reads as its zero, one that is unknown is skipped.
    pub fn from_block(block: &[u8]) -> Result<Status> {
        let mut id = None;
        for entry in tlv::entries(block) {
            let (key, value) = entry?;
            if key == status_key::ID {
                id = Some(SessionId::from_bytes(value)?);
            }
        }
        let mut status = Status::new(id.ok_or(Error::Malformed("status without an id"))?);
        for entry in tlv::entries(block) {
            let (key, value) = entry?;
            match key {
                status_key::PID => status.pid = tlv::read_u32(value, "status pid")?,
                status_key::PROGRAM => status.program = Some(value.to_vec()),
                status_key::CWD => status.cwd = Some(value.to_vec()),
                status_key::SIZE => {
                    status.size = Size::read(&mut Fields::new(value, "status size"))?
                }
                status_key::CLIENTS => status.clients = tlv::read_u32(value, "status clients")?,
                status_key::STATE => status.state = State(tlv::read_u8(value, "status state")?),
                status_key::OFFSET => status.offset = tlv::read_u64(value, "status offset")?,
                status_key::RETAINED_FROM => {
                    status.retained_from = tlv::read_u64(value, "status retained")?
                }
                status_key::EXIT => status.exit = Some(Exit::from_bytes(value)?),
                status_key::VERSION => status.version = tlv::read_text(value, "status version")?,
                status_key::WARDEN_VERSION => {
                    status.warden_version = tlv::read_text(value, "status warden version")?
                }
                status_key::SECRET => {
                    status.secret = tlv::read_u8(value, "status secret")? & 1 != 0
                }
                status_key::REMOTE => status.remote = Some(value.to_vec()),
                status_key::REMOTE_PORT => {
                    status.remote_port = Some(tlv::read_u16(value, "status remote port")?)
                }
                _ => {}
            }
        }
        Ok(status)
    }
}

/// What the session's terminal is doing that the bytes of its output do not
/// say, as only the server (which holds the terminal's handle) can tell: the
/// payload of `state`. It is always the whole of it, so a field that is absent
/// now is nothing now, not "as before".
///
/// Nothing in it comes from the program's arguments except the target of a
/// remote session; the rest of a command line, and the environment, never
/// leave the server.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TerminalState {
    /// The name of the program in the terminal's foreground (as
    /// [`Status::program`]).
    pub program: Option<Vec<u8>>,
    /// The foreground program's working directory (as [`Status::cwd`]).
    pub cwd: Option<Vec<u8>>,
    /// The terminal does not show what is typed and takes it a line at a time:
    /// what a program sets to ask for a password. Written only when set.
    pub secret: bool,
    /// The target of the interactive ssh or mosh session in the foreground, as
    /// the user typed it (`user@host`, `host`, a name from ssh's configuration;
    /// an `ssh://` target without its scheme and port); a person is shown it
    /// with control characters removed. Nothing else of the command line is
    /// in it.
    pub remote: Option<Vec<u8>>,
    /// The port the user asked ssh for (`-p`, `-o Port=` or an `ssh://`
    /// target). Absent when they asked for none, which says nothing of the port
    /// ssh will use: its configuration decides. Absent for mosh.
    pub remote_port: Option<u16>,
}

mod state_key {
    pub const PROGRAM: u16 = 1;
    pub const CWD: u16 = 2;
    pub const SECRET: u16 = 3;
    pub const REMOTE: u16 = 4;
    pub const REMOTE_PORT: u16 = 5;
}

impl TerminalState {
    /// The wire form, a [`tlv`] block with the keys in ascending order; a field
    /// with nothing to say is not written.
    pub fn to_block(&self) -> Vec<u8> {
        let mut w = tlv::Writer::new();
        if let Some(program) = &self.program {
            w.put(state_key::PROGRAM, program);
        }
        if let Some(cwd) = &self.cwd {
            w.put(state_key::CWD, cwd);
        }
        if self.secret {
            w.put_u8(state_key::SECRET, 1);
        }
        if let Some(remote) = &self.remote {
            w.put(state_key::REMOTE, remote);
        }
        if let Some(port) = self.remote_port {
            w.put_u16(state_key::REMOTE_PORT, port);
        }
        w.finish()
    }

    /// Reads the wire form. Every key is optional and reads as nothing when
    /// absent; one that is unknown is skipped.
    pub fn from_block(block: &[u8]) -> Result<TerminalState> {
        let mut state = TerminalState::default();
        for entry in tlv::entries(block) {
            let (key, value) = entry?;
            match key {
                state_key::PROGRAM => state.program = Some(value.to_vec()),
                state_key::CWD => state.cwd = Some(value.to_vec()),
                state_key::SECRET => state.secret = tlv::read_u8(value, "state secret")? & 1 != 0,
                state_key::REMOTE => state.remote = Some(value.to_vec()),
                state_key::REMOTE_PORT => {
                    state.remote_port = Some(tlv::read_u16(value, "state remote port")?)
                }
                _ => {}
            }
        }
        Ok(state)
    }
}

/// What a server can do beyond terminal sessions: the answer to `features`.
/// A key that is absent reads as its zero, so a server that knows nothing of
/// streams answers (if at all) with the default, which has them off.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Features {
    /// Whether the server takes [`stream`]s.
    pub streams: bool,
    /// Why not, when it knows streams and has them off: text for a person, for
    /// a log (the client removes control characters before showing it).
    pub reason: Option<String>,
    /// The most streams the server keeps open at once on a connection (0: not
    /// said, so [`stream::MAX_STREAMS`]).
    pub max_streams: u32,
    /// The largest window the server takes for a stream: a client gives no
    /// larger one (0: not said, so [`stream::MAX_WINDOW`]). What the server
    /// gives is its own choice, never more than [`stream::MAX_WINDOW`].
    pub max_window: u32,
    /// The most bytes the server takes in one stream `data` message (0: not
    /// said, so [`stream::MAX_DATA`]).
    pub max_data: u32,
    /// Whether the server runs programs for the client: a stream whose `open`
    /// is a [`stream::Command`]. Independent of `streams`: an administrator
    /// who turns streams off has not turned commands off.
    pub commands: bool,
    /// Whether the bridge keeps a mailbox and gives its client the tickets of
    /// the mail that comes to it (see [`Offers::tickets`]). Written only when
    /// set, so a bridge without one is the bytes it was before there was mail.
    pub mailbox: bool,
}

mod features_key {
    pub const STREAMS: u16 = 1;
    pub const REASON: u16 = 2;
    pub const MAX_STREAMS: u16 = 3;
    pub const MAX_WINDOW: u16 = 4;
    pub const MAX_DATA: u16 = 5;
    pub const COMMANDS: u16 = 6;
    pub const MAILBOX: u16 = 7;
}

impl Features {
    /// What a server with streams on and the limits of this version says.
    pub fn with_streams() -> Features {
        Features {
            streams: true,
            reason: None,
            max_streams: stream::MAX_STREAMS,
            max_window: stream::MAX_WINDOW,
            max_data: u32::try_from(stream::MAX_DATA).unwrap_or(u32::MAX),
            commands: false,
            mailbox: false,
        }
    }

    /// The same features, and the server runs programs too.
    pub fn with_commands(self) -> Features {
        Features {
            commands: true,
            ..self
        }
    }

    /// The same features, and the bridge keeps a mailbox.
    pub fn with_mailbox(self) -> Features {
        Features {
            mailbox: true,
            ..self
        }
    }

    /// What a server that knows streams and has them turned off says, and why.
    pub fn streams_off(why: impl Into<String>) -> Features {
        Features {
            reason: Some(why.into()),
            ..Features::default()
        }
    }

    /// The wire form, a [`tlv`] block with the keys in ascending order. A limit
    /// of zero is not written, nor is `commands` when it is clear: a reply
    /// from before commands existed, and one from a server without them, are
    /// the same bytes.
    pub fn to_block(&self) -> Vec<u8> {
        let mut w = tlv::Writer::new();
        w.put_u8(features_key::STREAMS, u8::from(self.streams));
        if let Some(why) = &self.reason {
            w.put(features_key::REASON, why.as_bytes());
        }
        for (key, limit) in [
            (features_key::MAX_STREAMS, self.max_streams),
            (features_key::MAX_WINDOW, self.max_window),
            (features_key::MAX_DATA, self.max_data),
        ] {
            if limit != 0 {
                w.put_u32(key, limit);
            }
        }
        if self.commands {
            w.put_u8(features_key::COMMANDS, 1);
        }
        if self.mailbox {
            w.put_u8(features_key::MAILBOX, 1);
        }
        w.finish()
    }

    /// Reads the wire form. Every key is optional and reads as its zero when
    /// absent; one that is unknown is skipped.
    pub fn from_block(block: &[u8]) -> Result<Features> {
        let mut features = Features::default();
        for entry in tlv::entries(block) {
            let (key, value) = entry?;
            match key {
                features_key::STREAMS => {
                    features.streams = tlv::read_u8(value, "features streams")? & 1 != 0
                }
                features_key::REASON => {
                    features.reason = Some(tlv::read_text(value, "features reason")?)
                }
                features_key::MAX_STREAMS => {
                    features.max_streams = tlv::read_u32(value, "features max streams")?
                }
                features_key::MAX_WINDOW => {
                    features.max_window = tlv::read_u32(value, "features max window")?
                }
                features_key::MAX_DATA => {
                    features.max_data = tlv::read_u32(value, "features max data")?
                }
                features_key::COMMANDS => {
                    features.commands = tlv::read_u8(value, "features commands")? & 1 != 0
                }
                features_key::MAILBOX => {
                    features.mailbox = tlv::read_u8(value, "features mailbox")? & 1 != 0
                }
                _ => {}
            }
        }
        Ok(features)
    }
}

/// What a client says about itself in the body of a `features`. Nothing in it is
/// a promise the bridge can check; a key that is absent reads as its zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Offers {
    /// Whether the client is an app that takes tickets: the bridge gives it one
    /// for each conversation that comes to its mailbox, and gives none to a client
    /// that did not say so.
    pub tickets: bool,
}

mod offers_key {
    pub const TICKETS: u16 = 1;
}

impl Offers {
    /// A client that takes tickets.
    pub fn with_tickets() -> Offers {
        Offers { tickets: true }
    }

    /// The wire form, a [`tlv`] block with the keys in ascending order. It is
    /// never empty, so that a body is told from the absence of one.
    pub fn to_block(&self) -> Vec<u8> {
        let mut w = tlv::Writer::new();
        w.put_u8(offers_key::TICKETS, u8::from(self.tickets));
        w.finish()
    }

    /// Reads the wire form. Every key is optional and reads as zero when
    /// absent; one that is unknown is skipped.
    pub fn from_block(block: &[u8]) -> Result<Offers> {
        let mut offers = Offers::default();
        for entry in tlv::entries(block) {
            let (key, value) = entry?;
            if key == offers_key::TICKETS {
                offers.tickets = tlv::read_u8(value, "features tickets")? & 1 != 0;
            }
        }
        Ok(offers)
    }
}

/// Where a bridge's client stands for mail: the payload of `ask_reply`. The
/// terminal it is about is the session id of the packet. A key that is absent
/// reads as its zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Standing {
    /// Whether the client is attached to that terminal.
    pub attached: bool,
    /// How long since the client last used that terminal when it is attached,
    /// and since it last used any terminal of the server when it is not.
    /// Using one is attaching to it or sending it input. Whole milliseconds on
    /// the wire.
    pub idle: Duration,
    /// Whether the client takes tickets (see [`Offers::tickets`]).
    pub tickets: bool,
}

mod standing_key {
    pub const ATTACHED: u16 = 1;
    pub const IDLE: u16 = 2;
    pub const TICKETS: u16 = 3;
}

impl Standing {
    /// A client that is not attached to the terminal, took no tickets, and
    /// has been idle this long.
    pub fn idle(idle: Duration) -> Standing {
        Standing {
            idle,
            ..Standing::default()
        }
    }

    /// The same, and the client is attached to the terminal.
    pub fn with_attached(self) -> Standing {
        Standing {
            attached: true,
            ..self
        }
    }

    /// The same, and the client takes tickets.
    pub fn with_tickets(self) -> Standing {
        Standing {
            tickets: true,
            ..self
        }
    }

    /// The wire form, a [`tlv`] block with the keys in ascending order.
    /// Durations too long for the field (584 million years) are written as the
    /// longest.
    pub fn to_block(&self) -> Vec<u8> {
        let mut w = tlv::Writer::new();
        w.put_u8(standing_key::ATTACHED, u8::from(self.attached));
        w.put_u64(
            standing_key::IDLE,
            u64::try_from(self.idle.as_millis()).unwrap_or(u64::MAX),
        );
        w.put_u8(standing_key::TICKETS, u8::from(self.tickets));
        w.finish()
    }

    /// Reads the wire form. Every key is optional and reads as zero when
    /// absent; one that is unknown is skipped.
    pub fn from_block(block: &[u8]) -> Result<Standing> {
        let mut standing = Standing::default();
        for entry in tlv::entries(block) {
            let (key, value) = entry?;
            match key {
                standing_key::ATTACHED => {
                    standing.attached = tlv::read_u8(value, "ask reply attached")? & 1 != 0
                }
                standing_key::IDLE => {
                    standing.idle = Duration::from_millis(tlv::read_u64(value, "ask reply idle")?)
                }
                standing_key::TICKETS => {
                    standing.tickets = tlv::read_u8(value, "ask reply tickets")? & 1 != 0
                }
                _ => {}
            }
        }
        Ok(standing)
    }
}

/// The key under which a `list_reply` block holds each session's status.
const LISTED: u16 = 1;

/// A message, without the session it concerns (see [`Packet`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// Client to server: stream the session starting at `offset` (or
    /// [`OFFSET_END`]). Answered by `Attached`, then a picture of the screen
    /// when the bytes asked for are gone or the `view` asks for one (or asks for
    /// one if they hold a question already answered), then `Output`s.
    Attach {
        /// The first byte the client wants.
        offset: u64,
        /// The client's terminal and what it wants shown first; `None` from
        /// a client that says neither.
        view: Option<View>,
    },
    /// Client to server: stop streaming the session; the session lives on.
    Detach,
    /// Client to server: the terminal is now this size.
    Resize(Size),
    /// Client to server: end the session (`force`: SIGKILL rather than SIGHUP).
    Terminate {
        /// Kill rather than hang up.
        force: bool,
    },
    /// Client to server: replace the session's relay with the binary at this
    /// absolute path.
    Upgrade {
        /// The path, as bytes.
        path: Vec<u8>,
    },
    /// Client to server: ask for the session's [`Status`].
    Status,
    /// Client to server: bytes for the program's terminal.
    Input(Vec<u8>),
    /// Client to server: ask for the [`Status`] of every session.
    List,
    /// Client to server: start a session. The session id in the packet is the
    /// one asked for; without one the server makes a name.
    New(NewSession),
    /// Client to server: answer with a `Pong` carrying the same bytes.
    Ping(Vec<u8>),
    /// Client to server: say what you can do beyond sessions. Answered by
    /// `FeaturesReply`, which a server that has it sends before the `Pong` of a
    /// `Ping` that follows (and one that has not skips this and sends only the
    /// `Pong`).
    Features,
    /// Client to server: the same as `Features`, and the client says what it
    /// takes. A `features` with a body that cannot be read is read as one
    /// without, so a body is never a reason to refuse the question.
    FeaturesWith(Offers),
    /// Client to server: ask where the asker stands for mail. The session id of
    /// the packet, if any, is the terminal the question is about; without one it
    /// is about the terminal the asker runs in, which the bridge finds out
    /// itself. Answered by `AskReply`.
    Ask,
    /// Client to server: the client refuses a ticket. The sender's `open` is
    /// answered `failed` with [`stream::FailCode::DECLINED`] and this text.
    Decline {
        /// The ticket refused.
        ticket: u32,
        /// In the client's own words, for a person, who sees it with control
        /// characters removed.
        text: String,
    },
    /// Server to client: the stream starts at `offset`; `truncated` says the
    /// bytes the client asked for are not coming and a picture is, because they
    /// had left the journal or held a question the server answered (see
    /// [`View::picture_if_answered`]).
    Attached {
        /// The offset of the first `Output` that follows (and of the picture
        /// before it, if one is sent).
        offset: u64,
        /// Whether the picture that follows takes the place of bytes the
        /// client asked for: the journal no longer held the offset asked for,
        /// or the view asked for a picture in place of bytes that hold a
        /// question the server already answered. Not set for a picture the
        /// client asked for outright.
        truncated: bool,
        /// How far the server has answered the questions the program asked its
        /// terminal: every question in the output before this offset was
        /// answered by the server, or left to a client that was attached when
        /// the output was read; the program is owed no answer to them. A client
        /// with a terminal of its own that is shown that output again must not
        /// send that terminal's answers to those questions; the program would
        /// be answered twice. 0 says nothing was answered, and is also what a
        /// server from before this field says; it is not written, so the two are
        /// the same bytes.
        answered: u64,
    },
    /// Server to client: program output starting at `offset`.
    Output {
        /// The offset of the first byte of `data` in the program's output.
        offset: u64,
        /// The bytes, exactly as the program wrote them.
        data: Vec<u8>,
    },
    /// Server to client: the program has exited and everything it wrote has
    /// been sent.
    Ended(Exit),
    /// Server to client: the request failed.
    Error {
        /// Why.
        code: ErrorCode,
        /// For a log, never a program: a client strips control characters
        /// before showing it.
        text: String,
    },
    /// Server to client: the answer to `Ping`.
    Pong(Vec<u8>),
    /// Server to client: the answer to `Status`.
    StatusReply(Status),
    /// Server to client: the answer to `List`.
    ListReply(Vec<Status>),
    /// Server to client: the answer to `New`.
    Created(Status),
    /// Server to client: the answer to `Features`.
    FeaturesReply(Features),
    /// Server to client: what the session's terminal is doing (`state` on the
    /// wire; not [`State`], where the session is in its life). Sent to a client
    /// right after `Attached` and again whenever any of it changes, whole each
    /// time. A server that does not know it sends none, and a client that does
    /// not know it skips it.
    TerminalState(TerminalState),
    /// Server to client: the answer to `Ask`. The session id of the packet is the
    /// terminal the answer is about, and none when there is none.
    AskReply(Standing),
    /// Server to client: bytes are waiting for the client. The number is the
    /// ticket's, which the client answers with an `open` that carries it
    /// ([`stream::Message::OpenTicket`]) or with `Decline`. The session id of
    /// the packet is the terminal the bytes come from, and none when they come
    /// from none.
    Ticket(u32),
    /// Server to client: a piece of the screen as the bytes that build it. A
    /// picture follows an `Attached` and is one or more pieces in order, the
    /// last without `more`; the `Output` after it starts at `offset`.
    Picture {
        /// The offset of the program's output the picture is of: the stream
        /// goes on from it.
        offset: u64,
        /// The columns of the terminal the picture was made for.
        cols: u16,
        /// The rows of the terminal the picture was made for.
        rows: u16,
        /// More pieces of this picture follow.
        more: bool,
        /// The bytes, to be written to a freshly reset terminal in order.
        data: Vec<u8>,
    },
}

impl Message {
    /// An `attach` that says nothing of the client's terminal.
    pub fn attach(offset: u64) -> Message {
        Message::Attach { offset, view: None }
    }
}

impl Message {
    /// The kind byte of the message.
    pub fn kind(&self) -> u8 {
        match self {
            Message::Attach { .. } => kind::ATTACH,
            Message::Detach => kind::DETACH,
            Message::Resize(_) => kind::RESIZE,
            Message::Terminate { .. } => kind::TERMINATE,
            Message::Upgrade { .. } => kind::UPGRADE,
            Message::Status => kind::STATUS,
            Message::Input(_) => kind::INPUT,
            Message::List => kind::LIST,
            Message::New(_) => kind::NEW,
            Message::Ping(_) => kind::PING,
            Message::Features | Message::FeaturesWith(_) => kind::FEATURES,
            Message::Ask => kind::ASK,
            Message::Decline { .. } => kind::DECLINE,
            Message::Attached { .. } => kind::ATTACHED,
            Message::Output { .. } => kind::OUTPUT,
            Message::Ended(_) => kind::ENDED,
            Message::Error { .. } => kind::ERROR,
            Message::Pong(_) => kind::PONG,
            Message::StatusReply(_) => kind::STATUS_REPLY,
            Message::ListReply(_) => kind::LIST_REPLY,
            Message::Created(_) => kind::CREATED,
            Message::Picture { .. } => kind::PICTURE,
            Message::FeaturesReply(_) => kind::FEATURES_REPLY,
            Message::TerminalState(_) => kind::STATE,
            Message::AskReply(_) => kind::ASK_REPLY,
            Message::Ticket(_) => kind::TICKET,
        }
    }

    fn payload(&self) -> Result<Vec<u8>> {
        Ok(match self {
            Message::Detach
            | Message::Status
            | Message::List
            | Message::Features
            | Message::Ask => Vec::new(),
            Message::FeaturesWith(offers) => offers.to_block(),
            Message::Decline { ticket, text } => {
                let mut out = ticket.to_be_bytes().to_vec();
                out.extend_from_slice(text.as_bytes());
                out
            }
            Message::Ticket(ticket) => ticket.to_be_bytes().to_vec(),
            Message::Attach { offset, view } => {
                let mut out = offset.to_be_bytes().to_vec();
                if let Some(view) = view {
                    view.write(&mut out);
                }
                out
            }
            Message::Resize(size) => {
                let mut out = Vec::with_capacity(Size::LEN);
                size.write(&mut out);
                out
            }
            Message::Terminate { force } => vec![u8::from(*force)],
            Message::Upgrade { path } => {
                if path.is_empty() || path.len() > MAX_PATH || path.contains(&0) {
                    return Err(Error::Malformed("upgrade path"));
                }
                path.clone()
            }
            Message::Input(bytes) | Message::Ping(bytes) | Message::Pong(bytes) => bytes.clone(),
            Message::New(new) => new.encode()?,
            Message::Attached {
                offset,
                truncated,
                answered,
            } => {
                let mut out = offset.to_be_bytes().to_vec();
                out.push(u8::from(*truncated));
                if *answered != 0 {
                    out.extend_from_slice(&answered.to_be_bytes());
                }
                out
            }
            Message::Output { offset, data } => {
                let mut out = Vec::with_capacity(8 + data.len());
                out.extend_from_slice(&offset.to_be_bytes());
                out.extend_from_slice(data);
                out
            }
            Message::Ended(exit) => exit.to_bytes().to_vec(),
            Message::Error { code, text } => {
                let mut out = code.0.to_be_bytes().to_vec();
                out.extend_from_slice(text.as_bytes());
                out
            }
            Message::Picture {
                offset,
                cols,
                rows,
                more,
                data,
            } => {
                if data.len() > MAX_PICTURE {
                    return Err(Error::Malformed("picture"));
                }
                let mut out = Vec::with_capacity(13 + data.len());
                out.extend_from_slice(&offset.to_be_bytes());
                out.extend_from_slice(&cols.to_be_bytes());
                out.extend_from_slice(&rows.to_be_bytes());
                out.push(u8::from(*more));
                out.extend_from_slice(data);
                out
            }
            Message::StatusReply(status) | Message::Created(status) => status.to_block(),
            Message::FeaturesReply(features) => features.to_block(),
            Message::TerminalState(state) => state.to_block(),
            Message::AskReply(standing) => standing.to_block(),
            Message::ListReply(all) => {
                let mut w = tlv::Writer::new();
                for status in all {
                    w.put(LISTED, &status.to_block());
                }
                w.finish()
            }
        })
    }

    /// `Ok(None)` for a kind this version does not know.
    fn parse(kind: u8, payload: &[u8]) -> Result<Option<Message>> {
        let mut f = Fields::new(payload, kind_name(kind));
        Ok(Some(match kind {
            kind::ATTACH => {
                let offset = f.u64()?;
                // Appended after the offset by a client that has more to say; an
                // `attach` without it, or with less than a whole view after the
                // offset, is the one that was always there (what follows a fixed
                // layout is ignored).
                let view = if f.len() < View::LEN {
                    None
                } else {
                    Some(View::read(&mut f)?)
                };
                Message::Attach { offset, view }
            }
            kind::DETACH => Message::Detach,
            kind::RESIZE => Message::Resize(Size::read(&mut f)?),
            kind::TERMINATE => Message::Terminate {
                force: f.u8()? & 1 != 0,
            },
            kind::UPGRADE => {
                if payload.is_empty() || payload.len() > MAX_PATH || payload.contains(&0) {
                    return Err(Error::Malformed("upgrade path"));
                }
                Message::Upgrade {
                    path: payload.to_vec(),
                }
            }
            kind::STATUS => Message::Status,
            kind::INPUT => Message::Input(payload.to_vec()),
            kind::LIST => Message::List,
            kind::NEW => Message::New(NewSession::decode(payload)?),
            kind::PING => Message::Ping(payload.to_vec()),
            // A body that is not there or cannot be read is a `features` that
            // says nothing, as it has always been: the question is never refused
            // for what is said with it.
            kind::FEATURES => match Offers::from_block(payload) {
                Ok(offers) if !payload.is_empty() => Message::FeaturesWith(offers),
                _ => Message::Features,
            },
            kind::ASK => Message::Ask,
            kind::DECLINE => Message::Decline {
                ticket: f.u32()?,
                text: String::from_utf8_lossy(f.rest()).into_owned(),
            },
            kind::ATTACHED => {
                let offset = f.u64()?;
                let truncated = f.u8()? & 1 != 0;
                // Appended after the flags by a server that has more to say; an
                // `attached` without a whole one, as the first servers sent it,
                // answered nothing.
                let answered = if f.len() < 8 { 0 } else { f.u64()? };
                Message::Attached {
                    offset,
                    truncated,
                    answered,
                }
            }
            kind::OUTPUT => Message::Output {
                offset: f.u64()?,
                data: f.rest().to_vec(),
            },
            kind::ENDED => Message::Ended(Exit::read(&mut f)?),
            kind::ERROR => {
                let code = ErrorCode(f.u16()?);
                Message::Error {
                    code,
                    text: String::from_utf8_lossy(f.rest()).into_owned(),
                }
            }
            kind::PONG => Message::Pong(payload.to_vec()),
            kind::STATUS_REPLY => Message::StatusReply(Status::from_block(payload)?),
            kind::LIST_REPLY => {
                let mut all = Vec::new();
                for entry in tlv::entries(payload) {
                    let (key, value) = entry?;
                    if key == LISTED {
                        all.push(Status::from_block(value)?);
                    }
                }
                Message::ListReply(all)
            }
            kind::CREATED => Message::Created(Status::from_block(payload)?),
            kind::FEATURES_REPLY => Message::FeaturesReply(Features::from_block(payload)?),
            kind::STATE => Message::TerminalState(TerminalState::from_block(payload)?),
            kind::ASK_REPLY => Message::AskReply(Standing::from_block(payload)?),
            kind::TICKET => Message::Ticket(f.u32()?),
            kind::PICTURE => Message::Picture {
                offset: f.u64()?,
                cols: f.u16()?,
                rows: f.u16()?,
                more: f.u8()? & 1 != 0,
                data: f.rest().to_vec(),
            },
            _ => return Ok(None),
        }))
    }
}

/// A message and the session it concerns: what one frame carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Packet {
    /// The session, or `None` for a connection-level message (`list`, `ping`,
    /// a `new` that lets the server choose the name).
    pub session: Option<SessionId>,
    /// The message.
    pub message: Message,
}

impl Packet {
    /// A message about a session.
    pub fn about(session: SessionId, message: Message) -> Packet {
        Packet {
            session: Some(session),
            message,
        }
    }

    /// A message about the connection.
    pub fn connection(message: Message) -> Packet {
        Packet {
            session: None,
            message,
        }
    }

    /// The frame this packet is.
    pub fn to_frame(&self) -> Result<RawFrame> {
        Ok(RawFrame {
            kind: self.message.kind(),
            channel: CHANNEL_SESSION,
            session: self
                .session
                .as_ref()
                .map(|s| s.as_str().as_bytes().to_vec())
                .unwrap_or_default(),
            payload: self.message.payload()?,
        })
    }

    /// Reads a packet out of a frame. `Ok(None)` for a frame this version
    /// skips: a channel other than the session channel, or a kind it does not
    /// know.
    pub fn from_frame(frame: RawFrame) -> Result<Option<Packet>> {
        if frame.channel != CHANNEL_SESSION {
            return Ok(None);
        }
        let Some(message) = Message::parse(frame.kind, &frame.payload)? else {
            return Ok(None);
        };
        let session = if frame.session.is_empty() {
            None
        } else {
            Some(SessionId::from_bytes(&frame.session)?)
        };
        Ok(Some(Packet { session, message }))
    }

    /// Appends the packet's bytes to `out` (for a writer that buffers).
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        frame::encode_frame(out, &self.to_frame()?)
    }
}

/// Reads the next packet, skipping frames this version does not know.
/// `Ok(None)` is the stream ending between frames. After an
/// [`Error::is_recoverable`] error the next call reads the next frame.
pub fn read_packet<R: Read>(reader: &mut R) -> Result<Option<Packet>> {
    frame::read_with(reader, Packet::from_frame)
}

/// Writes one packet.
pub fn write_packet<W: Write>(writer: &mut W, packet: &Packet) -> Result<()> {
    frame::write_frame(writer, &packet.to_frame()?)
}

/// Turns bytes pushed in arbitrary pieces into packets: [`read_packet`] for a
/// reader that cannot block.
#[derive(Debug, Default)]
pub struct PacketDecoder {
    frames: FrameDecoder,
}

impl PacketDecoder {
    /// An empty decoder.
    pub fn new() -> PacketDecoder {
        PacketDecoder::default()
    }

    /// Adds bytes that arrived.
    pub fn push(&mut self, bytes: &[u8]) {
        self.frames.push(bytes);
    }

    /// How many pushed bytes are not yet part of a returned packet.
    pub fn pending(&self) -> usize {
        self.frames.pending()
    }

    /// The next packet, or `None` when more bytes are needed.
    pub fn next_packet(&mut self) -> Result<Option<Packet>> {
        self.frames.next_with(Packet::from_frame)
    }
}

/// Reads a fixed layout field by field.
struct Fields<'a> {
    rest: &'a [u8],
    what: &'static str,
}

impl<'a> Fields<'a> {
    fn new(rest: &'a [u8], what: &'static str) -> Fields<'a> {
        Fields { rest, what }
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let (head, tail) = self
            .rest
            .split_first_chunk::<N>()
            .ok_or(Error::Malformed(self.what))?;
        self.rest = tail;
        Ok(*head)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(u8::from_be_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_be_bytes(self.array()?))
    }

    fn len(&self) -> usize {
        self.rest.len()
    }

    fn rest(self) -> &'a [u8] {
        self.rest
    }
}
