//! Starting a session: what `kablo new` does.
//!
//! The command makes the socket directory if it is not there, starts a warden
//! (this same binary, `kablo warden`) with a socket as its standard input,
//! sends it the request over that socket and waits for one line back: `ok`
//! when the first relay is set up and the session answers on its socket, or
//! `error: …` with the reason. The warden is not waited for and not kept as a
//! child: it detaches itself into a session of its own and outlives this
//! process.
//!
//! **Surviving the end of a login.** A warden detached by `setsid` is out of
//! the terminal's session and process group, which is all most machines ask.
//! Where systemd kills a user's processes when their last login ends, the
//! warden is started through `systemd-run --user --scope` instead, which puts
//! it in a unit of its own that the login's end does not take with it. That
//! holds only while the user manager stays up, and the manager stops shortly
//! after the last login unless the user *lingers*
//! (`loginctl enable-linger`, which is a file under
//! `/var/lib/systemd/linger`): without it the manager takes its units, the
//! scope with them, and the sessions die at logout. So the scope is used only
//! for a user who lingers; for anyone else a plain `setsid` is the better bet,
//! since it lives in the login's own scope, which survives wherever
//! `KillUserProcesses` is `no` (the usual default). A `systemd-run` that does
//! not start anything (no bus, an old version) is not an error: the warden is
//! started again, without it.
//!
//! **A program that was asked for.** A request that says so
//! ([`NewSession::program`]: everything the `bateri` command starts, the bootstrap
//! of a server's shell included, and the programs the app starts) is looked at
//! before anything starts: the program must be there and be executable, or the
//! request is refused with what the warden would have said, and no terminal is
//! made for it. Only that is looked at: a program that is there and then fails
//! to run (a script with no interpreter, a binary for another processor) is
//! started all the same. Then bateri's `start`, if the machine
//! has one (`~/.local/share/bateri/shell/start`), is what starts it, and without
//! one a program that is to stay open is followed by the login shell by a line of
//! `sh`. This is here, in the one function that every start passes, and not in
//! the warden, which does what its request says and nothing else. A request that
//! does not say it is a program (the app's plain shell pane, its ssh keeper,
//! `kablo new`) is started as it is.

use std::ffi::OsStr;
use std::io::{self, BufRead, BufReader, Read};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use kablo_bateri::paths;
use kablo_proto::SessionId;
use kablo_proto::v1::{Message, NewSession, Packet, ProgramEnd};

use crate::{sockdir, sys};

/// The longest reply line read from the warden.
const REPLY_LIMIT: u64 = 4096;

/// How long a warden may take to say whether the session is up.
const REPLY_WAIT: Duration = Duration::from_secs(10);

/// The environment variable that names the `systemd-run` to use, or, empty,
/// forbids using one. Without it the machine is looked at.
pub const ENV_SYSTEMD_RUN: &str = "KABLO_SYSTEMD_RUN";

/// The environment variable that names the directory in which systemd keeps
/// its marks of users who linger. Without it, systemd's own.
pub const ENV_LINGER_DIR: &str = "KABLO_LINGER_DIR";

/// Where systemd keeps the marks of users who linger.
const LINGER_DIR: &str = "/var/lib/systemd/linger";

/// Why a session did not start.
#[derive(Debug, PartialEq, Eq)]
pub enum Failure {
    /// A session with that id is already there.
    Exists(SessionId),
    /// Anything else, in words for a person.
    Other(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Exists(id) => f.write_str(&exists_text(id)),
            Failure::Other(why) => f.write_str(why),
        }
    }
}

/// What the warden says when the id is taken. One text, for both ends.
pub fn exists_text(id: &SessionId) -> String {
    format!("a session called {id} exists already")
}

/// A name for a session whose maker did not choose one: eight hex digits
/// from the system's randomness.
pub fn fresh_id() -> io::Result<SessionId> {
    let mut bytes = [0u8; 4];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    SessionId::new(&name).map_err(io::Error::other)
}

/// How one try at starting the warden went.
enum Try {
    /// The session is up.
    Ready,
    /// The warden said why it will not start.
    Refused(Failure),
    /// Nothing was said and the process ended: it did not get to run.
    Silent(String),
}

/// Starts session `id` in socket directory `dir`. `Ok` means the session is
/// up and its socket answers, with the label the request asked for.
pub fn launch(dir: &Path, id: &SessionId, request: &NewSession) -> Result<(), Failure> {
    launch_without(dir, id, request, &[])
}

/// [`launch`] for a session whose program does not get the variables `unset` names, whatever this
/// process has: the keeper is started without them and the program inherits the keeper's
/// environment. (`--env` can set a variable but not take one away.)
pub fn launch_without(
    dir: &Path,
    id: &SessionId,
    request: &NewSession,
    unset: &[&str],
) -> Result<(), Failure> {
    let request = &as_asked(request, start_file)?;
    start(dir, id, request, unset)?;
    // The session is new, which is what only this knows: the file of a label
    // that an earlier session of the same name left behind (a relay that was
    // killed does not remove its own) is not this one's, and what the request
    // asks is. It cannot be done sooner, since a session with this name that is
    // there refuses to start and its label is its own; the moment between the
    // relay answering and this is the one in which that file can be read as this
    // session's. A label that cannot be written does not undo a session that is
    // up: it is seen in its `status`, which says none.
    let label = request.label.as_deref().unwrap_or_default();
    let _ = sockdir::write_label(dir, id, label);
    Ok(())
}

/// The error the warden gives when the program of a session cannot be started, whatever the
/// reason: the words of `warden`'s own, so that a request refused here reads as one it refused.
const CANNOT_START: &str = "cannot start the program: ";

/// Where a program is looked for when neither the request nor this process has a `PATH`.
const DEFAULT_PATH: &[u8] = b"/usr/bin:/bin";

/// What `sh` runs to follow a program with the login shell when there is no `start` to do it.
const FOLLOW_ON: &str = "\"$@\"; exec \"${SHELL:-/bin/sh}\" -l";

/// bateri's `start`, when this machine has one that can be read.
fn start_file() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    if !home.is_absolute() {
        return None;
    }
    let file = home.join(paths::DIR).join(paths::SHELL).join(paths::START);
    sys::readable(&file).then_some(file)
}

/// The request a keeper is given for `request`: itself when it does not say it is for a program
/// somebody asked for; else, after finding that the program is there and may be executed (not
/// that it then runs), the request that has it started by `start` (`sh start KEEP PROGRAM...`,
/// `KEEP` being `1` when the terminal carries on with the login shell after it), or, with no
/// `start`, by the line of `sh` that follows a program that stays open with the login shell, or
/// the program alone. `start` is looked for only once the program is found.
fn as_asked(
    request: &NewSession,
    start: impl FnOnce() -> Option<PathBuf>,
) -> Result<NewSession, Failure> {
    let (Some(end), Some(program)) = (request.program, request.argv.first()) else {
        return Ok(request.clone());
    };
    find_program(request, program).map_err(|e| Failure::Other(format!("{CANNOT_START}{e}")))?;
    let keep = matches!(end, ProgramEnd::StaysOpen);
    let mut asked = request.clone();
    asked.program = None;
    let mut argv: Vec<Vec<u8>> = match start() {
        Some(start) => vec![
            b"sh".to_vec(),
            start.as_os_str().as_bytes().to_vec(),
            if keep { b"1".to_vec() } else { b"0".to_vec() },
        ],
        None if keep => vec![
            b"sh".to_vec(),
            b"-c".to_vec(),
            FOLLOW_ON.as_bytes().to_vec(),
            b"sh".to_vec(),
        ],
        None => return Ok(asked),
    };
    argv.extend(request.argv.iter().cloned());
    asked.argv = argv;
    Ok(asked)
}

/// Whether `program` can be run for `request`, as `exec` would find it: a word with a `/` is a
/// path, from the directory the request starts in when it is not absolute; any other is looked for
/// in the `:`-separated directories of the `PATH` the program is given (the request's, else this
/// process's), an empty one being the directory it starts in. The error is the one `exec` gives;
/// that the file may be executed is all that is known of it, not that it runs.
fn find_program(request: &NewSession, program: &[u8]) -> io::Result<()> {
    let not_there = || io::Error::from_raw_os_error(libc::ENOENT);
    if program.is_empty() {
        return Err(not_there());
    }
    let cwd = request
        .cwd
        .as_deref()
        .map(|cwd| Path::new(OsStr::from_bytes(cwd)));
    let from_cwd = |path: PathBuf| match cwd {
        Some(cwd) if path.is_relative() => cwd.join(path),
        _ => path,
    };
    if program.contains(&b'/') {
        return sys::executable(&from_cwd(PathBuf::from(OsStr::from_bytes(program))));
    }
    let path = request
        .env
        .iter()
        .rev()
        .find(|(name, _)| name == b"PATH")
        .map(|(_, value)| value.clone())
        .or_else(|| std::env::var_os("PATH").map(|path| path.as_bytes().to_vec()))
        .unwrap_or_else(|| DEFAULT_PATH.to_vec());
    let mut denied = false;
    let mut last = not_there();
    for dir in path.split(|byte| *byte == b':') {
        let candidate = Path::new(OsStr::from_bytes(dir)).join(OsStr::from_bytes(program));
        match sys::executable(&from_cwd(candidate)) {
            Ok(()) => return Ok(()),
            Err(e) => match e.raw_os_error() {
                Some(libc::EACCES) => denied = true,
                Some(
                    libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::ENODEV | libc::ETIMEDOUT,
                ) => last = e,
                _ => return Err(e),
            },
        }
    }
    Err(if denied {
        io::Error::from_raw_os_error(libc::EACCES)
    } else {
        last
    })
}

/// Starts the warden of session `id`; the label is not looked at.
fn start(dir: &Path, id: &SessionId, request: &NewSession, unset: &[&str]) -> Result<(), Failure> {
    sockdir::ensure_dir(dir).map_err(|e| Failure::Other(e.to_string()))?;
    let exe =
        std::env::current_exe().map_err(|e| Failure::Other(format!("cannot find myself: {e}")))?;
    if let Some(runner) = scope_runner() {
        match attempt(&exe, Some(&runner), dir, id, request, unset) {
            Try::Ready => return Ok(()),
            Try::Refused(why) => return Err(why),
            // `systemd-run` did not start the warden: do without it.
            Try::Silent(_) => {}
        }
    }
    match attempt(&exe, None, dir, id, request, unset) {
        Try::Ready => Ok(()),
        Try::Refused(why) => Err(why),
        Try::Silent(why) => Err(Failure::Other(why)),
    }
}

/// The command that starts the warden, under `runner` (a `systemd-run`) if
/// there is one.
fn command(
    exe: &Path,
    runner: Option<&Path>,
    dir: &Path,
    id: &SessionId,
    unset: &[&str],
) -> Command {
    let mut command = match runner {
        Some(runner) => {
            let mut command = Command::new(runner);
            command
                .args(["--user", "--scope", "--quiet", "--collect"])
                .arg(format!("--description=kablo session {id}"))
                .arg("--")
                .arg(exe);
            command
        }
        None => Command::new(exe),
    };
    command
        .arg("warden")
        .arg("--id")
        .arg(id.as_str())
        .arg("--dir")
        .arg(dir);
    for name in unset {
        command.env_remove(name);
    }
    command
}

fn attempt(
    exe: &Path,
    runner: Option<&Path>,
    dir: &Path,
    id: &SessionId,
    request: &NewSession,
    unset: &[&str],
) -> Try {
    let (ours, theirs) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(e) => return Try::Refused(Failure::Other(e.to_string())),
    };
    let mut child = match command(exe, runner, dir, id, unset)
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) if runner.is_some() => return Try::Silent(e.to_string()),
        Err(e) => {
            return Try::Refused(Failure::Other(format!(
                "cannot start the session keeper: {e}"
            )));
        }
    };
    let packet = Packet::about(id.clone(), Message::New(request.clone()));
    let sent = kablo_proto::v1::write_packet(&mut &ours, &packet);
    if let Err(e) = sent {
        // Either the process is gone already (a `systemd-run` that refused to
        // start anything is quick enough to be) or it is waiting for a
        // request that will not come.
        end(&mut child);
        let why = format!("the request cannot be sent: {e}");
        return if runner.is_some() {
            Try::Silent(why)
        } else {
            Try::Refused(Failure::Other(why))
        };
    }
    let _ = ours.shutdown(Shutdown::Write);
    let _ = ours.set_read_timeout(Some(REPLY_WAIT));
    let mut line = String::new();
    let read = BufReader::new((&ours).take(REPLY_LIMIT)).read_line(&mut line);
    if let Err(e) = read
        && matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    {
        end(&mut child);
        let why = "the session keeper did not answer in time".to_owned();
        // A `systemd-run` that is stuck (a user manager that does not
        // answer) is not a verdict on the keeper: try without it.
        return if runner.is_some() {
            Try::Silent(why)
        } else {
            Try::Refused(Failure::Other(why))
        };
    }
    let line = line.trim_end();
    if line == "ok" {
        // The warden is on its own now, but its parent still has to wait for
        // it: the bridge starts sessions for as long as its connection lives,
        // and a keeper nobody waits for stays a zombie after it ends.
        reap(child);
        return Try::Ready;
    }
    let status = child.wait();
    match line.strip_prefix("error: ") {
        Some(why) if why == exists_text(id) => Try::Refused(Failure::Exists(id.clone())),
        Some(why) => Try::Refused(Failure::Other(why.to_owned())),
        None => Try::Silent(match status {
            Ok(status) => format!("the session keeper ended before it was ready ({status})"),
            Err(e) => format!("the session keeper ended before it was ready ({e})"),
        }),
    }
}

/// Waits for `child` on a thread of its own. A thread that cannot be made
/// leaves the child as it was before this: a zombie until this process ends.
fn reap(mut child: Child) {
    let _ = std::thread::Builder::new()
        .name("reap".to_owned())
        .spawn(move || {
            let _ = child.wait();
        });
}

fn end(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// The `systemd-run` to start wardens under, if this machine looks like it has
/// a user manager for it to ask **and** that manager outlives the user's last
/// login (they linger). [`ENV_SYSTEMD_RUN`] overrides the looking for the
/// program, not the lingering; [`ENV_LINGER_DIR`] moves the lingering's marks.
pub fn scope_runner() -> Option<PathBuf> {
    let runner = match std::env::var_os(ENV_SYSTEMD_RUN) {
        Some(path) if path.is_empty() => return None,
        Some(path) => PathBuf::from(path),
        None => detect_runner()?,
    };
    let dir = match std::env::var_os(ENV_LINGER_DIR) {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(LINGER_DIR),
    };
    let user = sys::passwd()?.name;
    lingers(&dir, &user).then_some(runner)
}

/// Whether systemd has the user called `name` lingering: its mark is a file of
/// that name in `dir`.
fn lingers(dir: &Path, name: &[u8]) -> bool {
    use std::os::unix::ffi::OsStrExt;
    // A name that is a path would look somewhere else.
    if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
        return false;
    }
    dir.join(std::ffi::OsStr::from_bytes(name)).exists()
}

/// The machine's `systemd-run`, if it has a systemd with a user manager.
fn detect_runner() -> Option<PathBuf> {
    if !cfg!(target_os = "linux") || !Path::new("/run/systemd/system").is_dir() {
        return None;
    }
    let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?);
    if !runtime.join("systemd").is_dir() {
        return None;
    }
    find_in_path("systemd-run", std::env::var_os("PATH")?.as_os_str())
        .or_else(|| Some(PathBuf::from("/usr/bin/systemd-run")).filter(|p| p.is_file()))
}

/// The first executable called `name` in the `:`-separated directories.
fn find_in_path(name: &str, path: &std::ffi::OsStr) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    std::env::split_paths(path)
        .map(|dir| dir.join(name))
        .find(|candidate| {
            candidate
                .metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warden_runs_under_systemd_run_only_when_there_is_one() {
        let id = SessionId::new("work").unwrap();
        let plain = command(Path::new("/bin/kablo"), None, Path::new("/tmp/d"), &id, &[]);
        let args: Vec<_> = plain.get_args().collect();
        assert_eq!(plain.get_program(), "/bin/kablo");
        assert_eq!(args, ["warden", "--id", "work", "--dir", "/tmp/d"]);

        let scoped = command(
            Path::new("/bin/kablo"),
            Some(Path::new("/usr/bin/systemd-run")),
            Path::new("/tmp/d"),
            &id,
            &[],
        );
        assert_eq!(scoped.get_program(), "/usr/bin/systemd-run");
        let args: Vec<_> = scoped.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--description=kablo session work",
                "--",
                "/bin/kablo",
                "warden",
                "--id",
                "work",
                "--dir",
                "/tmp/d"
            ]
        );
    }

    #[test]
    fn a_program_is_found_on_a_path_only_if_it_can_run() {
        use std::os::unix::fs::PermissionsExt;
        let dir = PathBuf::from(format!("/tmp/kt-{}-find", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        let tool = dir.join("tool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = std::env::join_paths(["/nonexistent", dir.to_str().unwrap()]).unwrap();
        assert_eq!(find_in_path("tool", &path), None);
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in_path("tool", &path), Some(tool));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_user_lingers_when_systemd_has_a_mark_with_their_name() {
        let dir = PathBuf::from(format!("/tmp/kt-{}-linger", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("ada"), "").unwrap();
        assert!(lingers(&dir, b"ada"));
        assert!(!lingers(&dir, b"grace"), "somebody else's mark is not ours");
        assert!(!lingers(&dir, b""));
        assert!(
            !lingers(&dir, b".."),
            "a name is not a way out of the directory"
        );
        assert!(!lingers(&dir, b"../kt/ada"));
        assert!(
            !lingers(&dir.join("not-there"), b"ada"),
            "no directory, no lingering"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn asking(words: &[&str], end: Option<ProgramEnd>) -> NewSession {
        let mut request = NewSession::default();
        request.argv = words.iter().map(|w| w.as_bytes().to_vec()).collect();
        request.program = end;
        request
    }

    fn words(request: &NewSession) -> Vec<String> {
        request
            .argv
            .iter()
            .map(|w| String::from_utf8_lossy(w).into_owned())
            .collect()
    }

    #[test]
    fn a_program_that_was_asked_for_is_started_by_start_when_there_is_one() {
        let start = Path::new("/home/u/.local/share/bateri/shell/start");
        for (end, keep) in [(ProgramEnd::Closes, "0"), (ProgramEnd::StaysOpen, "1")] {
            let asked = as_asked(&asking(&["/bin/sh", "-c", "x"], Some(end)), || {
                Some(start.to_path_buf())
            })
            .unwrap();
            let start = start.display().to_string();
            assert_eq!(words(&asked), ["sh", &start, keep, "/bin/sh", "-c", "x"]);
            assert_eq!(asked.program, None, "the keeper is not told");
        }
    }

    #[test]
    fn without_a_start_only_a_program_that_stays_open_is_followed_by_the_login_shell() {
        let asked = as_asked(
            &asking(&["/bin/sh", "-c", "x"], Some(ProgramEnd::StaysOpen)),
            || None,
        )
        .unwrap();
        assert_eq!(
            words(&asked),
            [
                "sh",
                "-c",
                "\"$@\"; exec \"${SHELL:-/bin/sh}\" -l",
                "sh",
                "/bin/sh",
                "-c",
                "x"
            ]
        );
        let closes = as_asked(&asking(&["/bin/sh"], Some(ProgramEnd::Closes)), || None).unwrap();
        assert_eq!(words(&closes), ["/bin/sh"]);
    }

    #[test]
    fn a_request_that_does_not_say_it_is_a_program_is_started_as_it_is() {
        // Where `start` is is not even looked at for one.
        let never = || -> Option<PathBuf> { panic!("`start` was looked for") };
        let plain = asking(&["no-such-program-here"], None);
        assert_eq!(as_asked(&plain, never).unwrap(), plain);
        // A program with no words is a shell, whatever else is said.
        let shell = asking(&[], Some(ProgramEnd::StaysOpen));
        assert_eq!(as_asked(&shell, never).unwrap(), shell);
    }

    #[test]
    fn a_program_that_is_not_there_is_refused_before_start_is_looked_for() {
        let never = || -> Option<PathBuf> { panic!("`start` was looked for") };
        for end in [ProgramEnd::Closes, ProgramEnd::StaysOpen] {
            let why = as_asked(&asking(&["no-such-program-here"], Some(end)), never)
                .unwrap_err()
                .to_string();
            assert_eq!(
                why,
                "cannot start the program: No such file or directory (os error 2)"
            );
        }
    }

    #[test]
    fn a_program_is_looked_for_as_exec_looks_for_it() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kt-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (first, second) = (dir.join("first"), dir.join("second"));
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        // A file that cannot run, ahead of one that can, with the same name.
        std::fs::write(first.join("tool"), "#!/bin/sh\n").unwrap();
        std::fs::write(second.join("tool"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(second.join("tool"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::write(dir.join("plain"), "").unwrap();
        let mut request = NewSession::default();
        request.cwd = Some(dir.as_os_str().as_bytes().to_vec());
        let on = |path: &str| {
            let mut request = request.clone();
            request.env = vec![(b"PATH".to_vec(), path.as_bytes().to_vec())];
            request
        };
        let both = format!("{}:{}", first.display(), second.display());
        let errno = |result: io::Result<()>| result.map_err(|e| e.raw_os_error());
        assert_eq!(errno(find_program(&on(&both), b"tool")), Ok(()));
        // The one that cannot run is no program, and is what is said when nothing else is.
        let only = first.display().to_string();
        assert_eq!(
            errno(find_program(&on(&only), b"tool")),
            Err(Some(libc::EACCES))
        );
        assert_eq!(
            errno(find_program(&on(&only), b"nothing")),
            Err(Some(libc::ENOENT))
        );
        // An empty directory of the path is the one the program starts in, a relative one is
        // below it, and the last `PATH` of the request is the one that counts.
        assert_eq!(errno(find_program(&on("second"), b"tool")), Ok(()));
        assert_eq!(
            errno(find_program(&on(":second"), b"plain")),
            Err(Some(libc::EACCES))
        );
        let mut twice = on("/nowhere");
        twice.env.push((b"PATH".to_vec(), both.into_bytes()));
        assert_eq!(errno(find_program(&twice, b"tool")), Ok(()));
        // A word with a slash is a path, from the directory the program starts in.
        assert_eq!(errno(find_program(&request, b"second/tool")), Ok(()));
        assert_eq!(
            errno(find_program(&request, b"second/")),
            Err(Some(libc::EACCES))
        );
        assert_eq!(
            errno(find_program(&request, b"second/tool/inside")),
            Err(Some(libc::ENOTDIR))
        );
        assert_eq!(errno(find_program(&request, b"")), Err(Some(libc::ENOENT)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_id_that_is_taken_is_told_apart() {
        let id = SessionId::new("x").unwrap();
        assert_eq!(Failure::Exists(id.clone()).to_string(), exists_text(&id));
    }
}
