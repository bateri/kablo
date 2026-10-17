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

use std::io::{self, BufRead, BufReader, Read};
use std::net::Shutdown;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use kablo_proto::SessionId;
use kablo_proto::v1::{Message, NewSession, Packet};

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
    start(dir, id, request)?;
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

/// Starts the warden of session `id`; the label is not looked at.
fn start(dir: &Path, id: &SessionId, request: &NewSession) -> Result<(), Failure> {
    sockdir::ensure_dir(dir).map_err(|e| Failure::Other(e.to_string()))?;
    let exe =
        std::env::current_exe().map_err(|e| Failure::Other(format!("cannot find myself: {e}")))?;
    if let Some(runner) = scope_runner() {
        match attempt(&exe, Some(&runner), dir, id, request) {
            Try::Ready => return Ok(()),
            Try::Refused(why) => return Err(why),
            // `systemd-run` did not start the warden: do without it.
            Try::Silent(_) => {}
        }
    }
    match attempt(&exe, None, dir, id, request) {
        Try::Ready => Ok(()),
        Try::Refused(why) => Err(why),
        Try::Silent(why) => Err(Failure::Other(why)),
    }
}

/// The command that starts the warden, under `runner` (a `systemd-run`) if
/// there is one.
fn command(exe: &Path, runner: Option<&Path>, dir: &Path, id: &SessionId) -> Command {
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
    command
}

fn attempt(
    exe: &Path,
    runner: Option<&Path>,
    dir: &Path,
    id: &SessionId,
    request: &NewSession,
) -> Try {
    let (ours, theirs) = match UnixStream::pair() {
        Ok(pair) => pair,
        Err(e) => return Try::Refused(Failure::Other(e.to_string())),
    };
    let mut child = match command(exe, runner, dir, id)
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
        let plain = command(Path::new("/bin/kablo"), None, Path::new("/tmp/d"), &id);
        let args: Vec<_> = plain.get_args().collect();
        assert_eq!(plain.get_program(), "/bin/kablo");
        assert_eq!(args, ["warden", "--id", "work", "--dir", "/tmp/d"]);

        let scoped = command(
            Path::new("/bin/kablo"),
            Some(Path::new("/usr/bin/systemd-run")),
            Path::new("/tmp/d"),
            &id,
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

    #[test]
    fn the_id_that_is_taken_is_told_apart() {
        let id = SessionId::new("x").unwrap();
        assert_eq!(Failure::Exists(id.clone()).to_string(), exists_text(&id));
    }
}
