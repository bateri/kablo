//! `kablo upgrade`: replace the relay of a session, or of every session on
//! this machine, with the one in another binary, without ending anything.
//!
//! The command asks a session's relay (`upgrade` with the binary's absolute
//! path) and then listens. There are two ways it ends. The relay may answer
//! with an `error` (the binary is not one to run, the new relay stopped before
//! it took over): the old relay keeps serving and the command says why. Or the
//! connection closes: the old relay has let go and exited, and a session whose
//! relay had been replaced says nothing more. The command then asks the
//! session for its status, which only the relay that took over can answer, so
//! "replaced" is printed only for a session that is there to be asked.

use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{ErrorCode, Message, Packet, Status};

use crate::peer::{Gone, Peer};
use crate::{sockdir, sys};

/// How long one session has to be replaced and to answer afterwards.
pub const WAIT: Duration = Duration::from_secs(20);

/// How long a relay has to answer the handshake of the request: a socket
/// that takes the connection and says nothing has nobody serving it.
const OPEN_WAIT: Duration = Duration::from_secs(3);

/// Which sessions to replace the relay of.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// Every session this user has on this machine.
    All,
    /// One.
    One(SessionId),
}

/// Replaces the relays and says how each went. Returns the exit status: 0 when
/// every session was replaced (also when there were none), 1 otherwise.
pub fn run(
    dir: &Path,
    target: Target,
    binary: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    let ids = match target {
        Target::One(id) => vec![id],
        Target::All => match sockdir::session_ids(dir) {
            Ok(ids) => ids,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                let _ = writeln!(err, "kablo: {e}");
                return 1;
            }
        },
    };
    let mut status = 0;
    for id in ids {
        match replace(dir, &id, binary) {
            Ok(now) => {
                let _ = writeln!(out, "{id}: relay replaced (now {})", now.version);
            }
            Err(why) => {
                let _ = writeln!(err, "kablo: {id}: {why}");
                status = 1;
            }
        }
    }
    status
}

/// Replaces the relay of `id`; the status the relay that took over gave.
fn replace(dir: &Path, id: &SessionId, binary: &Path) -> Result<Status, String> {
    let path = sockdir::socket_path(dir, id);
    let by = Instant::now() + WAIT;
    let mut peer = connect(&path)?;
    let request = Packet::about(
        id.clone(),
        Message::Upgrade {
            path: binary.as_os_str().as_bytes().to_vec(),
        },
    );
    if !peer.queue(&request) {
        return Err("that path is too long to send".to_owned());
    }
    let opens_by = (Instant::now() + OPEN_WAIT).min(by);
    loop {
        let until = if peer.is_open() { by } else { opens_by };
        let heard = hear(&mut peer, until);
        for packet in heard.packets {
            if let Message::Error { code, text } = packet.message {
                return Err(if code == ErrorCode::UPGRADE_FAILED {
                    format!("not replaced: {}", plain(&text))
                } else {
                    plain(&text)
                });
            }
        }
        match heard.end {
            End::Open => {}
            End::Closed => break,
            End::Gone(why) => return Err(why),
            End::Late if !peer.is_open() => {
                return Err("nobody is serving its socket".to_owned());
            }
            End::Late => return Err("the relay did not answer in time".to_owned()),
        }
    }
    asked(&path, id, by)
}

/// The status of `id`, asked of whatever relay is serving once the one that
/// was is gone. Tried again while the relay that took over is coming up.
fn asked(path: &Path, id: &SessionId, by: Instant) -> Result<Status, String> {
    loop {
        if let Ok(mut peer) = connect(path) {
            peer.queue(&Packet::about(id.clone(), Message::Status));
            loop {
                let heard = hear(&mut peer, by);
                for packet in heard.packets {
                    match packet.message {
                        Message::StatusReply(status) => return Ok(status),
                        Message::Error { text, .. } => return Err(plain(&text)),
                        _ => {}
                    }
                }
                match heard.end {
                    End::Open => {}
                    End::Closed | End::Gone(_) => break,
                    End::Late => return Err(not_back()),
                }
            }
        }
        if Instant::now() >= by {
            return Err(not_back());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn not_back() -> String {
    "the session does not answer after the change".to_owned()
}

fn connect(path: &Path) -> Result<Peer, String> {
    Peer::connect(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "there is no such session".to_owned(),
        std::io::ErrorKind::ConnectionRefused => "nobody answers on its socket".to_owned(),
        _ => e.to_string(),
    })
}

/// How a wait for a connection ended.
enum End {
    /// It is open and nothing ended it.
    Open,
    /// The other end closed it.
    Closed,
    /// It failed in a way worth a sentence.
    Gone(String),
    /// The time ran out.
    Late,
}

/// What came of one wait.
struct Heard {
    packets: Vec<Packet>,
    end: End,
}

/// Writes what is queued and waits (to `by`) for the connection to say
/// something.
fn hear(peer: &mut Peer, by: Instant) -> Heard {
    let mut heard = Heard {
        packets: Vec::new(),
        end: End::Open,
    };
    if peer.flush().is_err() {
        heard.end = End::Closed;
        return heard;
    }
    let left = by.saturating_duration_since(Instant::now());
    if left.is_zero() {
        heard.end = End::Late;
        return heard;
    }
    let mut set = [sys::want(peer.fd(), peer.interest(true))];
    if sys::poll(&mut set, Some(left)).is_err() {
        heard.end = End::Late;
        return heard;
    }
    let revents = set.first().map_or(0, |fd| fd.revents);
    if revents == 0 {
        return heard;
    }
    if peer.flush().is_err() {
        heard.end = End::Closed;
        return heard;
    }
    if let Err(gone) = peer.read(&mut heard.packets) {
        heard.end = match gone {
            Gone::Closed => End::Closed,
            Gone::Refused => End::Gone("the relay speaks no version this kablo does".to_owned()),
            Gone::Broken(why) => End::Gone(why),
        };
    }
    heard
}

/// A relay's words for a person: its control characters gone.
fn plain(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relays_words_lose_their_control_characters() {
        assert_eq!(plain("no\u{1b}[31m good\n"), "no[31m good");
    }
}
