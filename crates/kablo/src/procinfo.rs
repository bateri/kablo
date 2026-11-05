//! What is in the foreground of a session's terminal, for `status` and for
//! `state`: the name of the program and the directory it is in, and whether the
//! terminal is being asked for a secret.
//!
//! The foreground process group of the terminal is the thing the user is
//! looking at (the shell at its prompt, or the editor the shell started), and
//! its leader's name and working directory say what that is. Linux reads
//! `/proc`, macOS asks libproc.
//!
//! **The one thing read of a command line is its first word, and only
//! for a program the kernel names with a version number** (see
//! [`shown_name`]). A release kept in a directory of versions and started
//! through a link of the name the user knows (`claude`, which is the file
//! `2.1.296`) is called by the kernel after the file, and the name the user
//! knows is the one they started it by: the last part of `argv[0]`. Nothing
//! else of the line is looked at, and the environment still is not.

use std::os::fd::BorrowedFd;

use kablo_proto::v1::TerminalState;

use crate::sys;

/// The longest name a program is shown by when it is called after its first
/// argument and not what the kernel calls it: a file name is at most 255
/// bytes, and something longer is not the name of a program.
const MAX_LAUNCH_NAME: usize = 255;

/// The most of a command line read to find its first word on Linux. The word
/// is cut at its NUL and the rest of what was read is dropped on the spot; a
/// first word that fills the buffer is longer than any name and is not one.
#[cfg(target_os = "linux")]
const ARG0_LIMIT: usize = 1024;

/// What the terminal looks like now, looked at from outside: the same answer
/// for every question that is put to it. See [`Watch`] for the one that asks
/// again and again.
pub fn look(master: BorrowedFd<'_>, shell_pid: u32) -> TerminalState {
    Watch::default().look(master, shell_pid)
}

/// A look at the terminal that remembers the name it worked out, so that a
/// program that stays in the foreground while it streams output has its
/// command line read once, not on every look.
#[derive(Default)]
pub struct Watch {
    called: Option<Called>,
}

/// The name a program is shown by when the kernel names it with a version: it
/// stays in front while it streams output (a coding agent) and is looked at
/// after every burst of it, so its first word is read once.
struct Called {
    pid: u32,
    kernel: Vec<u8>,
    shown: Vec<u8>,
}

impl Watch {
    /// The state of the terminal `master` belongs to; the shell `shell_pid`
    /// stands for the foreground when the terminal names no group.
    pub fn look(&mut self, master: BorrowedFd<'_>, shell_pid: u32) -> TerminalState {
        self.look_with_pid(master, shell_pid).0
    }

    /// What [`Watch::look`] says, and the process id of the foreground it is
    /// about: the leader of the foreground group, or `shell_pid` when the
    /// terminal names none or the leader is gone. One decision for both, so
    /// that the name and the process it names cannot come apart.
    pub fn look_with_pid(
        &mut self,
        master: BorrowedFd<'_>,
        shell_pid: u32,
    ) -> (TerminalState, u32) {
        let group = sys::foreground_group(master).and_then(|group| u32::try_from(group).ok());
        let mut pid = group.unwrap_or(shell_pid);
        let mut kernel = name_of(pid);
        // The leader of the group is gone and the shell has not yet taken the
        // terminal back (a program that was interrupted): the shell is what
        // the person will find, and a state with nothing in it would only
        // flash.
        if kernel.is_none() && pid != shell_pid {
            pid = shell_pid;
            kernel = name_of(pid);
        }
        let mut state = TerminalState::default();
        state.cwd = cwd_of(pid);
        state.secret = sys::hides_typing(master);
        state.program = kernel.map(|kernel| self.called(pid, kernel));
        (state, pid)
    }

    /// The name process `pid` is shown by, given what the kernel calls it: the
    /// same unless that is a version number (see [`shown_name`]). The first
    /// word of its command line is read only then, once for the same process
    /// under the same name, and again at the next look when it could not be
    /// read.
    fn called(&mut self, pid: u32, kernel: Vec<u8>) -> Vec<u8> {
        if !is_version(&kernel) {
            self.called = None;
            return kernel;
        }
        if let Some(called) = self
            .called
            .as_ref()
            .filter(|called| called.pid == pid && called.kernel == kernel)
        {
            return called.shown.clone();
        }
        let Some(first) = arg0_of(pid) else {
            return kernel;
        };
        let shown = shown_name(&kernel, Some(&first));
        self.called = Some(Called {
            pid,
            kernel,
            shown: shown.clone(),
        });
        shown
    }
}

/// Whether the kernel's name for a program is a version number: digits and
/// dots, with a digit among them (`2.1.296`). A release kept in a directory of
/// versions is a file with such a name.
fn is_version(name: &[u8]) -> bool {
    name.iter().any(u8::is_ascii_digit) && name.iter().all(|&b| b.is_ascii_digit() || b == b'.')
}

/// The name a person is shown for the program the kernel calls `kernel`. That
/// name, unless it is a version number, and then the name the program was
/// started by, the last part of its first argument (`claude` for the file
/// `2.1.296` started through a link `~/.local/bin/claude`): what the person
/// knows it as. When the first argument is not known, has no last part, is
/// longer than a file name, or is not one word (it holds a space or a control
/// character: a process that sets its own title writes it over its arguments,
/// and what stands there then is a command line, not a name), the kernel's name
/// stands.
fn shown_name(kernel: &[u8], arg0: Option<&[u8]>) -> Vec<u8> {
    let name = arg0
        .filter(|_| is_version(kernel))
        .and_then(|arg0| arg0.rsplit(|&b| b == b'/').next())
        .filter(|name| {
            !name.is_empty()
                && name.len() <= MAX_LAUNCH_NAME
                && name.iter().all(|&b| b > b' ' && b != 0x7f)
                // Text is judged by its characters, so the controls of the
                // second block (U+0080 to U+009F) are refused with the rest;
                // what is not text is bytes, as the kernel's own name may be.
                && std::str::from_utf8(name)
                    .map_or(true, |text| !text.chars().any(|c| c.is_control() || c.is_whitespace()))
        });
    name.unwrap_or(kernel).to_vec()
}

/// The first word of the command line of `pid`, and nothing after it: one read
/// of at most [`ARG0_LIMIT`] bytes, cut at the first NUL, and whatever the read
/// brought of the next arguments is dropped with the buffer.
#[cfg(target_os = "linux")]
fn arg0_of(pid: u32) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut buf = [0u8; ARG0_LIMIT];
    let len = std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .read(&mut buf)
        .ok()?;
    let word = buf.get(..len)?.split(|&b| b == 0).next()?;
    (!word.is_empty() && word.len() < ARG0_LIMIT).then(|| word.to_vec())
}

#[cfg(target_os = "linux")]
fn name_of(pid: u32) -> Option<Vec<u8>> {
    let mut name = std::fs::read(format!("/proc/{pid}/comm")).ok()?;
    while name.last() == Some(&b'\n') {
        name.pop();
    }
    (!name.is_empty()).then_some(name)
}

#[cfg(target_os = "linux")]
fn cwd_of(pid: u32) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStringExt;
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.into_os_string().into_vec())
}

/// The first argument of a process of the same user, and nothing after it: the
/// block the kernel gives holds the arguments and the environment, and only the
/// first word is copied out of it before the block is dropped.
#[cfg(target_os = "macos")]
fn arg0_of(pid: u32) -> Option<Vec<u8>> {
    parse_arg0(&procargs(pid)?).map(<[u8]>::to_vec)
}

/// The block `sysctl(KERN_PROCARGS2)` gives for a process: `argc`, the path of
/// the executable, the arguments **and the environment**. Whoever asks for it
/// walks only the part it needs.
#[cfg(target_os = "macos")]
fn procargs(pid: u32) -> Option<Vec<u8>> {
    let pid = libc::c_int::try_from(pid).ok()?;
    // The buffer is `kern.argmax` long: a query with an empty buffer gives
    // this ceiling, not the real size. It does not change while the system
    // runs, so it is asked once.
    static ARGMAX: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let capacity = (*ARGMAX.get_or_init(|| {
        let mut argmax: libc::c_int = 0;
        let mut size = size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: `mib` has two elements and belongs to the frame; the output
        // is a single `c_int` and its size goes to the call as it is.
        let status = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&raw mut argmax).cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        usize::try_from(argmax).ok().filter(|_| status == 0)
    }))?;
    let mut buf = vec![0u8; capacity];
    let mut len = capacity;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: the buffer is `capacity` bytes and belongs to this frame; the
    // kernel writes at most `len` bytes and stores what it wrote in `len`.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &raw mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    if len > buf.len() {
        return None;
    }
    buf.truncate(len);
    Some(buf)
}

/// The layout of a [`procargs`] block up to the arguments: `argc` (4 bytes),
/// the executable's path and NUL padding, then `argc` arguments each ended by
/// a NUL, and the environment after them. Gives `argc` and the block from the
/// first argument on.
#[cfg(any(target_os = "macos", test))]
fn procargs_head(buf: &[u8]) -> Option<(usize, &[u8])> {
    let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
    let rest = buf.get(4..)?;
    // The path, then padding: argv[0] cannot be empty, so the first byte that
    // is not NUL after the path is where the arguments start.
    let path_end = rest.iter().position(|&b| b == 0)?;
    let start = path_end + rest.get(path_end..)?.iter().position(|&b| b != 0)?;
    Some((argc, rest.get(start..)?))
}

/// The pure half of [`arg0_of`] on macOS: the first argument and nothing after
/// it. The walk stops at its NUL, so the rest of the arguments and the
/// environment are not looked at.
#[cfg(any(target_os = "macos", test))]
fn parse_arg0(buf: &[u8]) -> Option<&[u8]> {
    let (argc, args) = procargs_head(buf)?;
    let word = args.split(|&b| b == 0).next()?;
    (argc > 0 && !word.is_empty()).then_some(word)
}

#[cfg(target_os = "macos")]
fn name_of(pid: u32) -> Option<Vec<u8>> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a writable buffer of the length passed.
    let len = unsafe {
        libc::proc_name(
            libc::c_int::try_from(pid).ok()?,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
        )
    };
    let len = usize::try_from(len).ok().filter(|&len| len > 0)?;
    buf.get(..len).map(<[u8]>::to_vec)
}

#[cfg(target_os = "macos")]
fn cwd_of(pid: u32) -> Option<Vec<u8>> {
    // SAFETY: an all-zero `proc_vnodepathinfo` is valid; the call fills it.
    let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
    let size = size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    // SAFETY: `info` is a writable buffer of the size passed.
    let got = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if got != size {
        return None;
    }
    // libc spells the 1024-byte path as 32 rows of 32.
    let path: Vec<u8> = info
        .pvi_cdir
        .vip_path
        .iter()
        .flatten()
        .map(|&c| c as u8)
        .take_while(|&b| b != 0)
        .collect();
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::process::CommandExt;

    #[test]
    fn the_foreground_of_a_terminal_is_named() {
        let pty = crate::pty::open(crate::pty::DEFAULT_SIZE).unwrap();
        let mut request = kablo_proto::v1::NewSession::default();
        request.argv = vec![b"/bin/sleep".to_vec(), b"5".to_vec()];
        request.cwd = Some(b"/".to_vec());
        let mut child = crate::pty::spawn(&pty.slave, &request).unwrap();
        let state = look(pty.master.as_fd(), child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(state.program.as_deref(), Some(&b"sleep"[..]));
        assert_eq!(state.cwd.as_deref(), Some(&b"/"[..]));
        // Not ssh, not asking for a password.
        assert_eq!(
            (state.remote, state.remote_port, state.secret),
            (None, None, false)
        );
    }

    #[test]
    fn the_foreground_is_told_with_the_process_it_is() {
        let pty = crate::pty::open(crate::pty::DEFAULT_SIZE).unwrap();
        let mut request = kablo_proto::v1::NewSession::default();
        request.argv = vec![b"/bin/sleep".to_vec(), b"5".to_vec()];
        let mut child = crate::pty::spawn(&pty.slave, &request).unwrap();
        let (state, pid) = Watch::default().look_with_pid(pty.master.as_fd(), child.id());
        // A program started on a terminal leads the group that has it.
        assert_eq!(pid, child.id());
        assert_eq!(state.program.as_deref(), Some(&b"sleep"[..]));
        child.kill().unwrap();
        child.wait().unwrap();
        // The leader is gone and the terminal names it still: the shell the
        // look was given stands in, and so does its number.
        let shell = std::process::id();
        let (_, pid) = Watch::default().look_with_pid(pty.master.as_fd(), shell);
        assert_eq!(pid, shell);
    }

    #[test]
    fn a_kernel_name_that_is_a_version_is_told_by_the_name_the_program_was_started_by() {
        let shown = |kernel: &str, arg0: Option<&str>| {
            String::from_utf8(shown_name(kernel.as_bytes(), arg0.map(str::as_bytes))).unwrap()
        };
        // A release kept under its version and started through a link.
        assert_eq!(
            shown("2.1.296", Some("/home/u/.local/bin/claude")),
            "claude"
        );
        assert_eq!(shown("2.1.296", Some("claude")), "claude");
        assert_eq!(shown("2", Some("./bin/tool")), "tool");
        // Any other name is the kernel's, and the first word does not change it.
        assert_eq!(shown("npm", Some("/usr/bin/other")), "npm");
        assert_eq!(shown("node", Some("claude")), "node");
        assert_eq!(shown("v2.1", Some("claude")), "v2.1");
        assert_eq!(shown("1.2a", Some("claude")), "1.2a");
        assert_eq!(shown("...", Some("claude")), "...");
        assert_eq!(shown("2.1.296-beta", Some("claude")), "2.1.296-beta");
        // The first word cannot be read, or says no name.
        assert_eq!(shown("2.1.296", None), "2.1.296");
        assert_eq!(shown("2.1.296", Some("")), "2.1.296");
        assert_eq!(shown("2.1.296", Some("/home/u/bin/")), "2.1.296");
        assert_eq!(shown("2.1.296", Some("/")), "2.1.296");
        // Longer than a file name, or not one word: a command line a process
        // wrote over its arguments, or text with a control character in it.
        let long = format!("/bin/{}", "c".repeat(MAX_LAUNCH_NAME + 1));
        assert_eq!(shown("2.1.296", Some(&long)), "2.1.296");
        let longest = format!("/bin/{}", "c".repeat(MAX_LAUNCH_NAME));
        assert_eq!(shown("2.1.296", Some(&longest)), &longest[5..]);
        assert_eq!(shown("2.1.296", Some("claude --resume x")), "2.1.296");
        assert_eq!(shown("2.1.296", Some("/bin/clau de")), "2.1.296");
        assert_eq!(shown("2.1.296", Some("/bin/claude\u{1b}[2J")), "2.1.296");
        assert_eq!(shown("2.1.296", Some("/bin/claude\u{7f}")), "2.1.296");
        assert_eq!(shown("2.1.296", Some("/bin/claude\u{9b}31m")), "2.1.296");
        assert_eq!(shown("2.1.296", Some("/bin/clau\u{a0}de")), "2.1.296");
        assert_eq!(shown("2.1.296", Some("/bin/cl\u{e4}ude")), "cl\u{e4}ude");
        // Not text either: the name is bytes, as the kernel's is.
        assert_eq!(
            shown_name(b"2.1.296", Some(b"/bin/cl\xffaude")),
            b"cl\xffaude"
        );
    }

    #[test]
    fn a_version_is_digits_and_dots_with_a_digit_among_them() {
        for name in ["2.1.296", "7", "0.0.1", "10.15", ".5", "1."] {
            assert!(is_version(name.as_bytes()), "{name}");
        }
        for name in [
            "",
            ".",
            "...",
            "v2",
            "1.2a",
            "2-1",
            "node",
            "python3.12",
            "1 2",
        ] {
            assert!(!is_version(name.as_bytes()), "{name}");
        }
    }

    #[test]
    fn the_first_word_is_read_only_for_a_version_and_only_once() {
        let own = std::process::id();
        let mut watch = Watch::default();
        // Not a version: the first word is not looked at, nor remembered.
        assert_eq!(watch.called(own, b"npm".to_vec()), b"npm");
        assert!(watch.called.is_none());
        // A version: the name the test binary was started by.
        let started = std::env::args().next().unwrap();
        let name = started.rsplit('/').next().unwrap().as_bytes().to_vec();
        assert_eq!(watch.called(own, b"2.1.296".to_vec()), name);
        assert!(watch.called.is_some());
        // What was read is told again without reading it: another process of
        // the same name is a new look.
        watch.called.as_mut().unwrap().shown = b"remembered".to_vec();
        assert_eq!(watch.called(own, b"2.1.296".to_vec()), b"remembered");
        // No process has this number (it is past the largest a system gives).
        assert_eq!(
            watch.called(2_147_483_647, b"2.1.296".to_vec()),
            b"2.1.296",
            "its first word cannot be read, so the kernel's name stands"
        );
        // A look that could not read stored nothing over what was there.
        assert_eq!(watch.called.as_ref().unwrap().pid, own);
        // A foreground that is not a version forgets it.
        assert_eq!(watch.called(own, b"sleep".to_vec()), b"sleep");
        assert!(watch.called.is_none());
    }

    #[test]
    fn the_first_word_of_a_command_line_is_the_one_it_was_started_with_and_no_more() {
        let own = arg0_of(std::process::id()).expect("the test's own first word");
        assert_eq!(own, std::env::args().next().unwrap().into_bytes());

        // A program given a name of its own and arguments that are not names.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg0("claude")
            .arg("30")
            .spawn()
            .unwrap();
        // The kernel may show the program it became a moment after `spawn` returns.
        let start = std::time::Instant::now();
        let got = loop {
            let got = arg0_of(child.id());
            if got.as_deref() == Some(&b"claude"[..])
                || start.elapsed() > std::time::Duration::from_secs(5)
            {
                break got;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let as_shown = {
            let mut watch = Watch::default();
            watch.called(child.id(), b"2.1.296".to_vec())
        };
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(got.as_deref(), Some(&b"claude"[..]));
        assert_eq!(as_shown, b"claude");
    }

    #[test]
    fn a_first_word_too_long_for_a_name_is_not_told() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg0("x".repeat(3000))
            .arg("30")
            .spawn()
            .unwrap();
        let got = arg0_of(child.id());
        let shown = {
            let mut watch = Watch::default();
            watch.called(child.id(), b"2.1.296".to_vec())
        };
        child.kill().unwrap();
        child.wait().unwrap();
        // Linux does not read so far; macOS does and the name is refused.
        assert!(got.is_none_or(|word| word.len() > MAX_LAUNCH_NAME));
        assert_eq!(shown, b"2.1.296");
    }

    #[test]
    fn the_first_argument_comes_out_of_a_procargs_buffer_and_nothing_after_it() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/ssh\0\0\0\0");
        buf.extend_from_slice(b"ssh\0-p\0\x32\0");
        buf.extend_from_slice(b"PASSWORD=hunter2\0HOME=/home/u\0");
        assert_eq!(parse_arg0(&buf), Some(&b"ssh"[..]));
        // The path the kernel ran is not the name the program was started by.
        let mut linked = 1i32.to_ne_bytes().to_vec();
        linked.extend_from_slice(b"/opt/claude/versions/2.1.296\0\0\0");
        linked.extend_from_slice(b"claude\0HOME=/home/u\0");
        assert_eq!(parse_arg0(&linked), Some(&b"claude"[..]));
        // No arguments, a buffer cut short, an empty first one.
        assert_eq!(parse_arg0(&0i32.to_ne_bytes()), None);
        assert_eq!(parse_arg0(&[1, 0]), None);
        let mut none = 0i32.to_ne_bytes().to_vec();
        none.extend_from_slice(b"/bin/ls\0\0HOME=/home/u\0");
        assert_eq!(parse_arg0(&none), None);
    }
}
