//! Which session a connecting process comes from.
//!
//! The kernel knows it and the process cannot lie about it: the connecting
//! process is found by its pid (`sys::peer_pid`), and from there by the
//! terminal it is attached to, then by its parents. A session's shell is
//! started with `setsid` + `TIOCSCTTY`, so the controlling terminal of
//! everything that stays in the shell's session is the session's own pty, and
//! a process that left that session (a daemon, `setsid`) is still the shell's
//! descendant for as long as its parents live. A process that is neither is
//! from no terminal: cron, a service, an `ssh host kablo ...` command.
//!
//! Only running sessions take part. The shell pid and terminal device of a
//! session that ended can belong to any other process by now.

use crate::sys::{self, Lineage};
use kablo_proto::SessionId;
use kablo_proto::v1::{State, Status};

/// How many parents the search climbs. A shell's descendants are a few
/// levels deep; the cap keeps a cycle in a damaged process table from being
/// walked for ever.
const MAX_ANCESTORS: usize = 64;

/// Where a connecting process comes from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A process of the terminal of this session.
    Terminal(SessionId),
    /// A process of no session's terminal, or one that cannot be read.
    None,
}

/// One running session as the search sees it. The shell's pid and terminal
/// stay the same for the life of the session, so a caller can keep these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The session.
    pub id: SessionId,
    /// The shell's process id.
    pub shell: u32,
    /// The shell's controlling terminal: the session's pty. `None` when it
    /// could not be read; the search then goes by the parents only.
    pub terminal: Option<u64>,
}

impl Candidate {
    /// The candidate a session makes, if it is running: an ended or broken
    /// session is not one.
    pub fn of(status: &Status) -> Option<Candidate> {
        (status.state == State::RUNNING).then(|| Candidate {
            id: status.id.clone(),
            shell: status.pid,
            terminal: sys::lineage(status.pid).and_then(|shell| shell.terminal),
        })
    }
}

/// The session the process `peer` comes from.
pub fn find(peer: u32, candidates: &[Candidate]) -> Origin {
    find_with(peer, candidates, sys::lineage)
}

/// [`find`] with the process table as an argument.
fn find_with(
    peer: u32,
    candidates: &[Candidate],
    lineage: impl Fn(u32) -> Option<Lineage>,
) -> Origin {
    let found = |candidate: &Candidate| Origin::Terminal(candidate.id.clone());
    if let Some(device) = lineage(peer).and_then(|own| own.terminal)
        && let Some(candidate) = candidates.iter().find(|c| c.terminal == Some(device))
    {
        return found(candidate);
    }
    let mut pid = peer;
    for _ in 0..MAX_ANCESTORS {
        if let Some(candidate) = candidates.iter().find(|c| c.shell == pid) {
            return found(candidate);
        }
        // Process 1 is the top, and 0 is what a parent outside this pid
        // namespace reads as.
        if pid <= 1 {
            break;
        }
        match lineage(pid) {
            Some(own) => pid = own.parent,
            None => break,
        }
    }
    Origin::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty;
    use kablo_proto::v1::NewSession;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    fn id(name: &str) -> SessionId {
        SessionId::new(name).unwrap()
    }

    fn running(name: &str, shell: u32, terminal: Option<u64>) -> Candidate {
        Candidate {
            id: id(name),
            shell,
            terminal,
        }
    }

    /// A process table: `(pid, parent, terminal)`.
    fn table(rows: &[(u32, u32, Option<u64>)]) -> impl Fn(u32) -> Option<Lineage> {
        let rows: HashMap<u32, Lineage> = rows
            .iter()
            .map(|&(pid, parent, terminal)| (pid, Lineage { parent, terminal }))
            .collect();
        move |pid| rows.get(&pid).copied()
    }

    /// A session on a real pty whose shell has started a background command
    /// of its own, the way a program of a terminal does.
    struct LiveSession {
        shell: Child,
        /// The background command: the shell's child, on the shell's terminal.
        program: u32,
        /// Keeps the pty open.
        _pty: pty::Pty,
    }

    impl LiveSession {
        fn new() -> LiveSession {
            let pty = pty::open(pty::DEFAULT_SIZE).unwrap();
            let mut request = NewSession::default();
            request.argv = vec![
                b"/bin/sh".to_vec(),
                b"-c".to_vec(),
                b"sleep 60 & echo program=$!; wait".to_vec(),
            ];
            let shell = pty::spawn(&pty.slave, &request).unwrap();
            let program = read_program(&pty);
            LiveSession {
                shell,
                program,
                _pty: pty,
            }
        }

        fn candidate(&self, name: &str) -> Candidate {
            let terminal = sys::lineage(self.shell.id()).unwrap().terminal;
            assert!(terminal.is_some(), "the shell has a controlling terminal");
            running(name, self.shell.id(), terminal)
        }
    }

    impl Drop for LiveSession {
        fn drop(&mut self) {
            sys::kill(self.program as libc::pid_t, libc::SIGKILL).unwrap();
            self.shell.kill().unwrap();
            self.shell.wait().unwrap();
        }
    }

    /// Reads `program=<pid>` off the master side of `pty`, which is not
    /// blocking.
    fn read_program(pty: &pty::Pty) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        loop {
            let mut buf = [0u8; 256];
            // SAFETY: `buf` is a writable buffer of the length passed.
            let got =
                unsafe { libc::read(pty.master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if let Ok(got) = usize::try_from(got) {
                seen.extend_from_slice(&buf[..got]);
            }
            let text = String::from_utf8_lossy(&seen);
            if let Some(rest) = text.split("program=").nth(1)
                && let Some(digits) = rest.split(['\r', '\n']).next()
                && rest.len() > digits.len()
            {
                return digits.parse().unwrap();
            }
            assert!(Instant::now() < deadline, "the shell said nothing: {text}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn a_process_on_the_terminal_of_a_session_comes_from_it() {
        let terminal = LiveSession::new();
        let session = terminal.candidate("work");
        let others = [running("other", 1_000_000, Some(1))];
        let all = [others[0].clone(), session.clone()];
        assert_eq!(find(terminal.program, &all), Origin::Terminal(id("work")));
        // By the terminal alone: the shell's pid is not the one it names.
        let by_device = [running("work", 1_000_000, session.terminal)];
        assert_eq!(
            find(terminal.program, &by_device),
            Origin::Terminal(id("work"))
        );
    }

    #[test]
    fn a_process_that_left_the_terminal_comes_from_the_session_of_its_parents() {
        // The test process stands for the session's shell. Below it is a
        // shell that made a session of its own and has no terminal, and the
        // peer is the command that shell started: a grandchild, off the
        // terminal, found by its parents.
        let mut detached = Command::new("/bin/sh");
        detached
            .args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(Stdio::piped());
        // SAFETY: `setsid` is async-signal-safe.
        unsafe {
            detached.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut middle = detached.spawn().unwrap();
        let mut line = String::new();
        BufReader::new(middle.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let peer: u32 = line.trim().parse().unwrap();

        assert_eq!(sys::lineage(peer).unwrap().terminal, None);
        assert_eq!(sys::lineage(peer).unwrap().parent, middle.id());
        let shell = running("work", std::process::id(), None);
        let found = find(peer, &[shell]);
        sys::kill(peer as libc::pid_t, libc::SIGKILL).unwrap();
        middle.kill().unwrap();
        middle.wait().unwrap();
        assert_eq!(found, Origin::Terminal(id("work")));
    }

    #[test]
    fn a_stranger_and_init_come_from_no_terminal() {
        let terminal = LiveSession::new();
        let session = [terminal.candidate("work")];
        let mut stranger = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        // A child of this process is not a descendant of the session's shell.
        assert_eq!(find(stranger.id(), &session), Origin::None);
        stranger.kill().unwrap();
        stranger.wait().unwrap();
        assert_eq!(find(1, &session), Origin::None);
        assert_eq!(find(0, &session), Origin::None);
        // No process has this id.
        assert_eq!(find(u32::MAX - 1, &session), Origin::None);
    }

    #[test]
    fn an_ended_session_is_no_candidate_and_its_pid_and_terminal_match_nothing() {
        let terminal = LiveSession::new();
        let mut status = Status::new(id("work"));
        status.pid = terminal.shell.id();
        assert_eq!(
            Candidate::of(&status),
            Some(terminal.candidate("work")),
            "the same session, running, is found"
        );
        for state in [State::ENDED, State::BROKEN] {
            status.state = state;
            assert_eq!(Candidate::of(&status), None);
        }
        // What is left to search is nothing, and a process that really is on
        // that terminal finds no session.
        let candidates: Vec<Candidate> = [&status].into_iter().filter_map(Candidate::of).collect();
        assert_eq!(find(terminal.program, &candidates), Origin::None);
    }

    #[test]
    fn the_terminal_wins_over_the_parents_and_none_means_the_parents_decide() {
        // 10 is a shell on terminal 7; 20 is on terminal 8 and a child of 11.
        let process = table(&[(10, 1, Some(7)), (11, 1, Some(8)), (20, 11, Some(8))]);
        let both = [running("a", 10, Some(7)), running("b", 11, Some(8))];
        assert_eq!(find_with(20, &both, &process), Origin::Terminal(id("b")));
        // No terminal known for either: the parent chain finds b.
        let blind = [running("a", 10, None), running("b", 11, None)];
        assert_eq!(find_with(20, &blind, &process), Origin::Terminal(id("b")));
        // The shell itself is of its session.
        assert_eq!(find_with(10, &blind, &process), Origin::Terminal(id("a")));
    }

    #[test]
    fn the_climb_stops_at_a_loop_a_gap_and_the_cap() {
        let shell = [running("a", 5, None)];
        // 1 -> 2 -> 1: a damaged table does not hang the search.
        let cycle = table(&[(1, 2, None), (2, 1, None)]);
        assert_eq!(find_with(2, &shell, &cycle), Origin::None);
        // 30 -> 20 -> (20 cannot be read).
        let gap = table(&[(30, 20, None)]);
        assert_eq!(find_with(30, &shell, &gap), Origin::None);
        // The search looks at MAX_ANCESTORS processes on the way up: starting
        // at 2, the last one it looks at is `1 + MAX_ANCESTORS`.
        let last = u32::try_from(MAX_ANCESTORS).unwrap();
        let rows: Vec<(u32, u32, Option<u64>)> =
            (2..=last + 10).map(|pid| (pid, pid + 1, None)).collect();
        let chain = table(&rows);
        let within = [running("a", 1 + last, None)];
        assert_eq!(find_with(2, &within, &chain), Origin::Terminal(id("a")));
        let beyond = [running("a", 2 + last, None)];
        assert_eq!(find_with(2, &beyond, &chain), Origin::None);
    }
}
