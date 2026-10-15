//! The terminal and the shell that runs in it.
//!
//! The warden opens a pseudo-terminal pair, starts the program on the slave
//! side as the leader of a new session with the slave as its controlling
//! terminal, and closes its own copy of the slave so that the master sees the
//! end of the program's output. The master stays non-blocking and
//! close-on-exec in every process that holds it.

use std::ffi::{CStr, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command, ExitStatus, Stdio};

use kablo_proto::v1::{Exit, NewSession, Size};

use crate::sys;

/// The size of a terminal nobody asked a size for.
pub const DEFAULT_SIZE: Size = Size {
    cols: 80,
    rows: 24,
    xpixel: 0,
    ypixel: 0,
};

/// Both sides of a new terminal.
#[derive(Debug)]
pub struct Pty {
    /// The side the keeper reads and writes.
    pub master: OwnedFd,
    /// The side the program gets.
    pub slave: OwnedFd,
}

/// Opens a terminal of `size`.
pub fn open(size: Size) -> io::Result<Pty> {
    // SAFETY: no pointers; the result is checked.
    let raw = sys::cvt(unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) })?;
    // SAFETY: a descriptor just returned to us, owned from here.
    let master = unsafe { OwnedFd::from_raw_fd(raw) };
    // Explicitly: `posix_openpt` honours `O_CLOEXEC` on Linux only.
    sys::set_cloexec(master.as_raw_fd())?;
    sys::set_nonblocking(master.as_raw_fd())?;
    // SAFETY: `master` is an open pseudo-terminal master.
    sys::cvt(unsafe { libc::grantpt(master.as_raw_fd()) })?;
    // SAFETY: as above.
    sys::cvt(unsafe { libc::unlockpt(master.as_raw_fd()) })?;
    let name = slave_name(&master)?;
    // SAFETY: `name` is a C string for the call.
    let raw = sys::cvt(unsafe {
        libc::open(
            name.as_ptr(),
            libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: a descriptor just returned to us, owned from here.
    let slave = unsafe { OwnedFd::from_raw_fd(raw) };
    use std::os::fd::AsFd;
    sys::set_window_size(master.as_fd(), size)?;
    utf8_input(&slave);
    Ok(Pty { master, slave })
}

/// The path of the slave side of `master`.
#[cfg(target_os = "linux")]
fn slave_name(master: &OwnedFd) -> io::Result<std::ffi::CString> {
    let mut buf = [0 as libc::c_char; 128];
    // SAFETY: `buf` is a writable buffer of the length passed.
    let status = unsafe { libc::ptsname_r(master.as_raw_fd(), buf.as_mut_ptr(), buf.len()) };
    if status != 0 {
        return Err(io::Error::from_raw_os_error(status));
    }
    // SAFETY: `ptsname_r` wrote a NUL-terminated string into `buf`.
    Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }.to_owned())
}

/// The path of the slave side of `master`.
#[cfg(not(target_os = "linux"))]
fn slave_name(master: &OwnedFd) -> io::Result<std::ffi::CString> {
    // SAFETY: `ptsname` returns a pointer into static storage (or null); it
    // is copied at once, and the keeper is a single thread.
    let name = unsafe { libc::ptsname(master.as_raw_fd()) };
    if name.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a non-null result is a NUL-terminated string.
    Ok(unsafe { CStr::from_ptr(name) }.to_owned())
}

/// Tells the line discipline the input is UTF-8, so that erasing a character
/// erases a whole one. A failure leaves the default, which is only less
/// polite.
fn utf8_input(slave: &OwnedFd) {
    // SAFETY: an all-zero `termios` is valid; `tcgetattr` fills it.
    let mut attrs: libc::termios = unsafe { std::mem::zeroed() };
    // SAFETY: `slave` is open and `attrs` a valid out-pointer.
    if unsafe { libc::tcgetattr(slave.as_raw_fd(), &raw mut attrs) } != 0 {
        return;
    }
    attrs.c_iflag |= libc::IUTF8;
    // SAFETY: `slave` is open and `attrs` a valid input.
    unsafe { libc::tcsetattr(slave.as_raw_fd(), libc::TCSANOW, &raw const attrs) };
}

/// Starts the program of `request` on the terminal whose slave side is
/// `slave`: the user's login shell when the request names no program.
///
/// The environment is the keeper's own, with `TERM` and `COLORTERM` set to
/// what a modern terminal has (the request may override them), then the
/// request's variables. The program never sees a descriptor the keeper holds:
/// all of them are close-on-exec.
pub fn spawn(slave: &OwnedFd, request: &NewSession) -> io::Result<Child> {
    let mut command = match request.argv.split_first() {
        Some((program, args)) => {
            let mut command = Command::new(OsStr::from_bytes(program));
            command.args(args.iter().map(|a| OsStr::from_bytes(a)));
            command
        }
        None => {
            let shell = login_shell(&request.env);
            let mut command = Command::new(OsStr::from_bytes(&shell));
            // A login shell is told so by a leading dash in its name.
            let name = shell.rsplit(|&b| b == b'/').next().unwrap_or(&shell);
            let mut dashed = vec![b'-'];
            dashed.extend_from_slice(name);
            command.arg0(OsStr::from_bytes(&dashed));
            command
        }
    };
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    for (name, value) in &request.env {
        command.env(OsStr::from_bytes(name), OsStr::from_bytes(value));
    }
    if let Some(cwd) = &request.cwd {
        command.current_dir(OsStr::from_bytes(cwd));
        // A shell takes `PWD` only if it names the directory it is in.
        if cwd.first() == Some(&b'/') {
            command.env("PWD", OsStr::from_bytes(cwd));
        }
    }
    command
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave.try_clone()?));
    // SAFETY: the closure runs between `fork` and `exec` and only calls
    // async-signal-safe functions (`signal`, `setsid`, `ioctl`).
    unsafe {
        command.pre_exec(|| {
            // Signals the starter of the keeper left ignored (`nohup kablo
            // new`, a background job of a script) stay ignored through every
            // `exec`: a shell that shrugs off a hang-up cannot be terminated.
            for signal in INHERITED_SIGNALS {
                libc::signal(signal, libc::SIG_DFL);
            }
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            // Standard input is the slave by now: make it the controlling
            // terminal of the new session.
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

/// The signals a shell or a command starts with the default action for,
/// whatever the keeper inherited.
pub const INHERITED_SIGNALS: [libc::c_int; 10] = [
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTERM,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGALRM,
    libc::SIGCHLD,
    libc::SIGPIPE,
];

/// The shell to run when the request names no program: its own `SHELL`, the
/// keeper's, the user's passwd entry, `/bin/sh`.
fn login_shell(request_env: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let named =
        |value: &[u8]| (!value.is_empty() && value.first() == Some(&b'/')).then(|| value.to_vec());
    if let Some((_, value)) = request_env.iter().rev().find(|(name, _)| name == b"SHELL") {
        if let Some(shell) = named(value) {
            return shell;
        }
    }
    if let Some(value) = std::env::var_os("SHELL") {
        if let Some(shell) = named(value.as_bytes()) {
            return shell;
        }
    }
    passwd_shell()
        .and_then(|shell| named(&shell))
        .unwrap_or_else(|| b"/bin/sh".to_vec())
}

/// The shell in the user's passwd entry.
fn passwd_shell() -> Option<Vec<u8>> {
    sys::passwd().map(|entry| entry.shell)
}

/// How a program ended, as the protocol says it.
pub fn exit_of(status: ExitStatus) -> Exit {
    match (status.code(), status.signal()) {
        (Some(code), _) => Exit::Code(code),
        (None, Some(signal)) => Exit::Signal(signal),
        (None, None) => Exit::Code(-1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn read_until_end(master: &OwnedFd) -> Vec<u8> {
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let mut set = [sys::want(master.as_raw_fd(), libc::POLLIN)];
            sys::poll(&mut set, Some(std::time::Duration::from_secs(10))).unwrap();
            // SAFETY: the buffer is valid for its length.
            let n = unsafe { libc::read(master.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                out.extend_from_slice(buf.get(..n as usize).unwrap());
            } else if n == 0 || io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock {
                return out;
            }
        }
    }

    #[test]
    fn a_program_runs_with_the_terminal_as_its_own() {
        let pty = open(Size {
            cols: 100,
            rows: 30,
            xpixel: 0,
            ypixel: 0,
        })
        .unwrap();
        let request = NewSession {
            argv: vec![
                b"/bin/sh".to_vec(),
                b"-c".to_vec(),
                b"tty >/dev/null && stty size; echo $TERM $COLORTERM $X; exit 7".to_vec(),
            ],
            env: vec![(b"X".to_vec(), b"y".to_vec())],
            ..NewSession::default()
        };
        let mut child = spawn(&pty.slave, &request).unwrap();
        let Pty { master, slave } = pty;
        drop(slave);
        let output = String::from_utf8(read_until_end(&master)).unwrap();
        assert!(output.contains("30 100"), "{output:?}");
        assert!(output.contains("xterm-256color truecolor y"), "{output:?}");
        assert_eq!(exit_of(child.wait().unwrap()), Exit::Code(7));
        assert_eq!(sys::window_size(master.as_fd()).unwrap().cols, 100);
    }

    #[test]
    fn a_signal_is_not_an_exit_code() {
        let pty = open(DEFAULT_SIZE).unwrap();
        let request = NewSession {
            argv: vec![b"/bin/sh".to_vec(), b"-c".to_vec(), b"kill -9 $$".to_vec()],
            ..NewSession::default()
        };
        let mut child = spawn(&pty.slave, &request).unwrap();
        assert_eq!(exit_of(child.wait().unwrap()), Exit::Signal(libc::SIGKILL));
    }

    #[test]
    fn the_request_may_override_the_defaults_and_pick_the_directory() {
        let pty = open(DEFAULT_SIZE).unwrap();
        let request = NewSession {
            argv: vec![
                b"/bin/sh".to_vec(),
                b"-c".to_vec(),
                b"echo $TERM; pwd".to_vec(),
            ],
            env: vec![(b"TERM".to_vec(), b"dumb".to_vec())],
            cwd: Some(b"/".to_vec()),
            ..NewSession::default()
        };
        let mut child = spawn(&pty.slave, &request).unwrap();
        let Pty { master, slave } = pty;
        drop(slave);
        let output = String::from_utf8(read_until_end(&master)).unwrap();
        assert_eq!(output, "dumb\r\n/\r\n");
        child.wait().unwrap();
    }

    #[test]
    fn a_program_that_is_not_there_is_an_error_not_a_session() {
        let pty = open(DEFAULT_SIZE).unwrap();
        let request = NewSession {
            argv: vec![b"/nonexistent/program".to_vec()],
            ..NewSession::default()
        };
        let err = spawn(&pty.slave, &request).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_login_shell_is_found() {
        let shell = login_shell(&[]);
        assert!(shell.starts_with(b"/"));
        let chosen = login_shell(&[(b"SHELL".to_vec(), b"/bin/sh".to_vec())]);
        assert_eq!(chosen, b"/bin/sh");
        // Not a path: ignored.
        let ignored = login_shell(&[(b"SHELL".to_vec(), b"zsh".to_vec())]);
        assert!(ignored.starts_with(b"/"));
    }
}
