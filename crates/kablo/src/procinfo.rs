//! What is in the foreground of a session's terminal, for `status` and for
//! `state`: the name of the program and the directory it is in, whether the
//! terminal is being asked for a secret, and the target of the ssh or mosh
//! session the program is.
//!
//! The foreground process group of the terminal is the thing the user is
//! looking at (the shell at its prompt, or the editor the shell started), and
//! its leader's name and working directory say what that is. Linux reads
//! `/proc`, macOS asks libproc.
//!
//! **A command line is read only for a program that can be an ssh or mosh
//! session** (see [`is_candidate`]), never for any other, and only the target
//! leaves this module: the rest of the arguments (a `-i` key, a
//! `ProxyCommand`, the command run on the other side) are walked to find
//! where the target is and dropped. The environment is never read. A wrapper
//! that was handed a password on its command line (`sshpass -p …`) is not a
//! candidate and its arguments are not looked at.

use std::os::fd::BorrowedFd;

use kablo_proto::v1::TerminalState;

use crate::sys;

/// The longest target told to a client: a host name is at most 253 bytes, and
/// something longer is not a host name.
const MAX_TARGET: usize = 255;

/// The most of a command line read on Linux. Past it the line is cut, which
/// hides nothing the target needs: it comes before the arguments that are long.
#[cfg(target_os = "linux")]
const ARGV_LIMIT: u64 = 64 * 1024;

/// The target of an interactive ssh or mosh session.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Remote {
    /// As the user typed it; only the `ssh://` form is stripped of its scheme.
    host: Vec<u8>,
    /// The port asked of ssh, when it was asked.
    port: Option<u16>,
}

/// What the terminal looks like now, looked at from outside: the same answer
/// for every question that is put to it. See [`Watch`] for the one that asks
/// again and again.
pub fn look(master: BorrowedFd<'_>, shell_pid: u32) -> TerminalState {
    Watch::default().look(master, shell_pid)
}

/// A look at the terminal that remembers the command line it read, so that a
/// program that stays in the foreground while it streams output (an ssh
/// session) has its command line read once, not on every look.
#[derive(Default)]
pub struct Watch {
    seen: Option<Seen>,
}

/// The command line of a candidate and what it said.
struct Seen {
    pid: u32,
    name: Vec<u8>,
    remote: Option<Remote>,
}

impl Watch {
    /// The state of the terminal `master` belongs to; the shell `shell_pid`
    /// stands for the foreground when the terminal names no group.
    pub fn look(&mut self, master: BorrowedFd<'_>, shell_pid: u32) -> TerminalState {
        let group = sys::foreground_group(master).and_then(|group| u32::try_from(group).ok());
        let mut pid = group.unwrap_or(shell_pid);
        let mut program = name_of(pid);
        // The leader of the group is gone and the shell has not yet taken the
        // terminal back (a program that was interrupted): the shell is what
        // the person will find, and a state with nothing in it would only
        // flash.
        if program.is_none() && pid != shell_pid {
            pid = shell_pid;
            program = name_of(pid);
        }
        let remote = self.remote_of(pid, program.as_deref());
        let mut state = TerminalState::default();
        state.cwd = cwd_of(pid);
        state.secret = sys::hides_typing(master);
        state.remote_port = remote.as_ref().and_then(|remote| remote.port);
        state.remote = remote.map(|remote| remote.host);
        state.program = program;
        state
    }

    /// The remote session process `pid`, named `name`, is, if it is one. What
    /// was read of the same process under the same name is not read again; a
    /// foreground that is not a candidate forgets it.
    fn remote_of(&mut self, pid: u32, name: Option<&[u8]>) -> Option<Remote> {
        let Some(name) = name.filter(|name| is_candidate(name)) else {
            self.seen = None;
            return None;
        };
        if let Some(seen) = self
            .seen
            .as_ref()
            .filter(|seen| seen.pid == pid && seen.name == name)
        {
            return seen.remote.clone();
        }
        let remote = args_of(pid).and_then(|args| remote_target(name, &args));
        self.seen = Some(Seen {
            pid,
            name: name.to_vec(),
            remote: remote.clone(),
        });
        remote
    }
}

/// A name whose command line is worth reading: ssh, mosh's client, the mosh
/// script (Linux names a script by its file) and the Perl that runs it
/// (`/usr/bin/perl` `exec`s into `perl5.NN`, so by prefix).
fn is_candidate(name: &[u8]) -> bool {
    name == b"ssh" || name == b"mosh-client" || name == b"mosh" || name.starts_with(b"perl")
}

/// The target of a candidate's command line (`args[0]` is the program's own
/// name): `None` when it is not an interactive remote session, or its target
/// cannot be told.
fn remote_target(name: &[u8], args: &[String]) -> Option<Remote> {
    let rest = args.get(1..)?;
    let remote = match name {
        b"ssh" => ssh_target(rest)?,
        b"mosh-client" => mosh_client_target(rest)?,
        _ => {
            // An interpreter: the first argument that is not an option is the
            // script, and only the mosh script is a remote session.
            let script = rest.iter().position(|arg| !arg.starts_with('-'))?;
            let (path, script_args) = rest.get(script..)?.split_first()?;
            if path.rsplit('/').next() != Some("mosh") {
                return None;
            }
            Remote {
                host: mosh_target(script_args)?.into_bytes(),
                port: None,
            }
        }
    };
    let host = without_secret(String::from_utf8_lossy(&remote.host).into_owned()).into_bytes();
    (!host.is_empty() && host.len() <= MAX_TARGET).then_some(Remote { host, ..remote })
}

/// ssh's short options that take a value (`ssh(1)`'s SYNOPSIS).
const SSH_VALUED: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// Options whose presence makes the session non-interactive: tunnel (`-N`),
/// forwarding (`-W`), control (`-O`), query (`-Q`, `-G`, `-V`) and no pty (`-T`).
const SSH_NON_INTERACTIVE: &str = "NWOQGVT";

/// What a walk of an ssh command line learns on the way to the target.
#[derive(Default)]
struct SshWalk {
    /// `-t` or `RequestTTY=yes|force`.
    tty: bool,
    /// A non-interactive mode: [`SSH_NON_INTERACTIVE`], `RequestTTY=no` or
    /// `SessionType=none|subsystem`.
    quiet: bool,
    /// The port ssh will use: the first of `-p`, `-o Port=` and the URI's, in
    /// the order ssh reads them (each is taken only while no port is set).
    port: Option<u16>,
}

/// ssh's arguments (without the program's name) → the target of the
/// interactive session they start.
///
/// With a command after the target the session is not interactive unless `-t`
/// forces a terminal: `ssh prod uptime` is a one-second command.
fn ssh_target(args: &[String]) -> Option<Remote> {
    let mut walk = SshWalk::default();
    let (index, terminated) = walk.options(args, 0);
    let target = args.get(index)?;
    // The URI's port comes after the options before the target and before
    // those after it.
    let (host, uri_port) = ssh_host(target);
    walk.port = walk.port.or(uri_port);
    // OpenSSH parses options **again** after the target (`ssh prod -p 2222`);
    // it does not if they ended with `--`.
    let rest = if terminated {
        index + 1
    } else {
        walk.options(args, index + 1).0
    };
    let command = rest < args.len();
    if walk.quiet || (command && !walk.tty) {
        return None;
    }
    Some(Remote {
        host: host.into_bytes(),
        port: walk.port,
    })
}

impl SshWalk {
    /// Reads the options in `args[index..]`; returns the index of the first
    /// argument that is not one and whether the options ended with `--`.
    fn options(&mut self, args: &[String], mut index: usize) -> (usize, bool) {
        while let Some(arg) = args.get(index) {
            if arg == "--" {
                return (index + 1, true);
            }
            let Some(cluster) = arg.strip_prefix('-').filter(|rest| !rest.is_empty()) else {
                break;
            };
            index += 1;
            for (at, flag) in cluster.char_indices() {
                if SSH_NON_INTERACTIVE.contains(flag) {
                    self.quiet = true;
                }
                if flag == 't' {
                    self.tty = true;
                }
                if !SSH_VALUED.contains(flag) {
                    continue;
                }
                // The value is the rest of the cluster (`-p22`) or the next
                // argument, and it ends the cluster.
                let attached = cluster.get(at + flag.len_utf8()..).unwrap_or_default();
                let value = if attached.is_empty() {
                    index += 1;
                    args.get(index - 1).map(String::as_str)
                } else {
                    Some(attached)
                };
                match (flag, value) {
                    ('o', Some(value)) => self.config(value),
                    ('p', Some(value)) => self.port = self.port.or(port_of(value)),
                    _ => {}
                }
                break;
            }
        }
        (index, false)
    }

    /// `-o Key=value` (or with a space): the keys that matter here.
    fn config(&mut self, option: &str) {
        let (key, value) = option
            .split_once(['=', ' ', '\t'])
            .map_or((option, ""), |(key, value)| {
                (key, value.trim_start_matches(['=', ' ', '\t']).trim_end())
            });
        if key.eq_ignore_ascii_case("RequestTTY") {
            if value.eq_ignore_ascii_case("yes") || value.eq_ignore_ascii_case("force") {
                self.tty = true;
            } else if value.eq_ignore_ascii_case("no") {
                self.quiet = true;
            }
        } else if key.eq_ignore_ascii_case("SessionType") {
            if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("subsystem") {
                self.quiet = true;
            }
        } else if key.eq_ignore_ascii_case("Port") {
            self.port = self.port.or(port_of(value));
        }
    }
}

/// A port written as ssh takes it: decimal digits, not zero.
fn port_of(text: &str) -> Option<u16> {
    let text = text.trim();
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|&port| port != 0)
}

/// The target as written, and the port it carries. Only the `ssh://` form is
/// taken apart: the scheme and the port go, the host and the user stay.
fn ssh_host(target: &str) -> (String, Option<u16>) {
    let Some(rest) = target.strip_prefix("ssh://") else {
        return (target.to_owned(), None);
    };
    let rest = rest.split('/').next().unwrap_or_default();
    let (user, host) = match rest.rsplit_once('@') {
        Some((user, host)) => (Some(user), host),
        None => (None, rest),
    };
    let (host, port) = match host.strip_prefix('[') {
        // IPv6 is in square brackets; the port comes after the closing one.
        Some(inner) => {
            let (name, after) = inner.split_once(']').unwrap_or((inner, ""));
            (name, after.strip_prefix(':').and_then(port_of))
        }
        None => match host.split_once(':') {
            Some((name, port)) => (name, port_of(port)),
            None => (host, None),
        },
    };
    let host = match user {
        Some(user) => format!("{user}@{host}"),
        None => host.to_owned(),
    };
    (host, port)
}

/// The target without what follows a `:` in its user: a password has no place
/// in a target, in any form, but would be one if someone wrote it there
/// (`ssh://user:secret@host`, and `user:secret@host` for mosh and for ssh,
/// which takes the whole of it as a user name).
fn without_secret(target: String) -> String {
    match target.rsplit_once('@') {
        Some((user, host)) if user.contains(':') => {
            format!("{}@{host}", user.split(':').next().unwrap_or_default())
        }
        _ => target,
    }
}

/// The mosh script's options that take a value as a separate argument (the
/// `=` form like `--ssh=…` is already a single argument).
const MOSH_VALUED: [&str; 9] = [
    "-p",
    "--port",
    "--ssh",
    "--server",
    "--client",
    "--predict",
    "--family",
    "--bind-server",
    "--experimental-remote-ip",
];

/// The mosh script's arguments → its target: the first argument that is not an
/// option. A remote command runs in an interactive terminal in mosh too, so
/// it does not disqualify. (`-p` is mosh's UDP port, not ssh's, and is not a
/// `remote_port`.)
fn mosh_target<S: AsRef<str>>(args: &[S]) -> Option<String> {
    mosh_positionals(args)
        .first()
        .map(|word| (*word).to_owned())
}

/// The arguments that are not options (or their values), up to a `--`; the one
/// after a `--` that comes first is the target.
fn mosh_positionals<S: AsRef<str>>(args: &[S]) -> Vec<&str> {
    let mut found = Vec::new();
    let mut words = args.iter().map(AsRef::as_ref);
    while let Some(word) = words.next() {
        if word == "--" {
            if found.is_empty() {
                found.extend(words.next());
            }
            break;
        }
        if !word.starts_with('-') {
            found.push(word);
        } else if MOSH_VALUED.contains(&word) {
            words.next();
        }
    }
    found
}

/// `mosh-client`'s arguments → its target. The script passes the client its
/// own command line in a single argument (`"-# {argv} |"`), so the target
/// comes from reading that line as mosh would.
fn mosh_client_target(args: &[String]) -> Option<Remote> {
    let at = args.iter().position(|arg| arg.starts_with("-#"))?;
    let mut line = args.get(at)?.strip_prefix("-#").unwrap_or_default().trim();
    if line.is_empty() {
        line = args.get(at + 1)?.trim();
    }
    let line = line.strip_suffix('|').unwrap_or(line);
    // The line was joined without quoting: `--ssh="ssh -i ~/.ssh/k"` is split
    // into words and its value leaves words behind. A word with a character
    // that cannot be in a host is skipped.
    let words: Vec<&str> = line
        .split_whitespace()
        .filter(|word| !word.contains(['/', '=', '~']))
        .collect();
    let positional = mosh_positionals(&words);
    // The value of `--ssh` or `--server` is a command line of any number of
    // words, and nothing in the joined line tells where it ends: a word that
    // is not an option (`-i mykey`) is not told from the target. Such a line is
    // read only when it leaves one word to be the target.
    let open_value = line.split_whitespace().any(|word| {
        ["--ssh", "--server"].iter().any(|option| {
            word == *option
                || word
                    .strip_prefix(option)
                    .is_some_and(|rest| rest.starts_with('='))
        })
    });
    if open_value && positional.len() != 1 {
        return None;
    }
    Some(Remote {
        host: positional.first()?.as_bytes().to_vec(),
        port: None,
    })
}

/// The words of a command line as the kernel keeps it: arguments each ended by
/// a NUL.
#[cfg(target_os = "linux")]
fn words(raw: &[u8]) -> Vec<String> {
    raw.strip_suffix(&[0])
        .unwrap_or(raw)
        .split(|&b| b == 0)
        .map(|word| String::from_utf8_lossy(word).into_owned())
        .collect()
}

#[cfg(target_os = "linux")]
fn args_of(pid: u32) -> Option<Vec<String>> {
    use std::io::Read;
    let mut raw = Vec::new();
    std::fs::File::open(format!("/proc/{pid}/cmdline"))
        .ok()?
        .take(ARGV_LIMIT)
        .read_to_end(&mut raw)
        .ok()?;
    (!raw.is_empty()).then(|| words(&raw))
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

/// `sysctl(KERN_PROCARGS2)`: the arguments of a process of the same user.
#[cfg(target_os = "macos")]
fn args_of(pid: u32) -> Option<Vec<String>> {
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
    parse_procargs(buf.get(..len)?)
}

/// The pure half of [`args_of`] on macOS: the layout is `argc` (4 bytes), the
/// executable's path, NUL padding and `argc` arguments each ended by a NUL.
/// **The environment follows the arguments in the same buffer, and the walk
/// stops at the last argument**: nothing past it is looked at, let alone kept.
#[cfg(any(target_os = "macos", test))]
fn parse_procargs(buf: &[u8]) -> Option<Vec<String>> {
    let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
    let rest = buf.get(4..)?;
    // The path, then padding: argv[0] cannot be empty, so the first byte that
    // is not NUL after the path is where the arguments start.
    let path_end = rest.iter().position(|&b| b == 0)?;
    let start = path_end + rest.get(path_end..)?.iter().position(|&b| b != 0)?;
    let mut fields = rest.get(start..)?.split(|&b| b == 0);
    let mut args = Vec::new();
    for _ in 0..argc {
        args.push(String::from_utf8_lossy(fields.next()?).into_owned());
    }
    (!args.is_empty()).then_some(args)
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

    fn argv(line: &str) -> Vec<String> {
        line.split(' ').map(str::to_owned).collect()
    }

    fn remote(name: &str, line: &str) -> Option<(String, Option<u16>)> {
        remote_target(name.as_bytes(), &argv(line))
            .map(|remote| (String::from_utf8(remote.host).unwrap(), remote.port))
    }

    fn remote_in(name: &str, args: &[&str]) -> Option<(String, Option<u16>)> {
        let args: Vec<String> = args.iter().map(|&arg| arg.to_owned()).collect();
        remote_target(name.as_bytes(), &args)
            .map(|remote| (String::from_utf8(remote.host).unwrap(), remote.port))
    }

    fn host(host: &str) -> Option<(String, Option<u16>)> {
        Some((host.to_owned(), None))
    }

    fn port(host: &str, port: u16) -> Option<(String, Option<u16>)> {
        Some((host.to_owned(), Some(port)))
    }

    #[test]
    fn an_interactive_ssh_is_a_remote_session_by_its_target() {
        assert_eq!(remote("ssh", "ssh prod"), host("prod"));
        assert_eq!(
            remote("ssh", "/usr/bin/ssh deploy@10.0.0.5"),
            host("deploy@10.0.0.5")
        );
        // Options that take a value, attached or apart, and flags in a cluster.
        assert_eq!(
            remote("ssh", "ssh -v -i key -J jump -4 deploy@10.0.0.5"),
            host("deploy@10.0.0.5")
        );
        assert_eq!(remote("ssh", "ssh -vi key -4C prod"), host("prod"));
        assert_eq!(remote("ssh", "ssh -o User=x -- prod"), host("prod"));
        // OpenSSH reads options again after the target, unless `--` ended them.
        assert_eq!(remote("ssh", "ssh prod -v"), host("prod"));
        assert_eq!(remote("ssh", "ssh -- prod -v"), None);
    }

    #[test]
    fn the_port_ssh_was_asked_for_is_told_apart_from_the_target() {
        assert_eq!(
            remote("ssh", "ssh -o BatchMode=yes user@host -p 2222"),
            port("user@host", 2222)
        );
        assert_eq!(remote("ssh", "ssh -p2222 -4v prod"), port("prod", 2222));
        assert_eq!(remote("ssh", "ssh prod -p 22"), port("prod", 22));
        assert_eq!(remote("ssh", "ssh -oPort=2200 prod"), port("prod", 2200));
        assert_eq!(remote("ssh", "ssh -o Port=2200 prod"), port("prod", 2200));
        assert_eq!(
            remote_in("ssh", &["ssh", "-o", "Port 2200", "prod"]),
            port("prod", 2200)
        );
        // ssh takes a port only while none is set, whichever way it is given:
        // the first of them wins (`ssh -G` says so).
        assert_eq!(remote("ssh", "ssh -p 1 -p 2 prod"), port("prod", 1));
        assert_eq!(
            remote("ssh", "ssh -o Port=1 -o Port=2 prod"),
            port("prod", 1)
        );
        assert_eq!(remote("ssh", "ssh -o Port=1 -p 2 prod"), port("prod", 1));
        assert_eq!(remote("ssh", "ssh -p 2 -o Port=1 prod"), port("prod", 2));
        assert_eq!(remote("ssh", "ssh prod -p 1 -p 2"), port("prod", 1));
        // Not a port: ssh refuses it and the session never starts.
        assert_eq!(remote("ssh", "ssh -p 0 prod"), host("prod"));
        assert_eq!(remote("ssh", "ssh -p http prod"), host("prod"));
        assert_eq!(remote("ssh", "ssh -p 99999 prod"), host("prod"));
    }

    #[test]
    fn an_ssh_uri_loses_its_scheme_and_its_port_and_keeps_its_user() {
        assert_eq!(
            remote("ssh", "ssh ssh://deploy@h:2222"),
            port("deploy@h", 2222)
        );
        assert_eq!(remote("ssh", "ssh ssh://h"), host("h"));
        assert_eq!(remote("ssh", "ssh ssh://h/path"), host("h"));
        assert_eq!(remote("ssh", "ssh ssh://[::1]:22"), port("::1", 22));
        assert_eq!(remote("ssh", "ssh ssh://u@[fe80::1]"), host("u@fe80::1"));
        // The URI's port is read after the options before it and before those
        // after it; the first port set stays.
        assert_eq!(remote("ssh", "ssh -p 1 ssh://h:2"), port("h", 1));
        assert_eq!(remote("ssh", "ssh ssh://h:2 -p 1"), port("h", 2));
        assert_eq!(remote("ssh", "ssh ssh://h -p 1"), port("h", 1));
        // Written without a scheme the target is the user's own text.
        assert_eq!(remote("ssh", "ssh user@fe80::1"), host("user@fe80::1"));
    }

    #[test]
    fn a_password_in_a_target_is_not_part_of_it_in_any_form() {
        assert_eq!(
            remote("ssh", "ssh ssh://deploy:hunter2@h:22"),
            port("deploy@h", 22)
        );
        assert_eq!(remote("ssh", "ssh deploy:hunter2@h"), host("deploy@h"));
        assert_eq!(
            remote("ssh", "ssh deploy:hunter2@fe80::1"),
            host("deploy@fe80::1")
        );
        assert_eq!(
            remote("perl", "perl /usr/bin/mosh deploy:hunter2@h"),
            host("deploy@h")
        );
        assert_eq!(
            remote_in(
                "mosh-client",
                &["mosh-client", "-# deploy:hunter2@h |", "1.2.3.4", "1"]
            ),
            host("deploy@h")
        );
        // The password may hold an `@`.
        assert_eq!(remote("ssh", "ssh deploy:p@ss@h"), host("deploy@h"));
    }

    #[test]
    fn what_asks_for_no_terminal_or_no_session_is_not_remote() {
        // A command on the other side without `-t`.
        assert_eq!(remote("ssh", "ssh prod uptime"), None);
        assert_eq!(remote("ssh", "ssh prod -p 2222 uptime"), None);
        assert_eq!(remote("ssh", "ssh -t prod tmux"), host("prod"));
        assert_eq!(
            remote("ssh", "ssh -o RequestTTY=force prod tmux"),
            host("prod")
        );
        assert_eq!(remote("ssh", "ssh -o requesttty=no prod"), None);
        // Tunnels, control, queries and no session.
        assert_eq!(remote("ssh", "ssh -N -L 1:x:1 prod"), None);
        assert_eq!(remote("ssh", "ssh -O check prod"), None);
        assert_eq!(remote("ssh", "ssh -W host:22 jump"), None);
        assert_eq!(remote("ssh", "ssh -G prod"), None);
        assert_eq!(remote("ssh", "ssh -oSessionType=none prod"), None);
        assert_eq!(remote("ssh", "ssh -T prod"), None);
        // No target, or one that is not a host name.
        assert_eq!(remote("ssh", "ssh"), None);
        assert_eq!(remote("ssh", "ssh -v"), None);
        let long = format!("ssh {}", "h".repeat(MAX_TARGET + 1));
        assert_eq!(remote("ssh", &long), None);
    }

    #[test]
    fn the_mosh_script_and_its_client_name_the_target_and_no_port() {
        // macOS runs the script by its interpreter, Linux names it by its file.
        assert_eq!(
            remote("perl5.34", "perl5.34 /usr/bin/mosh prod"),
            host("prod")
        );
        assert_eq!(
            remote("perl", "perl -w /opt/bin/mosh deploy@prod"),
            host("deploy@prod")
        );
        assert_eq!(
            remote("mosh", "perl /usr/bin/mosh --ssh=ssh -p 60001 prod"),
            host("prod")
        );
        assert_eq!(
            remote("perl", "perl /usr/bin/mosh --port 60001 prod -- tmux"),
            host("prod")
        );
        assert_eq!(remote("perl", "perl /usr/bin/mosh -- prod"), host("prod"));
        // The script gives the client its own line as one argument.
        assert_eq!(
            remote_in(
                "mosh-client",
                &["mosh-client", "-# prod |", "10.0.0.5", "60001"]
            ),
            host("prod")
        );
        assert_eq!(
            remote_in(
                "mosh-client",
                &[
                    "mosh-client",
                    "-# -p 60001 --ssh=ssh deploy@prod |",
                    "10.0.0.5",
                    "60001"
                ]
            ),
            host("deploy@prod")
        );
        // The line was joined without quotes: the words of the `--ssh` value
        // are not the target.
        assert_eq!(
            remote_in(
                "mosh-client",
                &[
                    "mosh-client",
                    "-# --ssh=ssh -i ~/.ssh/k -o Port=2 prod |",
                    "10.0.0.5",
                    "1"
                ]
            ),
            host("prod")
        );
        // The value of `--ssh` is any number of words: with one that is not
        // an option (`mykey`, `bob`) left beside the target there is no telling
        // which is the target, and none is told.
        for line in [
            "-# --ssh=ssh -i mykey prod |",
            "-# --ssh=ssh -l bob prod |",
            "-# --ssh ssh -l bob prod |",
            "-# --server=mosh-server new -s prod |",
            "-# --ssh=ssh -i mykey prod -- tmux |",
        ] {
            assert_eq!(
                remote_in("mosh-client", &["mosh-client", line, "10.0.0.5", "1"]),
                None,
                "{line}"
            );
        }
        // Where it leaves the target alone it is read.
        assert_eq!(
            remote_in(
                "mosh-client",
                &["mosh-client", "-# --ssh=ssh -v prod |", "10.0.0.5", "1"]
            ),
            host("prod")
        );
        // Perl, but not mosh; a client without the line it is started with.
        assert_eq!(remote("perl", "perl script.pl prod"), None);
        assert_eq!(remote("perl", "perl"), None);
        assert_eq!(
            remote_in("mosh-client", &["mosh-client", "10.0.0.5", "60001"]),
            None
        );
    }

    #[test]
    fn only_the_programs_that_can_be_remote_have_their_command_line_read() {
        for name in ["ssh", "mosh-client", "mosh", "perl", "perl5.34"] {
            assert!(is_candidate(name.as_bytes()), "{name}");
        }
        // A wrapper that was given a password is not looked into.
        for name in [
            "sshpass",
            "sh",
            "zsh",
            "sleep",
            "sshd",
            "ssh-agent",
            "moshi",
            "python3",
        ] {
            assert!(!is_candidate(name.as_bytes()), "{name}");
        }
        let mut watch = Watch::default();
        assert_eq!(watch.remote_of(std::process::id(), Some(b"sshpass")), None);
        assert!(watch.seen.is_none());
    }

    #[test]
    fn a_command_line_is_read_back_as_it_was_started() {
        let args = args_of(std::process::id()).expect("the test's own command line");
        assert_eq!(args, std::env::args().collect::<Vec<_>>());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_arguments_of_a_process_are_split_at_the_nuls() {
        assert_eq!(words(b"ssh\0-p\x002222\0host\0"), argv("ssh -p 2222 host"));
        // An empty argument is an argument.
        assert_eq!(words(b"a\0\0b\0"), vec!["a", "", "b"]);
    }

    #[test]
    fn the_arguments_come_out_of_a_procargs_buffer_and_the_environment_stays_in() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/ssh\0\0\0\0");
        buf.extend_from_slice(b"ssh\0-p\0\x32\0");
        buf.extend_from_slice(b"PASSWORD=hunter2\0HOME=/home/u\0");
        let args = parse_procargs(&buf).unwrap();
        assert_eq!(args, vec!["ssh", "-p", "2"]);
        // Fewer arguments than it says, a buffer cut short, no arguments.
        let mut short = 5i32.to_ne_bytes().to_vec();
        short.extend_from_slice(b"/bin/ls\0\0ls\0-l\0");
        assert_eq!(parse_procargs(&short), None);
        assert_eq!(parse_procargs(&[1, 0]), None);
        assert_eq!(parse_procargs(&0i32.to_ne_bytes()), None);
    }
}
