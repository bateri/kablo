//! A program run for a client: what a command stream is the other end of.
//!
//! A command starts the way a login does when nobody is at a terminal. The
//! program gets a session and a process group of its own, so a signal sent to
//! the group reaches it and everything it started, and so it has no controlling
//! terminal, whatever the bridge has. It gets three pipes for its standard
//! input, output and error, the environment of the bridge with what the client
//! asked for added, and the user's home as its directory unless the client
//! named another (one that is not absolute counts from home). The argument
//! vector goes to `exec` as it is: no shell is asked to read a line made of it.
//!
//! The pipes this end holds are not blocking and are for the owner's `poll`.
//! The program's end is learned from a descriptor too ([`sys::exit_watch`]), so
//! nothing waits for it and nothing asks again and again.
//!
//! A program is collected, once, by whoever holds it: [`Program::reap`] does it
//! when the end has been seen, and dropping a program that has not ended kills
//! its group and waits for the leader, so that nothing is left behind by a path
//! that forgot (a test that fails, a bridge that panics).

use std::ffi::OsStr;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use kablo_proto::v1::Exit;
use kablo_proto::v1::stream::Command;

use crate::pty::{INHERITED_SIGNALS, exit_of};
use crate::sys;

/// How long a program that has been told to end (`SIGTERM` to its group) has
/// before the group is killed. The wait stops when the group is gone: a program
/// that does as it is told costs no more than the time it takes.
pub const COMMAND_KILL_WAIT: Duration = Duration::from_secs(2);

/// The ends of a program's standard streams that this end holds.
pub struct Pipes {
    /// Written to, for the program's input.
    pub stdin: OwnedFd,
    /// Read from: what the program wrote.
    pub stdout: OwnedFd,
    /// Read from: what the program wrote as errors.
    pub stderr: OwnedFd,
}

/// A running program: its process, and the descriptor that says it ended.
pub struct Program {
    proc: sys::Proc,
    /// The program's process group: its own number, as it leads a session.
    group: libc::pid_t,
    /// Becomes readable when the program ends; gone once it has been collected,
    /// as it stays readable after.
    watch: Option<OwnedFd>,
    exit: Option<Exit>,
}

/// Starts `command`. The caller has checked it ([`Command::check`]); an error
/// is the system's, as `exec` says it ("No such file or directory").
pub fn start(command: &Command) -> io::Result<(Program, Pipes)> {
    let (program, args) = command
        .argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no program"))?;
    let mut spawn = std::process::Command::new(OsStr::from_bytes(program));
    spawn.args(args.iter().map(|a| OsStr::from_bytes(a)));
    if let Some(dir) = start_dir(command.cwd.as_deref()) {
        // A directory that was asked for and is not there is said as that: the
        // system's answer to the `exec` that follows would name the program.
        if command.cwd.is_some() {
            check_dir(&dir)?;
        }
        spawn.current_dir(&dir);
        // A shell takes `PWD` only if it names the directory it is in.
        spawn.env("PWD", &dir);
    }
    for (name, value) in command.vars() {
        spawn.env(OsStr::from_bytes(name), OsStr::from_bytes(value));
    }
    spawn
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: the closure runs between `fork` and `exec` and only calls
    // async-signal-safe functions (`signal`, `setsid`).
    unsafe {
        spawn.pre_exec(|| {
            // Signals the starter of the bridge left ignored (`nohup kablo
            // attach`) stay ignored through every `exec`: a program that shrugs
            // off a hang-up could not be told to end.
            for signal in INHERITED_SIGNALS {
                libc::signal(signal, libc::SIG_DFL);
            }
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = spawn.spawn()?;
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(io::Error::other("no pipes to the program"));
    };
    let pipes = Pipes {
        stdin: stdin.into(),
        stdout: stdout.into(),
        stderr: stderr.into(),
    };
    let proc = sys::Proc::new(child)?;
    let group = libc::pid_t::try_from(proc.id()).map_err(io::Error::other)?;
    // From here an early return drops the program, which ends it.
    let mut program = Program {
        proc,
        group,
        watch: None,
        exit: None,
    };
    for pipe in [&pipes.stdin, &pipes.stdout, &pipes.stderr] {
        sys::set_nonblocking(pipe.as_raw_fd())?;
    }
    match sys::exit_watch(program.proc.id()) {
        Ok(watch) => program.watch = Some(watch),
        // A program that ended before it could be watched is not an error.
        Err(e) => match program.proc.try_wait() {
            Ok(Some(status)) => program.exit = Some(exit_of(status)),
            _ => return Err(e),
        },
    }
    Ok((program, pipes))
}

/// Whether the directory a command asked to start in can be entered as far as
/// can be told before trying; the error says which directory.
fn check_dir(dir: &Path) -> io::Result<()> {
    let found = std::fs::metadata(dir).and_then(|meta| {
        if meta.is_dir() {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::ENOTDIR))
        }
    });
    found.map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("cannot enter {}: {}", dir.display(), system_words(&e)),
        )
    })
}

/// Where a command starts: the directory it asked for, which counts from home
/// when it is not absolute, or home. A home that is not there (an account made
/// without one) is not a reason for a command to fail, as it is not for a
/// login: the command starts where the bridge is.
fn start_dir(asked: Option<&[u8]>) -> Option<PathBuf> {
    let home = || {
        let from_passwd = sys::passwd()
            .map(|entry| entry.home)
            .filter(|home| !home.is_empty())
            .map(|home| PathBuf::from(OsStr::from_bytes(&home)));
        from_passwd
            .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
            .filter(|home| home.is_absolute())
    };
    match asked {
        Some(asked) => {
            let asked = Path::new(OsStr::from_bytes(asked));
            if asked.is_absolute() {
                Some(asked.to_path_buf())
            } else {
                Some(home().map_or_else(|| asked.to_path_buf(), |home| home.join(asked)))
            }
        }
        None => home().filter(|home| home.is_dir()),
    }
}

/// The system's words for an error: what `strerror` says, without the number
/// `std` appends ("No such file or directory (os error 2)").
pub fn system_words(error: &io::Error) -> String {
    let text = error.to_string();
    match text.rfind(" (os error ") {
        Some(at) => text.get(..at).unwrap_or(&text).to_owned(),
        None => text,
    }
}

impl Program {
    /// The program's process number: its group's too, and what a stream's
    /// program is told apart by.
    pub fn id(&self) -> u32 {
        self.proc.id()
    }

    /// How the program ended, once it has been seen to ([`Program::reap`]).
    pub fn exit(&self) -> Option<Exit> {
        self.exit
    }

    /// The descriptor that becomes readable when the program ends; none once it
    /// has been collected.
    pub fn watch_fd(&self) -> Option<RawFd> {
        self.watch.as_ref().map(AsRawFd::as_raw_fd)
    }

    /// Collects the program if it has ended, and says how. A wait that fails
    /// (somebody else collected it) counts as an end with no status, or the
    /// watch would stay readable for ever.
    pub fn reap(&mut self) -> Option<Exit> {
        if self.exit.is_none() {
            match self.proc.try_wait() {
                Ok(None) => {}
                Ok(Some(status)) => self.ended(exit_of(status)),
                Err(_) => self.ended(Exit::Code(-1)),
            }
        }
        self.exit
    }

    fn ended(&mut self, exit: Exit) {
        self.exit = Some(exit);
        self.watch = None;
    }

    /// Tells the program and everything it started to end.
    ///
    /// A group is signalled after its leader has been collected too: what the
    /// program started outlives it when it holds the pipes. The number could be
    /// someone else's by then only if every process of the group is gone and the
    /// system has gone round all its numbers since, which does not happen in the
    /// seconds a program is given to end.
    pub fn terminate(&self) {
        let _ = sys::kill_group(self.group, libc::SIGTERM);
    }

    /// Kills the program and everything it started.
    pub fn kill(&self) {
        let _ = sys::kill_group(self.group, libc::SIGKILL);
    }

    /// Whether anything of the program's group is still there.
    pub fn group_exists(&self) -> bool {
        sys::group_exists(self.group)
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        if self.exit.is_none() {
            self.kill();
            let _ = self.proc.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsFd, BorrowedFd};
    use std::time::Instant;

    use super::*;

    const PATIENCE: Duration = Duration::from_secs(30);

    fn command(argv: &[&str]) -> Command {
        Command {
            argv: argv.iter().map(|a| a.as_bytes().to_vec()).collect(),
            ..Command::default()
        }
    }

    /// Reads a pipe until it ends.
    fn read_all(pipe: BorrowedFd<'_>) -> Vec<u8> {
        let start = Instant::now();
        let mut all = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match sys::read_fd(pipe, &mut buf) {
                Ok(0) => return all,
                Ok(n) => all.extend_from_slice(buf.get(..n).unwrap()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let mut set = [sys::want(pipe.as_raw_fd(), libc::POLLIN)];
                    sys::poll(&mut set, Some(Duration::from_millis(20))).unwrap();
                }
                Err(e) => panic!("{e}"),
            }
            assert!(start.elapsed() < PATIENCE, "the pipe never ended");
        }
    }

    /// Waits for the program's end.
    fn finished(program: &mut Program) -> Exit {
        let start = Instant::now();
        loop {
            if let Some(exit) = program.reap() {
                return exit;
            }
            let mut set = [sys::want(program.watch_fd().unwrap(), libc::POLLIN)];
            sys::poll(&mut set, Some(Duration::from_millis(20))).unwrap();
            assert!(start.elapsed() < PATIENCE, "the program never ended");
        }
    }

    #[test]
    fn a_program_has_three_pipes_and_an_end() {
        let (mut program, pipes) = start(&command(&[
            "/bin/sh",
            "-c",
            "cat; echo out; echo err >&2; exit 3",
        ]))
        .unwrap();
        sys::write_fd(pipes.stdin.as_fd(), b"in\n").unwrap();
        drop(pipes.stdin);
        assert_eq!(read_all(pipes.stdout.as_fd()), b"in\nout\n");
        assert_eq!(read_all(pipes.stderr.as_fd()), b"err\n");
        assert_eq!(finished(&mut program), Exit::Code(3));
        assert!(
            program.watch_fd().is_none(),
            "a collected program is not watched"
        );
    }

    #[test]
    fn the_arguments_reach_the_program_as_they_are() {
        // Nothing in them is a shell's to read.
        let (mut program, pipes) = start(&command(&["/bin/echo", "$HOME;", "'a b'", "*"])).unwrap();
        assert_eq!(read_all(pipes.stdout.as_fd()), b"$HOME; 'a b' *\n");
        assert_eq!(finished(&mut program), Exit::Code(0));
    }

    #[test]
    fn the_environment_and_the_directory_are_the_clients() {
        let dir = std::env::temp_dir().canonicalize().unwrap();
        let mut asked = command(&["/bin/sh", "-c", "echo $KABLO_TEST_VAR; pwd -P"]);
        asked.env = vec![b"KABLO_TEST_VAR=a=b".to_vec()];
        asked.cwd = Some(dir.as_os_str().as_bytes().to_vec());
        let (mut program, pipes) = start(&asked).unwrap();
        let said = read_all(pipes.stdout.as_fd());
        assert_eq!(
            String::from_utf8(said).unwrap(),
            format!("a=b\n{}\n", dir.display())
        );
        assert_eq!(finished(&mut program), Exit::Code(0));
    }

    #[test]
    fn a_program_that_cannot_start_says_why_in_the_systems_words() {
        let error = start(&command(&["/no/such/program"])).err().unwrap();
        assert_eq!(system_words(&error), "No such file or directory");
        let mut asked = command(&["/bin/true"]);
        asked.cwd = Some(b"/no/such/directory".to_vec());
        let error = start(&asked).err().unwrap();
        // The directory is named: the system's answer to the `exec` that
        // follows would have named the program.
        assert_eq!(
            system_words(&error),
            "cannot enter /no/such/directory: No such file or directory"
        );
        // A file is not a directory either.
        asked.cwd = Some(b"/bin/sh".to_vec());
        let error = start(&asked).err().unwrap();
        assert_eq!(
            system_words(&error),
            "cannot enter /bin/sh: Not a directory"
        );
    }

    #[test]
    fn a_program_leads_a_group_of_its_own() {
        // A signal to the group must reach the program and what it starts, and
        // nothing else: the group is the program's own number.
        let script = "echo $$; ps -o pgid= -p $$";
        let (mut program, pipes) = start(&command(&["/bin/sh", "-c", script])).unwrap();
        let said = String::from_utf8(read_all(pipes.stdout.as_fd())).unwrap();
        let mut lines = said.lines().map(str::trim);
        let pid = lines.next().unwrap();
        assert_eq!(lines.next(), Some(pid), "its own group: {said}");
        assert_eq!(finished(&mut program), Exit::Code(0));
    }

    #[test]
    fn terminating_ends_the_program_and_what_it_started() {
        let (mut program, pipes) = start(&command(&[
            "/bin/sh",
            "-c",
            "sleep 60 & echo started; wait",
        ]))
        .unwrap();
        // Standard output is the pipe: the child holds it until it ends.
        let mut set = [sys::want(pipes.stdout.as_raw_fd(), libc::POLLIN)];
        sys::poll(&mut set, Some(PATIENCE)).unwrap();
        assert!(program.group_exists());
        program.terminate();
        assert_eq!(finished(&mut program), Exit::Signal(libc::SIGTERM));
        // The group goes with the leader, and with it the holders of the pipe.
        let start = Instant::now();
        while program.group_exists() {
            assert!(start.elapsed() < PATIENCE, "the group never went");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(read_all(pipes.stdout.as_fd()), b"started\n");
    }

    #[test]
    fn a_program_that_is_dropped_is_killed_and_collected() {
        let (program, _pipes) = start(&command(&["/bin/sleep", "60"])).unwrap();
        let pid = libc::pid_t::try_from(program.id()).unwrap();
        drop(program);
        // Collected: not even a zombie answers.
        // SAFETY: signal 0 only checks.
        let found = unsafe { libc::kill(pid, 0) };
        assert_eq!(found, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
}
