//! The carrier: the command that ends in a bridge (`ssh host kablo attach`), and
//! the first words spoken with it, for the commands of kablo that are clients of
//! one (`kablo forward`, `kablo exec`).
//!
//! The command is taken as it is, so ssh's options, jump hosts, keys and
//! passwords stay in the user's ssh configuration and `ssh` prompts for what it
//! needs on the terminal: its standard error is ours. What it writes on its
//! standard output is the bridge's bytes and what it reads is ours.

use std::ffi::OsString;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use kablo_proto::WIRE;
use kablo_proto::v1::stream::{Incoming, IncomingDecoder};
use kablo_proto::v1::{Features, Message, Packet};

use crate::handshake::{self, ClientStep};
use crate::outbound;
use crate::sys;

/// Bytes read from the carrier at a time.
pub const READ_CHUNK: usize = outbound::CHUNK;

/// How long a command gets to end once this has finished with it, before it is
/// killed. Only the way out waits; nothing else here does.
const REAP_GRACE: Duration = Duration::from_secs(2);

/// How many characters of a text from the server are shown.
pub const SHOWN: usize = 200;

/// The two ends of the command that reaches the bridge, as seen from here.
#[derive(Debug)]
pub struct Wire {
    /// What the command writes: the bridge's bytes.
    pub input: OwnedFd,
    /// What the command reads: ours.
    pub output: OwnedFd,
}

/// Why a client of a bridge ended. It has no good ending of its own: what
/// finishes the work (a command's exit status) is not an `End`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum End {
    /// The command ended or stopped taking bytes.
    Carrier,
    /// A signal asked to stop.
    Signal(i32),
    /// The server will not do it, in words for a person.
    Declined(String),
    /// Something went wrong, in words for a person.
    Failed(String),
}

/// A text from the server as a person may be shown it: no control characters
/// (it could move the cursor or set the title) and not too long.
pub fn clean(text: &str) -> String {
    text.chars()
        .filter(|&c| !c.is_control() && !changes_direction(c))
        .take(SHOWN)
        .collect()
}

/// Characters that are not control characters and still change how a line
/// reads: the marks and overrides that reverse the direction of text, and the
/// zero-width ones.
fn changes_direction(c: char) -> bool {
    matches!(c,
        '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}')
}

/// A signal waiting in the signal pipe, if one is.
pub fn signal_in(signals: BorrowedFd<'_>) -> Option<i32> {
    let mut byte = [0u8; 1];
    match sys::read_fd(signals, &mut byte) {
        Ok(1) => byte.first().map(|&signal| i32::from(signal)),
        _ => None,
    }
}

/// Waits until the carrier has something to say (or a signal comes), reads
/// it, and appends it to `into`.
fn read_more(
    input: BorrowedFd<'_>,
    signals: Option<BorrowedFd<'_>>,
    buf: &mut [u8],
    into: &mut Vec<u8>,
) -> Result<(), End> {
    let mut set = vec![sys::want(input.as_raw_fd(), libc::POLLIN)];
    if let Some(signals) = signals {
        set.push(sys::want(signals.as_raw_fd(), libc::POLLIN));
    }
    sys::poll(&mut set, None)
        .map_err(|e| End::Failed(format!("cannot wait for the server: {e}")))?;
    if set.get(1).is_some_and(|fd| fd.revents != 0)
        && let Some(signal) = signals.and_then(signal_in)
    {
        return Err(End::Signal(signal));
    }
    if set.first().is_some_and(|fd| fd.revents != 0) {
        match sys::read_fd(input, buf) {
            Ok(0) | Err(_) => return Err(End::Carrier),
            Ok(n) => into.extend_from_slice(buf.get(..n).unwrap_or_default()),
        }
    }
    Ok(())
}

/// Writes all of `bytes` to the carrier, waiting for it where it is slow.
pub fn write_all(output: BorrowedFd<'_>, mut bytes: &[u8]) -> Result<(), End> {
    while !bytes.is_empty() {
        match sys::write_fd(output, bytes) {
            Ok(0) | Err(_) => return Err(End::Carrier),
            Ok(n) => bytes = bytes.get(n..).unwrap_or_default(),
        }
    }
    Ok(())
}

/// The handshake, then `features` and, right behind it, a `ping` carrying
/// `ping`: the first thing the bridge says after that tells what it is, and
/// `verdict` reads that. Nothing here has a time limit: the command may be
/// waiting for a password.
pub fn negotiate(
    carrier: &Wire,
    signals: Option<BorrowedFd<'_>>,
    decoder: &mut IncomingDecoder,
    ping: &[u8],
    verdict: impl Fn(&Message) -> Result<Features, End>,
) -> Result<Features, End> {
    let mut hello = Vec::new();
    WIRE.write_hello(&mut hello)
        .map_err(|e| End::Failed(e.to_string()))?;
    write_all(carrier.output.as_fd(), &hello)?;

    let mut buf = vec![0u8; READ_CHUNK];
    let mut seen = Vec::new();
    let consumed = loop {
        read_more(carrier.input.as_fd(), signals, &mut buf, &mut seen)?;
        match handshake::reply(&WIRE, &seen) {
            Ok(ClientStep::Need) => {}
            Ok(ClientStep::Done { consumed, .. }) => break consumed,
            Ok(ClientStep::Refused) => {
                return Err(End::Declined(
                    "the kablo on the server speaks no version of the protocol this one does"
                        .to_owned(),
                ));
            }
            Err(e) => {
                return Err(End::Failed(format!(
                    "what the command printed is not a kablo bridge: {e}"
                )));
            }
        }
    };
    decoder.push(seen.get(consumed..).unwrap_or_default());

    let mut asks = Vec::new();
    for message in [Message::Features, Message::Ping(ping.to_vec())] {
        Packet::connection(message)
            .encode(&mut asks)
            .map_err(|e| End::Failed(e.to_string()))?;
    }
    write_all(carrier.output.as_fd(), &asks)?;

    loop {
        match decoder.next_incoming() {
            Ok(Some(Incoming::Session(packet))) => return verdict(&packet.message),
            // Nothing has been opened: a stream message answers nothing, nor
            // does a frame of a channel this version does not have.
            Ok(Some(_)) => {}
            Ok(None) => {
                let mut more = Vec::new();
                read_more(carrier.input.as_fd(), signals, &mut buf, &mut more)?;
                decoder.push(&more);
            }
            Err(e) if e.is_recoverable() => {}
            Err(e) => {
                return Err(End::Failed(format!(
                    "the server's answer is not the protocol: {e}"
                )));
            }
        }
    }
}

/// A carrier that has been started: the process, and the name it is called by
/// in what is said to a person.
pub struct Carrier {
    child: Child,
    name: String,
}

/// Starts the command that reaches the bridge (the program first), with pipes
/// for its standard input and output. The error is in words for a person.
pub fn start(command: &[OsString]) -> Result<(Carrier, Wire), String> {
    let Some((program, arguments)) = command.split_first() else {
        return Err("the command after `--` is missing".to_owned());
    };
    let name = Path::new(program)
        .file_name()
        .unwrap_or(program)
        .to_string_lossy()
        .into_owned();
    // A program that cannot be run is named as it was given, not by its last
    // part: "ssh -o x host kablo attach" quoted as one word would otherwise be
    // "kablo attach", which reads as the server's kablo being missing.
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", program.to_string_lossy()))?;
    let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("cannot reach {name}'s input and output"));
    };
    let wire = Wire {
        input: OwnedFd::from(stdout),
        output: OwnedFd::from(stdin),
    };
    Ok((Carrier { child, name }, wire))
}

impl Carrier {
    /// The name it is called by.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Collects the command. With `stop` it is asked to end first; one that
    /// has not within [`REAP_GRACE`] is killed. This is the only wait in the
    /// module and it is on the way out.
    pub fn reap(&mut self, stop: bool) -> Option<ExitStatus> {
        if stop && let Ok(pid) = libc::pid_t::try_from(self.child.id()) {
            let _ = sys::kill(pid, libc::SIGTERM);
        }
        let start = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if start.elapsed() >= REAP_GRACE {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = self.child.kill();
        self.child.wait().ok()
    }
}

/// How a command ended, as a person reads it after its name.
pub fn describe(status: Option<ExitStatus>) -> String {
    match status {
        Some(status) => match (status.code(), status.signal()) {
            (Some(code), _) => format!("ended (exit status {code})"),
            (None, Some(signal)) => format!("was killed by signal {signal}"),
            (None, None) => "ended".to_owned(),
        },
        None => "ended".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_server_says_is_shown_without_control_characters() {
        assert_eq!(clean("a\u{1b}[31mb\nc\u{7}"), "a[31mbc");
        assert_eq!(
            clean("name\u{202e}gnp.exe\u{2066}\u{200b}"),
            "namegnp.exe",
            "no text that reads backwards"
        );
        assert_eq!(clean(&"x".repeat(1000)).len(), SHOWN);
    }
}
