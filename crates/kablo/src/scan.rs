//! Asking every session in the socket directory who it is.
//!
//! A session's socket file says a session was started, not that it is still
//! there: a keeper that was killed leaves its file behind, and a keeper whose
//! relay cannot be started still holds the socket while nothing answers on
//! it. A scan tells the three apart by trying. Connecting to a file nobody
//! listens on is refused outright, so it is not a session and is left out. A
//! connection that is taken but never answered is the third case, and it is
//! the keeper's way of saying *broken*: the keeper is too small to speak the
//! protocol, so a session whose relay is gone for good shows as
//! [`State::BROKEN`], decided by the silence lasting past the deadline.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kablo_proto::SessionId;
use kablo_proto::v1::{Message, Packet, State, Status};

use crate::peer::Peer;
use crate::{sockdir, sys};

/// How long a session may take to answer before it is called broken. A relay
/// being started or replaced answers well within it.
pub const WAIT: Duration = Duration::from_millis(1500);

/// How many times a connection that drops before it has answered is made
/// again before the session is called broken (a relay replaced mid-question
/// is not).
const TRIES: u8 = 3;

/// One session being asked.
#[derive(Debug)]
struct Probe {
    id: SessionId,
    peer: Option<Peer>,
    tries: u8,
    finished: bool,
}

/// Every session in a directory, asked at once.
#[derive(Debug)]
pub struct Scan {
    dir: PathBuf,
    probes: Vec<Probe>,
    found: Vec<Status>,
    deadline: Instant,
}

impl Scan {
    /// Starts asking each session in `dir`. A directory that does not exist
    /// has no sessions; one that is not safe to use is an error.
    pub fn start(dir: &Path, wait: Duration) -> io::Result<Scan> {
        let ids = match sockdir::check_dir(dir) {
            Ok(()) => sockdir::session_ids(dir)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Scan::of(dir, ids, wait))
    }

    /// Starts asking the sessions `ids`, whether or not they exist.
    pub fn of(dir: &Path, ids: Vec<SessionId>, wait: Duration) -> Scan {
        let mut scan = Scan {
            dir: dir.to_owned(),
            probes: ids
                .into_iter()
                .map(|id| Probe {
                    id,
                    peer: None,
                    tries: 0,
                    finished: false,
                })
                .collect(),
            found: Vec::new(),
            deadline: Instant::now() + wait,
        };
        for index in 0..scan.probes.len() {
            scan.dial(index);
        }
        scan
    }

    /// Connects probe `index` and sends its question.
    fn dial(&mut self, index: usize) {
        let Some(probe) = self.probes.get_mut(index) else {
            return;
        };
        let path = sockdir::socket_path(&self.dir, &probe.id);
        match Peer::connect(&path) {
            Ok(mut peer) => {
                peer.queue(&Packet::about(probe.id.clone(), Message::Status));
                if peer.flush().is_err() {
                    return self.lost(index);
                }
                probe.peer = Some(peer);
            }
            // The listener's queue is full of connections nobody takes.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => self.broken(index),
            // Nobody listens: a file a dead session left, or a session that
            // ended while we looked. Not a session.
            Err(_) => self.skip(index),
        }
    }

    fn skip(&mut self, index: usize) {
        if let Some(probe) = self.probes.get_mut(index) {
            probe.finished = true;
            probe.peer = None;
        }
    }

    fn broken(&mut self, index: usize) {
        if let Some(probe) = self.probes.get_mut(index) {
            let mut status = Status::new(probe.id.clone());
            status.state = State::BROKEN;
            self.found.push(status);
        }
        self.skip(index);
    }

    /// The connection of probe `index` ended without an answer.
    fn lost(&mut self, index: usize) {
        let Some(probe) = self.probes.get_mut(index) else {
            return;
        };
        probe.peer = None;
        probe.tries += 1;
        if probe.tries < TRIES {
            self.dial(index);
        } else {
            self.broken(index);
        }
    }

    /// The descriptors to wait on: `(probe index, pollfd)`.
    pub fn interest(&self) -> Vec<(usize, libc::pollfd)> {
        self.probes
            .iter()
            .enumerate()
            .filter_map(|(index, probe)| {
                let peer = probe.peer.as_ref()?;
                Some((index, sys::want(peer.fd(), peer.interest(true))))
            })
            .collect()
    }

    /// Probe `index`'s descriptor reported `revents`.
    pub fn ready(&mut self, index: usize, revents: libc::c_short) {
        let Some(probe) = self.probes.get_mut(index) else {
            return;
        };
        let Some(peer) = probe.peer.as_mut() else {
            return;
        };
        let mut lost = false;
        if revents & libc::POLLOUT != 0 && peer.flush().is_err() {
            lost = true;
        }
        let mut answer = None;
        if !lost && sys::is_ready(revents, libc::POLLIN) {
            let mut got = Vec::new();
            lost = peer.read(&mut got).is_err();
            for packet in got {
                match packet.message {
                    Message::StatusReply(status) if status.id == probe.id => {
                        answer = Some(status);
                        break;
                    }
                    _ => {}
                }
            }
        }
        if let Some(status) = answer {
            self.found.push(status);
            self.skip(index);
        } else if lost {
            self.lost(index);
        }
    }

    /// Calls every session that has not answered by now broken, once the
    /// deadline has passed.
    pub fn expire(&mut self, now: Instant) {
        if now < self.deadline {
            return;
        }
        for index in 0..self.probes.len() {
            if self.probes.get(index).is_some_and(|probe| !probe.finished) {
                self.broken(index);
            }
        }
    }

    /// When the scan gives up on the sessions that have not answered; `None`
    /// once every one has.
    pub fn deadline(&self) -> Option<Instant> {
        (!self.is_finished()).then_some(self.deadline)
    }

    /// Whether every session has been settled.
    pub fn is_finished(&self) -> bool {
        self.probes.iter().all(|probe| probe.finished)
    }

    /// What was found, by session id.
    pub fn into_statuses(mut self) -> Vec<Status> {
        self.found.sort_by(|a, b| a.id.cmp(&b.id));
        self.found
    }

    /// Waits for the whole scan. For a caller with nothing else to do.
    pub fn run(mut self) -> Vec<Status> {
        while !self.is_finished() {
            let interest = self.interest();
            let mut set: Vec<libc::pollfd> = interest.iter().map(|(_, fd)| *fd).collect();
            let wait = self
                .deadline()
                .map(|at| at.saturating_duration_since(Instant::now()));
            if sys::poll(&mut set, wait).is_err() {
                break;
            }
            for ((index, _), fd) in interest.iter().zip(&set) {
                if fd.revents != 0 {
                    self.ready(*index, fd.revents);
                }
            }
            self.expire(Instant::now());
        }
        self.into_statuses()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/kt-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        sockdir::ensure_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_directory_has_no_sessions() {
        let dir = PathBuf::from(format!("/tmp/kt-{}-scan-none", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(Scan::start(&dir, WAIT).unwrap().run().is_empty());
    }

    #[test]
    fn a_dead_socket_is_left_out_and_a_mute_one_is_broken() {
        let dir = scratch("scan");
        // The file a killed keeper leaves: bound once, nobody listening.
        let stale = sockdir::socket_path(&dir, &SessionId::new("stale").unwrap());
        drop(UnixListener::bind(&stale).unwrap());
        assert!(stale.exists());
        // A keeper without a relay: it holds the socket and never accepts.
        let mute = sockdir::socket_path(&dir, &SessionId::new("mute").unwrap());
        let _held = UnixListener::bind(&mute).unwrap();

        let start = Instant::now();
        let found = Scan::start(&dir, Duration::from_millis(300)).unwrap().run();
        assert!(start.elapsed() >= Duration::from_millis(300));
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].id.as_str(), "mute");
        assert_eq!(found[0].state, State::BROKEN);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
