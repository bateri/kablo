//! `kablo upgrade`: replace the relay of a session with the one in another
//! binary, without ending anything. The
//! warden of the session renews itself into the same binary right after, when
//! it can.
//!
//! The command asks a session's relay (`upgrade` with the binary's absolute
//! path) and then listens. There are two ways it ends. The relay may answer
//! with an `error` (the binary is not one to run, the new relay stopped before
//! it took over): the old relay keeps serving and the command says why. Or the
//! connection closes: the old relay has let go and exited, and a session whose
//! relay had been replaced says nothing more. The command then asks the
//! session for its status, which only the relay that took over can answer, so
//! "replaced" is printed only for a session that is there to be asked.
//!
//! The warden renews itself a moment after the relay took over, when the
//! session is at rest, and tells the relay which build it now is. The command
//! waits a little for that to show in the status ([`RENEWAL_WAIT`]) and says
//! so when the warden stays: a warden from before renewals existed (it never
//! said its version) is not waited for. Two builds of one release cannot be
//! told apart by their version, so for those the command cannot see whether
//! the warden has renewed; a development build is named by its commit as well
//! (`0.2.0-dev+3f2a9c1d0b44`), and is told apart from the release and from
//! another commit.
//!
//! **A session's kablo only goes forward.** The warden of a release does not give
//! its session's place to the binary of an *older* release ([`forward_only`]): the session
//! stays as it is and the request says why. Two computers that share a server
//! and bring different releases would otherwise move its terminals back and
//! forth, each to its own, and a computer that is behind is one that is simply
//! asked again, with a release that is not behind, by whoever is ahead. A build
//! for development is exempt both ways.

use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{ErrorCode, Message, Packet, Status};

use crate::peer::{Gone, Peer};
use crate::{sockdir, sys};

/// How long one session has to be replaced and to answer afterwards.
pub const WAIT: Duration = Duration::from_secs(20);

/// How long the warden of a session whose relay was replaced has to show,
/// in the session's status, that it is the new build too.
pub const RENEWAL_WAIT: Duration = Duration::from_secs(3);

/// How long a relay has to answer the handshake of the request: a socket
/// that takes the connection and says nothing has nobody serving it.
const OPEN_WAIT: Duration = Duration::from_secs(3);

/// How long the binary named by a request has to say which release it is. It answers in a few
/// milliseconds; one that does not (a machine that is struggling) is not waited for longer, and
/// the rule is not applied to it.
const VERSION_WAIT: Duration = Duration::from_secs(3);

/// Whether the session of release `ours` may give its place to the binary `exe` (one the warden
/// has found fit to run, [`crate::warden`]): `Err` says why not. Only a binary that says it is a
/// **release older than** `ours` is refused; one that is a build for development, or one that
/// says nothing, is not.
pub fn forward_only(exe: &Path, ours: &str) -> Result<(), String> {
    let Some(theirs) = release_of(exe) else {
        return Ok(());
    };
    if older_release(&theirs, ours) {
        return Err(format!(
            "{theirs} is older than the {ours} this terminal runs; a terminal's kablo only goes forward"
        ));
    }
    Ok(())
}

/// The release the binary `exe` says it is (`kablo version`), when it says one in time.
fn release_of(exe: &Path) -> Option<String> {
    let mut child = Command::new(exe)
        .arg("version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let by = Instant::now() + VERSION_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(None) if Instant::now() < by => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut said = String::new();
    child.stdout.take()?.read_to_string(&mut said).ok()?;
    said.split_whitespace()
        .find_map(|word| word.strip_prefix("version="))
        .map(str::to_owned)
}

/// A release's numbers: `0.9.1` is `[0, 9, 1]`. `None` for a build for development
/// (`0.9.1-dev+3f2a9c1d0b44`) and for a name that is not one.
fn numbers(release: &str) -> Option<[u64; 3]> {
    let mut parts = release.split('.');
    let found = [
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    ];
    parts.next().is_none().then_some(found)
}

/// Whether `theirs` is a release older than the release `ours`. A build for development on either
/// side is not one.
fn older_release(theirs: &str, ours: &str) -> bool {
    matches!((numbers(theirs), numbers(ours)), (Some(theirs), Some(ours)) if theirs < ours)
}

/// Replaces the relay of the session `id` and says how it went. Returns the
/// exit status: 0 when it was replaced, 1 otherwise.
pub fn run(
    dir: &Path,
    id: &SessionId,
    binary: &Path,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> u8 {
    match replace(dir, id, binary) {
        Ok(now) => {
            let _ = writeln!(out, "{}", report(id, &now));
            0
        }
        Err(why) => {
            let _ = writeln!(err, "kablo: {id}: {why}");
            1
        }
    }
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
    let now = asked(&path, id, by)?;
    Ok(warden_followed(&path, id, now))
}

/// What is said of a session whose relay was replaced, given the status the
/// new relay gave.
fn report(id: &SessionId, now: &Status) -> String {
    let version = plain(&now.version);
    if now.warden_version == now.version {
        format!("{id}: replaced (now {version})")
    } else if now.warden_version.is_empty() {
        format!(
            "{id}: relay replaced (now {version}); its warden is of an earlier kind and stays until the session ends"
        )
    } else {
        format!(
            "{id}: relay replaced (now {version}); its warden stays at {} until the session ends",
            plain(&now.warden_version)
        )
    }
}

/// The status of `id` once its warden has had time to renew itself: `now`
/// when it already is the build the relay is, or never said what it is.
fn warden_followed(path: &Path, id: &SessionId, mut now: Status) -> Status {
    let by = Instant::now() + RENEWAL_WAIT;
    while !now.warden_version.is_empty() && now.warden_version != now.version && Instant::now() < by
    {
        std::thread::sleep(Duration::from_millis(50));
        match asked(path, id, by) {
            Ok(again) => now = again,
            Err(_) => break,
        }
    }
    now
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

    fn status_of(version: &str, warden: &str) -> Status {
        let mut status = Status::new(SessionId::new("work").unwrap());
        status.version = version.to_owned();
        status.warden_version = warden.to_owned();
        status
    }

    #[test]
    fn only_a_release_older_than_ours_is_refused() {
        // Older releases: by any of the three numbers, and as numbers (`0.10` is after `0.9`).
        assert!(older_release("0.9.0", "0.9.1"));
        assert!(older_release("0.8.9", "0.9.1"));
        assert!(older_release("0.9.9", "1.0.0"));
        assert!(!older_release("0.10.0", "0.9.1"));
        // The same and newer ones go.
        assert!(!older_release("0.9.1", "0.9.1"));
        assert!(!older_release("0.9.2", "0.9.1"));
        // A build for development is exempt, whichever side it is on, and so is a name that is not
        // a release.
        assert!(!older_release("0.8.0-dev+3f2a9c1d0b44", "0.9.1"));
        assert!(!older_release("0.8.0", "0.9.1-dev+3f2a9c1d0b44"));
        assert!(!older_release("0.9.1", "0.9.1-dev+3f2a9c1d0b44"));
        assert!(!older_release("old", "0.9.1"));
        assert!(!older_release("0.8", "0.9.1"));
        assert!(!older_release("0.8.0.1", "0.9.1"));
    }

    #[test]
    fn the_line_says_whether_the_warden_followed() {
        let id = SessionId::new("work").unwrap();
        assert_eq!(
            report(&id, &status_of("0.2.0", "0.2.0")),
            "work: replaced (now 0.2.0)"
        );
        let stays = report(&id, &status_of("0.2.0", "0.1.0"));
        assert!(
            stays.starts_with("work: relay replaced (now 0.2.0); "),
            "{stays}"
        );
        assert!(
            stays.contains("its warden stays at 0.1.0 until the session ends"),
            "{stays}"
        );
        let old = report(&id, &status_of("0.2.0", ""));
        assert!(
            old.starts_with("work: relay replaced (now 0.2.0); "),
            "{old}"
        );
        assert!(old.contains("earlier kind"), "{old}");
    }

    #[test]
    fn a_relays_words_lose_their_control_characters() {
        assert_eq!(plain("no\u{1b}[31m good\n"), "no[31m good");
    }
}
