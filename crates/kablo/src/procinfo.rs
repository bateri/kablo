//! What is in the foreground of a session's terminal: the name of the program
//! and the directory it is in, for `status`.
//!
//! The foreground process group of the terminal is the thing the user is
//! looking at (the shell at its prompt, or the editor the shell started), and
//! its leader's name and working directory say what that is. Linux reads
//! `/proc`, macOS asks libproc.

use std::os::fd::BorrowedFd;

use crate::sys;

/// The foreground program of a terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Foreground {
    /// Its name.
    pub program: Option<Vec<u8>>,
    /// Its working directory.
    pub cwd: Option<Vec<u8>>,
}

/// The foreground program of the terminal `master` belongs to; the shell
/// `shell_pid` when the terminal names no group.
pub fn foreground(master: BorrowedFd<'_>, shell_pid: u32) -> Foreground {
    let pid = sys::foreground_group(master)
        .and_then(|group| u32::try_from(group).ok())
        .unwrap_or(shell_pid);
    Foreground {
        program: name_of(pid),
        cwd: cwd_of(pid),
    }
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
        let request = kablo_proto::v1::NewSession {
            argv: vec![b"/bin/sleep".to_vec(), b"5".to_vec()],
            cwd: Some(b"/".to_vec()),
            ..Default::default()
        };
        let mut child = crate::pty::spawn(&pty.slave, &request).unwrap();
        let info = foreground(pty.master.as_fd(), child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(info.program.as_deref(), Some(&b"sleep"[..]));
        assert_eq!(info.cwd.as_deref(), Some(&b"/"[..]));
    }
}
